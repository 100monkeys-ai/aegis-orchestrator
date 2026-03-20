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
    DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
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
    /// Expected format: `/aegis/smcp/{node_id}/{volume_id}/{internal_path}`
    fn extract_remote_metadata(
        &self,
        path: &str,
    ) -> Result<(String, String, String), StorageError> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 4 && parts[0] == "aegis" && parts[1] == "smcp" {
            let node_id = parts[2].to_string();
            let volume_id = parts[3].to_string();
            let internal_path = format!("/{}", parts[4..].join("/"));
            Ok((node_id, volume_id, internal_path))
        } else {
            Err(StorageError::InvalidPath(format!(
                "Invalid SMCP path struct: {path}"
            )))
        }
    }

    fn unsupported(&self, path: &str) -> StorageError {
        StorageError::Unavailable(format!(
            "SMCP-backed storage is not available in Phase 1 for path {path}"
        ))
    }
}

#[async_trait]
impl StorageProvider for SmcpStorageProvider {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP create dir on node {} vol {} path {}",
            node_id,
            volume_id,
            internal_path
        );
        Err(self.unsupported(path))
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP delete dir on node {} vol {} path {}",
            node_id,
            volume_id,
            internal_path
        );
        Err(self.unsupported(path))
    }

    async fn set_quota(&self, path: &str, _bytes: u64) -> Result<(), StorageError> {
        Err(self.unsupported(path))
    }

    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        Err(self.unsupported(path))
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        Err(StorageError::Unavailable(
            "SMCP-backed storage is not available in Phase 1".to_string(),
        ))
    }

    // --- POSIX File Operations (ADR-036 / ADR-047) ---

    async fn open_file(&self, path: &str, _mode: OpenMode) -> Result<FileHandle, StorageError> {
        self.extract_remote_metadata(path)?;
        Err(self.unsupported(path))
    }

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(&path)?;

        tracing::debug!(
            "SMCP read node {} vol {} path {} offset {} length {}",
            node_id,
            volume_id,
            internal_path,
            offset,
            length
        );
        Err(self.unsupported(&path))
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(&path)?;

        tracing::debug!(
            "SMCP write node {} vol {} path {} offset {} length {}",
            node_id,
            volume_id,
            internal_path,
            offset,
            data.len()
        );
        Err(self.unsupported(&path))
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP stat node {} vol {} path {}",
            node_id,
            volume_id,
            internal_path
        );
        Err(self.unsupported(path))
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP readdir node {} vol {} path {}",
            node_id,
            volume_id,
            internal_path
        );
        Err(self.unsupported(path))
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        self.extract_remote_metadata(path)?;
        Err(self.unsupported(path))
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_path) = self.extract_remote_metadata(path)?;
        tracing::debug!(
            "SMCP delete_file node {} vol {} path {}",
            node_id,
            volume_id,
            internal_path
        );
        Err(self.unsupported(path))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let (node_id, volume_id, internal_from) = self.extract_remote_metadata(from)?;
        let (_, _, internal_to) = self.extract_remote_metadata(to)?;
        tracing::debug!(
            "SMCP rename node {} vol {} from {} to {}",
            node_id,
            volume_id,
            internal_from,
            internal_to
        );
        Err(self.unsupported(from))
    }
}
