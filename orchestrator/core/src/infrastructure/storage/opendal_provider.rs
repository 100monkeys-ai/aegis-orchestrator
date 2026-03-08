// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! OpenDAL Storage Provider
//!
//! Provides a unified StorageProvider implementation using Apache OpenDAL.
//! This allows Agent volumes to be backed by S3, GCS, Azure, WebDAV, etc.,
//! as proposed in ADR-047.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for opendal

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError, StorageProvider,
};
use async_trait::async_trait;
use opendal::Operator;

pub struct OpenDalStorageProvider {
    operator: Operator,
}

impl OpenDalStorageProvider {
    /// Create a new OpenDalStorageProvider using an existing Operator
    pub fn new(operator: Operator) -> Self {
        Self { operator }
    }
}

// Convert OpenDAL Error to our internal StorageError
impl From<opendal::Error> for StorageError {
    fn from(err: opendal::Error) -> Self {
        match err.kind() {
            opendal::ErrorKind::NotFound => StorageError::NotFound(err.to_string()),
            opendal::ErrorKind::PermissionDenied => StorageError::PermissionDenied(err.to_string()),
            opendal::ErrorKind::AlreadyExists => StorageError::AlreadyExists(err.to_string()),
            opendal::ErrorKind::RateLimited => StorageError::Network(err.to_string()),
            _ => StorageError::Unknown(err.to_string()),
        }
    }
}

#[async_trait]
impl StorageProvider for OpenDalStorageProvider {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        let path = if !path.ends_with('/') {
            format!("{}/", path)
        } else {
            path.to_string()
        };
        self.operator.create_dir(&path).await.map_err(Into::into)
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        self.operator.remove_all(path).await.map_err(Into::into)
    }

    async fn set_quota(&self, _path: &str, _bytes: u64) -> Result<(), StorageError> {
        // External APIs manage quota, typically not through standard FS interfaces.
        Ok(())
    }

    async fn get_usage(&self, _path: &str) -> Result<u64, StorageError> {
        // Proper usage would require recursive STAT or native API usage
        Ok(0)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        self.operator.check().await.map_err(Into::into)
    }

    // --- POSIX File Operations (ADR-036) ---

    async fn open_file(&self, path: &str, _mode: OpenMode) -> Result<FileHandle, StorageError> {
        // OpenDAL doesn't keep a persistent remote file session open across all APIs.
        // We encode the path into the FileHandle.
        let path_bytes = path.as_bytes().to_vec();
        Ok(FileHandle(path_bytes))
    }

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;

        let data = self
            .operator
            .read_with(&path)
            .range(offset..offset + length as u64)
            .await?;
        Ok(data.to_vec())
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        _offset: u64, // Note: standard OpenDAL operator lacks random write for arbitrary cloud backends without specialized config. Append/overwrite is preferred.
        data: &[u8],
    ) -> Result<usize, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;

        self.operator.write(&path, data.to_vec()).await?;
        Ok(data.len())
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let meta = self.operator.stat(path).await?;

        let file_type = match meta.mode() {
            opendal::EntryMode::FILE => FileType::File,
            opendal::EntryMode::DIR => FileType::Directory,
            _ => FileType::File,
        };

        let size = meta.content_length();
        let mtime = meta
            .last_modified()
            .and_then(|t| {
                let system_time: std::time::SystemTime = t.into();
                system_time
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_secs() as i64)
            })
            .unwrap_or(0);

        Ok(FileAttributes {
            file_type,
            size,
            mtime,
            atime: mtime,
            ctime: mtime,
            mode: if file_type == FileType::Directory {
                0o755
            } else {
                0o644
            },
            uid: 1000,
            gid: 1000,
            nlink: 1,
        })
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let entries = self.operator.list(path).await?;
        let mut result = Vec::new();
        for entry in entries {
            let meta = entry.metadata();
            let file_type = match meta.mode() {
                opendal::EntryMode::DIR => FileType::Directory,
                _ => FileType::File,
            };
            result.push(DirEntry {
                name: entry.name().to_string(),
                file_type,
            });
        }
        Ok(result)
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        self.operator.write(path, Vec::<u8>::new()).await?;
        let path_bytes = path.as_bytes().to_vec();
        Ok(FileHandle(path_bytes))
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        self.operator.delete(path).await.map_err(Into::into)
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        self.operator.rename(from, to).await.map_err(Into::into)
    }
}
