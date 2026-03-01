// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! SMCP Storage Provider
//!
//! Proxies file operations via SMCP Envelopes (Secure Model Context Protocol)
//! to a remote node, per ADR-047.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for smcp remote storage proxying

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError, StorageProvider,
};
use async_trait::async_trait;

#[derive(Default)]
pub struct SmcpStorageProvider {
    // node_id: String, // Kept for eventual gRPC routing
}

impl SmcpStorageProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Helper to convert our internal path mapping back into metadata
    /// Expected format: `/aegis/volumes/{tenant_id}/{volume_id}/{internal_path}`
    fn extract_remote_metadata(&self, path: &str) -> Result<(String, String), StorageError> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 4 && parts[0] == "aegis" && parts[1] == "volumes" {
            let volume_id = parts[3].to_string();
            let internal_path = format!("/{}", parts[4..].join("/"));
            Ok((volume_id, internal_path))
        } else {
            Err(StorageError::InvalidPath(format!(
                "Invalid SMCP path struct: {}",
                path
            )))
        }
    }
}

#[async_trait]
impl StorageProvider for SmcpStorageProvider {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        let (volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP create dir on vol {} path {}",
            volume_id,
            internal_path
        );
        // Stub: In fully implemented phase, send SMCP envelope here
        Ok(())
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        let (volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP delete dir on vol {} path {}",
            volume_id,
            internal_path
        );
        // Stub: Send SMCP envelope
        Ok(())
    }

    async fn set_quota(&self, _path: &str, _bytes: u64) -> Result<(), StorageError> {
        Ok(())
    }

    async fn get_usage(&self, _path: &str) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        Ok(())
    }

    // --- POSIX File Operations (ADR-036 / ADR-047) ---

    async fn open_file(&self, path: &str, _mode: OpenMode) -> Result<FileHandle, StorageError> {
        self.extract_remote_metadata(path)?;
        Ok(FileHandle(path.as_bytes().to_vec()))
    }

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        let (volume_id, internal_path) = self.extract_remote_metadata(&path)?;

        tracing::debug!(
            "SMCP read vol {} path {} offset {} length {}",
            volume_id,
            internal_path,
            offset,
            length
        );
        // Stub: Send SMCP Request envelope and await response payload
        Ok(Vec::new())
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        let (volume_id, internal_path) = self.extract_remote_metadata(&path)?;

        tracing::debug!(
            "SMCP write vol {} path {} offset {} length {}",
            volume_id,
            internal_path,
            offset,
            data.len()
        );
        // Stub: Send SMCP Request envelope
        Ok(data.len())
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let (volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!("SMCP stat vol {} path {}", volume_id, internal_path);

        // Stub attributes
        Ok(FileAttributes {
            file_type: FileType::Directory,
            size: 0,
            mtime: 0,
            atime: 0,
            ctime: 0,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            nlink: 1,
        })
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let (volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!("SMCP readdir vol {} path {}", volume_id, internal_path);
        Ok(Vec::new())
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        self.extract_remote_metadata(path)?;
        Ok(FileHandle(path.as_bytes().to_vec()))
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let (volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!("SMCP delete_file vol {} path {}", volume_id, internal_path);
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let (volume_id, internal_from) = self.extract_remote_metadata(from)?;
        let (_, internal_to) = self.extract_remote_metadata(to)?;
        tracing::debug!(
            "SMCP rename vol {} from {} to {}",
            volume_id,
            internal_from,
            internal_to
        );
        Ok(())
    }
}
