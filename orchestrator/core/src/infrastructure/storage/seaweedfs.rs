// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! SeaweedFS Storage Provider Implementation
//!
//! Provides SeaweedFS-backed distributed storage for AEGIS volumes.
//! Implements the StorageProvider trait as an Anti-Corruption Layer.
//!
//! # Architecture
//!
//! SeaweedFS components:
//! - **Master**: Metadata and leader election
//! - **Volume Server**: Data storage with replication
//! - **Filer**: File system metadata + HTTP API (what we use)
//!
//! # API Endpoints
//!
//! - `GET /dir/status?path=/path` - Get directory info
//! - `POST /dir/` - Create directory
//! - `DELETE /dir/?path=/path` - Delete directory
//! - `POST /quota?path=/path&bytes=1000000` - Set quota
//! - `GET /` - Health check

use async_trait::async_trait;
use reqwest::{Client, StatusCode, multipart};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use chrono;
use crate::domain::storage::{StorageProvider, StorageError, FileHandle, OpenMode, FileAttributes, DirEntry, FileType};

/// SeaweedFS Filer adapter
pub struct SeaweedFSAdapter {
    /// HTTP client for communicating with filer
    client: Client,
    
    /// Filer base URL (e.g., "http://localhost:8888")
    filer_url: String,
}

impl SeaweedFSAdapter {
    /// Create new SeaweedFS adapter
    ///
    /// # Arguments
    /// * `filer_url` - Base URL of SeaweedFS filer (e.g., "http://localhost:8888")
    ///
    /// # Returns
    /// * `Self` - Configured adapter instance
    pub fn new(filer_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");
        
        Self {
            client,
            filer_url: filer_url.into(),
        }
    }

    /// Create adapter with custom timeout
    pub fn with_timeout(filer_url: impl Into<String>, timeout: Duration) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .expect("Failed to create HTTP client");
        
        Self {
            client,
            filer_url: filer_url.into(),
        }
    }

    /// Build full URL for API endpoint
    fn build_url(&self, path: &str) -> String {
        format!("{}{}", self.filer_url, path)
    }
}

#[async_trait]
impl StorageProvider for SeaweedFSAdapter {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string()
            ));
        }

        // SeaweedFS filer creates directories automatically when files are uploaded
        // But we can explicitly create them via POST to /dir/
        let url = self.build_url("/dir/");
        
        let form = multipart::Form::new()
            .text("path", path.to_string());
        
        let response = self.client
            .post(&url)
            .multipart(form)
            .send()
            .await?;

        match response.status() {
            StatusCode::CREATED | StatusCode::OK => Ok(()),
            StatusCode::CONFLICT => {
                // Directory already exists - treat as success (idempotent operation)
                tracing::debug!("Directory {} already exists, treating as success", path);
                Ok(())
            }
            status => {
                let error_msg = response.text().await
                    .unwrap_or_else(|_| format!("HTTP {}", status));
                Err(StorageError::Unknown(format!(
                    "Failed to create directory {}: {}",
                    path, error_msg
                )))
            }
        }
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string()
            ));
        }

        let url = self.build_url("/dir/");
        
        let response = self.client
            .delete(&url)
            .query(&[("path", path), ("recursive", "true")])
            .send()
            .await?;

        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            StatusCode::NOT_FOUND => {
                Err(StorageError::NotFound(path.to_string()))
            }
            status => {
                let error_msg = response.text().await
                    .unwrap_or_else(|_| format!("HTTP {}", status));
                Err(StorageError::Unknown(format!(
                    "Failed to delete directory {}: {}",
                    path, error_msg
                )))
            }
        }
    }

    async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string()
            ));
        }

        let url = self.build_url("/quota");
        
        let form = multipart::Form::new()
            .text("path", path.to_string())
            .text("bytes", bytes.to_string());
        
        let response = self.client
            .post(&url)
            .multipart(form)
            .send()
            .await?;

        match response.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(()),
            StatusCode::NOT_FOUND => {
                Err(StorageError::NotFound(path.to_string()))
            }
            status => {
                let error_msg = response.text().await
                    .unwrap_or_else(|_| format!("HTTP {}", status));
                Err(StorageError::Unknown(format!(
                    "Failed to set quota for {}: {}",
                    path, error_msg
                )))
            }
        }
    }

    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string()
            ));
        }

        let url = self.build_url("/dir/status");
        
        let response = self.client
            .get(&url)
            .query(&[("path", path)])
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let status: DirectoryStatus = response.json().await
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                
                Ok(status.total_size)
            }
            StatusCode::NOT_FOUND => {
                Err(StorageError::NotFound(path.to_string()))
            }
            status => {
                let error_msg = response.text().await
                    .unwrap_or_else(|_| format!("HTTP {}", status));
                Err(StorageError::Unknown(format!(
                    "Failed to get usage for {}: {}",
                    path, error_msg
                )))
            }
        }
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        let url = self.build_url("/");
        
        let response = self.client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(StorageError::Unavailable(format!(
                "Filer returned status {}",
                response.status()
            )))
        }
    }

    async fn list_directories(&self, path: &str) -> Result<Vec<String>, StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string()
            ));
        }

        let url = self.build_url(path);
        
        let response = self.client
            .get(&url)
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let listing: DirectoryListing = response.json().await
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                
                let dirs = listing.entries
                    .into_iter()
                    .filter(|e| e.is_directory)
                    .map(|e| e.name)
                    .collect();
                
                Ok(dirs)
            }
            StatusCode::NOT_FOUND => {
                Err(StorageError::NotFound(path.to_string()))
            }
            status => {
                let error_msg = response.text().await
                    .unwrap_or_else(|_| format!("HTTP {}", status));
                Err(StorageError::Unknown(format!(
                    "Failed to list directories in {}: {}",
                    path, error_msg
                )))
            }
        }
    }

    // --- POSIX File Operations (ADR-036) ---

    async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
        // For SeaweedFS HTTP API, we don't need to actually "open" files
        // The FileHandle just stores the path for subsequent operations
        // Real implementations would validate file exists for ReadOnly mode
        
        if matches!(mode, OpenMode::ReadOnly) {
            // Verify file exists via HEAD request
            let url = self.build_url(path);
            let response = self.client.head(&url).send().await?;
            
            if !response.status().is_success() {
                return Err(StorageError::FileNotFound(path.to_string()));
            }
        }
        
        // Create file handle encoding path
        let handle_data = path.as_bytes().to_vec();
        Ok(FileHandle(handle_data))
    }

    async fn read_at(&self, handle: &FileHandle, offset: u64, length: usize) -> Result<Vec<u8>, StorageError> {
        // Decode path from handle
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid file handle".to_string()))?;
        
        let url = self.build_url(&path);
        
        // Use HTTP Range header for partial reads
        let range_header = format!("bytes={}-{}", offset, offset + length as u64 - 1);
        
        let response = self.client
            .get(&url)
            .header("Range", range_header)
            .send()
            .await?;
        
        match response.status() {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                let bytes = response.bytes().await?;
                Ok(bytes.to_vec())
            }
            StatusCode::NOT_FOUND => {
                Err(StorageError::FileNotFound(path))
            }
            status => {
                Err(StorageError::Unknown(format!(
                    "Failed to read file {}: HTTP {}",
                    path, status
                )))
            }
        }
    }

    async fn write_at(&self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<usize, StorageError> {
        // Decode path from handle
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid file handle".to_string()))?;
        
        let url = self.build_url(&path);
        
        // For simplicity, we'll read existing content, modify, and write back
        // A production implementation would use proper partial write support
        // or append-only writes for efficiency
        
        let mut content = if offset > 0 {
            // Read existing content if we're writing at an offset
            let response = self.client.get(&url).send().await;
            if let Ok(resp) = response {
                if resp.status().is_success() {
                    resp.bytes().await.unwrap_or_default().to_vec()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        
        // Extend content if needed
        if content.len() < offset as usize {
            content.resize(offset as usize, 0);
        }
        
        // Write data at offset
        if offset as usize + data.len() > content.len() {
            content.resize(offset as usize + data.len(), 0);
        }
        content[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        
        // Write back to SeaweedFS
        let response = self.client
            .post(&url)
            .body(content)
            .send()
            .await?;
        
        if response.status().is_success() {
            Ok(data.len())
        } else {
            Err(StorageError::Unknown(format!(
                "Failed to write file {}: HTTP {}",
                path, response.status()
            )))
        }
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        // HTTP-based storage doesn't need explicit close
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let url = self.build_url(path);
        
        let response = self.client.head(&url).send().await?;
        
        match response.status() {
            StatusCode::OK => {
                // Extract metadata from headers
                let size = response.headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                
                let mtime = response.headers()
                    .get("last-modified")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| chrono::DateTime::parse_from_rfc2822(s).ok())
                    .map(|dt| dt.timestamp())
                    .unwrap_or_else(|| chrono::Utc::now().timestamp());
                
                Ok(FileAttributes {
                    file_type: FileType::File,
                    size,
                    mtime,
                    atime: mtime,
                    ctime: mtime,
                    mode: 0o644,
                    uid: 1000,
                    gid: 1000,
                    nlink: 1,
                })
            }
            StatusCode::NOT_FOUND => {
                Err(StorageError::FileNotFound(path.to_string()))
            }
            status => {
                Err(StorageError::Unknown(format!(
                    "Failed to stat {}: HTTP {}",
                    path, status
                )))
            }
        }
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let url = self.build_url(path);
        
        let response = self.client.get(&url).send().await?;
        
        match response.status() {
            StatusCode::OK => {
                let listing: DirectoryListing = response.json().await
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;
                
                let entries = listing.entries
                    .into_iter()
                    .map(|e| DirEntry {
                        name: e.name.rsplit('/').next().unwrap_or(&e.name).to_string(),
                        file_type: if e.is_directory { FileType::Directory } else { FileType::File },
                    })
                    .collect();
                
                Ok(entries)
            }
            StatusCode::NOT_FOUND => {
                Err(StorageError::NotFound(path.to_string()))
            }
            status => {
                Err(StorageError::Unknown(format!(
                    "Failed to readdir {}: HTTP {}",
                    path, status
                )))
            }
        }
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        let url = self.build_url(path);
        
        // Create empty file
        let response = self.client
            .post(&url)
            .body(Vec::<u8>::new())
            .send()
            .await?;
        
        if response.status().is_success() {
            let handle_data = path.as_bytes().to_vec();
            Ok(FileHandle(handle_data))
        } else {
            Err(StorageError::Unknown(format!(
                "Failed to create file {}: HTTP {}",
                path, response.status()
            )))
        }
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let url = self.build_url(path);
        
        let response = self.client.delete(&url).send().await?;
        
        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            StatusCode::NOT_FOUND => Err(StorageError::FileNotFound(path.to_string())),
            status => {
                Err(StorageError::Unknown(format!(
                    "Failed to delete file {}: HTTP {}",
                    path, status
                )))
            }
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        // SeaweedFS doesn't have a native rename operation via HTTP API
        // We implement it as copy + delete
        // Note: This is not atomic, but acceptable for Phase 1
        
        // 1. Check source exists
        let from_url = self.build_url(from);
        let check_response = self.client.head(&from_url).send().await?;
        
        if !check_response.status().is_success() {
            return Err(StorageError::FileNotFound(from.to_string()));
        }
        
        // 2. Read source file
        let read_response = self.client.get(&from_url).send().await?;
        
        if !read_response.status().is_success() {
            return Err(StorageError::Unknown(format!(
                "Failed to read source file {}",
                from
            )));
        }
        
        let data = read_response.bytes().await?
            .to_vec();
        
        // 3. Write to destination
        let to_url = self.build_url(to);
        let write_response = self.client
            .post(&to_url)
            .body(data)
            .send()
            .await?;
        
        if !write_response.status().is_success() {
            return Err(StorageError::Unknown(format!(
                "Failed to write destination file {}",
                to
            )));
        }
        
        // 4. Delete source
        let delete_response = self.client.delete(&from_url).send().await?;
        
        if !delete_response.status().is_success() {
            // Log warning but don't fail - destination file was created
            tracing::warn!("Failed to delete source file {} after rename", from);
        }
        
        Ok(())
    }
}

// ============================================================================
// SeaweedFS API Response Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct DirectoryStatus {
    #[serde(rename = "TotalSize")]
    total_size: u64,
    
    #[serde(rename = "FileCount")]
    file_count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct DirectoryListing {
    #[serde(rename = "Path")]
    path: String,
    
    #[serde(rename = "Entries")]
    entries: Vec<DirectoryEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DirectoryEntry {
    #[serde(rename = "FullPath")]
    name: String,
    
    #[serde(rename = "Mtime")]
    mtime: String,
    
    #[serde(rename = "Mode")]
    mode: u32,
    
    #[serde(default)]
    #[serde(rename = "IsDirectory")]
    is_directory: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_creation() {
        let adapter = SeaweedFSAdapter::new("http://localhost:8888");
        assert_eq!(adapter.filer_url, "http://localhost:8888");
    }

    #[test]
    fn test_url_building() {
        let adapter = SeaweedFSAdapter::new("http://localhost:8888");
        assert_eq!(adapter.build_url("/dir/"), "http://localhost:8888/dir/");
        assert_eq!(adapter.build_url("/quota"), "http://localhost:8888/quota");
    }

    #[tokio::test]
    async fn test_invalid_path_rejection() {
        let adapter = SeaweedFSAdapter::new("http://localhost:8888");
        
        // Path must start with /
        let result = adapter.create_directory("invalid/path").await;
        assert!(matches!(result, Err(StorageError::InvalidPath(_))));
    }

    // Integration tests require running SeaweedFS instance
    // Run these manually with: cargo test --package orchestrator --lib -- --ignored
    
    #[tokio::test]
    #[ignore]
    async fn integration_test_directory_lifecycle() {
        let adapter = SeaweedFSAdapter::new("http://localhost:8888");
        
        // Health check
        adapter.health_check().await.unwrap();
        
        // Create directory
        let path = "/test/integration/dir";
        adapter.create_directory(path).await.unwrap();
        
        // Test idempotency - creating same directory again should succeed
        adapter.create_directory(path).await.unwrap();
        
        // Set quota
        adapter.set_quota(path, 10_000_000).await.unwrap();
        
        // Get usage (should be 0 initially)
        let usage = adapter.get_usage(path).await.unwrap();
        assert_eq!(usage, 0);
        
        // Delete directory
        adapter.delete_directory(path).await.unwrap();
        
        // Verify deletion
        let result = adapter.get_usage(path).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }
}
