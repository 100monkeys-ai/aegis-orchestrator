// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Storage Provider Trait - Anti-Corruption Layer for SeaweedFS
//!
//! Provides abstraction over storage backend (SeaweedFS) to isolate
//! domain from external technology choices. Enables testing with mocks
//! and potential future migration to different storage systems.
//!
//! Follows DDD Anti-Corruption Layer pattern from AGENTS.md.
//!
//! Extended with POSIX file operations per ADR-036 for NFS Server Gateway.

use async_trait::async_trait;
use thiserror::Error;
use serde::{Serialize, Deserialize};

/// Opaque file handle for POSIX operations
///
/// Represents an open file. The internal structure is provider-specific.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct FileHandle(pub Vec<u8>);

/// File open mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OpenMode {
    /// Read-only access
    ReadOnly,
    /// Write-only access
    WriteOnly,
    /// Read-write access
    ReadWrite,
    /// Create new file (with write access)
    Create,
}

/// File type for directory entries and attributes
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileType {
    /// Regular file
    File,
    /// Directory
    Directory,
    /// Symbolic link
    Symlink,
}

/// POSIX file attributes
///
/// Represents file metadata returned by stat() operations.
/// Used by NFS server to provide file information to clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAttributes {
    /// File type (file, directory, symlink)
    pub file_type: FileType,
    /// File size in bytes
    pub size: u64,
    /// Last modification time (Unix timestamp)
    pub mtime: i64,
    /// Last access time (Unix timestamp)
    pub atime: i64,
    /// Creation time (Unix timestamp)
    pub ctime: i64,
    /// POSIX permissions (e.g., 0o755)
    /// Note: Overridden by UID/GID squashing in NFS gateway
    pub mode: u32,
    /// User ID (overridden by container UID in NFS gateway)
    pub uid: u32,
    /// Group ID (overridden by container GID in NFS gateway)
    pub gid: u32,
    /// Hard link count
    pub nlink: u32,
}

/// Directory entry
///
/// Represents a single entry in directory listing (readdir).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    /// File/directory name (not including path)
    pub name: String,
    /// File type
    pub file_type: FileType,
}

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

    // --- POSIX File Operations (ADR-036) ---

    /// Open a file and return a handle
    ///
    /// # Arguments
    /// * `path` - Remote file path
    /// * `mode` - Open mode (ReadOnly, WriteOnly, ReadWrite, Create)
    ///
    /// # Returns
    /// * `Ok(FileHandle)` - Opaque file handle for subsequent operations
    /// * `Err(StorageError)` if open failed
    async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError>;

    /// Read data from file at specific offset
    ///
    /// # Arguments
    /// * `handle` - File handle from open_file
    /// * `offset` - Byte offset to start reading
    /// * `length` - Number of bytes to read
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Data read (may be shorter than length at EOF)
    /// * `Err(StorageError)` if read failed
    async fn read_at(&self, handle: &FileHandle, offset: u64, length: usize) -> Result<Vec<u8>, StorageError>;

    /// Write data to file at specific offset
    ///
    /// # Arguments
    /// * `handle` - File handle from open_file
    /// * `offset` - Byte offset to start writing
    /// * `data` - Data to write
    ///
    /// # Returns
    /// * `Ok(usize)` - Number of bytes written
    /// * `Err(StorageError)` if write failed
    async fn write_at(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<usize, StorageError>;

    /// Close file handle
    ///
    /// # Arguments
    /// * `handle` - File handle to close
    ///
    /// # Returns
    /// * `Ok(())` if closed successfully
    /// * `Err(StorageError)` if close failed
    async fn close_file(&self, handle: &FileHandle) -> Result<(), StorageError>;

    /// Get file attributes (stat)
    ///
    /// # Arguments
    /// * `path` - Remote file path
    ///
    /// # Returns
    /// * `Ok(FileAttributes)` - File metadata
    /// * `Err(StorageError)` if stat failed
    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError>;

    /// List directory contents (readdir)
    ///
    /// # Arguments
    /// * `path` - Remote directory path
    ///
    /// # Returns
    /// * `Ok(Vec<DirEntry>)` - Directory entries
    /// * `Err(StorageError)` if readdir failed
    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError>;

    /// Create a new file
    ///
    /// # Arguments
    /// * `path` - Remote file path
    /// * `mode` - POSIX permissions (e.g., 0o644)
    ///
    /// # Returns
    /// * `Ok(FileHandle)` - Handle to newly created file
    /// * `Err(StorageError)` if create failed
    async fn create_file(&self, path: &str, mode: u32) -> Result<FileHandle, StorageError>;

    /// Delete a file
    ///
    /// # Arguments
    /// * `path` - Remote file path
    ///
    /// # Returns
    /// * `Ok(())` if deleted successfully
    /// * `Err(StorageError)` if delete failed
    async fn delete_file(&self, path: &str) -> Result<(), StorageError>;

    /// Rename/move a file or directory
    ///
    /// # Arguments
    /// * `from` - Source path
    /// * `to` - Destination path
    ///
    /// # Returns
    /// * `Ok(())` if renamed successfully
    /// * `Err(StorageError)` if rename failed
    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError>;
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
    
    #[error("File not found: {0}")]
    FileNotFound(String),

    #[error("IO error: {0}")]
    IoError(String),

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

impl From<std::io::Error> for StorageError {
    fn from(err: std::io::Error) -> Self {
        StorageError::IoError(err.to_string())
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

        async fn rename(&self, _from_path: &str, _to_path: &str) -> Result<(), StorageError> {
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
