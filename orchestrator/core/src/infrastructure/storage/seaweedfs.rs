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
//! - `GET /path?limit=N&lastFileName=X` with `Accept: application/json` - List a directory, paged
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
use std::collections::VecDeque;
use std::time::Duration;

/// Hard ceiling for any single response body materialised from SeaweedFS.
/// Per security audit 002 §4.22, raw `.bytes()` reads are unbounded and a
/// hostile / compromised filer could exhaust orchestrator memory. The cap
/// is the smaller of the per-volume request size and 100 MiB. Reads larger
/// than this MUST be issued via ranged `read_at` against the upper layers.
const SEAWEEDFS_MAX_RESPONSE_BYTES: u64 = 100 * 1024 * 1024;

/// Ceiling for an error body the filer answers, read only to name it.
const SEAWEEDFS_MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

/// The most entries, files and directories together, one usage walk lists
/// under a volume before it stops and reports the usage unreadable.
const USAGE_MAX_ENTRIES: usize = 100_000;

/// The deepest directory, counted from the volume's own (depth 0), one usage
/// walk lists before it stops and reports the usage unreadable.
const USAGE_MAX_DEPTH: usize = 64;

/// Entries asked of the filer per page of a listing.
const USAGE_PAGE_LIMIT: usize = 1000;

/// The bounds of one usage walk.
#[derive(Debug, Clone, Copy)]
struct UsageBounds {
    max_entries: usize,
    max_depth: usize,
    page_limit: usize,
}

const USAGE_BOUNDS: UsageBounds = UsageBounds {
    max_entries: USAGE_MAX_ENTRIES,
    max_depth: USAGE_MAX_DEPTH,
    page_limit: USAGE_PAGE_LIMIT,
};

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

    /// The sum of the file sizes under `root` by the filer's listing, or the
    /// reason it could not be read. Directories are walked breadth first;
    /// the walk stops at the first failure or bound.
    async fn sum_listed_sizes(&self, root: &str, bounds: UsageBounds) -> Result<u64, String> {
        let mut pending: VecDeque<(String, usize)> = VecDeque::from([(root.to_string(), 0)]);
        let mut entries_seen: usize = 0;
        let mut total: u64 = 0;

        while let Some((dir, depth)) = pending.pop_front() {
            let mut after: Option<String> = None;
            loop {
                let page = self
                    .listing_page(&dir, after.as_deref(), bounds.page_limit)
                    .await?;
                let more = page.should_display_load_more;
                let last = page.last_file_name.clone();
                let entries = page.into_entries();
                let page_was_empty = entries.is_empty();

                for entry in entries {
                    entries_seen += 1;
                    if entries_seen > bounds.max_entries {
                        return Err(format!(
                            "the listing of {root} holds more than {} entries",
                            bounds.max_entries
                        ));
                    }
                    if entry.is_directory() {
                        if depth + 1 > bounds.max_depth {
                            return Err(format!(
                                "{} is deeper than {} directories under {root}",
                                entry.full_path, bounds.max_depth
                            ));
                        }
                        pending.push_back((entry.full_path, depth + 1));
                    } else {
                        total = total.saturating_add(entry.file_size);
                    }
                }

                if !more || page_was_empty {
                    break;
                }
                if last.is_empty() || after.as_deref() == Some(last.as_str()) {
                    return Err(format!(
                        "the listing of {dir} asked for another page after {last:?} and named no new last entry"
                    ));
                }
                after = Some(last);
            }
        }

        Ok(total)
    }

    /// One page of the filer's listing of `dir`, the page after `after` when
    /// given, or the reason it could not be read.
    async fn listing_page(
        &self,
        dir: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<DirectoryListing, String> {
        let mut query: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(after) = after {
            query.push(("lastFileName", after.to_string()));
        }

        let response = self
            .client
            .get(self.build_url(dir))
            .header("Accept", "application/json")
            .query(&query)
            .send()
            .await
            .map_err(|e| format!("the listing of {dir} was not answered: {e}"))?;

        let status = response.status();
        if status != StatusCode::OK {
            let body = read_capped_body(response, SEAWEEDFS_MAX_ERROR_BODY_BYTES)
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            return Err(format!(
                "the listing of {dir} answered HTTP {status}: {body}"
            ));
        }

        let body = read_capped_body(response, SEAWEEDFS_MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| format!("the listing of {dir} could not be read: {e}"))?;
        serde_json::from_slice(&body)
            .map_err(|e| format!("the listing of {dir} could not be decoded: {e}"))
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

    /// The sum of the `FileSize` of every file under `path`, as the filer
    /// lists them: each directory listed page by page and walked in turn,
    /// within the walk's bounds (100,000 entries, a depth of 64). A listing
    /// that fails, a page that cannot be read, a missing directory or a
    /// bound reached answers [`StorageError::UsageUnreadable`], never a
    /// smaller number.
    async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
        // Validate path
        if !path.starts_with('/') {
            return Err(StorageError::InvalidPath(
                "Path must start with /".to_string(),
            ));
        }

        self.sum_listed_sizes(path, USAGE_BOUNDS)
            .await
            .map_err(|cause| {
                tracing::warn!(path, cause = %cause, "volume usage could not be read from the filer's listing");
                StorageError::UsageUnreadable(cause)
            })
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

        if matches!(mode, OpenMode::ReadOnly) {
            // A read opens a file that exists and is not a directory. The
            // kind is read from the filer's own entry metadata, never from a
            // HEAD: the filer answers a HEAD of a directory 200, and a GET of
            // one with its HTML listing, which a read would return as the
            // file's content.
            match self.entry_metadata(path).await {
                Ok(entry) if entry.is_directory() => {
                    return Err(StorageError::IsDirectory(path.to_string()));
                }
                Ok(_) => {}
                // Any other answer of the filer is not found, as the HEAD's
                // non-success answer was.
                Err(StorageError::Unknown(_)) => {
                    return Err(StorageError::FileNotFound(path.to_string()));
                }
                Err(e) => return Err(e),
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

        // A read of no bytes reads nothing: its range would have no last
        // byte, so the filer is not asked.
        if length == 0 {
            return Ok(Vec::new());
        }

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
            // The range starts at or past the file's end, an empty file's
            // included (RFC 9110 15.5.17): there is nothing to read there,
            // as the local provider reads it.
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Vec::new()),
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

    /// The name of the page's last entry, the `lastFileName` that asks for
    /// the page after it.
    #[serde(rename = "LastFileName")]
    last_file_name: String,

    /// Whether the filer holds entries past this page (the page was full).
    #[serde(rename = "ShouldDisplayLoadMore")]
    should_display_load_more: bool,
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

    /// The file's size in bytes; zero for a directory.
    #[serde(rename = "FileSize", default)]
    file_size: u64,
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

    /// The filer's listing of `path` as `listDirectoryHandler` writes it:
    /// each `(name, size)` a file of `size` bytes, a `None` size a
    /// directory (`Mode` carrying `os.ModeDir`).
    fn usage_listing(
        path: &str,
        entries: &[(&str, Option<u64>)],
        last: &str,
        more: bool,
    ) -> String {
        let entries: Vec<serde_json::Value> = entries
            .iter()
            .map(|(name, size)| match size {
                Some(size) => serde_json::json!({
                    "FullPath": format!("{path}/{name}"),
                    "Mtime": "2026-10-15T19:00:00Z",
                    "Mode": 420,
                    "FileSize": size,
                }),
                None => serde_json::json!({
                    "FullPath": format!("{path}/{name}"),
                    "Mtime": "2026-10-15T19:00:00Z",
                    "Mode": 2147484141u32,
                    "FileSize": 0,
                }),
            })
            .collect();
        serde_json::json!({
            "Version": "30GB 3.97",
            "Path": path,
            "Entries": if entries.is_empty() { serde_json::Value::Null } else { serde_json::Value::Array(entries) },
            "Limit": 1000,
            "LastFileName": last,
            "ShouldDisplayLoadMore": more,
            "EmptyFolder": false,
        })
        .to_string()
    }

    /// The filer serves no `/dir/status` route: every probe of it answers
    /// 404, as production's filer does.
    async fn filer_without_dir_status(server: &mut mockito::ServerGuard) {
        server
            .mock("GET", "/dir/status")
            .match_query(mockito::Matcher::Any)
            .with_status(404)
            .create_async()
            .await;
    }

    /// One page of `path`'s listing: the first page (no `lastFileName`)
    /// when `after` is `None`, else the page after `after`.
    async fn filer_listing_page(
        server: &mut mockito::ServerGuard,
        path: &str,
        after: Option<&str>,
        body: String,
    ) -> mockito::Mock {
        let query = match after {
            None => mockito::Matcher::Regex("^limit=[0-9]+$".into()),
            Some(after) => mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("limit".into(), "1000".into()),
                mockito::Matcher::UrlEncoded("lastFileName".into(), after.into()),
            ]),
        };
        server
            .mock("GET", path)
            .match_header("accept", "application/json")
            .match_query(query)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await
    }

    /// Regression (volume-usage-reads-zero): usage was asked of a
    /// `/dir/status` route the filer does not serve, and its 404 read as
    /// zero, so every volume read `0 B`. A listing with three files, one of
    /// them in a nested directory, sums their `FileSize`.
    #[tokio::test]
    async fn usage_sums_the_file_sizes_of_a_listing_and_its_nested_directory() {
        let mut server = mockito::Server::new_async().await;
        filer_without_dir_status(&mut server).await;
        let root = filer_listing_page(
            &mut server,
            "/tenant/vol",
            None,
            usage_listing(
                "/tenant/vol",
                &[
                    (".keep", Some(0)),
                    ("DIALOGUE.ogg", Some(2_525_169)),
                    ("generated", None),
                    ("notes.md", Some(1_200)),
                ],
                "notes.md",
                false,
            ),
        )
        .await;
        let nested = filer_listing_page(
            &mut server,
            "/tenant/vol/generated",
            None,
            usage_listing(
                "/tenant/vol/generated",
                &[("image.png", Some(48_000))],
                "image.png",
                false,
            ),
        )
        .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;

        assert_eq!(
            result,
            Ok(2_525_169 + 1_200 + 48_000),
            "usage must be the sum of the listed files' FileSize, nested directory included"
        );
        root.assert_async().await;
        nested.assert_async().await;
    }

    /// A listing the filer answers in two pages (`ShouldDisplayLoadMore`,
    /// then the page after `LastFileName`) sums both pages.
    #[tokio::test]
    async fn usage_sums_both_pages_of_a_paged_listing() {
        let mut server = mockito::Server::new_async().await;
        filer_without_dir_status(&mut server).await;
        let first = filer_listing_page(
            &mut server,
            "/tenant/vol",
            None,
            usage_listing(
                "/tenant/vol",
                &[("a.bin", Some(1_000)), ("b.bin", Some(2_000))],
                "b.bin",
                true,
            ),
        )
        .await;
        let second = filer_listing_page(
            &mut server,
            "/tenant/vol",
            Some("b.bin"),
            usage_listing("/tenant/vol", &[("c.bin", Some(4_000))], "c.bin", false),
        )
        .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;

        assert_eq!(
            result,
            Ok(7_000),
            "usage must sum every page of the listing"
        );
        first.assert_async().await;
        second.assert_async().await;
    }

    /// Replaces `test_get_usage_returns_zero_for_not_found`: a filer that
    /// answers the listing with a 500 answers an error, never zero, and the
    /// error's sentence is the one the write path refuses with.
    #[tokio::test]
    async fn usage_of_a_filer_500_is_an_error_not_zero() {
        let mut server = mockito::Server::new_async().await;
        filer_without_dir_status(&mut server).await;
        server
            .mock("GET", "/tenant/vol")
            .match_query(mockito::Matcher::Any)
            .with_status(500)
            .with_body("filer: internal error")
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;

        let error = result.expect_err("a filer 500 must be an error, not a usage");
        assert_eq!(
            error.to_string(),
            "The volume's usage could not be read; try again."
        );
    }

    /// A new volume's directory holds only its `.keep` sentinel: its usage
    /// is read from the listing and is zero, so its first write is admitted.
    #[tokio::test]
    async fn usage_of_a_directory_holding_only_its_keep_is_zero() {
        let mut server = mockito::Server::new_async().await;
        filer_without_dir_status(&mut server).await;
        let listing = filer_listing_page(
            &mut server,
            "/tenant/vol",
            None,
            usage_listing("/tenant/vol", &[(".keep", Some(0))], ".keep", false),
        )
        .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;

        assert_eq!(result, Ok(0));
        listing.assert_async().await;
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
            .mock("GET", "/tenant/vol")
            .match_header("accept", "application/json")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(usage_listing(
                "/tenant/vol",
                &[("a.bin", Some(4096))],
                "a.bin",
                false,
            ))
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let result = adapter.get_usage("/tenant/vol").await;
        assert_eq!(
            result,
            Ok(4096),
            "get_usage must parse the JSON listing when Accept header is present"
        );
    }

    /// A walk past its entry bound stops and reports the usage unreadable,
    /// never the smaller sum it had reached.
    #[tokio::test]
    async fn usage_past_the_entry_bound_is_unreadable_not_a_smaller_sum() {
        let mut server = mockito::Server::new_async().await;
        filer_listing_page(
            &mut server,
            "/tenant/vol",
            None,
            usage_listing(
                "/tenant/vol",
                &[("a", Some(1)), ("b", Some(1)), ("c", Some(1))],
                "c",
                false,
            ),
        )
        .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let bounds = UsageBounds {
            max_entries: 2,
            ..USAGE_BOUNDS
        };
        let result = adapter.sum_listed_sizes("/tenant/vol", bounds).await;

        let cause = result.expect_err("past the entry bound the usage is unreadable");
        assert!(cause.contains("more than 2 entries"), "{cause}");
    }

    /// A walk that meets a directory past its depth bound stops and reports
    /// the usage unreadable.
    #[tokio::test]
    async fn usage_past_the_depth_bound_is_unreadable() {
        let mut server = mockito::Server::new_async().await;
        filer_listing_page(
            &mut server,
            "/tenant/vol",
            None,
            usage_listing("/tenant/vol", &[("one", None)], "one", false),
        )
        .await;
        filer_listing_page(
            &mut server,
            "/tenant/vol/one",
            None,
            usage_listing("/tenant/vol/one", &[("two", None)], "two", false),
        )
        .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let bounds = UsageBounds {
            max_depth: 1,
            ..USAGE_BOUNDS
        };
        let result = adapter.sum_listed_sizes("/tenant/vol", bounds).await;

        let cause = result.expect_err("past the depth bound the usage is unreadable");
        assert!(cause.contains("deeper than 1 directories"), "{cause}");
    }

    /// The walk's bounds are the ones the usage decision names: 100,000
    /// entries and a depth of 64 per volume.
    #[test]
    fn usage_bounds_are_a_hundred_thousand_entries_and_depth_sixty_four() {
        assert_eq!(USAGE_BOUNDS.max_entries, 100_000);
        assert_eq!(USAGE_BOUNDS.max_depth, 64);
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

    /// Regression (fs-read-of-volume-root-returns-filer-html-2026-10-14): a
    /// read of a volume's root passed the HEAD existence check, which the
    /// filer answers 200 for a directory, and the read then fetched the
    /// filer's HTML listing as the file's content. Opening a directory for
    /// reading is refused from the filer's entry metadata, and nothing is
    /// fetched as content.
    #[tokio::test]
    async fn open_file_of_a_directory_for_reading_is_refused_and_fetches_no_listing() {
        let mut server = filer_answering("/tenant/vol/", DIRECTORY_METADATA, 0).await;
        let listing = server
            .mock("GET", "/tenant/vol/")
            .match_query(mockito::Matcher::Missing)
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html>SeaweedFS Filer</html>")
            .expect(0)
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let opened = adapter.open_file("/tenant/vol/", OpenMode::ReadOnly).await;
        let read = match &opened {
            Ok(handle) => Some(
                adapter
                    .read_at(handle, 0, 1024)
                    .await
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            ),
            Err(_) => None,
        };

        assert!(
            matches!(opened, Err(StorageError::IsDirectory(_))),
            "opening a directory for reading was not refused as a directory: {opened:?}, then read {read:?}"
        );
        listing.assert_async().await;
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

        // Verify deletion: the usage of a missing directory cannot be read
        let result = adapter.get_usage(path).await;
        assert!(matches!(result, Err(StorageError::UsageUnreadable(_))));
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

    /// Proof 19's `fs.read` of an empty `test/__init__.py` (run `ebd5f9ae`):
    /// a range whose first byte is 0 cannot be satisfied by a file of no
    /// bytes, so the filer answers 416, which was a storage error naming the
    /// filer path. An empty file reads as empty, as the local provider reads
    /// it (AEGIS ADR-048, Update of 2026-10-15).
    #[tokio::test]
    async fn read_at_of_an_empty_file_reads_empty() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/aegis/volumes/x/test/__init__.py")
            .match_header("range", "bytes=0-10485759")
            .with_status(416)
            .with_header("content-range", "bytes */0")
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let handle = FileHandle(b"/aegis/volumes/x/test/__init__.py".to_vec());
        let read = adapter.read_at(&handle, 0, 10 * 1024 * 1024).await;
        assert_eq!(read, Ok(Vec::new()), "an empty file must read as empty");
    }

    /// A read of no bytes reads nothing and asks the filer nothing: the
    /// range `offset..=offset + length - 1` has no last byte for a length of
    /// 0 (and overflowed at offset 0).
    #[tokio::test]
    async fn read_at_of_length_zero_sends_no_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/aegis/volumes/x/empty")
            .expect(0)
            .create_async()
            .await;

        let adapter = SeaweedFSAdapter::new(server.url());
        let handle = FileHandle(b"/aegis/volumes/x/empty".to_vec());
        let read = adapter.read_at(&handle, 0, 0).await;
        assert_eq!(read, Ok(Vec::new()), "a read of no bytes reads nothing");
        mock.assert_async().await;
    }
}
