// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Remote Storage gRPC Server (ADR-064)
//!
//! Receives authenticated gRPC calls from remote nodes and delegates
//! to the local `StorageProvider`. Each RPC authenticates the caller
//! via the `SealNodeEnvelope` carried in the request, then maps the
//! proto request into a domain `StorageProvider` call and converts
//! the result back into proto responses.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Server-side handler for cross-node volume operations

use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::domain::cluster::{NodeClusterRepository, NodeId, NodeSecurityToken};
use crate::domain::storage::{FileHandle, FileType, OpenMode, StorageError, StorageProvider};
use crate::infrastructure::aegis_cluster_proto::SealNodeEnvelope as ProtoEnvelope;
use crate::infrastructure::aegis_remote_storage_proto::{
    remote_storage_service_server::RemoteStorageService, CloseFileRequest, CreateFileRequest,
    DirEntryProto, GetUsageRequest, GetUsageResponse, HealthCheckRequest, OpenFileRequest,
    OpenFileResponse, ReadAtRequest, ReadAtResponse, ReaddirResponse, RemoteStorageRequest,
    RemoteStorageResponse, RenameRequest, SetQuotaRequest, StatResponse, WriteAtRequest,
    WriteAtResponse,
};

/// gRPC handler for `RemoteStorageService`.
///
/// Delegates every RPC to the local [`StorageProvider`] after verifying
/// the SEAL node envelope.
pub struct RemoteStorageServiceHandler {
    storage_provider: Arc<dyn StorageProvider>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
}

impl RemoteStorageServiceHandler {
    pub fn new(
        storage_provider: Arc<dyn StorageProvider>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
    ) -> Self {
        Self {
            storage_provider,
            cluster_repo,
        }
    }

    /// Authenticate the caller by verifying the SEAL node envelope:
    /// 1. Parse JWT to extract node_id
    /// 2. Look up registered public key
    /// 3. Verify Ed25519 signature over inner_payload
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

        // 3. Verify Ed25519 signature over inner_payload
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

    /// Convert a `StorageError` into an appropriate gRPC `Status`.
    fn storage_err_to_status(e: StorageError) -> Status {
        match e {
            StorageError::NotFound(m) | StorageError::FileNotFound(m) => Status::not_found(m),
            StorageError::PermissionDenied(m) => Status::permission_denied(m),
            StorageError::AlreadyExists(m) => Status::already_exists(m),
            StorageError::QuotaExceeded { .. } => Status::resource_exhausted("quota exceeded"),
            StorageError::Unavailable(m) => Status::unavailable(m),
            StorageError::InvalidPath(m) => Status::invalid_argument(m),
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
}

#[tonic::async_trait]
impl RemoteStorageService for RemoteStorageServiceHandler {
    async fn create_directory(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        self.storage_provider
            .create_directory(&path)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn delete_directory(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        self.storage_provider
            .delete_directory(&path)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn set_quota(
        &self,
        request: Request<SetQuotaRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        self.storage_provider
            .set_quota(&path, req.bytes)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn get_usage(
        &self,
        request: Request<GetUsageRequest>,
    ) -> Result<Response<GetUsageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        let bytes_used = self
            .storage_provider
            .get_usage(&path)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(GetUsageResponse { bytes_used }))
    }

    async fn open_file(
        &self,
        request: Request<OpenFileRequest>,
    ) -> Result<Response<OpenFileResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);
        let mode = Self::proto_mode_to_open_mode(req.mode);

        let handle = self
            .storage_provider
            .open_file(&path, mode)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(OpenFileResponse {
            file_handle: handle.0,
        }))
    }

    async fn read_at(
        &self,
        request: Request<ReadAtRequest>,
    ) -> Result<Response<ReadAtResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let handle = FileHandle(req.file_handle);

        let data = self
            .storage_provider
            .read_at(&handle, req.offset, req.length as usize)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(ReadAtResponse { data }))
    }

    async fn write_at(
        &self,
        request: Request<WriteAtRequest>,
    ) -> Result<Response<WriteAtResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let handle = FileHandle(req.file_handle);

        let bytes_written = self
            .storage_provider
            .write_at(&handle, req.offset, &req.data)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(WriteAtResponse {
            bytes_written: bytes_written as u32,
        }))
    }

    async fn close_file(
        &self,
        request: Request<CloseFileRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let handle = FileHandle(req.file_handle);

        self.storage_provider
            .close_file(&handle)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn stat(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<StatResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        let attrs = self
            .storage_provider
            .stat(&path)
            .await
            .map_err(Self::storage_err_to_status)?;

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
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        let entries = self
            .storage_provider
            .readdir(&path)
            .await
            .map_err(Self::storage_err_to_status)?;

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
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        let handle = self
            .storage_provider
            .create_file(&path, req.mode)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(OpenFileResponse {
            file_handle: handle.0,
        }))
    }

    async fn delete_file(
        &self,
        request: Request<RemoteStorageRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let path = Self::full_path(&req.volume_id, &req.path);

        self.storage_provider
            .delete_file(&path)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;
        let from = Self::full_path(&req.volume_id, &req.from_path);
        let to = Self::full_path(&req.volume_id, &req.to_path);

        self.storage_provider
            .rename(&from, &to)
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }

    async fn health_check(
        &self,
        request: Request<HealthCheckRequest>,
    ) -> Result<Response<RemoteStorageResponse>, Status> {
        let req = request.into_inner();
        self.authenticate(req.envelope).await?;

        self.storage_provider
            .health_check()
            .await
            .map_err(Self::storage_err_to_status)?;

        Ok(Response::new(Self::ok_response()))
    }
}
