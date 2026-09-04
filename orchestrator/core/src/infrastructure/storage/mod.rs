// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Storage Infrastructure Module
//!
//! Provides concrete implementations of the StorageProvider trait
//! for distributed file system backends.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for mod

pub mod local_host_provider;
pub mod opendal_provider;
pub mod remote_storage_server;
pub mod seaweedfs;

use crate::domain::storage::{
    DirEntry, FileAttributes, FileHandle, FileType, OpenMode, StorageProvider,
};

pub use local_host_provider::LocalHostStorageProvider;
pub use opendal_provider::OpenDalStorageProvider;
pub use remote_storage_server::RemoteStorageServiceHandler;
pub use seaweedfs::SeaweedFSAdapter;
pub mod seal_provider;
use opendal::Operator;
pub use seal_provider::SealStorageProvider;

use anyhow::Context as _;
use std::sync::Arc;

/// Storage backend configuration
#[derive(Debug, Clone)]
pub enum StorageBackend {
    /// SeaweedFS distributed storage (production)
    SeaweedFS { filer_url: String },

    /// Local host mount point for direct host IO (ADR-047)
    LocalHost { mount_point: String },

    /// OpenDAL unified storage backend (ADR-047)
    OpenDal {
        provider: String,
        options: std::collections::HashMap<String, String>,
    },

    /// Test storage for unit testing
    Mock,
}

/// Factory function to create storage provider from configuration.
///
/// # Errors
///
/// Returns an error if the backend configuration is invalid (e.g., unrecognized
/// OpenDAL scheme or a `LocalHost` mount point that cannot be initialised).
/// Callers should handle this at startup; a bad storage config is fatal.
pub fn create_storage_provider(
    backend: StorageBackend,
) -> Result<Arc<dyn StorageProvider>, anyhow::Error> {
    match backend {
        StorageBackend::SeaweedFS { filer_url } => Ok(Arc::new(SeaweedFSAdapter::new(filer_url))),
        StorageBackend::LocalHost { mount_point } => {
            let provider = LocalHostStorageProvider::new(mount_point)
                .context("Failed to create LocalHostStorageProvider")?;
            Ok(Arc::new(provider))
        }
        StorageBackend::OpenDal { provider, options } => {
            // Validate the operator-supplied scheme against the schemes this
            // build can actually construct, and do it before anything is built.
            //
            // Why the registry and not a parse: OpenDAL removed the `Scheme`
            // enum, and `Operator::via_iter` now dispatches through
            // `OperatorRegistry`, whose `schemes()` is documented as exactly the
            // set `from_uri` can construct for the compiled-in `services-*`
            // features. The old `provider.parse::<opendal::Scheme>()` validated
            // against every scheme OpenDAL has ever known regardless of
            // features, so `provider = "s3"` passed the check and then failed
            // one line later with a different sentence. This validates against
            // the set that can be built.
            //
            // It also keeps a URI-shaped value rejected. `via_iter` on a string
            // containing "://" is parsed as a URI, so a `provider` of
            // "s3://bucket" would be silently accepted as scheme "s3" with
            // authority "bucket" — a change in what a config file means, not in
            // an error message. It is not a registered scheme, so it is refused
            // here exactly as it is today.
            //
            // `register`/`OperatorUri` both lower-case, so the comparison does.
            opendal::init_default_registry();
            let mut available: Vec<String> = opendal::OperatorRegistry::get()
                .schemes()
                .into_iter()
                .collect();
            available.sort();
            let requested = provider.to_ascii_lowercase();
            if !available.contains(&requested) {
                anyhow::bail!(
                    "Invalid OpenDAL scheme: '{provider}'. This build registers: {}",
                    available.join(", ")
                );
            }
            let op = Operator::via_iter(&provider, options).with_context(|| {
                format!("Failed to create OpenDAL operator for scheme '{provider}'")
            })?;
            Ok(Arc::new(OpenDalStorageProvider::new(op)))
        }
        StorageBackend::Mock => Ok(Arc::new(test_support::TestStorageProvider::new())),
    }
}

// Re-export the test storage provider for testing.
pub use test_support::TestStorageProvider;

mod test_support {
    use super::*;
    use crate::domain::storage::StorageError;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    pub struct TestStorageProvider {
        pub directories: Arc<Mutex<HashMap<String, u64>>>,
        pub quotas: Arc<Mutex<HashMap<String, u64>>>,
    }

    impl TestStorageProvider {
        pub fn new() -> Self {
            Self {
                directories: Arc::new(Mutex::new(HashMap::new())),
                quotas: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    impl Default for TestStorageProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl StorageProvider for TestStorageProvider {
        async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
            let mut dirs = self.directories.lock().await;
            if dirs.contains_key(path) {
                return Err(StorageError::AlreadyExists(path.to_string()));
            }
            dirs.insert(path.to_string(), 0);
            Ok(())
        }

        async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
            let mut dirs = self.directories.lock().await;
            if !dirs.contains_key(path) {
                return Err(StorageError::NotFound(path.to_string()));
            }
            dirs.remove(path);
            let mut quotas = self.quotas.lock().await;
            quotas.remove(path);
            Ok(())
        }

        async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
            let dirs = self.directories.lock().await;
            if !dirs.contains_key(path) {
                return Err(StorageError::NotFound(path.to_string()));
            }
            drop(dirs);
            let mut quotas = self.quotas.lock().await;
            quotas.insert(path.to_string(), bytes);
            Ok(())
        }

        async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
            let dirs = self.directories.lock().await;
            dirs.get(path)
                .copied()
                .ok_or_else(|| StorageError::NotFound(path.to_string()))
        }

        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }

        // POSIX file operations (ADR-036)
        async fn open_file(
            &self,
            _path: &str,
            _mode: OpenMode,
        ) -> Result<FileHandle, StorageError> {
            Ok(FileHandle(b"test-handle".to_vec()))
        }

        async fn read_at(
            &self,
            _handle: &FileHandle,
            _offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            Ok(vec![0u8; length])
        }

        async fn write_at(
            &self,
            _handle: &FileHandle,
            _offset: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
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
            Ok(FileHandle(format!("test-handle-{path}").into_bytes()))
        }

        async fn delete_file(&self, _path: &str) -> Result<(), StorageError> {
            Ok(())
        }

        async fn rename(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_factory_seaweedfs() {
        let provider = create_storage_provider(StorageBackend::SeaweedFS {
            filer_url: "http://localhost:8888".to_string(),
        })
        .expect("SeaweedFS storage provider should be created successfully");

        assert_eq!(
            Arc::strong_count(&provider),
            1,
            "unexpected extra Arc references"
        );
    }

    #[test]
    fn test_factory_test_backend() {
        let provider = create_storage_provider(StorageBackend::Mock)
            .expect("test storage provider should be created successfully");

        assert_eq!(
            Arc::strong_count(&provider),
            1,
            "unexpected extra Arc references"
        );
    }

    fn opendal_factory_error(provider: &str) -> String {
        let result = create_storage_provider(StorageBackend::OpenDal {
            provider: provider.to_string(),
            options: std::collections::HashMap::new(),
        });
        assert!(
            result.is_err(),
            "provider '{provider}' should return Err, not panic and not succeed"
        );
        result
            .err()
            .expect("provider should return Err")
            .to_string()
    }

    #[test]
    fn test_factory_invalid_opendal_scheme_returns_error() {
        let err_msg = opendal_factory_error("not-a-real-scheme");
        assert!(
            err_msg.contains("not-a-real-scheme"),
            "error message should identify the bad scheme; got: {err_msg}"
        );
        assert!(
            err_msg.starts_with("Invalid OpenDAL scheme: 'not-a-real-scheme'"),
            "the rejection sentence operators key on must be preserved; got: {err_msg}"
        );
        assert!(
            err_msg.contains("This build registers: "),
            "the rejection must name what this build can construct; got: {err_msg}"
        );
    }

    /// A scheme OpenDAL knows but this build does not compile in is rejected by
    /// the same sentence as an unknown one, because an operator can act on
    /// neither differently. Before the registry guard this string passed the
    /// scheme check and failed one line later with a different sentence.
    #[test]
    fn test_factory_rejects_a_scheme_this_build_does_not_register() {
        let err_msg = opendal_factory_error("s3");
        assert!(
            err_msg.starts_with("Invalid OpenDAL scheme: 's3'"),
            "an unregistered-but-known scheme must be refused by the scheme \
             sentence, not by the operator-construction sentence; got: {err_msg}"
        );
    }

    /// A URI-shaped provider value must stay rejected. `Operator::via_iter`
    /// parses a string containing "://" as a URI, so without this guard
    /// "memory://x" would be silently accepted as scheme "memory" — a change in
    /// what a configuration file means, not in an error message.
    #[test]
    fn test_factory_rejects_uri_shaped_provider() {
        let err_msg = opendal_factory_error("memory://some-authority");
        assert!(
            err_msg.starts_with("Invalid OpenDAL scheme: 'memory://some-authority'"),
            "a URI-shaped provider must be refused as a scheme, not parsed as a \
             URI; got: {err_msg}"
        );
    }

    /// The scheme comparison is case-insensitive because OpenDAL's registry and
    /// its URI parser both lower-case. This is the positive control for the two
    /// rejection tests above: without it they would pass against a guard that
    /// rejects everything.
    // ------------------------------------------------------------------
    // The three StorageProvider implementations disagree about deleting a
    // path that is not there, and nothing said so until now. Each test below
    // asserts one provider's CURRENT behaviour, by name. The OpenDAL half is
    // `delete_directory_on_a_missing_path_returns_ok_unlike_the_other_providers`
    // in `opendal_provider`'s test module. Reconciling the three is a
    // behaviour change and is deliberately not made here.
    // ------------------------------------------------------------------

    /// `LocalHostStorageProvider` refuses a delete of an absent path.
    #[tokio::test]
    async fn test_local_host_delete_directory_on_a_missing_path_returns_not_found() {
        let tmp = tempfile::TempDir::new().expect("scratch root");
        let provider = LocalHostStorageProvider::new(tmp.path())
            .expect("provider over an existing scratch root");

        let result = provider.delete_directory("/never-existed").await;

        assert!(
            matches!(
                result,
                Err(crate::domain::storage::StorageError::NotFound(_))
            ),
            "local host returns NotFound where OpenDAL returns Ok; got {result:?}"
        );
    }

    /// The in-repo test double refuses it too, which is why every mocked test
    /// of `delete_directory` in this repository agrees with local host and
    /// disagrees with the provider that actually ships for OpenDAL backends.
    #[tokio::test]
    async fn test_mock_delete_directory_on_a_missing_path_returns_not_found() {
        let provider = test_support::TestStorageProvider::new();

        let result = provider.delete_directory("/never-existed").await;

        assert!(
            matches!(
                result,
                Err(crate::domain::storage::StorageError::NotFound(_))
            ),
            "the test double returns NotFound where OpenDAL returns Ok; got {result:?}"
        );
    }

    #[test]
    fn test_factory_accepts_the_registered_scheme_case_insensitively() {
        for provider in ["memory", "MEMORY", "Memory"] {
            let result = create_storage_provider(StorageBackend::OpenDal {
                provider: provider.to_string(),
                options: std::collections::HashMap::new(),
            });
            assert!(
                result.is_ok(),
                "'{provider}' is registered by this build and must be accepted"
            );
        }
    }
}
