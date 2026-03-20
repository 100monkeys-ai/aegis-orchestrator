// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Local Host Storage Provider
//!
//! Provides direct access to local host paths on the orchestrator via std::fs.
//! Uses PathSanitizer within AegisFSAL to restrict agents to their assigned
//! volume roots, per ADR-047.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for local_host

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageError, StorageProvider,
};
use async_trait::async_trait;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub struct LocalHostStorageProvider {
    /// The root path on the local host to mount
    mount_point: PathBuf,
}

impl LocalHostStorageProvider {
    pub fn new(mount_point: impl Into<PathBuf>) -> Result<Self, StorageError> {
        let path = mount_point.into();
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(|e| {
                StorageError::IoError(format!(
                    "Failed to create mount point {}: {}",
                    path.display(),
                    e
                ))
            })?;
        }
        Ok(Self { mount_point: path })
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        let path = path.strip_prefix('/').unwrap_or(path);
        self.mount_point.join(path)
    }
}

#[async_trait]
impl StorageProvider for LocalHostStorageProvider {
    async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
        let fs_path = self.resolve_path(path);
        if fs_path.exists() {
            return Err(StorageError::AlreadyExists(path.to_string()));
        }
        std::fs::create_dir_all(&fs_path)
            .map_err(|e| StorageError::IoError(format!("Create dir failed: {e}")))?;
        Ok(())
    }

    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        let fs_path = self.resolve_path(path);
        if !fs_path.exists() {
            return Err(StorageError::NotFound(path.to_string()));
        }
        std::fs::remove_dir_all(&fs_path)
            .map_err(|e| StorageError::IoError(format!("Delete dir failed: {e}")))?;
        Ok(())
    }

    async fn set_quota(&self, _path: &str, _bytes: u64) -> Result<(), StorageError> {
        // Handled by AegisFSAL
        Ok(())
    }

    async fn get_usage(&self, _path: &str) -> Result<u64, StorageError> {
        // Usage tracking is handled by AegisFSAL.
        Ok(0)
    }

    async fn health_check(&self) -> Result<(), StorageError> {
        if self.mount_point.exists() {
            Ok(())
        } else {
            Err(StorageError::IoError("Mount point missing".to_string()))
        }
    }

    // --- POSIX File Operations (ADR-036) ---

    async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
        let fs_path = self.resolve_path(path);
        match mode {
            OpenMode::ReadOnly => {
                std::fs::metadata(&fs_path)
                    .map_err(|e| StorageError::FileNotFound(format!("{path}: {e}")))?;
            }
            OpenMode::WriteOnly => {
                File::options()
                    .write(true)
                    .open(&fs_path)
                    .map_err(|e| StorageError::FileNotFound(format!("{path}: {e}")))?;
            }
            OpenMode::ReadWrite => {
                File::options()
                    .read(true)
                    .write(true)
                    .open(&fs_path)
                    .map_err(|e| StorageError::FileNotFound(format!("{path}: {e}")))?;
            }
            OpenMode::Create => {
                File::create(&fs_path)
                    .map_err(|e| StorageError::IoError(format!("Create failed {path}: {e}")))?;
            }
        };

        Ok(FileHandle(path.as_bytes().to_vec()))
    }

    async fn read_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        let fs_path = self.resolve_path(&path);

        let mut file =
            File::open(&fs_path).map_err(|e| StorageError::FileNotFound(e.to_string()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| StorageError::IoError(e.to_string()))?;

        let mut buffer = vec![0u8; length];
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| StorageError::IoError(e.to_string()))?;
        buffer.truncate(bytes_read);
        Ok(buffer)
    }

    async fn write_at(
        &self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<usize, StorageError> {
        let path = String::from_utf8(handle.0.clone())
            .map_err(|_| StorageError::InvalidPath("Invalid handle".into()))?;
        let fs_path = self.resolve_path(&path);

        let mut file = File::options()
            .write(true)
            .open(&fs_path)
            .map_err(|e| StorageError::FileNotFound(e.to_string()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| StorageError::IoError(e.to_string()))?;
        file.write_all(data)
            .map_err(|e| StorageError::IoError(e.to_string()))?;
        Ok(data.len())
    }

    async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
        Ok(())
    }

    async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
        let fs_path = self.resolve_path(path);
        let metadata =
            std::fs::metadata(&fs_path).map_err(|e| StorageError::FileNotFound(e.to_string()))?;

        let file_type = if metadata.is_dir() {
            FileType::Directory
        } else {
            FileType::File
        };

        #[cfg(unix)]
        let (uid, gid, mode, nlink) = (
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
            metadata.nlink() as u32,
        );
        #[cfg(not(unix))]
        let (uid, gid, mode, nlink) = (
            1000,
            1000,
            if file_type == FileType::Directory {
                0o755
            } else {
                0o644
            },
            1,
        );

        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        Ok(FileAttributes {
            file_type,
            size: metadata.len(),
            mtime,
            atime: mtime,
            ctime: mtime,
            mode,
            uid,
            gid,
            nlink,
        })
    }

    async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
        let fs_path = self.resolve_path(path);
        let mut entries = Vec::new();

        for entry in std::fs::read_dir(&fs_path)
            .map_err(|_| StorageError::IoError("readdir failed".into()))?
        {
            let entry = entry.map_err(|_| StorageError::IoError("entry read failed".into()))?;
            let metadata = entry
                .metadata()
                .map_err(|_| StorageError::IoError("meta failed".into()))?;
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                file_type: if metadata.is_dir() {
                    FileType::Directory
                } else {
                    FileType::File
                },
            });
        }
        Ok(entries)
    }

    async fn create_file(&self, path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
        let fs_path = self.resolve_path(path);
        if let Some(p) = fs_path.parent() {
            std::fs::create_dir_all(p).map_err(|e| StorageError::IoError(e.to_string()))?;
        }
        File::create(&fs_path).map_err(|e| StorageError::IoError(e.to_string()))?;
        Ok(FileHandle(path.as_bytes().to_vec()))
    }

    async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
        let fs_path = self.resolve_path(path);
        std::fs::remove_file(&fs_path).map_err(|e| StorageError::IoError(e.to_string()))
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_path = self.resolve_path(from);
        let to_path = self.resolve_path(to);
        std::fs::rename(from_path, to_path).map_err(|e| StorageError::IoError(e.to_string()))
    }
}
