// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Storage Router
//!
//! Provides a composite `StorageProvider` that dispatches POSIX file operations
//! to the appropriate underlying backend based on the Volume configuration.
//!
//! By default, paths are expected to be formatted by `AegisFSAL` such that
//! the backend can determine the necessary context.
//! For example, SeaweedFS receives: `/aegis/volumes/{tenant_id}/{volume_id}/path/to/file`
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for dynamic multi-backend volume routing

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
};
use crate::infrastructure::storage::{LocalHostStorageProvider, SmcpStorageProvider};
use async_trait::async_trait;
use std::sync::Arc;

pub struct StorageRouter {
    local_provider: Arc<LocalHostStorageProvider>,
    #[allow(dead_code)]
    smcp_provider: Arc<SmcpStorageProvider>,
    default_provider: Arc<dyn StorageProvider>,
}

impl StorageRouter {
    pub fn new(
        default_provider: Arc<dyn StorageProvider>,
        local_provider: Arc<LocalHostStorageProvider>,
        smcp_provider: Arc<SmcpStorageProvider>,
    ) -> Self {
        Self {
            default_provider,
            local_provider,
            smcp_provider,
        }
    }

    fn provider_for_path(&self, path: &str) -> Result<Arc<dyn StorageProvider>, StorageError> {
        if path.starts_with("/aegis/smcp/") || path.starts_with("aegis/smcp/") {
            return Ok(self.smcp_provider.clone());
        }
        if path.starts_with("/aegis/opendal/") || path.starts_with("aegis/opendal/") {
            return Ok(self.default_provider.clone());
        }
        if path.starts_with("/aegis/volumes/") || path.starts_with("aegis/volumes/") {
            return Ok(self.default_provider.clone());
        }
        if path.starts_with('/') {
            return Ok(self.local_provider.clone());
        }

        Err(StorageError::InvalidPath(format!(
            "storage router could not determine provider for path: {path}"
        )))
    }
}

#[async_trait]
impl StorageProvider for StorageRouter {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        self.provider_for_path(path)?.create_directory(path).await
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        self.provider_for_path(path)?.delete_directory(path).await
    }

    async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
        self.provider_for_path(path)?.set_quota(path, bytes).await
    }

    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        self.provider_for_path(path)?.get_usage(path).await
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        // Check default provider health.
        self.default_provider.health_check().await
    }

    async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
        self.provider_for_path(path)?.open_file(path, mode).await
    }

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        // Handle decoding gives the path back, so we resolve provider from that path
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        self.provider_for_path(&path)?
            .read_at(handle, offset, length)
            .await
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        self.provider_for_path(&path)?
            .write_at(handle, offset, data)
            .await
    }

    async fn close_file(&self, handle: &FileHandle) -> Result<(), StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        self.provider_for_path(&path)?.close_file(handle).await
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        self.provider_for_path(path)?.stat(path).await
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        self.provider_for_path(path)?.readdir(path).await
    }

    async fn create_file(&self, path: &str, mode: u32) -> Result<FileHandle, StorageError> {
        self.provider_for_path(path)?.create_file(path, mode).await
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        self.provider_for_path(path)?.delete_file(path).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        // Note: Renaming across different providers is highly discouraged and generally unsupported in standard POSIX.
        // We defer to the provider that owns the 'from' path which usually fails across filesystems.
        self.provider_for_path(from)?.rename(from, to).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::storage::TestStorageProvider;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_storage_router_routing() {
        let default_provider = Arc::new(TestStorageProvider::new());
        // Since LocalHostStorageProvider relies on actual local path access, we bypass initializing failing path
        let local_provider = Arc::new(LocalHostStorageProvider::new("/tmp").unwrap());
        let smcp_provider = Arc::new(SmcpStorageProvider::new());

        let router = StorageRouter::new(
            default_provider.clone(),
            local_provider.clone(),
            smcp_provider.clone(),
        );

        // Default routing for standard AEGIS prefix
        assert!(Arc::ptr_eq(
            &router
                .provider_for_path("/aegis/volumes/my_tenant/my_vol/file.txt")
                .unwrap(),
            &(default_provider.clone() as Arc<dyn StorageProvider>)
        ));
        assert!(Arc::ptr_eq(
            &router
                .provider_for_path("aegis/volumes/my_tenant/my_vol/file.txt")
                .unwrap(),
            &(default_provider.clone() as Arc<dyn StorageProvider>)
        ));
        assert!(Arc::ptr_eq(
            &router
                .provider_for_path("/aegis/smcp/node-1/vol-1/file.txt")
                .unwrap(),
            &(smcp_provider.clone() as Arc<dyn StorageProvider>)
        ));

        // Path should route to local
        assert!(Arc::ptr_eq(
            &router.provider_for_path("/opt/data/some/path.txt").unwrap(),
            &(local_provider.clone() as Arc<dyn StorageProvider>)
        ));
    }
}
