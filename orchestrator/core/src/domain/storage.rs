// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Storage Provider Trait - Anti-Corruption Layer for SeaweedFS
//!
//! Provides abstraction over storage backend (SeaweedFS) to isolate
//! domain from external technology choices. Enables testing with mocks
//! and potential future migration to different storage systems.
//!
//! Follows DDD Anti-Corruption Layer pattern from AGENTS.md.

use async_trait::async_trait;
use thiserror::Error;

/// Storage provider trait abstracting SeaweedFS operations
///
/// This trait defines the interface for interacting with the distributed
/// storage backend (SeaweedFS). Implementations handle:
/// - Directory creation/deletion
/// - Quota enforcement
/// - Usage tracking
/// - Health monitoring
///
/// The trait isolates the domain layer from SeaweedFS-specific details,
/// allowing for easier testing and potential backend changes.
#[async_trait]
pub trait StorageProvider: Send + Sync {
    /// Create a directory in the storage backend
    ///
    /// # Arguments
    /// * `path` - Remote path (e.g., "/aegis/volumes/tenant-id/volume-id")
    ///
    /// # Returns
    /// * `Ok(())` if directory was created successfully
    /// * `Err(StorageError)` if creation failed
    async fn create_directory(&self, path: &str) -> Result<(), StorageError>;

    /// Delete a directory and all its contents
    ///
    /// # Arguments
    /// * `path` - Remote path to delete
    ///
    /// # Returns
    /// * `Ok(())` if directory was deleted successfully
    /// * `Err(StorageError)` if deletion failed
    async fn delete_directory(&self, path: &str) -> Result<(), StorageError>;

    /// Set storage quota for a directory
    ///
    /// # Arguments
    /// * `path` - Remote path
    /// * `bytes` - Maximum size in bytes
    ///
    /// # Returns
    /// * `Ok(())` if quota was set successfully
    /// * `Err(StorageError)` if quota setting failed
    async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError>;

    /// Get current storage usage for a directory
    ///
    /// # Arguments
    /// * `path` - Remote path
    ///
    /// # Returns
    /// * `Ok(u64)` - Current usage in bytes
    /// * `Err(StorageError)` if query failed
    async fn get_usage(&self, path: &str) -> Result<u64, StorageError>;

    /// Check health of storage backend
    ///
    /// # Returns
    /// * `Ok(())` if storage backend is healthy
    /// * `Err(StorageError)` if health check failed
    async fn health_check(&self) -> Result<(), StorageError>;

    /// List directories under a path (optional, for debugging)
    ///
    /// # Arguments
    /// * `path` - Remote path to list
    ///
    /// # Returns
    /// * `Ok(Vec<String>)` - List of directory names
    /// * `Err(StorageError)` if listing failed
    async fn list_directories(&self, path: &str) -> Result<Vec<String>, StorageError> {
        // Default implementation returns empty list
        // Concrete implementations can override
        let _ = path; // Suppress unused warning
        Ok(Vec::new())
    }
}

/// Storage errors
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Directory not found: {0}")]
    NotFound(String),

    #[error("Directory already exists: {0}")]
    AlreadyExists(String),

    #[error("Quota exceeded: path={path}, limit={limit_bytes}, actual={actual_bytes}")]
    QuotaExceeded {
        path: String,
        limit_bytes: u64,
        actual_bytes: u64,
    },

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Timeout while communicating with storage backend")]
    Timeout,

    #[error("Backend unavailable: {0}")]
    Unavailable(String),

    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Unknown storage error: {0}")]
    Unknown(String),
}

impl From<reqwest::Error> for StorageError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            StorageError::Timeout
        } else if err.is_connect() {
            StorageError::Network(err.to_string())
        } else {
            StorageError::Unknown(err.to_string())
        }
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(err: serde_json::Error) -> Self {
        StorageError::Serialization(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock storage provider for testing
    pub struct MockStorageProvider {
        pub directories: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
        pub quotas: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    }

    impl MockStorageProvider {
        pub fn new() -> Self {
            Self {
                directories: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                quotas: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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

        async fn list_directories(&self, _path: &str) -> Result<Vec<String>, StorageError> {
            let dirs = self.directories.lock().unwrap();
            Ok(dirs.keys().cloned().collect())
        }
    }

    #[tokio::test]
    async fn test_mock_storage_provider() {
        let provider = MockStorageProvider::new();

        // Create directory
        provider.create_directory("/test/path").await.unwrap();

        // Verify it exists
        let usage = provider.get_usage("/test/path").await.unwrap();
        assert_eq!(usage, 0);

        // Set quota
        provider.set_quota("/test/path", 1000).await.unwrap();

        // List directories
        let dirs = provider.list_directories("/test").await.unwrap();
        assert!(dirs.contains(&"/test/path".to_string()));

        // Delete directory
        provider.delete_directory("/test/path").await.unwrap();

        // Verify it's gone
        assert!(provider.get_usage("/test/path").await.is_err());
    }

    #[tokio::test]
    async fn test_mock_storage_provider_errors() {
        let provider = MockStorageProvider::new();

        // Try to delete non-existent directory
        let result = provider.delete_directory("/nonexistent").await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));

        // Try to set quota on non-existent directory
        let result = provider.set_quota("/nonexistent", 1000).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));

        // Create directory
        provider.create_directory("/test/path").await.unwrap();

        // Try to create duplicate
        let result = provider.create_directory("/test/path").await;
        assert!(matches!(result, Err(StorageError::AlreadyExists(_))));
    }
}
