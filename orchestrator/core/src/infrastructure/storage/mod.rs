// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Storage Infrastructure Module
//!
//! Provides concrete implementations of the StorageProvider trait
//! for distributed file system backends.

pub mod seaweedfs;
pub mod local;

use crate::domain::storage::{StorageProvider, FileHandle, OpenMode, FileAttributes, DirEntry, FileType};

pub use seaweedfs::SeaweedFSAdapter;
pub use local::LocalStorageProvider;

use std::sync::Arc;

/// Storage backend configuration
#[derive(Debug, Clone)]
pub enum StorageBackend {
    /// SeaweedFS distributed storage (production)
    SeaweedFS { filer_url: String },
    
    /// Local filesystem storage (development/testing)
    Local { base_path: String },
    
    /// Mock storage for unit testing
    Mock,
}

/// Factory function to create storage provider from configuration
pub fn create_storage_provider(backend: StorageBackend) -> Arc<dyn StorageProvider> {
    match backend {
        StorageBackend::SeaweedFS { filer_url } => {
            Arc::new(SeaweedFSAdapter::new(filer_url))
        }
        StorageBackend::Local { base_path } => {
            Arc::new(LocalStorageProvider::new(base_path)
                .expect("Failed to create LocalStorageProvider"))
        }
        StorageBackend::Mock => {
            // Return mock implementation
            Arc::new(mock::MockStorageProvider::new())
        }
    }
}

// Re-export MockStorageProvider for testing
pub use mock::MockStorageProvider;

mod mock {
    use super::*;
    use async_trait::async_trait;
    use crate::domain::storage::StorageError;
    use std::sync::{Arc, Mutex};
    use std::collections::HashMap;

    pub struct MockStorageProvider {
        pub directories: Arc<Mutex<HashMap<String, u64>>>,
        pub quotas: Arc<Mutex<HashMap<String, u64>>>,
    }

    impl MockStorageProvider {
        pub fn new() -> Self {
            Self {
                directories: Arc::new(Mutex::new(HashMap::new())),
                quotas: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    #[async_trait]
    impl StorageProvider for MockStorageProvider {
        async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
            let mut dirs = self.directories.lock().unwrap();
            if dirs.contains_key(path) {
                return Err(StorageError::AlreadyExists(path.to_string()));
            }
            dirs.insert(path.to_string(), 0);
            Ok(())
        }

        async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
            let mut dirs = self.directories.lock().unwrap();
            if !dirs.contains_key(path) {
                return Err(StorageError::NotFound(path.to_string()));
            }
            dirs.remove(path);
            let mut quotas = self.quotas.lock().unwrap();
            quotas.remove(path);
            Ok(())
        }

        async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
            let dirs = self.directories.lock().unwrap();
            if !dirs.contains_key(path) {
                return Err(StorageError::NotFound(path.to_string()));
            }
            let mut quotas = self.quotas.lock().unwrap();
            quotas.insert(path.to_string(), bytes);
            Ok(())
        }

        async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
            let dirs = self.directories.lock().unwrap();
            dirs.get(path)
                .copied()
                .ok_or_else(|| StorageError::NotFound(path.to_string()))
        }

        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }

        // POSIX file operations (ADR-036)
        async fn open_file(&self, _path: &str, _mode: OpenMode) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(b"mock-handle".to_vec()))
        }

        async fn read_at(&self, _handle: &FileHandle, _offset: u64, length: usize) -> Result<Vec<u8>, StorageError> {
            Ok(vec![0u8; length])
        }

        async fn write_at(&self, _handle: &FileHandle, _offset: u64, data: &[u8]) -> Result<usize, StorageError> {
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
            Ok(FileHandle(format!("mock-handle-{}", path).into_bytes()))
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
        });
        
        // Should not panic
        assert!(Arc::strong_count(&provider) == 1);
    }
    
    #[test]
    fn test_factory_local() {
        use tempfile::TempDir;
        
        let temp_dir = TempDir::new().unwrap();
        let provider = create_storage_provider(StorageBackend::Local {
            base_path: temp_dir.path().to_string_lossy().to_string(),
        });
        
        // Should not panic
        assert!(Arc::strong_count(&provider) == 1);
    }

    #[test]
    fn test_factory_mock() {
        let provider = create_storage_provider(StorageBackend::Mock);
        
        // Should not panic
        assert!(Arc::strong_count(&provider) == 1);
    }
}
