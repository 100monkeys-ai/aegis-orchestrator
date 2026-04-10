// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! FsalBackend trait — transport-agnostic abstraction over FSAL operations (ADR-107)
//!
//! The `FuseFsal` filesystem implementation calls through this trait instead
//! of directly referencing `AegisFSAL`. Two implementations exist:
//!
//! - `DirectFsalBackend`: wraps `Arc<AegisFSAL>` for in-process use
//!   (orchestrator hosting the FUSE daemon locally)
//! - `GrpcFsalBackend`: calls `FsalService` over gRPC for the host-side
//!   FUSE daemon running as a separate process
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Indirection layer enabling the FUSE daemon to run out-of-process

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{
    AegisFSAL, AegisFileHandle, CreateFsalFileRequest, FsalAccessPolicy, FsalError,
    RenameFsalRequest,
};
use crate::domain::storage::{DirEntry, FileAttributes};
use crate::domain::volume::VolumeId;

/// Transport-agnostic backend for FSAL operations.
///
/// The FUSE daemon delegates all filesystem operations through this trait,
/// allowing the same `FuseFsal` implementation to work both in-process
/// (direct FSAL calls) and out-of-process (gRPC calls to the orchestrator).
#[async_trait]
pub trait FsalBackend: Send + Sync + 'static {
    /// Get file attributes (stat).
    async fn getattr(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        uid: u32,
        gid: u32,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<FileAttributes, FsalError>;

    /// Lookup a child entry in a directory.
    async fn lookup(
        &self,
        handle: &AegisFileHandle,
        parent_path: &str,
        name: &str,
    ) -> Result<AegisFileHandle, FsalError>;

    /// List directory contents.
    async fn readdir(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<Vec<DirEntry>, FsalError>;

    /// Read data from a file.
    async fn read(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, FsalError>;

    /// Write data to a file.
    async fn write(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsalError>;

    /// Create a file and return its handle.
    async fn create_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<AegisFileHandle, FsalError>;

    /// Create a directory.
    async fn create_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError>;

    /// Delete a file.
    async fn delete_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError>;

    /// Delete a directory.
    async fn delete_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError>;

    /// Rename a file or directory.
    async fn rename(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        from_path: &str,
        to_path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError>;
}

/// Direct in-process FSAL backend — wraps `Arc<AegisFSAL>`.
///
/// Used when the FUSE daemon runs inside the orchestrator process.
/// All calls delegate directly to the shared FSAL instance with no
/// serialization overhead.
pub struct DirectFsalBackend(pub Arc<AegisFSAL>);

#[async_trait]
impl FsalBackend for DirectFsalBackend {
    async fn getattr(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        uid: u32,
        gid: u32,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<FileAttributes, FsalError> {
        self.0
            .getattr(
                execution_id,
                volume_id,
                path,
                uid,
                gid,
                workflow_execution_id,
            )
            .await
    }

    async fn lookup(
        &self,
        handle: &AegisFileHandle,
        parent_path: &str,
        name: &str,
    ) -> Result<AegisFileHandle, FsalError> {
        self.0.lookup(handle, parent_path, name).await
    }

    async fn readdir(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<Vec<DirEntry>, FsalError> {
        self.0
            .readdir(
                execution_id,
                volume_id,
                path,
                policy,
                None,
                None,
                workflow_execution_id,
            )
            .await
    }

    async fn read(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, FsalError> {
        self.0.read(handle, path, policy, offset, size).await
    }

    async fn write(
        &self,
        handle: &AegisFileHandle,
        path: &str,
        policy: &FsalAccessPolicy,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, FsalError> {
        self.0.write(handle, path, policy, offset, data).await
    }

    async fn create_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<AegisFileHandle, FsalError> {
        self.0
            .create_file(CreateFsalFileRequest {
                execution_id,
                volume_id,
                path,
                policy,
                emit_event: true,
                caller_node_id: None,
                host_node_id: None,
                workflow_execution_id,
            })
            .await
    }

    async fn create_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        self.0
            .create_directory(
                execution_id,
                volume_id,
                path,
                policy,
                None,
                None,
                workflow_execution_id,
            )
            .await
    }

    async fn delete_file(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        self.0
            .delete_file(
                execution_id,
                volume_id,
                path,
                policy,
                None,
                None,
                workflow_execution_id,
            )
            .await
    }

    async fn delete_directory(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        self.0
            .delete_directory(
                execution_id,
                volume_id,
                path,
                policy,
                None,
                None,
                workflow_execution_id,
            )
            .await
    }

    async fn rename(
        &self,
        execution_id: ExecutionId,
        volume_id: VolumeId,
        from_path: &str,
        to_path: &str,
        policy: &FsalAccessPolicy,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> Result<(), FsalError> {
        self.0
            .rename(RenameFsalRequest {
                execution_id,
                volume_id,
                from_path,
                to_path,
                policy,
                caller_node_id: None,
                host_node_id: None,
                workflow_execution_id,
            })
            .await
    }
}
