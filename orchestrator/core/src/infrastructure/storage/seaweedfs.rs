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

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError, StorageProvider,
};
use async_trait::async_trait;
use chrono;
use futures::StreamExt;
use reqwest::{multipart, Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Hard ceiling for any single response body materialised from SeaweedFS.
/// Per security audit 002 §4.22, raw `.bytes()` reads are unbounded and a
/// hostile / compromised filer could exhaust orchestrator memory. The cap
/// is the smaller of the per-volume request size and 100 MiB. Reads larger
/// than this MUST be issued via ranged `read_at` against the upper layers.
const SEAWEEDFS_MAX_RESPONSE_BYTES: u64 = 100 * 1024 * 1024;

/// Ceiling for an error body the filer answers, read only to name it.
const SEAWEEDFS_MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// Stream the response body chunk-by-chunk, enforcing a running byte cap.
/// Returns `StorageError::Unknown` the moment the running count exceeds
/// `max_bytes` — without buffering further chunks.
async fn read_capped_body(response: Response, max_bytes: u64) -> Result<Vec<u8>, StorageError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| StorageError::Unknown(format!("body stream error: {e}")))?;
        total = total.saturating_add(chunk.len() as u64);
        if total > max_bytes {
            drop(buf);
            return Err(StorageError::Unknown(format!(
                "SeaweedFS response exceeded {max_bytes}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

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

    /// The whole content of the file at `url`, for a write that splices
    /// into it or a truncate that cuts or extends it. A file the filer does
    /// not have reads as empty, as a write creates it; any other failure is
    /// an error, never an empty file, so a failed read cannot replace a
    /// file's content with only the bytes being written.
    async fn existing_content(&self, url: &str, path: &str) -> Result<Vec<u8>, StorageError> {
        let response = self.client.get(url).send().await?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(Vec::new()),
            status if status.is_success() => {
                read_capped_body(response, SEAWEEDFS_MAX_RESPONSE_BYTES).await
            }
            status => Err(StorageError::Unknown(format!(
                "Failed to read file {path} before writing it: HTTP {status}"
            ))),
        }
    }

    /// The filer's own entry for `path`: `GET <path>?metadata=true`, which
    /// SeaweedFS answers with the entry as JSON for a file and a directory
    /// alike (`weed/server/filer_server_handlers_read.go`,
    /// `GetOrHeadHandler`). The filer's root has no entry of its own and is
    /// a directory.
    async fn entry_metadata(&self, path: &str) -> Result<FilerEntryMetadata, StorageError> {
        if path.trim_matches('/').is_empty() {
            return Ok(FilerEntryMetadata {
                mode: GO_MODE_DIR,
                mtime: None,
            });
        }

        let response = self
            .client
            .get(self.build_url(path))
            .header("Accept", "application/json")
            .query(&[("metadata", "true")])
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let body = read_capped_body(response, SEAWEEDFS_MAX_RESPONSE_BYTES).await?;
                serde_json::from_slice(&body)
                    .map_err(|e| StorageError::Serialization(format!("metadata of {path}: {e}")))
            }
            StatusCode::NOT_FOUND => Err(StorageError::FileNotFound(path.to_string())),
            status => Err(StorageError::Unknown(format!(
                "Failed to stat {path}: HTTP {status}"
            ))),
        }
    }
}

#[async_trait]
impl StorageProvider for SeaweedFSAdapter {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string(),
            ));
        }

        // The filer's own mkdir: `POST <path>/` with no body
        // (`weed/server/filer_server_handlers_write.go`, `PostHandler`, a
        // path ending in `/` with no content). It makes every missing
        // ancestor, answers 201 for a directory it made and 409 for a path
        // it already holds. A multipart POST to `/dir/` is an upload into the
        // filer's own `/dir`, which makes nothing at `path`; and no file is
        // written into the directory, which a mount would show to the agent.
        let dir = path.trim_end_matches('/');
        if dir.is_empty() {
            return Ok(());
        }

        let response = self
            .client
            .post(self.build_url(&format!("{dir}/")))
            .send()
            .await?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::CONFLICT {
            tracing::debug!("Directory {} already exists, skipping", dir);
            return Ok(());
        }

        let body = read_capped_body(response, SEAWEEDFS_MAX_ERROR_BODY_BYTES)
            .await
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();
        Err(StorageError::Unknown(format!(
            "Failed to create directory {dir}: HTTP {status}: {body}"
        )))
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string(),
            ));
        }

        // The filer's own delete of a directory: `DELETE <path>/` with
        // `recursive=true`, which removes it with everything under it
        // (`weed/server/filer_server_handlers_write.go`, `DeleteHandler`).
        // `DELETE /dir/?path=<path>` is a delete of the filer's own `/dir`.
        let dir = path.trim_end_matches('/');
        if dir.is_empty() {
            return Err(StorageError::InvalidPath(
                "The filer's root is not deleted".to_string(),
            ));
        }
        let url = self.build_url(&format!("{dir}/"));

        let response = self
            .client
            .delete(&url)
            .query(&[("recursive", "true")])
            .send()
            .await?;

        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            StatusCode::NOT_FOUND => Err(StorageError::NotFound(path.to_string())),
            status => {
                let error_msg = response
                    .text()
                    .await
                    .unwrap_or_else(|_| format!("HTTP {status}"));
                Err(StorageError::Unknown(format!(
                    "Failed to delete directory {path}: {error_msg}"
                )))
            }
        }
    }

    async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string(),
            ));
        }

        let url = self.build_url("/quota");

        let form = multipart::Form::new()
            .text("path", path.to_string())
            .text("bytes", bytes.to_string());

        let response = self.client.post(&url).multipart(form).send().await?;

        match response.status() {
            StatusCode::OK | StatusCode::CREATED => Ok(()),
            StatusCode::NOT_FOUND => Err(StorageError::NotFound(path.to_string())),
            status => {
                let error_msg = response
                    .text()
                    .await
                    .unwrap_or_else(|_| format!("HTTP {status}"));
                Err(StorageError::Unknown(format!(
                    "Failed to set quota for {path}: {error_msg}"
                )))
            }
        }
    }

    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string(),
            ));
        }

        let url = self.build_url("/dir/status");

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .query(&[("path", path)])
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let status: DirectoryStatus = response
                    .json()
                    .await
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;

                Ok(status.total_size)
            }
            // A 404 from /dir/status means the directory has no tracked usage yet
            // (e.g. only contains a .keep sentinel). Treat as zero usage.
            StatusCode::NOT_FOUND => Ok(0),
            status => {
                let error_msg = response
                    .text()
                    .await
                    .unwrap_or_else(|_| format!("HTTP {status}"));
                Err(StorageError::Unknown(format!(
                    "Failed to get usage for {path}: {error_msg}"
                )))
            }
        }
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        let url = self.build_url("/");

        let response = self
            .client
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
                "Path must start with /".to_string(),
            ));
        }

        let url = self.build_url(path);

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let listing: DirectoryListing = response
                    .json()
                    .await
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;

                let dirs = listing
                    .into_entries()
                    .into_iter()
                    .filter(DirectoryEntry::is_directory)
                    .map(|e| e.full_path)
                    .collect();

                Ok(dirs)
            }
            StatusCode::NOT_FOUND => Err(StorageError::NotFound(path.to_string())),
            status => {
                let error_msg = response
                    .text()
                    .await
                    .unwrap_or_else(|_| format!("HTTP {status}"));
                Err(StorageError::Unknown(format!(
                    "Failed to list directories in {path}: {error_msg}"
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

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        // Decode path from handle
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid file handle".to_string()))?;

        let url = self.build_url(&path);

        // Use HTTP Range header for partial reads
        let range_header = format!("bytes={}-{}", offset, offset + length as u64 - 1);

        let response = self
            .client
            .get(&url)
            .header("Range", range_header)
            .send()
            .await?;

        match response.status() {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                // 4.22: cap the read at min(requested length + slack, hard cap).
                // The caller-requested `length` is the natural limit; we
                // additionally clamp at `SEAWEEDFS_MAX_RESPONSE_BYTES` so a
                // compromised filer cannot exceed it even if it ignores the
                // Range header.
                let cap = std::cmp::min(length as u64, SEAWEEDFS_MAX_RESPONSE_BYTES);
                read_capped_body(response, cap).await
            }
            StatusCode::NOT_FOUND => Err(StorageError::FileNotFound(path)),
            status => Err(StorageError::Unknown(format!(
                "Failed to read file {path}: HTTP {status}"
            ))),
        }
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        // Decode path from handle
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid file handle".to_string()))?;

        let url = self.build_url(&path);

        // The filer stores a file whole: a write is a read of the existing
        // content, the data spliced in at `offset`, and a PUT of the result.
        // The existing content is read at every offset, 0 included: git's
        // `index-pack --fix-thin` rewrites a pack's 12-byte header at offset
        // 0, and a write there must keep everything after it.
        let mut content = self.existing_content(&url, &path).await?;
        let offset = offset as usize;
        let end = offset + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset..end].copy_from_slice(data);

        // Write back to SeaweedFS
        let response = self.client.put(&url).body(content).send().await?;

        if response.status().is_success() {
            Ok(data.len())
        } else {
            Err(StorageError::Unknown(format!(
                "Failed to write file {}: HTTP {}",
                path,
                response.status()
            )))
        }
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        // HTTP-based storage doesn't need explicit close
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        // A HEAD cannot tell a directory from a file: the filer answers a
        // HEAD of an existing directory with 200, as it does a file's,
        // because it serves the directory's listing there. The kind is read
        // from the filer's own entry metadata instead.
        let entry = self.entry_metadata(path).await?;

        if entry.is_directory() {
            let mtime = entry.mtime_timestamp();
            return Ok(FileAttributes {
                file_type: FileType::Directory,
                size: 4096,
                mtime,
                atime: mtime,
                ctime: mtime,
                mode: 0o755,
                uid: 1000,
                gid: 1000,
                nlink: 2,
            });
        }

        let url = self.build_url(path);

        let response = self.client.head(&url).send().await?;

        match response.status() {
            StatusCode::OK => {
                // Extract metadata from headers
                let size = response
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);

                let mtime = response
                    .headers()
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
            StatusCode::NOT_FOUND => Err(StorageError::FileNotFound(path.to_string())),
            status => Err(StorageError::Unknown(format!(
                "Failed to stat {path}: HTTP {status}"
            ))),
        }
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let url = self.build_url(path);

        let response = self
            .client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await?;

        match response.status() {
            StatusCode::OK => {
                let listing: DirectoryListing = response
                    .json()
                    .await
                    .map_err(|e| StorageError::Serialization(e.to_string()))?;

                let entries = listing
                    .into_entries()
                    .iter()
                    .map(|e| DirEntry {
                        name: e.name().to_string(),
                        file_type: if e.is_directory() {
                            FileType::Directory
                        } else {
                            FileType::File
                        },
                    })
                    .collect();

                Ok(entries)
            }
            StatusCode::NOT_FOUND => Err(StorageError::NotFound(path.to_string())),
            status => Err(StorageError::Unknown(format!(
                "Failed to readdir {path}: HTTP {status}"
            ))),
        }
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        let url = self.build_url(path);

        // Create empty file
        let response = self.client.put(&url).body(Vec::<u8>::new()).send().await?;

        if response.status().is_success() {
            let handle_data = path.as_bytes().to_vec();
            Ok(FileHandle(handle_data))
        } else {
            Err(StorageError::Unknown(format!(
                "Failed to create file {}: HTTP {}",
                path,
                response.status()
            )))
        }
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let url = self.build_url(path);

        let response = self.client.delete(&url).send().await?;

        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            StatusCode::NOT_FOUND => Err(StorageError::FileNotFound(path.to_string())),
            status => Err(StorageError::Unknown(format!(
                "Failed to delete file {path}: HTTP {status}"
            ))),
        }
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        // The filer's own move: `POST <to>?mv.from=<from>` with no body
        // (`weed/server/filer_server_handlers_write.go`, `PostHandler` and
        // `move`). It moves a file or a directory with everything under it,
        // replaces an existing file at `to`, and answers 204. A POST without
        // `mv.from` is an upload, which the filer refuses unless it is
        // multipart.
        let response = self
            .client
            .post(self.build_url(to))
            .query(&[("mv.from", from)])
            .send()
            .await?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = read_capped_body(response, SEAWEEDFS_MAX_ERROR_BODY_BYTES)
            .await
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .unwrap_or_default();

        if status == StatusCode::NOT_FOUND || is_missing_move_source(status, &body) {
            return Err(StorageError::FileNotFound(from.to_string()));
        }

        Err(StorageError::Unknown(format!(
            "Failed to move {from} to {to}: HTTP {status}: {body}"
        )))
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<(), StorageError> {
        let url = self.build_url(path);
        let response = self.client.get(&url).send().await?;
        let mut content = match response.status() {
            status if status.is_success() => {
                read_capped_body(response, SEAWEEDFS_MAX_RESPONSE_BYTES).await?
            }
            StatusCode::NOT_FOUND => return Err(StorageError::FileNotFound(path.to_string())),
            status => {
                return Err(StorageError::Unknown(format!(
                    "Failed to read file {path} before truncating it: HTTP {status}"
                )))
            }
        };
        if size > SEAWEEDFS_MAX_RESPONSE_BYTES {
            return Err(StorageError::Unknown(format!(
                "Failed to truncate file {path}: {size} bytes exceeds the {SEAWEEDFS_MAX_RESPONSE_BYTES}-byte cap"
            )));
        }
        if content.len() as u64 == size {
            return Ok(());
        }
        content.resize(size as usize, 0);

        let response = self.client.put(&url).body(content).send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(StorageError::Unknown(format!(
                "Failed to truncate file {}: HTTP {}",
                path,
                response.status()
            )))
        }
    }
}

/// The filer answers a move from a path it does not hold with 400 and
/// `failed to get src entry '…', err: filer: no entry is found in filer
/// store` (`FilerServer.move`; `filer_pb.ErrNotFound`).
fn is_missing_move_source(status: StatusCode, body: &str) -> bool {
    status == StatusCode::BAD_REQUEST
        && body.contains("failed to get src entry")
        && body.contains("no entry is found in filer store")
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

/// The filer's listing for `GET <dir>` with `Accept: application/json`
/// (`weed/server/filer_server_handlers_read_dir.go`,
/// `listDirectoryHandler`): `{"Version", "Path", "Entries", "Limit",
/// "LastFileName", "ShouldDisplayLoadMore", "EmptyFolder"}`. `Entries` is
/// `null` for an empty directory. Every field has a default, so a listing
/// never fails to decode on an absent key.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DirectoryListing {
    #[serde(rename = "Path")]
    path: String,

    #[serde(rename = "Entries")]
    entries: Option<Vec<DirectoryEntry>>,
}

impl DirectoryListing {
    fn into_entries(self) -> Vec<DirectoryEntry> {
        self.entries.unwrap_or_default()
    }
}

/// One entry of the listing, the filer's `Entry` (`weed/filer/entry.go`):
/// its kind is `Mode`'s `os.ModeDir` bit, as `stat` reads it; the filer
/// sends no `IsDirectory` key.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct DirectoryEntry {
    #[serde(rename = "FullPath")]
    full_path: String,

    #[serde(rename = "Mtime")]
    mtime: Option<String>,

    #[serde(rename = "Mode")]
    mode: u32,
}

impl DirectoryEntry {
    fn is_directory(&self) -> bool {
        self.mode & GO_MODE_DIR != 0
    }

    fn name(&self) -> &str {
        self.full_path.rsplit('/').next().unwrap_or(&self.full_path)
    }
}

/// Go's `os.ModeDir`, the bit the filer's `Mode` carries for a directory
/// (`weed/filer/entry.go`, `Attr.IsDirectory`).
const GO_MODE_DIR: u32 = 1 << 31;

/// The fields `stat` reads from the filer's entry metadata: `Mode` is a Go
/// `os.FileMode`, `Mtime` an RFC 3339 time.
#[derive(Debug, Deserialize)]
struct FilerEntryMetadata {
    #[serde(rename = "Mode", default)]
    mode: u32,

    #[serde(rename = "Mtime", default)]
    mtime: Option<String>,
}

impl FilerEntryMetadata {
    fn is_directory(&self) -> bool {
        self.mode & GO_MODE_DIR != 0
    }

    fn mtime_timestamp(&self) -> i64 {
        self.mtime
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or_else(|| chrono::Utc::now().timestamp())
    }
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

    #[tokio::test]
    async fn test_get_usage_returns_zero_for_not_found() {
        // Regression: SeaweedFS returns 404 for /dir/status when a directory
        // was just created and only contains a .keep sentinel file.
        // get_usage must return Ok(0) — not StorageError::NotFound — so that
        // quota-guarded writes to newly created directories succeed.
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/dir/status")
            .match_query(mockito::Matcher::UrlEncoded(
                "path".into(),
                "/tenant/vol".into(),
            ))
            .with_status(404)
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;
        assert_eq!(result, Ok(0), "404 from /dir/status must yield Ok(0)");
    }

    // Regression tests for DirectoryListing deserialization

    #[test]
    fn test_directory_listing_with_entries_deserializes() {
        // The filer's listing: a directory's kind is `Mode`'s `os.ModeDir`
        let json = r#"{"Version":"2.x","Path":"/foo","Entries":[{"FullPath":"/foo/bar","Mtime":"2026-01-01T00:00:00Z","Mode":2147484141}],"Limit":100}"#;
        let listing: DirectoryListing =
            serde_json::from_str(json).expect("should deserialize canonical filer response");
        assert_eq!(listing.path, "/foo");
        let entries = listing.into_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].full_path, "/foo/bar");
        assert!(entries[0].is_directory());
    }

    #[test]
    fn test_directory_listing_absent_entries_deserializes_to_empty_vec() {
        // SeaweedFS may omit the Entries field for empty directories
        let json = r#"{"Path": "/foo"}"#;
        let listing: DirectoryListing =
            serde_json::from_str(json).expect("should deserialize when Entries is absent");
        assert_eq!(listing.path, "/foo");
        assert!(listing.into_entries().is_empty());
    }

    // Regression: readdir and list_directories must send Accept: application/json so that
    // SeaweedFS returns JSON instead of an HTML directory listing page.
    // Before this fix, the filer returned HTML, causing JSON deserialization to fail with
    // a Serialization error rather than returning directory entries.

    #[tokio::test]
    async fn test_readdir_sends_accept_json_header() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/tenant/vol")
            .match_header("accept", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"Path":"/tenant/vol","Entries":[]}"#)
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.readdir("/tenant/vol").await;
        assert!(
            result.is_ok(),
            "readdir must succeed when Accept header is present: {result:?}"
        );
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_list_directories_sends_accept_json_header() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/tenant/vol")
            .match_header("accept", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"Path":"/tenant/vol","Entries":[]}"#)
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.list_directories("/tenant/vol").await;
        assert!(
            result.is_ok(),
            "list_directories must succeed when Accept header is present: {result:?}"
        );
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_get_usage_sends_accept_json_header() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/dir/status")
            .match_header("accept", "application/json")
            .match_query(mockito::Matcher::UrlEncoded(
                "path".into(),
                "/tenant/vol".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"TotalSize":4096,"FileCount":1}"#)
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;
        assert_eq!(
            result,
            Ok(4096),
            "get_usage must parse JSON when Accept header is present"
        );
    }

    /// The filer's own answer for a directory: `GET <path>?metadata=true`
    /// returns the entry as JSON, its `Mode` a Go `os.FileMode` carrying
    /// `os.ModeDir` (`1 << 31`); here `ModeDir | 0o755`. A HEAD of the same
    /// path answers 200 with no sign of the kind, as the filer does for a
    /// directory (it serves the directory's listing).
    const DIRECTORY_METADATA: &str = r#"{"FullPath":"/tenant/vol/repo/.git","Mtime":"2026-10-11T14:27:33.5Z","Crtime":"2026-10-11T14:27:33.5Z","Mode":2147484141,"Uid":0,"Gid":0,"Mime":"","FileSize":0,"Extended":null}"#;

    async fn filer_answering(
        path: &str,
        metadata: &str,
        head_length: usize,
    ) -> mockito::ServerGuard {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", path)
            .match_query(mockito::Matcher::UrlEncoded(
                "metadata".into(),
                "true".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(metadata)
            .create_async()
            .await;
        server
            .mock("HEAD", path)
            .with_status(200)
            .with_header("last-modified", "Sun, 11 Oct 2026 14:27:33 GMT")
            .with_header("content-length", &head_length.to_string())
            .create_async()
            .await;
        server
    }

    /// Regression (clone-work-tree-io-error): `stat` answered every existing
    /// path as a regular file, so the FUSE LOOKUP of a clone's `.git`
    /// directory told the kernel it was a file and git's next `mkdir` under
    /// it failed with EIO.
    #[tokio::test]
    async fn stat_reports_a_directory_the_filer_marks_as_one() {
        let server = filer_answering("/tenant/vol/repo/.git", DIRECTORY_METADATA, 0).await;

        let attrs = SeaweedFSAdapter::new(server.url())
            .stat("/tenant/vol/repo/.git")
            .await
            .expect("stat of an existing directory");

        assert_eq!(
            (attrs.file_type, attrs.mode),
            (FileType::Directory, 0o755),
            "a directory the filer's metadata marks with ModeDir was not reported as a directory"
        );
    }

    /// The root of a volume, as the FSAL names it (`<remote_path>/`), is a
    /// directory.
    #[tokio::test]
    async fn stat_reports_a_volumes_root_as_a_directory() {
        let server = filer_answering("/tenant/vol/", DIRECTORY_METADATA, 0).await;

        let attrs = SeaweedFSAdapter::new(server.url())
            .stat("/tenant/vol/")
            .await
            .expect("stat of a volume's root");

        assert_eq!(
            (attrs.file_type, attrs.mode),
            (FileType::Directory, 0o755),
            "the root of a volume was not reported as a directory"
        );
    }

    /// A file stays a regular file, mode 0644, sized by its content-length.
    #[tokio::test]
    async fn stat_reports_a_file_with_its_content_length() {
        let file_metadata = r#"{"FullPath":"/tenant/vol/repo/HEAD","Mtime":"2026-10-11T14:27:33Z","Mode":420,"FileSize":23,"chunks":[]}"#;
        let server = filer_answering("/tenant/vol/repo/HEAD", file_metadata, 23).await;

        let attrs = SeaweedFSAdapter::new(server.url())
            .stat("/tenant/vol/repo/HEAD")
            .await
            .expect("stat of an existing file");

        assert_eq!(
            (attrs.file_type, attrs.mode, attrs.size),
            (FileType::File, 0o644, 23),
            "a file was not reported as a regular file with its content-length"
        );
    }

    /// A path the filer does not hold is not found.
    #[tokio::test]
    async fn stat_of_a_missing_path_is_not_found() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/tenant/vol/absent")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .create_async()
            .await;
        server
            .mock("HEAD", "/tenant/vol/absent")
            .with_status(404)
            .create_async()
            .await;

        let result = SeaweedFSAdapter::new(server.url())
            .stat("/tenant/vol/absent")
            .await;

        assert!(
            matches!(result, Err(StorageError::FileNotFound(_))),
            "expected FileNotFound, got {result:?}"
        );
    }

    /// Mocks every request method on any path as an error the test must
    /// never see, so a rename that sends anything but the one move fails.
    async fn refuse_every_other_request(server: &mut mockito::ServerGuard) -> Vec<mockito::Mock> {
        let mut mocks = Vec::new();
        for method in ["HEAD", "GET", "PUT", "DELETE"] {
            mocks.push(
                server
                    .mock(method, mockito::Matcher::Any)
                    .with_status(599)
                    .expect(0)
                    .create_async()
                    .await,
            );
        }
        mocks
    }

    /// The filer's move: `POST <to>?mv.from=<from>` with no body, answered
    /// with `status` and `body` (`FilerServer.move` answers 204 for a move it
    /// made).
    async fn filer_moving(
        server: &mut mockito::ServerGuard,
        from: &str,
        to: &str,
        status: usize,
        body: &str,
    ) -> mockito::Mock {
        server
            .mock("POST", to)
            .match_query(mockito::Matcher::UrlEncoded("mv.from".into(), from.into()))
            .match_body(mockito::Matcher::Exact(String::new()))
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await
    }

    /// Regression (clone-work-tree-io-error): `rename` copied the source with
    /// a raw-body POST, which the filer refuses (it reads a POST without
    /// `mv.from` as a multipart upload), so git's lockfile rename of
    /// `.git/config.lock` over `.git/config` failed with EIO. A rename is the
    /// filer's own move, one request with no body.
    #[tokio::test]
    async fn rename_is_one_move_by_the_filer_with_no_body() {
        let mut server = mockito::Server::new_async().await;
        let others = refuse_every_other_request(&mut server).await;
        let other_posts = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(599)
            .expect(0)
            .create_async()
            .await;
        let moved = filer_moving(
            &mut server,
            "/tenant/vol/repo/.git/config.lock",
            "/tenant/vol/repo/.git/config",
            204,
            "",
        )
        .await;

        let result = SeaweedFSAdapter::new(server.url())
            .rename(
                "/tenant/vol/repo/.git/config.lock",
                "/tenant/vol/repo/.git/config",
            )
            .await;

        assert_eq!(result, Ok(()), "the filer's move answered 204");
        moved.assert_async().await;
        other_posts.assert_async().await;
        for mock in others {
            mock.assert_async().await;
        }
    }

    /// A move the filer refuses surfaces the filer's status and its body.
    #[tokio::test]
    async fn rename_refused_by_the_filer_names_its_status_and_body() {
        let mut server = mockito::Server::new_async().await;
        let _others = refuse_every_other_request(&mut server).await;
        let filer_error = r#"{"error":"failed to move entry from '/tenant/vol/a' to '/tenant/vol/b', err: store unavailable"}"#;
        let _moved = filer_moving(
            &mut server,
            "/tenant/vol/a",
            "/tenant/vol/b",
            500,
            filer_error,
        )
        .await;

        let result = SeaweedFSAdapter::new(server.url())
            .rename("/tenant/vol/a", "/tenant/vol/b")
            .await;

        match result {
            Err(StorageError::Unknown(message)) => assert!(
                message.contains("500") && message.contains("store unavailable"),
                "the refusal did not name the filer's status and body: {message}"
            ),
            other => panic!("expected the filer's refusal, got {other:?}"),
        }
    }

    /// A missing source is not found. The filer answers a move from a path
    /// it does not hold with 400 and "failed to get src entry …: filer: no
    /// entry is found in filer store" (`FilerServer.move`).
    #[tokio::test]
    async fn rename_of_a_missing_source_is_not_found() {
        let mut server = mockito::Server::new_async().await;
        let moved = filer_moving(
            &mut server,
            "/tenant/vol/absent",
            "/tenant/vol/b",
            400,
            r#"{"error":"failed to get src entry '/tenant/vol/absent', err: filer: no entry is found in filer store"}"#,
        )
        .await;

        let result = SeaweedFSAdapter::new(server.url())
            .rename("/tenant/vol/absent", "/tenant/vol/b")
            .await;

        assert_eq!(
            result,
            Err(StorageError::FileNotFound("/tenant/vol/absent".to_string())),
            "a move from a missing source was not FileNotFound"
        );
        moved.assert_async().await;
    }

    /// The filer's listing as `listDirectoryHandler` writes it for
    /// `Accept: application/json`: each entry is the filer's `Entry`, its
    /// kind in `Mode` (`os.ModeDir` for a directory), and no `IsDirectory`
    /// key anywhere.
    const GIT_DIRECTORY_LISTING: &str = r#"{"Version":"30GB 3.97","Path":"/tenant/vol/repo/.git","Entries":[{"FullPath":"/tenant/vol/repo/.git/config","Mtime":"2026-10-11T16:40:46Z","Crtime":"2026-10-11T16:40:46Z","Ctime":"0001-01-01T00:00:00Z","Atime":"0001-01-01T00:00:00Z","Mode":420,"Uid":0,"Gid":0,"Mime":"","TtlSec":0,"UserName":"","GroupNames":null,"SymlinkTarget":"","Md5":null,"FileSize":92,"Rdev":0,"Inode":0,"Extended":null,"chunks":[{"file_id":"3,01637037d6","size":92,"modified_ts_ns":1760200846000000000}],"HardLinkId":null,"HardLinkCounter":0,"Content":null,"Remote":null,"Quota":0,"WORMEnforcedAtTsNs":0},{"FullPath":"/tenant/vol/repo/.git/hooks","Mtime":"2026-10-11T16:40:46Z","Crtime":"2026-10-11T16:40:46Z","Ctime":"0001-01-01T00:00:00Z","Atime":"0001-01-01T00:00:00Z","Mode":2147484141,"Uid":0,"Gid":0,"Mime":"","TtlSec":0,"UserName":"","GroupNames":null,"SymlinkTarget":"","Md5":null,"FileSize":0,"Rdev":0,"Inode":0,"Extended":null,"HardLinkId":null,"HardLinkCounter":0,"Content":null,"Remote":null,"Quota":0,"WORMEnforcedAtTsNs":0}],"Limit":100000,"LastFileName":"hooks","ShouldDisplayLoadMore":false,"EmptyFolder":false}"#;

    async fn filer_listing(path: &str, listing: &str) -> mockito::ServerGuard {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", path)
            .match_header("accept", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(listing)
            .create_async()
            .await;
        server
    }

    /// Regression (seaweedfs-readdir-isdirectory-key): the listing's kind was
    /// read from an `IsDirectory` key the filer never sends, so every entry
    /// was listed as a file.
    #[tokio::test]
    async fn readdir_reports_the_kind_the_filers_mode_carries() {
        let server = filer_listing("/tenant/vol/repo/.git", GIT_DIRECTORY_LISTING).await;

        let entries = SeaweedFSAdapter::new(server.url())
            .readdir("/tenant/vol/repo/.git")
            .await
            .expect("readdir of the filer's listing");

        let kinds: Vec<(String, FileType)> =
            entries.into_iter().map(|e| (e.name, e.file_type)).collect();
        assert_eq!(
            kinds,
            vec![
                ("config".to_string(), FileType::File),
                ("hooks".to_string(), FileType::Directory),
            ],
            "the listing's kinds were not read from each entry's Mode"
        );
    }

    /// `list_directories` keeps exactly the entries whose `Mode` marks a
    /// directory.
    #[tokio::test]
    async fn list_directories_keeps_the_entries_the_filers_mode_marks_as_directories() {
        let server = filer_listing("/tenant/vol/repo/.git", GIT_DIRECTORY_LISTING).await;

        let dirs = SeaweedFSAdapter::new(server.url())
            .list_directories("/tenant/vol/repo/.git")
            .await
            .expect("list_directories of the filer's listing");

        assert_eq!(
            dirs,
            vec!["/tenant/vol/repo/.git/hooks".to_string()],
            "the directories were not those whose Mode carries ModeDir"
        );
    }

    /// An empty directory's listing: the filer writes `"Entries":null` (its
    /// entries are a nil slice), and a listing may carry no `Entries` key at
    /// all, or an entry with no `Mtime` or `Mode`. Each decodes.
    #[tokio::test]
    async fn readdir_decodes_a_listing_with_no_entries_or_absent_keys() {
        for (listing, expected) in [
            (
                r#"{"Version":"30GB 3.97","Path":"/tenant/vol/repo/.git/hooks","Entries":null,"Limit":100000,"LastFileName":"","ShouldDisplayLoadMore":false,"EmptyFolder":true}"#,
                vec![],
            ),
            (r#"{"Path":"/tenant/vol/repo/.git/hooks"}"#, vec![]),
            (
                r#"{"Entries":[{"FullPath":"/tenant/vol/repo/.git/hooks/README"}]}"#,
                vec![("README".to_string(), FileType::File)],
            ),
        ] {
            let server = filer_listing("/tenant/vol/repo/.git/hooks", listing).await;

            let result = SeaweedFSAdapter::new(server.url())
                .readdir("/tenant/vol/repo/.git/hooks")
                .await
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|e| (e.name, e.file_type))
                        .collect::<Vec<_>>()
                });

            assert_eq!(result, Ok(expected), "the listing {listing} did not decode");
        }
    }

    /// Git's lockfile sequence through the `StorageProvider` trait, as a
    /// clone writes `.git/config`: create `config.lock`, write it, rename it
    /// over the existing `config` (the filer's move replaces an existing
    /// file, `moveSelfEntry`), then stat `config` and read the new size.
    #[tokio::test]
    async fn git_lockfile_sequence_replaces_the_destination() {
        let config = b"[core]\n\trepositoryformatversion = 0\n";
        let mut server = mockito::Server::new_async().await;
        let lock_created = server
            .mock("PUT", "/tenant/vol/repo/.git/config.lock")
            .match_body(mockito::Matcher::Exact(String::new()))
            .with_status(201)
            .expect(1)
            .create_async()
            .await;
        // The write reads the file it splices into: the empty `config.lock`
        // the create made.
        server
            .mock("GET", "/tenant/vol/repo/.git/config.lock")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_body("")
            .create_async()
            .await;
        let lock_written = server
            .mock("PUT", "/tenant/vol/repo/.git/config.lock")
            .match_body(mockito::Matcher::Exact(
                String::from_utf8(config.to_vec()).unwrap(),
            ))
            .with_status(201)
            .expect(1)
            .create_async()
            .await;
        let moved = filer_moving(
            &mut server,
            "/tenant/vol/repo/.git/config.lock",
            "/tenant/vol/repo/.git/config",
            204,
            "",
        )
        .await;
        let config_metadata = format!(
            r#"{{"FullPath":"/tenant/vol/repo/.git/config","Mtime":"2026-10-11T16:40:46Z","Mode":420,"FileSize":{}}}"#,
            config.len()
        );
        server
            .mock("GET", "/tenant/vol/repo/.git/config")
            .match_query(mockito::Matcher::UrlEncoded(
                "metadata".into(),
                "true".into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(&config_metadata)
            .create_async()
            .await;
        server
            .mock("HEAD", "/tenant/vol/repo/.git/config")
            .with_status(200)
            .with_header("last-modified", "Sun, 11 Oct 2026 16:40:46 GMT")
            .with_header("content-length", &config.len().to_string())
            .create_async()
            .await;

        let provider: Box<dyn StorageProvider> = Box::new(SeaweedFSAdapter::new(server.url()));
        let handle = provider
            .create_file("/tenant/vol/repo/.git/config.lock", 0o644)
            .await
            .expect("create config.lock");
        let written = provider
            .write_at(&handle, 0, config)
            .await
            .expect("write config.lock");
        provider
            .rename(
                "/tenant/vol/repo/.git/config.lock",
                "/tenant/vol/repo/.git/config",
            )
            .await
            .expect("rename config.lock over config");
        let attrs = provider
            .stat("/tenant/vol/repo/.git/config")
            .await
            .expect("stat config");

        assert_eq!(written, config.len());
        assert_eq!(
            (attrs.file_type, attrs.size),
            (FileType::File, config.len() as u64),
            "config did not read the size written to config.lock"
        );
        lock_created.assert_async().await;
        lock_written.assert_async().await;
        moved.assert_async().await;
    }

    /// The filer's own mkdir: `POST <path>/` with no body, answered with
    /// `status` and `body` (`{"name":"<leaf>"}` with 201 for a directory it
    /// made, 409 `{"error":"dir <path> already exists"}` for a path it holds).
    async fn filer_making_directory(
        server: &mut mockito::ServerGuard,
        path: &str,
        status: usize,
        body: &str,
    ) -> mockito::Mock {
        server
            .mock("POST", format!("{path}/").as_str())
            .match_body(mockito::Matcher::Exact(String::new()))
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await
    }

    /// Regression (clone-work-tree-io-error, the end-to-end harness against a
    /// real filer on 2026-10-11): `create_directory` POSTed a multipart form
    /// `path=<dir>` to `/dir/`, which the filer took as an upload into its
    /// own `/dir` (201 `{"size":8}`) and which made nothing at `<dir>`
    /// (`GET <dir>?metadata=true` 404); what made the directory was a `.keep`
    /// file written into it, and every directory a clone made through the
    /// mount then held a `.keep` that `git status` listed as untracked and
    /// `git add -A` committed. A directory is made by the filer's own mkdir,
    /// one request with no body, and no file is written into it.
    #[tokio::test]
    async fn create_directory_is_the_filers_own_mkdir_and_writes_no_file() {
        let mut server = mockito::Server::new_async().await;
        let others = refuse_every_other_request(&mut server).await;
        let made = filer_making_directory(
            &mut server,
            "/tenant/vol/repo/src/lib",
            201,
            r#"{"name":"lib"}"#,
        )
        .await;
        let other_posts = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(201)
            .with_body(r#"{"size":8}"#)
            .expect(0)
            .create_async()
            .await;

        let result = SeaweedFSAdapter::new(server.url())
            .create_directory("/tenant/vol/repo/src/lib")
            .await;

        assert_eq!(result, Ok(()), "the filer's mkdir answered 201");
        made.assert_async().await;
        other_posts.assert_async().await;
        for mock in others {
            mock.assert_async().await;
        }
    }

    /// A directory the filer already holds answers 409; making it again is
    /// not an error.
    #[tokio::test]
    async fn create_directory_of_a_directory_the_filer_holds_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let made = filer_making_directory(
            &mut server,
            "/tenant/vol/repo",
            409,
            r#"{"error":"dir /tenant/vol/repo already exists"}"#,
        )
        .await;

        let result = SeaweedFSAdapter::new(server.url())
            .create_directory("/tenant/vol/repo")
            .await;

        assert_eq!(result, Ok(()), "the filer holds the directory already");
        made.assert_async().await;
    }

    /// A mkdir the filer refuses otherwise names its status and body.
    #[tokio::test]
    async fn create_directory_refused_by_the_filer_names_its_status_and_body() {
        let mut server = mockito::Server::new_async().await;
        filer_making_directory(
            &mut server,
            "/tenant/vol/repo",
            500,
            r#"{"error":"filer store unavailable"}"#,
        )
        .await;

        let result = SeaweedFSAdapter::new(server.url())
            .create_directory("/tenant/vol/repo")
            .await;

        assert_eq!(
            result,
            Err(StorageError::Unknown(
                r#"Failed to create directory /tenant/vol/repo: HTTP 500 Internal Server Error: {"error":"filer store unavailable"}"#
                    .to_string()
            ))
        );
    }

    /// Regression (clone-work-tree-io-error, the end-to-end harness against a
    /// real filer on 2026-10-11): `delete_directory` sent
    /// `DELETE /dir/?path=<dir>&recursive=true`, which the filer reads as a
    /// delete of its own `/dir` (204) and which leaves `<dir>` in place
    /// (`GET <dir>?metadata=true` 200), so `git rm -r src/lib` through the
    /// mount answered success and `src/lib` was still there. A directory is
    /// deleted by the filer's own delete of `<dir>/`, with everything in it.
    #[tokio::test]
    async fn delete_directory_is_the_filers_own_delete_of_the_directory() {
        let mut server = mockito::Server::new_async().await;
        let deleted = server
            .mock("DELETE", "/tenant/vol/repo/src/lib/")
            .match_query(mockito::Matcher::UrlEncoded(
                "recursive".into(),
                "true".into(),
            ))
            .with_status(204)
            .expect(1)
            .create_async()
            .await;
        let filers_own_dir = server
            .mock("DELETE", "/dir/")
            .match_query(mockito::Matcher::Any)
            .with_status(204)
            .expect(0)
            .create_async()
            .await;

        let result = SeaweedFSAdapter::new(server.url())
            .delete_directory("/tenant/vol/repo/src/lib")
            .await;

        assert_eq!(result, Ok(()), "the filer's delete answered 204");
        deleted.assert_async().await;
        filers_own_dir.assert_async().await;
    }

    /// The filer's root is never deleted: a delete of `/` would be a delete
    /// of every volume.
    #[tokio::test]
    async fn delete_directory_of_the_filers_root_is_refused() {
        let mut server = mockito::Server::new_async().await;
        let any = server
            .mock("DELETE", mockito::Matcher::Any)
            .with_status(204)
            .expect(0)
            .create_async()
            .await;

        let result = SeaweedFSAdapter::new(server.url())
            .delete_directory("/")
            .await;

        assert!(
            matches!(result, Err(StorageError::InvalidPath(_))),
            "a delete of the filer's root was not refused: {result:?}"
        );
        any.assert_async().await;
    }

    /// Regression for security audit 002 §4.22. A hostile / compromised
    /// filer returns a body larger than the per-response cap. The capped
    /// stream reader MUST return `Err` *before* materialising the full
    /// payload — verified by setting the cap below the body size and
    /// checking that the error is the cap-exceeded variant.
    #[tokio::test]
    async fn read_capped_body_rejects_oversize_response() {
        let mut server = mockito::Server::new_async().await;
        // 1 MiB body, 256 KiB cap. The cap-exceeded check must fire well
        // before the full body is consumed.
        let body = vec![0u8; 1024 * 1024];
        let cap: u64 = 256 * 1024;
        let _mock = server
            .mock("GET", "/big")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/big", server.url()))
            .send()
            .await
            .expect("request");
        let result = read_capped_body(resp, cap).await;
        match result {
            Err(StorageError::Unknown(msg)) => {
                assert!(
                    msg.contains("exceeded") && msg.contains(&cap.to_string()),
                    "expected cap-exceeded error, got: {msg}"
                );
            }
            other => panic!("expected cap-exceeded Err, got {other:?}"),
        }
    }

    /// Companion test: a body smaller than the cap is returned in full.
    #[tokio::test]
    async fn read_capped_body_returns_body_under_cap() {
        let mut server = mockito::Server::new_async().await;
        let body = vec![0xABu8; 1024];
        let _mock = server
            .mock("GET", "/small")
            .with_status(200)
            .with_body(body.clone())
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/small", server.url()))
            .send()
            .await
            .expect("request");
        let got = read_capped_body(resp, 4096).await.expect("should succeed");
        assert_eq!(got, body);
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

    /// A filer holding `content` at `path`, answering a plain GET of it.
    async fn filer_holding(server: &mut mockito::ServerGuard, path: &str, content: &[u8]) {
        server
            .mock("GET", path)
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_body(content)
            .create_async()
            .await;
    }

    /// The one PUT of `path` whose body must be exactly `body`.
    async fn filer_expecting_put(
        server: &mut mockito::ServerGuard,
        path: &str,
        body: &[u8],
    ) -> mockito::Mock {
        server
            .mock("PUT", path)
            .match_body(mockito::Matcher::from(body.to_vec()))
            .with_status(201)
            .expect(1)
            .create_async()
            .await
    }

    /// Regression (`forge-clone-index-pack-failed`): `write_at` read the
    /// existing content only for an offset above 0, so git's
    /// `index-pack --fix-thin` rewriting a pack's 12-byte header at offset 0
    /// replaced the whole pack with those 12 bytes. A write at 0 shorter than
    /// the file keeps its tail.
    #[tokio::test]
    async fn write_at_offset_zero_keeps_the_rest_of_the_file() {
        let mut server = mockito::Server::new_async().await;
        filer_holding(&mut server, "/v/pack", b"0123456789").await;
        let put = filer_expecting_put(&mut server, "/v/pack", b"AB23456789").await;
        let adapter = SeaweedFSAdapter::new(server.url());

        let written = adapter
            .write_at(&FileHandle(b"/v/pack".to_vec()), 0, b"AB")
            .await;

        assert_eq!(written, Ok(2));
        put.assert_async().await;
    }

    /// A write past the end extends the file, zero-filling any gap.
    #[tokio::test]
    async fn write_at_past_the_end_extends_the_file() {
        let mut server = mockito::Server::new_async().await;
        filer_holding(&mut server, "/v/f", b"0123").await;
        let put = filer_expecting_put(&mut server, "/v/f", b"0123\0\0XY").await;
        let adapter = SeaweedFSAdapter::new(server.url());

        let written = adapter
            .write_at(&FileHandle(b"/v/f".to_vec()), 6, b"XY")
            .await;

        assert_eq!(written, Ok(2));
        put.assert_async().await;
    }

    /// A failed read of the existing content is an error, never an empty
    /// file: nothing is written over the file.
    #[tokio::test]
    async fn write_at_refuses_when_the_existing_content_cannot_be_read() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/v/f")
            .with_status(500)
            .create_async()
            .await;
        let put = server
            .mock("PUT", mockito::Matcher::Any)
            .with_status(201)
            .expect(0)
            .create_async()
            .await;
        let adapter = SeaweedFSAdapter::new(server.url());

        let written = adapter
            .write_at(&FileHandle(b"/v/f".to_vec()), 0, b"AB")
            .await;

        assert!(written.is_err(), "the write went ahead: {written:?}");
        put.assert_async().await;
    }

    /// `truncate` below the length cuts the file there.
    #[tokio::test]
    async fn truncate_below_the_length_cuts_the_file() {
        let mut server = mockito::Server::new_async().await;
        filer_holding(&mut server, "/v/f", b"0123456789").await;
        let put = filer_expecting_put(&mut server, "/v/f", b"012").await;
        let adapter = SeaweedFSAdapter::new(server.url());

        assert_eq!(adapter.truncate("/v/f", 3).await, Ok(()));
        put.assert_async().await;
    }

    /// `truncate` beyond the length extends the file with zero bytes.
    #[tokio::test]
    async fn truncate_beyond_the_length_extends_the_file_with_zeros() {
        let mut server = mockito::Server::new_async().await;
        filer_holding(&mut server, "/v/f", b"ab").await;
        let put = filer_expecting_put(&mut server, "/v/f", b"ab\0\0\0").await;
        let adapter = SeaweedFSAdapter::new(server.url());

        assert_eq!(adapter.truncate("/v/f", 5).await, Ok(()));
        put.assert_async().await;
    }
}
