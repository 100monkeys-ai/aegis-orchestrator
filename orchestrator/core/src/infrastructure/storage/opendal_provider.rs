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
            format!("{path}/")
        } else {
            path.to_string()
        };
        self.operator.create_dir(&path).await.map_err(Into::into)
    }

    /// Remove `path` and everything beneath it.
    ///
    /// A path that does not exist is **not** an error here: OpenDAL treats a
    /// delete of an absent path as a no-op. That differs from
    /// [`LocalHostStorageProvider`](super::LocalHostStorageProvider), which
    /// returns [`StorageError::NotFound`]. Both behaviours are pinned by tests
    /// in this module and in `super`.
    ///
    /// [`Operator::remove_all`] is deprecated in favour of this exact call —
    /// upstream defines it as `delete_with(path).recursive(true)` — and the
    /// deprecation is an error under CI's `-D warnings`.
    async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
        self.operator
            .delete_with(path)
            .recursive(true)
            .await
            .map_err(Into::into)
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

#[cfg(test)]
mod tests {
    //! Real-backend tests for [`OpenDalStorageProvider`].
    //!
    //! These run against OpenDAL's **`fs` service under a scratch root**, not
    //! against a mock. Everything else in this repository that exercises
    //! `delete_directory` does so through a hand-written stub that returns
    //! `NotFound` for a missing path — which is not what the OpenDAL provider
    //! does — so a behaviour difference in this file was previously invisible
    //! to the whole suite.
    //!
    //! Two rules the tests here keep, deliberately:
    //!
    //! 1. **The provider is built through the product's own factory**,
    //!    [`create_storage_provider`], so the tests are callers of the real
    //!    seam rather than a parallel assembly of it.
    //! 2. **Every assertion reads the scratch root back with `std::fs`**, never
    //!    with `Operator::list`. A check whose two arms both travel through the
    //!    thing under test agrees with itself for as long as the defect lives.
    //!
    //! `services-fs` is a dev-only feature (see `Cargo.toml`); the shipped
    //! binary still registers only `memory`.

    use super::*;
    use crate::infrastructure::storage::{create_storage_provider, StorageBackend};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A provider rooted at a scratch directory this test owns exclusively.
    fn fs_provider(root: &Path) -> Arc<dyn StorageProvider> {
        let mut options = HashMap::new();
        options.insert("root".to_string(), root.display().to_string());
        create_storage_provider(StorageBackend::OpenDal {
            provider: "fs".to_string(),
            options,
        })
        .expect("the fs scheme is registered in test builds via the services-fs dev-feature")
    }

    fn scratch() -> (TempDir, Arc<dyn StorageProvider>) {
        let tmp = TempDir::new().expect("scratch root");
        let provider = fs_provider(tmp.path());
        (tmp, provider)
    }

    /// Write a file directly with `std::fs`, creating parents. Used to arrange
    /// state without going through the code under test.
    fn place(root: &Path, rel: &str, contents: &str) {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().expect("a parent")).expect("mkdir -p");
        std::fs::write(&full, contents).expect("write fixture file");
    }

    #[tokio::test]
    async fn delete_directory_removes_the_directory_and_everything_under_it() {
        let (tmp, provider) = scratch();
        let root = tmp.path();

        place(root, "a/b/c.txt", "one");
        place(root, "a/b/d/e.txt", "two");
        place(root, "a/keep.txt", "sibling that must survive");

        provider
            .delete_directory("/a/b")
            .await
            .expect("delete_directory should remove the subtree");

        assert!(
            !root.join("a/b").exists(),
            "the directory itself must be gone, not just its contents"
        );
        assert!(
            !root.join("a/b/d/e.txt").exists(),
            "a nested file must be gone"
        );
        assert!(
            root.join("a/keep.txt").exists(),
            "a sibling outside the deleted subtree must survive"
        );
        assert!(root.join("a").exists(), "the parent must survive");
    }

    /// Pins a divergence between the three `StorageProvider` implementations.
    /// OpenDAL treats a delete of an absent path as a no-op and returns `Ok`;
    /// `LocalHostStorageProvider` and the in-repo test double both return
    /// `StorageError::NotFound`. This test asserts what OpenDAL does today; the
    /// matching assertions for the other two live in `super`'s test module.
    /// Reconciling the three is a behaviour change and is not made here.
    #[tokio::test]
    async fn delete_directory_on_a_missing_path_returns_ok_unlike_the_other_providers() {
        let (tmp, provider) = scratch();
        assert!(
            !tmp.path().join("never-existed").exists(),
            "precondition: the path is genuinely absent"
        );

        let result = provider.delete_directory("/never-existed").await;

        assert!(
            result.is_ok(),
            "OpenDAL's delete is a no-op on an absent path; got {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_directory_on_a_file_deletes_the_file() {
        let (tmp, provider) = scratch();
        let root = tmp.path();
        place(root, "lonely.txt", "not a directory");

        provider
            .delete_directory("/lonely.txt")
            .await
            .expect("deleting a file through delete_directory should succeed");

        assert!(!root.join("lonely.txt").exists(), "the file must be gone");
    }

    /// `create_directory` appends a trailing slash and `delete_directory` does
    /// not. Nothing asserted that the two agree, so this pins the round trip
    /// from both spellings.
    #[tokio::test]
    async fn create_then_delete_round_trip_with_and_without_a_trailing_slash() {
        let (tmp, provider) = scratch();
        let root = tmp.path();

        for spelling in ["/dir-a", "/dir-b/"] {
            provider
                .create_directory(spelling)
                .await
                .unwrap_or_else(|e| panic!("create_directory({spelling}) failed: {e}"));

            let on_disk = root.join(spelling.trim_matches('/'));
            assert!(
                on_disk.is_dir(),
                "create_directory({spelling}) must produce a real directory at {}",
                on_disk.display()
            );

            provider
                .delete_directory(spelling.trim_end_matches('/'))
                .await
                .unwrap_or_else(|e| panic!("delete_directory({spelling}) failed: {e}"));

            assert!(
                !on_disk.exists(),
                "delete_directory({spelling}) must remove {}",
                on_disk.display()
            );
        }
    }

    #[tokio::test]
    async fn create_write_stat_readdir_delete_file_rename_round_trip() {
        let (tmp, provider) = scratch();
        let root = tmp.path();
        let payload = b"hello aegis";

        // create_file + write_at
        let handle = provider
            .create_file("/data/note.txt", 0o644)
            .await
            .expect("create_file");
        let written = provider
            .write_at(&handle, 0, payload)
            .await
            .expect("write_at");
        assert_eq!(written, payload.len(), "write_at reports bytes written");
        assert_eq!(
            std::fs::read(root.join("data/note.txt")).expect("read back with std::fs"),
            payload,
            "the bytes on disk must be the bytes written"
        );

        // stat
        let attrs = provider.stat("/data/note.txt").await.expect("stat");
        assert_eq!(attrs.file_type, FileType::File, "stat reports a file");
        assert_eq!(
            attrs.size,
            payload.len() as u64,
            "stat reports the real size"
        );

        // open_file + read_at
        let handle = provider
            .open_file("/data/note.txt", OpenMode::ReadOnly)
            .await
            .expect("open_file");
        let read = provider
            .read_at(&handle, 0, payload.len())
            .await
            .expect("read_at");
        assert_eq!(read, payload, "read_at returns what was written");
        provider.close_file(&handle).await.expect("close_file");

        // readdir
        let entries = provider.readdir("/data/").await.expect("readdir");
        assert!(
            entries.iter().any(|e| e.name == "note.txt"),
            "readdir must list the file it contains; got {entries:?}"
        );

        // rename
        provider
            .rename("/data/note.txt", "/data/renamed.txt")
            .await
            .expect("rename");
        assert!(
            !root.join("data/note.txt").exists(),
            "the old name must be gone from disk"
        );
        assert!(
            root.join("data/renamed.txt").exists(),
            "the new name must be present on disk"
        );

        // delete_file
        provider
            .delete_file("/data/renamed.txt")
            .await
            .expect("delete_file");
        assert!(
            !root.join("data/renamed.txt").exists(),
            "delete_file must remove the file from disk"
        );
    }

    #[tokio::test]
    async fn health_check_succeeds_against_a_real_root() {
        let (_tmp, provider) = scratch();
        provider
            .health_check()
            .await
            .expect("health_check against an existing scratch root should succeed");
    }
}
