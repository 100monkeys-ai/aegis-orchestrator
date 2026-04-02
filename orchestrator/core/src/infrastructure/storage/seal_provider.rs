// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! SEAL Storage Provider — gRPC Client (ADR-064)
//!
//! Proxies `StorageProvider` calls to a remote node's `RemoteStorageService`
//! via authenticated gRPC. Each call:
//! 1. Parses the SEAL path to extract (node_id, volume_id, internal_path).
//! 2. Obtains (or caches) a gRPC channel to the target node.
//! 3. Wraps the request in an `SealNodeEnvelope` with an Ed25519 signature.
//! 4. Invokes the corresponding RPC and maps the response back to domain types.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Client-side gRPC proxy for cross-node volume operations

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::Signer;
use tonic::transport::Channel;

use crate::domain::cluster::NodeClusterRepository;
use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError, StorageProvider,
};
use crate::infrastructure::aegis_cluster_proto::SealNodeEnvelope as ProtoEnvelope;
use crate::infrastructure::aegis_remote_storage_proto::{
    remote_storage_service_client::RemoteStorageServiceClient, CreateFileRequest, GetUsageRequest,
    OpenFileRequest, RemoteStorageRequest, RenameRequest, SetQuotaRequest,
};

/// gRPC-backed storage provider for cross-node volume access.
///
/// Implements `StorageProvider` by forwarding every operation to the
/// `RemoteStorageService` running on the node that owns the volume.
///
/// When constructed without cluster credentials (via [`SealStorageProvider::new`]),
/// all operations return `StorageError::Unavailable`. Use
/// [`SealStorageProvider::with_cluster`] to enable real gRPC calls.
pub struct SealStorageProvider {
    connections: Arc<tokio::sync::RwLock<HashMap<String, RemoteStorageServiceClient<Channel>>>>,
    cluster_repo: Option<Arc<dyn NodeClusterRepository>>,
    signing_key: Option<Arc<ed25519_dalek::SigningKey>>,
    token: Arc<tokio::sync::RwLock<Option<String>>>,
}

impl Default for SealStorageProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SealStorageProvider {
    /// Create an unconfigured provider that returns `Unavailable` for all RPCs.
    ///
    /// Useful as a placeholder when clustering is not enabled.
    pub fn new() -> Self {
        Self {
            connections: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cluster_repo: None,
            signing_key: None,
            token: Arc::new(tokio::sync::RwLock::new(None)),
        }
    }

    /// Create a fully configured provider backed by real gRPC calls.
    pub fn with_cluster(
        cluster_repo: Arc<dyn NodeClusterRepository>,
        signing_key: Arc<ed25519_dalek::SigningKey>,
        token: Arc<tokio::sync::RwLock<Option<String>>>,
    ) -> Self {
        Self {
            connections: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cluster_repo: Some(cluster_repo),
            signing_key: Some(signing_key),
            token,
        }
    }

    /// Return an error if the provider is not configured with cluster credentials.
    fn require_cluster(&self) -> Result<(), StorageError> {
        if self.cluster_repo.is_none() || self.signing_key.is_none() {
            return Err(StorageError::Unavailable(
                "SEAL storage provider not configured with cluster credentials".to_string(),
            ));
        }
        Ok(())
    }

    /// Parse an SEAL-prefixed path into (node_id, volume_id, internal_path).
    ///
    /// Expected format: `/aegis/seal/{node_id}/{volume_id}/{internal_path...}`
    fn extract_remote_metadata(
        &self,
        path: &str,
    ) -> Result<(String, String, String), StorageError> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 4 && parts[0] == "aegis" && parts[1] == "seal" {
            let node_id = parts[2].to_string();
            let volume_id = parts[3].to_string();
            let internal_path = format!("/{}", parts[4..].join("/"));
            Ok((node_id, volume_id, internal_path))
        } else {
            Err(StorageError::InvalidPath(format!(
                "Invalid SEAL path struct: {path}"
            )))
        }
    }

    /// Get or create a cached gRPC client for the given node.
    async fn get_or_connect(
        &self,
        node_id: &str,
    ) -> Result<RemoteStorageServiceClient<Channel>, StorageError> {
        self.require_cluster()?;

        // Fast path: check read lock
        {
            let conns = self.connections.read().await;
            if let Some(client) = conns.get(node_id) {
                return Ok(client.clone());
            }
        }

        // Slow path: look up the node's gRPC address, connect, and cache
        let cluster_repo = self
            .cluster_repo
            .as_ref()
            .expect("checked in require_cluster");
        let nid = crate::domain::cluster::NodeId::from_string(node_id)
            .map_err(|e| StorageError::InvalidPath(format!("Invalid node_id: {e}")))?;

        let peer = cluster_repo
            .find_peer(&nid)
            .await
            .map_err(|e| StorageError::Network(format!("Cluster repo error: {e}")))?
            .ok_or_else(|| {
                StorageError::Unavailable(format!("Node {node_id} not found in cluster"))
            })?;

        let endpoint = if peer.grpc_address.starts_with("http") {
            peer.grpc_address.clone()
        } else {
            format!("http://{}", peer.grpc_address)
        };

        let channel = Channel::from_shared(endpoint.clone())
            .map_err(|e| StorageError::Network(format!("Invalid endpoint: {e}")))?
            .connect()
            .await
            .map_err(|e| StorageError::Network(format!("Failed to connect to {endpoint}: {e}")))?;

        let client = RemoteStorageServiceClient::new(channel);

        let mut conns = self.connections.write().await;
        conns.insert(node_id.to_string(), client.clone());

        Ok(client)
    }

    /// Build an `SealNodeEnvelope` signing the given payload bytes.
    async fn make_envelope(&self, payload: &[u8]) -> Result<ProtoEnvelope, StorageError> {
        self.require_cluster()?;
        let signing_key = self
            .signing_key
            .as_ref()
            .expect("checked in require_cluster");

        let token = self.token.read().await;
        let token_str = token.as_ref().ok_or_else(|| {
            StorageError::Unavailable(
                "No security token available -- node attestation required".to_string(),
            )
        })?;

        let signature = signing_key.sign(payload);

        Ok(ProtoEnvelope {
            node_security_token: token_str.clone(),
            signature: signature.to_bytes().to_vec(),
            inner_payload: payload.to_vec(),
        })
    }

    /// Convert a tonic Status into a StorageError.
    fn status_to_storage_err(status: tonic::Status) -> StorageError {
        match status.code() {
            tonic::Code::NotFound => StorageError::FileNotFound(status.message().to_string()),
            tonic::Code::PermissionDenied => {
                StorageError::PermissionDenied(status.message().to_string())
            }
            tonic::Code::AlreadyExists => StorageError::AlreadyExists(status.message().to_string()),
            tonic::Code::ResourceExhausted => StorageError::QuotaExceeded {
                path: String::new(),
                limit_bytes: 0,
                actual_bytes: 0,
            },
            tonic::Code::InvalidArgument => StorageError::InvalidPath(status.message().to_string()),
            tonic::Code::Unavailable => StorageError::Unavailable(status.message().to_string()),
            tonic::Code::Unauthenticated => {
                StorageError::PermissionDenied(status.message().to_string())
            }
            _ => StorageError::Unknown(status.message().to_string()),
        }
    }

    /// Convert a domain `OpenMode` to the proto integer representation.
    fn open_mode_to_proto(mode: OpenMode) -> i32 {
        match mode {
            OpenMode::ReadOnly => 0,
            OpenMode::WriteOnly => 1,
            OpenMode::ReadWrite => 2,
            OpenMode::Create => 3,
        }
    }

    /// Convert a proto file_type integer to domain `FileType`.
    fn proto_to_file_type(ft: u32) -> FileType {
        match ft {
            1 => FileType::Directory,
            2 => FileType::Symlink,
            _ => FileType::File,
        }
    }
}

#[async_trait]
impl StorageProvider for SealStorageProvider {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .create_directory(RemoteStorageRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        if resp.success {
            Ok(())
        } else {
            Err(StorageError::Unknown(resp.error_message))
        }
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .delete_directory(RemoteStorageRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        if resp.success {
            Ok(())
        } else {
            Err(StorageError::Unknown(resp.error_message))
        }
    }

    async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .set_quota(SetQuotaRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
                bytes,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        if resp.success {
            Ok(())
        } else {
            Err(StorageError::Unknown(resp.error_message))
        }
    }

    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .get_usage(GetUsageRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        Ok(resp.bytes_used)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        // Health check does not use a path, so we cannot route to a specific node.
        // For now, just return Ok — a real implementation would check all connected nodes.
        Ok(())
    }

    async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .open_file(OpenFileRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
                mode: Self::open_mode_to_proto(mode),
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        Ok(FileHandle(resp.file_handle))
    }

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        // The file handle must encode routing information. We store the SEAL
        // path as the handle bytes for read_at/write_at/close_file, but the
        // remote server uses opaque handles. For remote operations, the handle
        // returned from open_file is already the server's opaque handle bytes.
        // We need the node_id to route; we embed it in a prefixed handle format:
        // "{node_id}:{volume_id}:{server_handle_bytes}"
        //
        // However, the current handle is the raw server handle. To keep the API
        // simple, we require callers to use the same SealStorageProvider that
        // opened the file. We store the routing info in a separate in-memory map
        // keyed by handle bytes.
        //
        // For now, since the path-based routing for read_at/write_at requires
        // the caller to maintain context, we return an error explaining this.
        // A production implementation would use a handle registry.
        let _ = (handle, offset, length);
        Err(StorageError::Unavailable(
            "read_at via SEAL requires handle routing context; use open_file first".to_string(),
        ))
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        let _ = (handle, offset, data);
        Err(StorageError::Unavailable(
            "write_at via SEAL requires handle routing context; use open_file first".to_string(),
        ))
    }

    async fn close_file(&self, handle: &FileHandle) -> Result<(), StorageError> {
        let _ = handle;
        Err(StorageError::Unavailable(
            "close_file via SEAL requires handle routing context; use open_file first".to_string(),
        ))
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .stat(RemoteStorageRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        Ok(FileAttributes {
            file_type: Self::proto_to_file_type(resp.file_type),
            size: resp.size,
            mtime: resp.mtime,
            atime: resp.atime,
            ctime: resp.ctime,
            mode: resp.mode,
            uid: resp.uid,
            gid: resp.gid,
            nlink: resp.nlink,
        })
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .readdir(RemoteStorageRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        Ok(resp
            .entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                file_type: Self::proto_to_file_type(e.file_type),
            })
            .collect())
    }

    async fn create_file(&self, path: &str, mode: u32) -> Result<FileHandle, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .create_file(CreateFileRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
                mode,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        Ok(FileHandle(resp.file_handle))
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .delete_file(RemoteStorageRequest {
                envelope: Some(envelope),
                volume_id,
                path: internal_path,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        if resp.success {
            Ok(())
        } else {
            Err(StorageError::Unknown(resp.error_message))
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_from) = self.extract_remote_metadata(from)?;
        let (_, _, internal_to) = self.extract_remote_metadata(to)?;
        let mut client = self.get_or_connect(&node_id).await?;
        let envelope = self.make_envelope(b"").await?;

        let resp = client
            .rename(RenameRequest {
                envelope: Some(envelope),
                volume_id,
                from_path: internal_from,
                to_path: internal_to,
            })
            .await
            .map_err(Self::status_to_storage_err)?
            .into_inner();

        if resp.success {
            Ok(())
        } else {
            Err(StorageError::Unknown(resp.error_message))
        }
    }
}
