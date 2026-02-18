// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Storage Infrastructure Module
//!
//! Provides concrete implementations of the StorageProvider trait
//! for distributed file system backends.

pub mod seaweedfs;
pub mod local;

pub use seaweedfs::SeaweedFSAdapter;
pub use local::LocalStorageProvider;

use std::sync::Arc;
use crate::domain::storage::StorageProvider;

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
