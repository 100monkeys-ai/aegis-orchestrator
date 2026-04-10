// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! GrpcFsalBackend — remote FSAL backend via gRPC (ADR-107)
//!
//! Implements `FsalBackend` by calling the orchestrator's `FsalService`
//! over gRPC. Used by the host-side FUSE daemon when running as a separate
//! process from the orchestrator.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** gRPC client adapter for the FsalBackend trait

use async_trait::async_trait;
use tonic::transport::Channel;

use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{AegisFileHandle, FsalAccessPolicy, FsalError};
use crate::domain::storage::{DirEntry, FileAttributes, FileType};
use crate::domain::volume::VolumeId;
use crate::infrastructure::aegis_runtime_proto::{
    fsal_service_client::FsalServiceClient, FsalAccessPolicy as ProtoPolicy,
    FsalCreateFileRequest as ProtoCreateFile, FsalGetattrRequest, FsalLookupRequest,
    FsalMutateRequest, FsalReadRequest, FsalReaddirRequest, FsalRenameRequest, FsalWriteRequest,
};

use super::fsal_backend::FsalBackend;

/// gRPC-backed FSAL backend for the host-side FUSE daemon.
///
/// Connects to the orchestrator's `FsalService` and translates domain
/// types to/from protobuf messages.
pub struct GrpcFsalBackend {
    client: FsalServiceClient<Channel>,
}

impl GrpcFsalBackend {
    /// Create a new gRPC backend connected to the given orchestrator endpoint.
    pub async fn connect(endpoint: &str) -> Result<Self, tonic::transport::Error> {
        let client = FsalServiceClient::connect(endpoint.to_string()).await?;
        Ok(Self { client })
    }

    /// Create from an existing channel.
    pub fn from_channel(channel: Channel) -> Self {
        Self {
            client: FsalServiceClient::new(channel),
        }
    }
}

/// Convert domain FsalAccessPolicy to proto FsalAccessPolicy.
fn policy_to_proto(policy: &FsalAccessPolicy) -> Option<ProtoPolicy> {
    Some(ProtoPolicy {
        read_paths: policy.read.clone(),
        write_paths: policy.write.clone(),
    })
}

/// Parse a file type string from proto to domain FileType.
fn parse_file_type(s: &str) -> FileType {
    match s {
        "directory" => FileType::Directory,
        "symlink" => FileType::Symlink,
        _ => FileType::File,
    }
}

/// Map a gRPC status to an FsalError.
fn grpc_to_fsal_error(status: tonic::Status) -> FsalError {
    FsalError::Storage(crate::domain::storage::StorageError::IoError(
        status.message().to_string(),
    ))
}

#[async_trait]
impl FsalBackend for GrpcFsalBackend {
    async fn getattr(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        uid: u32,
        gid: u32,
    ) -> Result<FileAttributes, FsalError> {
        let resp = self
            .client
            .clone()
            .getattr(FsalGetattrRequest {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                path: path.to_string(),
                container_uid: uid,
                container_gid: gid,
            })
            .await
            .map_err(grpc_to_fsal_error)?
            .into_inner();

        Ok(FileAttributes {
            file_type: parse_file_type(&resp.file_type),
            size: resp.size,
            mtime: resp.mtime,
            atime: resp.atime,
            ctime: resp.ctime,
            mode: resp.mode,
            uid,
            gid,
            nlink: resp.nlink,
        })
    }

    async fn lookup(
        &self,
        handle: &AegisFileHandle,
        parent_path: &str,
        name: &str,
    ) -> Result<AegisFileHandle, FsalError> {
        let resp = self
            .client
            .clone()
            .lookup(FsalLookupRequest {
                execution_id: handle
                    .execution_id()
                    .map(|id| id.0.to_string())
                    .unwrap_or_default(),
                volume_id: handle.volume_id.0.to_string(),
                parent_path: parent_path.to_string(),
                name: name.to_string(),
            })
            .await
            .map_err(grpc_to_fsal_error)?
            .into_inner();

        AegisFileHandle::from_bytes(&resp.file_handle)
    }

    async fn readdir(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
    ) -> Result<Vec<DirEntry>, FsalError> {
        let resp = self
            .client
            .clone()
            .readdir(FsalReaddirRequest {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
            })
            .await
            .map_err(grpc_to_fsal_error)?
            .into_inner();

        Ok(resp
            .entries
            .into_iter()
            .map(|e| DirEntry {
                name: e.name,
                file_type: parse_file_type(&e.file_type),
            })
            .collect())
    }

    async fn read(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, FsalError> {
        let resp = self
            .client
            .clone()
            .read(FsalReadRequest {
                execution_id: handle
                    .execution_id()
                    .map(|id| id.0.to_string())
                    .unwrap_or_default(),
                volume_id: handle.volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
                offset,
                size: size as u32,
            })
            .await
            .map_err(grpc_to_fsal_error)?
            .into_inner();

        Ok(resp.data)
    }

    async fn write(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsalError> {
        let resp = self
            .client
            .clone()
            .write(FsalWriteRequest {
                execution_id: handle
                    .execution_id()
                    .map(|id| id.0.to_string())
                    .unwrap_or_default(),
                volume_id: handle.volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
                offset,
                data: data.to_vec(),
            })
            .await
            .map_err(grpc_to_fsal_error)?
            .into_inner();

        Ok(resp.bytes_written as usize)
    }

    async fn create_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
    ) -> Result<AegisFileHandle, FsalError> {
        let resp = self
            .client
            .clone()
            .create_file(ProtoCreateFile {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
            })
            .await
            .map_err(grpc_to_fsal_error)?
            .into_inner();

        AegisFileHandle::from_bytes(&resp.file_handle)
    }

    async fn create_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
    ) -> Result<(), FsalError> {
        self.client
            .clone()
            .create_directory(FsalMutateRequest {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
            })
            .await
            .map_err(grpc_to_fsal_error)?;
        Ok(())
    }

    async fn delete_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
    ) -> Result<(), FsalError> {
        self.client
            .clone()
            .delete_file(FsalMutateRequest {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
            })
            .await
            .map_err(grpc_to_fsal_error)?;
        Ok(())
    }

    async fn delete_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
    ) -> Result<(), FsalError> {
        self.client
            .clone()
            .delete_directory(FsalMutateRequest {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                path: path.to_string(),
                policy: policy_to_proto(policy),
            })
            .await
            .map_err(grpc_to_fsal_error)?;
        Ok(())
    }

    async fn rename(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        from_path: &str,
        to_path: &str,
        policy: &FsalAccessPolicy,
    ) -> Result<(), FsalError> {
        self.client
            .clone()
            .rename(FsalRenameRequest {
                execution_id: execution_id.0.to_string(),
                volume_id: volume_id.0.to_string(),
                from_path: from_path.to_string(),
                to_path: to_path.to_string(),
                policy: policy_to_proto(policy),
            })
            .await
            .map_err(grpc_to_fsal_error)?;
        Ok(())
    }
}
