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
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use crate::domain::storage::{StorageProvider, StorageError};

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
        
        let response = self.client
            .post(&url)
            .query(&[("path", path)])
            .send()
            .await?;

        match response.status() {
            StatusCode::CREATED | StatusCode::OK => Ok(()),
            StatusCode::CONFLICT => {
                Err(StorageError::AlreadyExists(path.to_string()))
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
        
        let response = self.client
            .post(&url)
            .query(&[("path", path), ("bytes", &bytes.to_string())])
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
