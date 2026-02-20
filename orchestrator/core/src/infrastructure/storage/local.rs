// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Local Filesystem Storage Provider
//!
//! Simple filesystem-based implementation of StorageProvider for single-node
//! development and testing. Not suitable for production multi-node deployments.
//!
//! **Architecture Context:**
//! This provider implements the Anti-Corruption Layer pattern from AGENTS.md,
//! isolating the domain from infrastructure details. Quota enforcement happens
//! at the Volume aggregate level (see ADR-032), not within this provider.
//!
//! **Limitations:**
//! - No multi-node volume sharing (files only accessible on local machine)
//! - No kernel-level quota enforcement (quotas stored but not enforced at filesystem layer)
//! - No replication or high availability
//! - Manual cleanup required if process crashes before TTL expires
//!
//! **Use Cases:**
//! - ✅ Local development without Docker Compose
//! - ✅ Edge devices without network access to distributed storage
//! - ✅ Unit/integration testing
//! - ❌ Production multi-node clusters (use SeaweedFS per ADR-032)

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write, Seek, SeekFrom};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use crate::domain::storage::{StorageProvider, StorageError, FileHandle, OpenMode, FileAttributes, DirEntry, FileType};

/// Local filesystem storage provider
///
/// Stores volumes as directories on the local filesystem.
/// Per ADR-032, quota enforcement happens at the Volume aggregate level
/// (Infrastructure & Hosting Context), not within this storage provider.
/// This provider is responsible for tracking quota metadata only.
pub struct LocalStorageProvider {
    /// Base directory for all volumes (e.g., "/var/lib/aegis/local-volumes")
    base_path: PathBuf,
    
    /// In-memory quota tracking (path -> max_bytes)
    /// Note: Quotas are enforced by Volume domain logic, not at filesystem layer.
    /// Per ADR-032, ephemeral storage volumes provide TTL-based lifecycle management.
    quotas: Arc<RwLock<HashMap<String, u64>>>,
}

impl LocalStorageProvider {
    /// Create new local storage provider
    ///
    /// # Arguments
    /// * `base_path` - Base directory for volume storage
    ///
    /// # Returns
    /// * `Result<Self, StorageError>` - Provider instance or error
    ///
    /// # Example
    /// ```ignore
    /// let provider = LocalStorageProvider::new("/var/lib/aegis/local-volumes")?;
    /// ```
    pub fn new(base_path: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let base_path = base_path.into();
        
        // Create base directory if it doesn't exist
        std::fs::create_dir_all(&base_path)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to create base directory {}: {}",
                base_path.display(),
                e
            )))?;
        
        // Verify directory is writable
        let test_file = base_path.join(".aegis-storage-test");
        std::fs::write(&test_file, b"test")
            .map_err(|e| StorageError::IoError(format!(
                "Base directory {} is not writable: {}",
                base_path.display(),
                e
            )))?;
        std::fs::remove_file(&test_file)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to cleanup test file: {}",
                e
            )))?;
        
        Ok(Self {
            base_path,
            quotas: Arc::new(RwLock::new(HashMap::new())),
        })
    }
    
    /// Resolve volume path to absolute filesystem path
    fn resolve_path(&self, path: &str) -> PathBuf {
        // Remove leading slash if present
        let path = path.strip_prefix('/').unwrap_or(path);
        self.base_path.join(path)
    }
    
    /// Calculate directory size recursively
    fn calculate_size(path: &Path) -> Result<u64, std::io::Error> {
        let mut total = 0u64;
        
        if path.is_dir() {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                
                if metadata.is_dir() {
                    total += Self::calculate_size(&entry.path())?;
                } else {
                    total += metadata.len();
                }
            }
        } else {
            total = path.metadata()?.len();
        }
        
        Ok(total)
    }
}

#[async_trait]
impl StorageProvider for LocalStorageProvider {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string()
            ));
        }
        
        let fs_path = self.resolve_path(path);
        
        // Check if directory already exists
        if fs_path.exists() {
            return Err(StorageError::AlreadyExists(path.to_string()));
        }
        
        // Create directory with all parent directories
        std::fs::create_dir_all(&fs_path)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to create directory {}: {}",
                path,
                e
            )))?;
        
        Ok(())
    }
    
    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        let fs_path = self.resolve_path(path);
        
        // Check if directory exists
        if !fs_path.exists() {
            return Err(StorageError::NotFound(path.to_string()));
        }
        
        // Remove directory and all contents
        std::fs::remove_dir_all(&fs_path)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to delete directory {}: {}",
                path,
                e
            )))?;
        
        // Remove quota metadata (enforcement is at Volume aggregate level per ADR-032)
        let mut quotas = self.quotas.write().unwrap();
        quotas.remove(path);
        
        Ok(())
    }
    
    async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
        let fs_path = self.resolve_path(path);
        
        // Verify directory exists
        if !fs_path.exists() {
            return Err(StorageError::NotFound(path.to_string()));
        }
        
        // Store quota metadata for tracking
        // Per ADR-032, actual enforcement happens at Volume aggregate level via
        // ephemeral storage containers, not at this infrastructure layer.
        let mut quotas = self.quotas.write().unwrap();
        quotas.insert(path.to_string(), bytes);
        
        Ok(())
    }
    
    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        let fs_path = self.resolve_path(path);
        
        // Verify directory exists
        if !fs_path.exists() {
            return Err(StorageError::NotFound(path.to_string()));
        }
        
        // Calculate total size recursively
        Self::calculate_size(&fs_path)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to calculate usage for {}: {}",
                path,
                e
            )))
    }
    
    async fn health_check(&self) -> Result<(), StorageError> {
        // Check if base directory exists and is writable
        if !self.base_path.exists() {
            return Err(StorageError::IoError(format!(
                "Base directory {} does not exist",
                self.base_path.display()
            )));
        }
        
        // Try to create a test file
        let test_file = self.base_path.join(".health-check");
        std::fs::write(&test_file, b"health-check")
            .map_err(|e| StorageError::IoError(format!(
                "Health check failed (not writable): {}",
                e
            )))?;
        
        std::fs::remove_file(&test_file)
            .map_err(|e| StorageError::IoError(format!(
                "Health check cleanup failed: {}",
                e
            )))?;
        
        Ok(())
    }
    
    async fn list_directories(&self, path: &str) -> Result<Vec<String>, StorageError> {
        let fs_path = self.resolve_path(path);
        
        // Verify directory exists
        if !fs_path.exists() {
            return Err(StorageError::NotFound(path.to_string()));
        }
        
        // List all subdirectories
        let mut directories = Vec::new();
        
        for entry in std::fs::read_dir(&fs_path)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to list directory {}: {}",
                path,
                e
            )))?
        {
            let entry = entry.map_err(|e| StorageError::IoError(format!(
                "Failed to read directory entry: {}",
                e
            )))?;
            
            if entry.metadata()
                .map_err(|e| StorageError::IoError(format!(
                    "Failed to get metadata: {}",
                    e
                )))?
                .is_dir()
            {
                if let Some(name) = entry.file_name().to_str() {
                    directories.push(name.to_string());
                }
            }
        }
        
        Ok(directories)
    }

    // --- POSIX File Operations (ADR-036) ---

    async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
        let fs_path = self.resolve_path(path);
        
        // Validate the file is accessible based on mode
        match mode {
            OpenMode::ReadOnly => {
                // Check path accessibility via metadata without consuming a file descriptor
                std::fs::metadata(&fs_path)
                    .map_err(|e| StorageError::FileNotFound(format!("{}: {}", path, e)))?;
            }
            OpenMode::WriteOnly => {
                File::options()
                    .write(true)
                    .open(&fs_path)
                    .map_err(|e| StorageError::FileNotFound(format!("{}: {}", path, e)))?;
            }
            OpenMode::ReadWrite => {
                File::options()
                    .read(true)
                    .write(true)
                    .open(&fs_path)
                    .map_err(|e| StorageError::FileNotFound(format!("{}: {}", path, e)))?;
            }
            OpenMode::Create => {
                File::create(&fs_path)
                    .map_err(|e| StorageError::IoError(format!("Failed to create {}: {}", path, e)))?;
            }
        };
        
        // Create file handle encoding path
        // In a real implementation, we might store File in a HashMap<FileHandle, File>
        // For simplicity, we encode path and reopen on each operation
        let handle_data = path.as_bytes().to_vec();
        Ok(FileHandle(handle_data))
    }

    async fn read_at(&self, handle: &FileHandle, offset: u64, length: usize) -> Result<Vec<u8>, StorageError> {
        // Decode path from handle
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid file handle".to_string()))?;
        
        let fs_path = self.resolve_path(&path);
        
        let mut file = File::open(&fs_path)
            .map_err(|e| StorageError::FileNotFound(format!("{}: {}", path, e)))?;
        
        // Seek to offset
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| StorageError::IoError(format!("Seek failed: {}", e)))?;
        
        // Read data
        let mut buffer = vec![0u8; length];
        let bytes_read = file.read(&mut buffer)
            .map_err(|e| StorageError::IoError(format!("Read failed: {}", e)))?;
        
        buffer.truncate(bytes_read);
        Ok(buffer)
    }

    async fn write_at(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<usize, StorageError> {
        // Decode path from handle
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid file handle".to_string()))?;
        
        let fs_path = self.resolve_path(&path);
        
        let mut file = File::options()
            .write(true)
            .open(&fs_path)
            .map_err(|e| StorageError::FileNotFound(format!("{}: {}", path, e)))?;
        
        // Seek to offset
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| StorageError::IoError(format!("Seek failed: {}", e)))?;
        
        // Write data
        file.write_all(data)
            .map_err(|e| StorageError::IoError(format!("Write failed: {}", e)))?;
        
        Ok(data.len())
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        // File automatically closed when dropped
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let fs_path = self.resolve_path(path);
        
        let metadata = std::fs::metadata(&fs_path)
            .map_err(|e| StorageError::FileNotFound(format!("{}: {}", path, e)))?;
        
        let file_type = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_symlink() {
            FileType::Symlink
        } else {
            FileType::File
        };
        
        // Use Unix-specific metadata for uid/gid
        #[cfg(unix)]
        let (uid, gid) = (metadata.uid(), metadata.gid());
        
        #[cfg(not(unix))]
        let (uid, gid) = (1000, 1000);
        
        let mtime = metadata.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        
        let atime = metadata.accessed()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(mtime);
        
        let ctime = metadata.created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(mtime);
        
        #[cfg(unix)]
        let mode = metadata.mode();
        
        #[cfg(not(unix))]
        let mode = if file_type == FileType::Directory { 0o755 } else { 0o644 };
        
        #[cfg(unix)]
        let nlink = metadata.nlink() as u32;
        
        #[cfg(not(unix))]
        let nlink = 1;
        
        Ok(FileAttributes {
            file_type,
            size: metadata.len(),
            mtime,
            atime,
            ctime,
            mode,
            uid,
            gid,
            nlink,
        })
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let fs_path = self.resolve_path(path);
        
        if !fs_path.exists() {
            return Err(StorageError::NotFound(path.to_string()));
        }
        
        let mut entries = Vec::new();
        
        for entry in std::fs::read_dir(&fs_path)
            .map_err(|e| StorageError::IoError(format!("Failed to read directory {}: {}", path, e)))?
        {
            let entry = entry
                .map_err(|e| StorageError::IoError(format!("Failed to read entry: {}", e)))?;
            
            let metadata = entry.metadata()
                .map_err(|e| StorageError::IoError(format!("Failed to get metadata: {}", e)))?;
            
            let file_type = if metadata.is_dir() {
                FileType::Directory
            } else if metadata.is_symlink() {
                FileType::Symlink
            } else {
                FileType::File
            };
            
            if let Some(name) = entry.file_name().to_str() {
                entries.push(DirEntry {
                    name: name.to_string(),
                    file_type,
                });
            }
        }
        
        Ok(entries)
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        let fs_path = self.resolve_path(path);
        
        // Create parent directories if needed
        if let Some(parent) = fs_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StorageError::IoError(format!("Failed to create parent dirs: {}", e)))?;
        }
        
        // Create file
        File::create(&fs_path)
            .map_err(|e| StorageError::IoError(format!("Failed to create {}: {}", path, e)))?;
        
        // Return handle
        let handle_data = path.as_bytes().to_vec();
        Ok(FileHandle(handle_data))
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let fs_path = self.resolve_path(path);
        
        if !fs_path.exists() {
            return Err(StorageError::FileNotFound(path.to_string()));
        }
        
        std::fs::remove_file(&fs_path)
            .map_err(|e| StorageError::IoError(format!("Failed to delete {}: {}", path, e)))?;
        
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_path = self.resolve_path(from);
        let to_path = self.resolve_path(to);
        
        if !from_path.exists() {
            return Err(StorageError::FileNotFound(from.to_string()));
        }
        
        // Create parent directory for destination if needed
        if let Some(parent) = to_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StorageError::IoError(format!(
                    "Failed to create parent directory for {}: {}",
                    to, e
                )))?;
        }
        
        // Perform atomic rename
        std::fs::rename(&from_path, &to_path)
            .map_err(|e| StorageError::IoError(format!(
                "Failed to rename {} to {}: {}",
                from, to, e
            )))?;
        
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[tokio::test]
    async fn test_create_and_delete_directory() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Create directory
        provider.create_directory("/test-volume").await.unwrap();
        
        // Verify it exists
        let fs_path = temp_dir.path().join("test-volume");
        assert!(fs_path.exists());
        
        // Delete directory
        provider.delete_directory("/test-volume").await.unwrap();
        
        // Verify it's gone
        assert!(!fs_path.exists());
    }
    
    #[tokio::test]
    async fn test_create_duplicate_fails() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Create directory
        provider.create_directory("/test-volume").await.unwrap();
        
        // Try to create again
        let result = provider.create_directory("/test-volume").await;
        assert!(matches!(result, Err(StorageError::AlreadyExists(_))));
    }
    
    #[tokio::test]
    async fn test_delete_nonexistent_fails() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Try to delete non-existent directory
        let result = provider.delete_directory("/nonexistent").await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }
    
    #[tokio::test]
    async fn test_set_quota_and_get_usage() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Create directory
        provider.create_directory("/test-volume").await.unwrap();
        
        // Set quota
        provider.set_quota("/test-volume", 1024 * 1024).await.unwrap();
        
        // Get initial usage (should be near zero)
        let usage = provider.get_usage("/test-volume").await.unwrap();
        assert!(usage < 1024); // Less than 1KB
        
        // Write a file
        let file_path = temp_dir.path().join("test-volume").join("test.txt");
        std::fs::write(&file_path, b"Hello, World!").unwrap();
        
        // Get usage again (should be file size)
        let usage = provider.get_usage("/test-volume").await.unwrap();
        assert!(usage >= 13); // At least 13 bytes ("Hello, World!")
    }
    
    #[tokio::test]
    async fn test_health_check() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Health check should pass
        provider.health_check().await.unwrap();
    }
    
    #[tokio::test]
    async fn test_list_directories() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Create multiple directories
        provider.create_directory("/volume-1").await.unwrap();
        provider.create_directory("/volume-2").await.unwrap();
        provider.create_directory("/volume-3").await.unwrap();
        
        // List directories
        let mut dirs = provider.list_directories("/").await.unwrap();
        dirs.sort();
        
        assert_eq!(dirs, vec!["volume-1", "volume-2", "volume-3"]);
    }
    
    #[tokio::test]
    async fn test_nested_directories() {
        let temp_dir = TempDir::new().unwrap();
        let provider = LocalStorageProvider::new(temp_dir.path()).unwrap();
        
        // Create nested directory structure
        provider.create_directory("/tenant-1/execution-123/workspace").await.unwrap();
        
        // Verify it exists
        let fs_path = temp_dir.path()
            .join("tenant-1")
            .join("execution-123")
            .join("workspace");
        assert!(fs_path.exists());
        
        // Calculate usage
        let usage = provider.get_usage("/tenant-1/execution-123/workspace").await.unwrap();
        assert!(usage < 1024);
    }
}
