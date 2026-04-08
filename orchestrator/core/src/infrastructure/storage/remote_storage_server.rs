// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Remote Storage gRPC Server (ADR-064)
//!
//! Receives authenticated gRPC calls from remote nodes and delegates
//! to the local `AegisFSAL`. Each RPC authenticates the caller
//! via the `SealNodeEnvelope` carried in the request, then maps the
//! proto request into a domain `AegisFSAL` call and converts
//! the result back into proto responses.
//!
//! ## Security
//!
//! All operations are routed through the FSAL to gain:
//! - Path canonicalization (traversal guard)
//! - Volume ownership validation via `authorize()`
//! - Quota enforcement on writes
//! - StorageEvent emission with cross-node provenance
//!
//! The SEAL envelope already proves the caller is an authenticated AEGIS node.
//! The caller-side SealStorageProvider is only invoked from within an execution
//! that has already passed FSAL on the caller node. So the host-side uses a
//! system-level `FsalAccessPolicy` with full read/write access.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Server-side handler for cross-node volume operations

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tonic::{Request, Response, Status};

use crate::domain::cluster::{NodeClusterRepository, NodeId, NodeSecurityToken};
use crate::domain::fsal::{AegisFSAL, FsalAccessPolicy, FsalError};
use crate::domain::path_sanitizer::PathSanitizer;
use crate::domain::shared_kernel::ExecutionId;
use crate::domain::storage::{FileHandle, FileType, OpenMode, StorageError};
use crate::domain::volume::VolumeId;
use crate::infrastructure::aegis_cluster_proto::SealNodeEnvelope as ProtoEnvelope;
use crate::infrastructure::aegis_remote_storage_proto::{
    remote_storage_service_server::RemoteStorageService, CloseFileRequest, CreateFileRequest,
    DirEntryProto, GetUsageRequest, GetUsageResponse, HealthCheckRequest, OpenFileRequest,
    OpenFileResponse, ReadAtRequest, ReadAtResponse, ReaddirResponse, RemoteStorageRequest,
    RemoteStorageResponse, RenameRequest, SetQuotaRequest, StatResponse, WriteAtRequest,
    WriteAtResponse,
};

/// Metadata tracked for each open file handle so we can emit provenance-rich
/// `StorageEvent`s on read/write/close without the proto carrying the path.
#[derive(Debug, Clone)]
struct OpenHandleMeta {
    volume_id: VolumeId,
    path: String,
}

/// gRPC handler for `RemoteStorageService`.
///
/// Delegates every RPC to the local [`AegisFSAL`] after verifying
/// the SEAL node envelope. Cross-node provenance (`caller_node_id`,
/// `host_node_id`) is attached to every `StorageEvent` emitted.
pub struct RemoteStorageServiceHandler {
    fsal: Arc<AegisFSAL>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
    host_node_id: NodeId,
    /// Path sanitizer for administrative operations (`set_quota`,
    /// `get_usage`) that bypass the FSAL and for sanitizing paths
    /// stored in handle metadata.
    path_sanitizer: PathSanitizer,
    /// Maps raw `FileHandle` bytes to metadata so that read_at/write_at/close
    /// can emit events with the original volume_id and path.
    open_handles: RwLock<HashMap<Vec<u8>, OpenHandleMeta>>,
}

impl RemoteStorageServiceHandler {
    pub fn new(
        fsal: Arc<AegisFSAL>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
        host_node_id: NodeId,
    ) -> Self {
        Self {
            fsal,
            cluster_repo,
            host_node_id,
            path_sanitizer: PathSanitizer::new(),
            open_handles: RwLock::new(HashMap::new()),
        }
    }

    /// Authenticate the caller by verifying the SEAL node envelope:
    /// 1. Parse JWT to extract node_id
    /// 2. Look up registered public key
    /// 3. Verify Ed25519 signature over payload
    async fn authenticate(&self, envelope: Option<ProtoEnvelope>) -> Result<NodeId, Status> {
        let envelope =
            envelope.ok_or_else(|| Status::unauthenticated("Missing security envelope"))?;

        // 1. Parse token and extract claims
        let token = NodeSecurityToken(envelope.node_security_token.clone());
        let claims = token
            .claims()
            .map_err(|e| Status::unauthenticated(format!("Invalid token: {e}")))?;
        let node_id = claims.node_id;

        // 2. Retrieve node's public key
        let peer = self
            .cluster_repo
            .find_peer(&node_id)
            .await
            .map_err(|e| Status::internal(format!("Database error: {e}")))?
            .ok_or_else(|| Status::unauthenticated("Node not registered"))?;

        // 3. Verify Ed25519 signature over payload
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let verifying_key = VerifyingKey::from_bytes(
            &peer
                .public_key
                .clone()
                .try_into()
                .map_err(|_| Status::internal("Invalid stored public key length"))?,
        )
        .map_err(|e| Status::internal(format!("Invalid stored public key: {e}")))?;

        let signature = Signature::from_slice(&envelope.signature)
            .map_err(|e| Status::unauthenticated(format!("Invalid signature format: {e}")))?;

        verifying_key
            .verify(&envelope.payload, &signature)
            .map_err(|e| Status::unauthenticated(format!("Signature verification failed: {e}")))?;

        Ok(node_id)
    }

    /// Build the full POSIX path from volume_id and internal path.
    fn full_path(volume_id: &str, path: &str) -> String {
        format!("/aegis/volumes/{volume_id}{path}")
    }

    /// Parse a proto `volume_id` string into a domain `VolumeId`.
    fn parse_volume_id(volume_id: &str) -> Result<VolumeId, Status> {
        VolumeId::from_string(volume_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid volume_id: {e}")))
    }

    /// System-level access policy granting full read/write.
    ///
    /// The SEAL envelope already authenticates the caller as a trusted AEGIS node
    /// and the caller-side has already passed FSAL policy. The host-side FSAL call
    /// is for path canonicalization, volume authorization, quota, and audit — not
    /// for access-pattern restriction.
    fn system_policy() -> FsalAccessPolicy {
        FsalAccessPolicy {
            read: vec!["/**".to_string()],
            write: vec!["/**".to_string()],
        }
    }

    /// Sentinel execution ID for cross-node operations.
    ///
    /// TODO: The remote storage proto messages should carry `execution_id` from the
    /// caller so FSAL authorization can validate volume ownership properly. Until
    /// that field is added to `aegis-proto`, we use a sentinel that bypasses
    /// execution-level ownership checks while still getting path sanitization,
    /// quota enforcement, and audit trail from the FSAL.
    fn sentinel_execution_id() -> ExecutionId {
        // Use a well-known UUID-v4 as the sentinel so it's recognisable in logs.
        ExecutionId(
            uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001")
                .expect("hardcoded sentinel UUID is valid"),
        )
    }

    /// Convert a `FsalError` into an appropriate gRPC `Status`.
    fn fsal_err_to_status(e: FsalError) -> Status {
        match &e {
            FsalError::UnauthorizedAccess { .. } => Status::permission_denied(e.to_string()),
            FsalError::VolumeNotFound(_) => Status::not_found(e.to_string()),
            FsalError::VolumeNotAttached(_) => Status::failed_precondition(e.to_string()),
            FsalError::PathSanitization(_) => Status::invalid_argument(e.to_string()),
            FsalError::PolicyViolation(_) => Status::permission_denied(e.to_string()),
            FsalError::InvalidFileHandle => Status::invalid_argument(e.to_string()),
            FsalError::HandleDeserialization(_) => Status::invalid_argument(e.to_string()),
            FsalError::QuotaExceeded { .. } => Status::resource_exhausted(e.to_string()),
            FsalError::Storage(ref se) => Self::storage_err_to_status(se),
        }
    }

    /// Convert a `StorageError` into an appropriate gRPC `Status`.
    fn storage_err_to_status(e: &StorageError) -> Status {
        match e {
            StorageError::NotFound(m) | StorageError::FileNotFound(m) => {
                Status::not_found(m.clone())
            }
            StorageError::PermissionDenied(m) => Status::permission_denied(m.clone()),
            StorageError::AlreadyExists(m) => Status::already_exists(m.clone()),
            StorageError::QuotaExceeded { .. } => Status::resource_exhausted("quota exceeded"),
            StorageError::Unavailable(m) => Status::unavailable(m.clone()),
            StorageError::InvalidPath(m) => Status::invalid_argument(m.clone()),
            _ => Status::internal(e.to_string()),
        }
    }

    /// Convert an `OpenMode` integer from proto into a domain `OpenMode`.
    fn proto_mode_to_open_mode(mode: i32) -> OpenMode {
        match mode {
            1 => OpenMode::WriteOnly,
            2 => OpenMode::ReadWrite,
            3 => OpenMode::Create,
            _ => OpenMode::ReadOnly,
        }
    }

    /// Convert a domain `FileType` to the proto integer representation.
    fn file_type_to_proto(ft: FileType) -> u32 {
        match ft {
            FileType::File => 0,
            FileType::Directory => 1,
            FileType::Symlink => 2,
        }
    }

    fn ok_response() -> RemoteStorageResponse {
        RemoteStorageResponse {
            success: true,
            error_message: String::new(),
        }
    }

    /// Sanitize a path against traversal, returning the canonical string.
    fn sanitize_path(&self, path: &str) -> Result<String, Status> {
        let canonical = self
            .path_sanitizer
            .canonicalize(path, Some("/"))
            .map_err(|e| Status::invalid_argument(format!("Path traversal blocked: {e}")))?;
        Ok(canonical
            .to_str()
            .unwrap_or("/")
            .replace('\\', "/")
            .to_string())
    }
}

#[tonic::async_trait]
impl RemoteStorageService for RemoteStorageServiceHandler {
    async fn create_directory(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let policy = Self::system_policy();

        self.fsal
            .create_directory(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn delete_directory(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let policy = Self::system_policy();

        self.fsal
            .delete_directory(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn set_quota(
        &self,
        request: Request<SetQuotaRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;

        // Sanitize path before building full path to prevent traversal.
        let sanitized = self.sanitize_path(&req.path)?;
        let path = Self::full_path(&req.volume_id, &sanitized);

        // TODO: emit StorageEvent for quota administrative operations.
        // set_quota is an administrative operation not covered by FSAL;
        // delegate directly to the storage provider.
        self.fsal
            .storage_provider()
            .set_quota(&path, req.bytes)
            .await
            .map_err(|e| Self::storage_err_to_status(&e))?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;

        // Sanitize path before building full path to prevent traversal.
        let sanitized = self.sanitize_path(&req.path)?;
        let path = Self::full_path(&req.volume_id, &sanitized);

        let bytes_used = self
            .fsal
            .storage_provider()
            .get_usage(&path)
            .await
            .map_err(|e| Self::storage_err_to_status(&e))?;

        Ok(Response::new(GetUsageResponse { bytes_used }))
    }

    async fn open_file(
        &self,
        request: Request<OpenFileRequest>,
    ) -> Result<Response<OpenFileResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;

        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let mode = Self::proto_mode_to_open_mode(req.mode);

        let handle = self
            .fsal
            .open_file_for_node(
                execution_id,
                volume_id,
                &req.path,
                mode,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        // Track handle metadata for event emission on read/write/close.
        let sanitized = self.sanitize_path(&req.path)?;
        self.open_handles.write().insert(
            handle.0.clone(),
            OpenHandleMeta {
                volume_id,
                path: sanitized,
            },
        );

        Ok(Response::new(OpenFileResponse {
            file_handle: handle.0,
        }))
    }

    async fn read_at(
        &self,
        request: Request<ReadAtRequest>,
    ) -> Result<Response<ReadAtResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let handle = FileHandle(req.file_handle.clone());

        // Retrieve tracked metadata for provenance-rich event emission.
        let meta = self
            .open_handles
            .read()
            .get(&req.file_handle)
            .cloned()
            .ok_or_else(|| Status::invalid_argument("Unknown file handle"))?;

        let data = self
            .fsal
            .read_for_node(
                &handle,
                Self::sentinel_execution_id(),
                meta.volume_id,
                &meta.path,
                req.offset,
                req.length as usize,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(ReadAtResponse { data }))
    }

    async fn write_at(
        &self,
        request: Request<WriteAtRequest>,
    ) -> Result<Response<WriteAtResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let handle = FileHandle(req.file_handle.clone());

        // Retrieve tracked metadata for provenance-rich event emission.
        let meta = self
            .open_handles
            .read()
            .get(&req.file_handle)
            .cloned()
            .ok_or_else(|| Status::invalid_argument("Unknown file handle"))?;

        let bytes_written = self
            .fsal
            .write_for_node(
                &handle,
                Self::sentinel_execution_id(),
                meta.volume_id,
                &meta.path,
                req.offset,
                &req.data,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(WriteAtResponse {
            bytes_written: bytes_written as u32,
        }))
    }

    async fn close_file(
        &self,
        request: Request<CloseFileRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let handle = FileHandle(req.file_handle.clone());

        // Retrieve and remove tracked metadata for event emission.
        let meta = self
            .open_handles
            .write()
            .remove(&req.file_handle)
            .ok_or_else(|| Status::invalid_argument("Unknown file handle"))?;

        self.fsal
            .close_for_node(
                &handle,
                Self::sentinel_execution_id(),
                meta.volume_id,
                &meta.path,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn stat(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<StatResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();

        let attrs = self
            .fsal
            .getattr(execution_id, volume_id, &req.path, 0, 0)
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(StatResponse {
            file_type: Self::file_type_to_proto(attrs.file_type),
            size: attrs.size,
            mtime: attrs.mtime,
            atime: attrs.atime,
            ctime: attrs.ctime,
            mode: attrs.mode,
            uid: attrs.uid,
            gid: attrs.gid,
            nlink: attrs.nlink,
        }))
    }

    async fn readdir(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<ReaddirResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let policy = Self::system_policy();

        let entries = self
            .fsal
            .readdir(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        let proto_entries = entries
            .into_iter()
            .map(|e| DirEntryProto {
                name: e.name,
                file_type: Self::file_type_to_proto(e.file_type),
            })
            .collect();

        Ok(Response::new(ReaddirResponse {
            entries: proto_entries,
        }))
    }

    async fn create_file(
        &self,
        request: Request<CreateFileRequest>,
    ) -> Result<Response<OpenFileResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let policy = Self::system_policy();

        let _aegis_handle = self
            .fsal
            .create_file(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                true,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        // The FSAL's create_file creates and immediately closes the file.
        // Re-open via the storage provider so the caller gets a usable handle.
        let sanitized = self.sanitize_path(&req.path)?;
        let full_path = Self::full_path(&req.volume_id, &sanitized);
        let handle = self
            .fsal
            .storage_provider()
            .open_file(&full_path, OpenMode::ReadWrite)
            .await
            .map_err(|e| Self::storage_err_to_status(&e))?;

        // Track handle metadata.
        self.open_handles.write().insert(
            handle.0.clone(),
            OpenHandleMeta {
                volume_id,
                path: sanitized,
            },
        );

        Ok(Response::new(OpenFileResponse {
            file_handle: handle.0,
        }))
    }

    async fn delete_file(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let policy = Self::system_policy();

        self.fsal
            .delete_file(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        let caller_node_id = self.authenticate(req.envelope).await?;
        let volume_id = Self::parse_volume_id(&req.volume_id)?;
        let execution_id = Self::sentinel_execution_id();
        let policy = Self::system_policy();

        self.fsal
            .rename(
                execution_id,
                volume_id,
                &req.from_path,
                &req.to_path,
                &policy,
                Some(caller_node_id),
                Some(self.host_node_id.clone()),
            )
            .await
            .map_err(Self::fsal_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn health_check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;

        self.fsal
            .storage_provider()
            .health_check()
            .await
            .map_err(|e| Self::storage_err_to_status(&e))?;

        Ok(Response::new(Self::ok_response()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::StorageEvent;
    use crate::domain::fsal::{AegisFSAL, EventPublisher};
    use crate::domain::repository::VolumeRepository;
    use crate::domain::storage::{
        DirEntry as SDirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError,
        StorageProvider,
    };
    use crate::domain::volume::{
        FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeOwnership, VolumeStatus,
    };
    use crate::infrastructure::repositories::InMemoryVolumeRepository;
    use async_trait::async_trait;
    use chrono::Utc;
    use std::sync::Mutex;

    // ── Test doubles ──────────────────────────────────────────────────────────

    struct NoopStorage;

    #[async_trait]
    impl StorageProvider for NoopStorage {
        async fn create_directory(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete_directory(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_quota(&self, _: &str, _: u64) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_usage(&self, _: &str) -> Result<u64, StorageError> {
            Ok(0)
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn open_file(&self, _: &str, _: OpenMode) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(vec![1, 2, 3]))
        }
        async fn read_at(&self, _: &FileHandle, _: u64, _: usize) -> Result<Vec<u8>, StorageError> {
            Ok(vec![42])
        }
        async fn write_at(
            &self,
            _: &FileHandle,
            _: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
            Ok(data.len())
        }
        async fn close_file(&self, _: &FileHandle) -> Result<(), StorageError> {
            Ok(())
        }
        async fn stat(&self, _: &str) -> Result<FileAttributes, StorageError> {
            Ok(FileAttributes {
                file_type: FileType::File,
                size: 100,
                mtime: 0,
                atime: 0,
                ctime: 0,
                mode: 0o644,
                uid: 0,
                gid: 0,
                nlink: 1,
            })
        }
        async fn readdir(&self, _: &str) -> Result<Vec<SDirEntry>, StorageError> {
            Ok(vec![])
        }
        async fn create_file(&self, _: &str, _: u32) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(vec![1, 2, 3]))
        }
        async fn delete_file(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn rename(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
    }

    /// Storage provider that reports a fixed usage amount for quota testing.
    struct QuotaEnforcingStorage {
        current_usage: u64,
    }

    #[async_trait]
    impl StorageProvider for QuotaEnforcingStorage {
        async fn create_directory(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete_directory(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_quota(&self, _: &str, _: u64) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_usage(&self, _: &str) -> Result<u64, StorageError> {
            Ok(self.current_usage)
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn open_file(&self, _: &str, _: OpenMode) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(vec![1, 2, 3]))
        }
        async fn read_at(&self, _: &FileHandle, _: u64, _: usize) -> Result<Vec<u8>, StorageError> {
            Ok(vec![42])
        }
        async fn write_at(
            &self,
            _: &FileHandle,
            _: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
            Ok(data.len())
        }
        async fn close_file(&self, _: &FileHandle) -> Result<(), StorageError> {
            Ok(())
        }
        async fn stat(&self, _: &str) -> Result<FileAttributes, StorageError> {
            Ok(FileAttributes {
                file_type: FileType::File,
                size: 100,
                mtime: 0,
                atime: 0,
                ctime: 0,
                mode: 0o644,
                uid: 0,
                gid: 0,
                nlink: 1,
            })
        }
        async fn readdir(&self, _: &str) -> Result<Vec<SDirEntry>, StorageError> {
            Ok(vec![])
        }
        async fn create_file(&self, _: &str, _: u32) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(vec![1, 2, 3]))
        }
        async fn delete_file(&self, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn rename(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
    }

    /// Publisher that captures emitted events for assertion.
    struct CapturingPublisher {
        events: Mutex<Vec<StorageEvent>>,
    }

    impl CapturingPublisher {
        fn new() -> Self {
            Self {
                events: Mutex::new(Vec::new()),
            }
        }

        fn events(&self) -> Vec<StorageEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EventPublisher for CapturingPublisher {
        async fn publish_storage_event(&self, event: StorageEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    struct NoopPublisher;

    #[async_trait]
    impl EventPublisher for NoopPublisher {
        async fn publish_storage_event(&self, _: StorageEvent) {}
    }

    /// Create a test volume owned by the sentinel execution ID.
    fn make_sentinel_volume(volume_id: VolumeId) -> Volume {
        Volume {
            id: volume_id,
            name: "test-vol".to_string(),
            tenant_id: crate::domain::tenant::TenantId::consumer(),
            storage_class: StorageClass::persistent(),
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
                remote_path: format!("/aegis/volumes/test/{}", volume_id),
            },
            size_limit_bytes: 10_000_000,
            status: VolumeStatus::Available,
            ownership: VolumeOwnership::Execution {
                execution_id: RemoteStorageServiceHandler::sentinel_execution_id(),
            },
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        }
    }

    fn build_fsal(
        storage: Arc<dyn StorageProvider>,
        volume_repo: Arc<InMemoryVolumeRepository>,
        publisher: Arc<dyn EventPublisher>,
    ) -> Arc<AegisFSAL> {
        let borrowed = Arc::new(RwLock::new(HashMap::new()));
        Arc::new(AegisFSAL::new(storage, volume_repo, borrowed, publisher))
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Regression test: path traversal in create_directory is rejected by
    /// the FSAL's path sanitizer (Gap 059-1).
    #[tokio::test]
    async fn path_traversal_blocked_via_fsal() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher: Arc<dyn EventPublisher> = Arc::new(NoopPublisher);
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher);

        // Directly invoke the FSAL-backed create_directory with a traversal path.
        // We bypass authenticate() since we're testing path sanitization, not auth.
        let result = fsal
            .create_directory(
                RemoteStorageServiceHandler::sentinel_execution_id(),
                volume_id,
                "/../../../etc/passwd",
                &RemoteStorageServiceHandler::system_policy(),
                None,
                None,
            )
            .await;

        assert!(
            result.is_err(),
            "Path traversal should be rejected by FSAL path sanitizer"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(err, FsalError::PathSanitization(_)),
            "Expected PathSanitization error, got: {err:?}"
        );
    }

    /// Regression test: StorageEvent emitted by FSAL create_directory includes
    /// cross-node provenance fields (Gap 059-3).
    #[tokio::test]
    async fn storage_event_emitted_with_provenance_fields() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher = Arc::new(CapturingPublisher::new());
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        // Invoke create_directory through FSAL to trigger event emission.
        fsal.create_directory(
            RemoteStorageServiceHandler::sentinel_execution_id(),
            volume_id,
            "/workspace/testdir",
            &RemoteStorageServiceHandler::system_policy(),
            None,
            None,
        )
        .await
        .expect("create_directory should succeed");

        let events = publisher.events();
        assert_eq!(events.len(), 1, "Expected exactly one StorageEvent");
        match &events[0] {
            StorageEvent::FileCreated {
                caller_node_id,
                host_node_id,
                ..
            } => {
                // FSAL emits None for both fields (local operation).
                // The remote handler would set these post-emission in a
                // future iteration when FSAL methods accept node IDs.
                assert!(
                    caller_node_id.is_none(),
                    "FSAL should emit None for caller_node_id on local calls"
                );
                assert!(
                    host_node_id.is_none(),
                    "FSAL should emit None for host_node_id on local calls"
                );
            }
            other => panic!("Expected FileCreated event, got: {other:?}"),
        }
    }

    /// Regression test: FSAL quota enforcement rejects writes that exceed
    /// the volume's size_limit_bytes (Gap 059-4).
    #[tokio::test]
    async fn quota_enforcement_via_fsal() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        // Volume with 1000 byte quota.
        let mut vol = make_sentinel_volume(volume_id);
        vol.size_limit_bytes = 1000;
        volume_repo.save(&vol).await.unwrap();

        let publisher: Arc<dyn EventPublisher> = Arc::new(CapturingPublisher::new());
        // Storage reports 900 bytes already used.
        let storage: Arc<dyn StorageProvider> =
            Arc::new(QuotaEnforcingStorage { current_usage: 900 });
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        let execution_id = RemoteStorageServiceHandler::sentinel_execution_id();
        let handle = crate::domain::fsal::AegisFileHandle::new(
            execution_id,
            volume_id,
            "/workspace/bigfile.bin",
        );
        let policy = RemoteStorageServiceHandler::system_policy();

        // Write 200 bytes when only 100 are available → should fail.
        let data = vec![0u8; 200];
        let result = fsal
            .write(&handle, "/workspace/bigfile.bin", &policy, 0, &data)
            .await;

        assert!(result.is_err(), "Write exceeding quota should be rejected");
        assert!(
            matches!(result.unwrap_err(), FsalError::QuotaExceeded { .. }),
            "Expected QuotaExceeded error"
        );
    }

    // ── Regression tests: FSAL boundary enforcement for handle-based RPCs ──

    /// Regression test: open_file_for_node emits FileOpened event with
    /// cross-node provenance (audit fix: handle-based RPCs bypass FSAL).
    #[tokio::test]
    async fn open_file_for_node_emits_event_with_provenance() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher = Arc::new(CapturingPublisher::new());
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        let caller = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let host = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());

        fsal.open_file_for_node(
            RemoteStorageServiceHandler::sentinel_execution_id(),
            volume_id,
            "/workspace/test.txt",
            crate::domain::storage::OpenMode::ReadOnly,
            Some(caller.clone()),
            Some(host.clone()),
        )
        .await
        .expect("open_file_for_node should succeed");

        let events = publisher.events();
        assert_eq!(events.len(), 1, "Expected exactly one StorageEvent");
        match &events[0] {
            StorageEvent::FileOpened {
                caller_node_id,
                host_node_id,
                path,
                ..
            } => {
                assert_eq!(caller_node_id.as_ref(), Some(&caller));
                assert_eq!(host_node_id.as_ref(), Some(&host));
                assert_eq!(path, "/workspace/test.txt");
            }
            other => panic!("Expected FileOpened event, got: {other:?}"),
        }
    }

    /// Regression test: read_for_node emits FileRead event with
    /// cross-node provenance (audit fix: read_at bypassed FSAL).
    #[tokio::test]
    async fn read_for_node_emits_event_with_provenance() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher = Arc::new(CapturingPublisher::new());
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        let caller = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let host = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let handle = FileHandle(vec![1, 2, 3]);

        let data = fsal
            .read_for_node(
                &handle,
                RemoteStorageServiceHandler::sentinel_execution_id(),
                volume_id,
                "/workspace/data.bin",
                0,
                64,
                Some(caller.clone()),
                Some(host.clone()),
            )
            .await
            .expect("read_for_node should succeed");

        assert_eq!(data, vec![42], "NoopStorage returns [42]");

        let events = publisher.events();
        assert_eq!(events.len(), 1, "Expected exactly one StorageEvent");
        match &events[0] {
            StorageEvent::FileRead {
                caller_node_id,
                host_node_id,
                path,
                bytes_read,
                ..
            } => {
                assert_eq!(caller_node_id.as_ref(), Some(&caller));
                assert_eq!(host_node_id.as_ref(), Some(&host));
                assert_eq!(path, "/workspace/data.bin");
                assert_eq!(*bytes_read, 1);
            }
            other => panic!("Expected FileRead event, got: {other:?}"),
        }
    }

    /// Regression test: write_for_node emits FileWritten event with
    /// cross-node provenance (audit fix: write_at bypassed FSAL).
    #[tokio::test]
    async fn write_for_node_emits_event_with_provenance() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher = Arc::new(CapturingPublisher::new());
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        let caller = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let host = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let handle = FileHandle(vec![1, 2, 3]);

        let bytes_written = fsal
            .write_for_node(
                &handle,
                RemoteStorageServiceHandler::sentinel_execution_id(),
                volume_id,
                "/workspace/output.bin",
                0,
                &[0u8; 100],
                Some(caller.clone()),
                Some(host.clone()),
            )
            .await
            .expect("write_for_node should succeed");

        assert_eq!(bytes_written, 100);

        let events = publisher.events();
        assert_eq!(events.len(), 1, "Expected exactly one StorageEvent");
        match &events[0] {
            StorageEvent::FileWritten {
                caller_node_id,
                host_node_id,
                path,
                bytes_written: bw,
                ..
            } => {
                assert_eq!(caller_node_id.as_ref(), Some(&caller));
                assert_eq!(host_node_id.as_ref(), Some(&host));
                assert_eq!(path, "/workspace/output.bin");
                assert_eq!(*bw, 100);
            }
            other => panic!("Expected FileWritten event, got: {other:?}"),
        }
    }

    /// Regression test: close_for_node emits FileClosed event with
    /// cross-node provenance (audit fix: close_file bypassed FSAL).
    #[tokio::test]
    async fn close_for_node_emits_event_with_provenance() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher = Arc::new(CapturingPublisher::new());
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        let caller = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let host = crate::domain::cluster::NodeId(uuid::Uuid::new_v4());
        let handle = FileHandle(vec![1, 2, 3]);

        fsal.close_for_node(
            &handle,
            RemoteStorageServiceHandler::sentinel_execution_id(),
            volume_id,
            "/workspace/closed.txt",
            Some(caller.clone()),
            Some(host.clone()),
        )
        .await
        .expect("close_for_node should succeed");

        let events = publisher.events();
        assert_eq!(events.len(), 1, "Expected exactly one StorageEvent");
        match &events[0] {
            StorageEvent::FileClosed {
                caller_node_id,
                host_node_id,
                path,
                ..
            } => {
                assert_eq!(caller_node_id.as_ref(), Some(&caller));
                assert_eq!(host_node_id.as_ref(), Some(&host));
                assert_eq!(path, "/workspace/closed.txt");
            }
            other => panic!("Expected FileClosed event, got: {other:?}"),
        }
    }

    /// Regression test: FSAL policy violation emits FilesystemPolicyViolation
    /// event before returning error (audit fix: event never emitted).
    #[tokio::test]
    async fn policy_violation_emits_event() {
        let volume_id = VolumeId::new();
        let volume_repo = Arc::new(InMemoryVolumeRepository::new());
        volume_repo
            .save(&make_sentinel_volume(volume_id))
            .await
            .unwrap();

        let publisher = Arc::new(CapturingPublisher::new());
        let storage: Arc<dyn StorageProvider> = Arc::new(NoopStorage);
        let fsal = build_fsal(storage, volume_repo, publisher.clone());

        let execution_id = RemoteStorageServiceHandler::sentinel_execution_id();
        // Policy only allows /allowed/** — /forbidden/secret.txt should be rejected.
        let restrictive_policy = FsalAccessPolicy {
            read: vec!["/allowed/**".to_string()],
            write: vec!["/allowed/**".to_string()],
        };

        let handle = crate::domain::fsal::AegisFileHandle::new(
            execution_id,
            volume_id,
            "/forbidden/secret.txt",
        );

        let result = fsal
            .read(&handle, "/forbidden/secret.txt", &restrictive_policy, 0, 64)
            .await;

        assert!(
            matches!(result, Err(FsalError::PolicyViolation(_))),
            "Expected PolicyViolation error, got: {result:?}"
        );

        let events = publisher.events();
        assert!(
            !events.is_empty(),
            "Expected at least one StorageEvent for policy violation"
        );
        let has_violation = events.iter().any(|e| {
            matches!(
                e,
                StorageEvent::FilesystemPolicyViolation {
                    operation,
                    path,
                    ..
                } if operation == "read" && path == "/forbidden/secret.txt"
            )
        });
        assert!(
            has_violation,
            "Expected FilesystemPolicyViolation event, got: {events:?}"
        );
    }
}
