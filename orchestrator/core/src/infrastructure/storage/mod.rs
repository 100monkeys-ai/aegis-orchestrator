// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Storage Infrastructure Module
//!
//! Provides concrete implementations of the StorageProvider trait
//! for distributed file system backends.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for mod

pub mod local_host_provider;
pub mod opendal_provider;
pub mod remote_storage_server;
pub mod seaweedfs;

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageProvider,
};

pub use local_host_provider::LocalHostStorageProvider;
pub use opendal_provider::OpenDalStorageProvider;
pub use remote_storage_server::RemoteStorageServiceHandler;
pub use seaweedfs::SeaweedFSAdapter;
pub mod seal_provider;
use opendal::Operator;
pub use seal_provider::SealStorageProvider;

use anyhow::Context as _;
use std::sync::Arc;

/// Storage backend configuration
#[derive(Debug, Clone)]
pub enum StorageBackend {
    /// SeaweedFS distributed storage (production)
    SeaweedFS { filer_url: String },

    /// Local host mount point for direct host IO (ADR-047)
    LocalHost { mount_point: String },

    /// OpenDAL unified storage backend (ADR-047)
    OpenDal {
        provider: String,
        options: std::collections::HashMap<String, String>,
    },

    /// Test storage for unit testing
    Mock,
}

/// Factory function to create storage provider from configuration.
///
/// # Errors
///
/// Returns an error if the backend configuration is invalid (e.g., unrecognized
/// OpenDAL scheme or a `LocalHost` mount point that cannot be initialised).
/// Callers should handle this at startup; a bad storage config is fatal.
pub fn create_storage_provider(
    backend: StorageBackend,
) -> Result<Arc<dyn StorageProvider>, anyhow::Error> {
    match backend {
        StorageBackend::SeaweedFS { filer_url } => Ok(Arc::new(SeaweedFSAdapter::new(filer_url))),
        StorageBackend::LocalHost { mount_point } => {
            let provider = LocalHostStorageProvider::new(mount_point)
                .context("Failed to create LocalHostStorageProvider")?;
            Ok(Arc::new(provider))
        }
        StorageBackend::OpenDal { provider, options } => {
            let scheme_name = provider.clone();
            let scheme: opendal::Scheme = provider
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid OpenDAL scheme: '{scheme_name}'"))?;
            let op = Operator::via_iter(scheme, options).with_context(|| {
                format!("Failed to create OpenDAL operator for scheme '{scheme_name}'")
            })?;
            Ok(Arc::new(OpenDalStorageProvider::new(op)))
        }
        StorageBackend::Mock => Ok(Arc::new(test_support::TestStorageProvider::new())),
    }
}

// Re-export the test storage provider for testing.
pub use test_support::TestStorageProvider;

mod test_support {
    use super::*;
    use crate::domain::storage::StorageError;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    pub struct TestStorageProvider {
        pub directories: Arc<Mutex<HashMap<String, u64>>>,
        pub quotas: Arc<Mutex<HashMap<String, u64>>>,
    }

    impl TestStorageProvider {
        pub fn new() -> Self {
            Self {
                directories: Arc::new(Mutex::new(HashMap::new())),
                quotas: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    impl Default for TestStorageProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl StorageProvider for TestStorageProvider {
        async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
            let mut dirs = self.directories.lock().await;
            if dirs.contains_key(path) {
                return Err(StorageError::AlreadyExists(path.to_string()));
            }
            dirs.insert(path.to_string(), 0);
            Ok(())
        }

        async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
            let mut dirs = self.directories.lock().await;
            if !dirs.contains_key(path) {
                return Err(StorageError::NotFound(path.to_string()));
            }
            dirs.remove(path);
            let mut quotas = self.quotas.lock().await;
            quotas.remove(path);
            Ok(())
        }

        async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
            let dirs = self.directories.lock().await;
            if !dirs.contains_key(path) {
                return Err(StorageError::NotFound(path.to_string()));
            }
            drop(dirs);
            let mut quotas = self.quotas.lock().await;
            quotas.insert(path.to_string(), bytes);
            Ok(())
        }

        async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
            let dirs = self.directories.lock().await;
            dirs.get(path)
                .copied()
                .ok_or_else(|| StorageError::NotFound(path.to_string()))
        }

        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }

        // POSIX file operations (ADR-036)
        async fn open_file(
            &self,
            _path: &str,
            _mode: OpenMode,
        ) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(b"test-handle".to_vec()))
        }

        async fn read_at(
            &self,
            _handle: &FileHandle,
            _offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            Ok(vec![0u8; length])
        }

        async fn write_at(
            &self,
            _handle: &FileHandle,
            _offset: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
            Ok(data.len())
        }

        async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
            Ok(())
        }

        async fn stat(&self, _path: &str) -> Result<FileAttributes, StorageError> {
            Ok(FileAttributes {
                file_type: FileType::File,
                size: 0,
                mode: 0o644,
                uid: 1000,
                gid: 1000,
                atime: 0,
                mtime: 0,
                ctime: 0,
                nlink: 1,
            })
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<DirEntry>, StorageError> {
            Ok(vec![])
        }

        async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(format!("test-handle-{path}").into_bytes()))
        }

        async fn delete_file(&self, _path: &str) -> Result<(), StorageError> {
            Ok(())
        }

        async fn rename(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_factory_seaweedfs() {
        let provider = create_storage_provider(StorageBackend::SeaweedFS {
            filer_url: "http://localhost:8888".to_string(),
        })
        .expect("SeaweedFS storage provider should be created successfully");

        assert_eq!(
            Arc::strong_count(&provider),
            1,
            "unexpected extra Arc references"
        );
    }

    #[test]
    fn test_factory_test_backend() {
        let provider = create_storage_provider(StorageBackend::Mock)
            .expect("test storage provider should be created successfully");

        assert_eq!(
            Arc::strong_count(&provider),
            1,
            "unexpected extra Arc references"
        );
    }

    #[test]
    fn test_factory_invalid_opendal_scheme_returns_error() {
        let result = create_storage_provider(StorageBackend::OpenDal {
            provider: "not-a-real-scheme".to_string(),
            options: std::collections::HashMap::new(),
        });
        assert!(
            result.is_err(),
            "invalid OpenDAL scheme should return Err, not panic"
        );
        let err_msg = result
            .err()
            .expect("invalid OpenDAL scheme should return Err")
            .to_string();
        assert!(
            err_msg.contains("not-a-real-scheme"),
            "error message should identify the bad scheme; got: {err_msg}"
        );
    }
}
