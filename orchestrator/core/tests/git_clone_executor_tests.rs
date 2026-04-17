// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `GitCloneExecutor` Integration Tests (BC-7, ADR-081 Waves A2 / A3)
//!
//! Clones a local bare git repository fixture with `libgit2` and
//! asserts:
//!
//! - Public clone succeeds, returns a resolvable commit SHA, and
//!   populates the target directory with real files.
//! - A bad URL surfaces as [`CloneError::Git`].
//! - [`GitCloneExecutor::select_strategy`] routes HostPath backends to
//!   libgit2 and SeaweedFS / OpenDAL / SEAL backends to EphemeralCli.
//! - [`GitCloneExecutor::fetch_and_checkout`] performs ref pinning for
//!   Branch / Tag / Commit variants.
//! - Sparse checkout prunes the working tree to the requested paths.
//!
//! ## URL validation bypass in tests
//!
//! `validate_repo_url` rejects `file://` URLs per ADR-081 §Security.
//! Tests here invoke the executor directly so they bypass the
//! service-layer validator — the executor itself makes no URL
//! assertions, which is the correct design: URL policy lives at the
//! service boundary. Production callers always go through
//! [`GitRepoService`] which enforces the allow-list.
//!
//! [`GitRepoService`]: aegis_orchestrator_core::application::git_repo_service::GitRepoService
//! [`CloneError::Git`]: aegis_orchestrator_core::application::git_clone_executor::CloneError
//! [`CloneStrategy::Libgit2`]: aegis_orchestrator_core::domain::git_repo::CloneStrategy

use std::path::Path;
use std::sync::Arc;

use aegis_orchestrator_core::application::git_clone_executor::{CloneError, GitCloneExecutor};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{CloneStrategy, GitRef, GitRepoBinding};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a fresh bare git repository containing a single commit with one
/// file, and return the `file://` URL pointing at the bare repo.
fn build_bare_fixture(dir: &Path) -> String {
    // Create a working repo, commit a file, then clone it as bare.
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();

    // Initialize a repository with an explicit 'main' branch so the
    // bare target we create next doesn't inherit the libgit2 default
    // (which depends on the host config).
    let mut init_opts = git2::RepositoryInitOptions::new();
    init_opts.initial_head("main");
    let repo = git2::Repository::init_opts(&work, &init_opts).unwrap();

    // Configure identity so commit() can be resolved.
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "A2 Tester").unwrap();
    cfg.set_str("user.email", "a2@aegis.test").unwrap();

    // Add a README and commit.
    std::fs::write(work.join("README.md"), b"hello from A2\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("A2 Tester", "a2@aegis.test").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();

    // Clone the working repo as a bare repo that the executor under test
    // will clone from.
    let bare = dir.join("fixture.git");
    let bare_url_source = format!("file://{}", work.display());
    let mut builder = git2::build::RepoBuilder::new();
    builder.bare(true);
    builder.clone(&bare_url_source, &bare).unwrap();

    format!("file://{}", bare.display())
}

struct NoopPublisher;
#[async_trait::async_trait]
impl EventPublisher for NoopPublisher {
    async fn publish_storage_event(
        &self,
        _event: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

fn test_executor() -> GitCloneExecutor {
    // Build the minimum dependencies that GitCloneExecutor holds. The
    // A2 executor only uses these fields during credential-resolution
    // branches we're not exercising here, so we construct them with
    // stub-quality impls.
    let event_bus = Arc::new(EventBus::new(16));
    let secrets_manager = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        event_bus,
    ));
    let volume_repo: Arc<dyn aegis_orchestrator_core::domain::repository::VolumeRepository> =
        Arc::new(InMemoryVolumeRepository::new());
    let event_publisher: Arc<dyn EventPublisher> = Arc::new(NoopPublisher);
    let fsal = Arc::new(AegisFSAL::new(
        stub_storage_provider(),
        volume_repo,
        Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
        event_publisher,
    ));

    GitCloneExecutor::new(secrets_manager, fsal, None)
}

fn stub_storage_provider() -> Arc<dyn aegis_orchestrator_core::domain::storage::StorageProvider> {
    // A no-op StorageProvider that panics on use. The clone path under
    // test writes directly to the passed `target_dir` via libgit2 — it
    // never calls the FSAL's storage provider. If a future test starts
    // exercising FSAL reads, replace this with a real in-memory impl.
    use aegis_orchestrator_core::domain::storage::{
        DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    struct Unused;
    #[async_trait::async_trait]
    impl StorageProvider for Unused {
        async fn create_directory(&self, _path: &str) -> Result<(), StorageError> {
            unreachable!("test storage provider should not be called")
        }
        async fn delete_directory(&self, _path: &str) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn set_quota(&self, _path: &str, _bytes: u64) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn get_usage(&self, _path: &str) -> Result<u64, StorageError> {
            unreachable!()
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn open_file(
            &self,
            _path: &str,
            _mode: OpenMode,
        ) -> Result<FileHandle, StorageError> {
            unreachable!()
        }
        async fn read_at(
            &self,
            _handle: &FileHandle,
            _offset: u64,
            _length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            unreachable!()
        }
        async fn write_at(
            &self,
            _handle: &FileHandle,
            _offset: u64,
            _data: &[u8],
        ) -> Result<usize, StorageError> {
            unreachable!()
        }
        async fn close_file(&self, _handle: &FileHandle) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn stat(&self, _path: &str) -> Result<FileAttributes, StorageError> {
            unreachable!()
        }
        async fn readdir(&self, _path: &str) -> Result<Vec<DirEntry>, StorageError> {
            unreachable!()
        }
        async fn create_file(&self, _path: &str, _mode: u32) -> Result<FileHandle, StorageError> {
            unreachable!()
        }
        async fn delete_file(&self, _path: &str) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn rename(&self, _from: &str, _to: &str) -> Result<(), StorageError> {
            unreachable!()
        }
    }
    Arc::new(Unused)
}

fn test_binding(url: &str) -> GitRepoBinding {
    GitRepoBinding::new(
        TenantId::consumer(),
        None,
        url.to_string(),
        GitRef::default(),
        None,
        VolumeId::new(),
        "fixture".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn clone_public_file_url_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let url = build_bare_fixture(tmp.path());
    let target = tmp.path().join("cloned");

    let binding = test_binding(&url);
    let executor = test_executor();

    let sha = executor
        .clone_libgit2(&binding, &target, None, true)
        .await
        .expect("clone should succeed");

    assert_eq!(sha.len(), 40, "commit SHA should be 40 hex chars");
    assert!(target.join("README.md").exists(), "working tree populated");
    let readme = std::fs::read_to_string(target.join("README.md")).unwrap();
    assert_eq!(readme, "hello from A2\n");
}

#[tokio::test]
async fn clone_bad_url_returns_clone_error() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("never");
    let binding = test_binding("file:///this/path/does/not/exist.git");
    let executor = test_executor();

    let err = executor
        .clone_libgit2(&binding, &target, None, true)
        .await
        .expect_err("bad URL must fail");
    assert!(
        matches!(err, CloneError::Git(_)),
        "expected CloneError::Git, got {err:?}"
    );
}

#[tokio::test]
async fn select_strategy_routes_by_volume_backend() {
    use aegis_orchestrator_core::domain::volume::{
        FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeOwnership, VolumeStatus,
    };

    let executor = test_executor();
    let binding = test_binding("https://github.com/octocat/Hello-World.git");

    // HostPath → Libgit2
    let hp_volume = Volume {
        id: VolumeId::new(),
        name: "hp".to_string(),
        tenant_id: TenantId::consumer(),
        storage_class: StorageClass::persistent(),
        backend: VolumeBackend::HostPath {
            path: std::path::PathBuf::from("/tmp/x"),
        },
        size_limit_bytes: 1,
        status: VolumeStatus::Available,
        ownership: VolumeOwnership::persistent("u"),
        created_at: chrono::Utc::now(),
        attached_at: None,
        detached_at: None,
        expires_at: None,
        host_node_id: None,
    };
    assert_eq!(
        executor.select_strategy(&binding, &hp_volume),
        CloneStrategy::Libgit2
    );

    // SeaweedFS → EphemeralCli
    let sw_volume = Volume {
        backend: VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://localhost:8888").unwrap(),
            remote_path: "/volumes/x".to_string(),
        },
        ..hp_volume
    };
    assert!(matches!(
        executor.select_strategy(&binding, &sw_volume),
        CloneStrategy::EphemeralCli { .. }
    ));
}

#[tokio::test]
async fn fetch_and_checkout_branch_fast_forwards() {
    let tmp = tempfile::tempdir().unwrap();
    let url = build_bare_fixture(tmp.path());
    let target = tmp.path().join("cloned");
    let executor = test_executor();
    let binding = test_binding(&url);

    // Initial clone
    let initial_sha = executor
        .clone_libgit2(&binding, &target, None, true)
        .await
        .expect("initial clone");

    // Make a second commit on the upstream work tree so a fetch will
    // actually advance HEAD.
    let work = tmp.path().join("work");
    let repo = git2::Repository::open(&work).unwrap();
    std::fs::write(work.join("new.txt"), b"second\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("new.txt")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("A3 Tester", "a3@aegis.test").unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
        .unwrap();

    // Push the new tip into the bare fixture that `binding.repo_url`
    // points at. libgit2 in the working repo needs to know about origin.
    let bare_path = tmp.path().join("fixture.git");
    let bare = git2::Repository::open(&bare_path).unwrap();
    // Copy the new commit into the bare repo by fetching it.
    let mut remote = bare
        .remote_anonymous(&format!("file://{}", work.display()))
        .unwrap();
    remote
        .fetch(&["+refs/heads/main:refs/heads/main"], None, None)
        .unwrap();

    let new_sha = executor
        .fetch_and_checkout(&binding, &target, None)
        .await
        .expect("fetch_and_checkout should succeed");
    assert_ne!(new_sha, initial_sha, "HEAD should advance after fetch");
    assert!(
        target.join("new.txt").exists(),
        "new file present after fetch"
    );
}

#[tokio::test]
async fn fetch_and_checkout_tag_pins_to_tag_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let url = build_bare_fixture(tmp.path());
    let target = tmp.path().join("cloned");
    let executor = test_executor();

    // Tag HEAD as v1.0.0 in the upstream work repo, then copy the tag
    // into the bare fixture.
    let work = tmp.path().join("work");
    {
        let repo = git2::Repository::open(&work).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("Tagger", "t@aegis.test").unwrap();
        repo.tag("v1.0.0", head.as_object(), &sig, "release", false)
            .unwrap();
    }
    {
        let bare_path = tmp.path().join("fixture.git");
        let bare = git2::Repository::open(&bare_path).unwrap();
        let mut remote = bare
            .remote_anonymous(&format!("file://{}", work.display()))
            .unwrap();
        remote
            .fetch(&["+refs/tags/v1.0.0:refs/tags/v1.0.0"], None, None)
            .unwrap();
    }

    // First do a plain clone with the executor (default branch main).
    let binding_branch = test_binding(&url);
    let branch_sha = executor
        .clone_libgit2(&binding_branch, &target, None, true)
        .await
        .unwrap();

    // Now construct a tag-pinned binding and fetch_and_checkout at the
    // same target. Tag resolves to the same commit as main.
    let binding_tag = GitRepoBinding::new(
        TenantId::consumer(),
        None,
        url.clone(),
        GitRef::Tag("v1.0.0".to_string()),
        None,
        VolumeId::new(),
        "fixture-tag".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    );

    let tag_sha = executor
        .fetch_and_checkout(&binding_tag, &target, None)
        .await
        .expect("tag fetch should succeed");
    assert_eq!(branch_sha, tag_sha);
}

#[tokio::test]
async fn fetch_and_checkout_commit_pins_to_exact_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let url = build_bare_fixture(tmp.path());
    let target = tmp.path().join("cloned");
    let executor = test_executor();

    let binding_branch = test_binding(&url);
    let initial_sha = executor
        .clone_libgit2(&binding_branch, &target, None, false)
        .await
        .unwrap();

    let binding_commit = GitRepoBinding::new(
        TenantId::consumer(),
        None,
        url.clone(),
        GitRef::Commit(initial_sha.clone()),
        None,
        VolumeId::new(),
        "fixture-commit".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    );
    let pinned_sha = executor
        .fetch_and_checkout(&binding_commit, &target, None)
        .await
        .expect("commit fetch should succeed");
    assert_eq!(initial_sha, pinned_sha);
}

#[tokio::test]
async fn sparse_checkout_prunes_working_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // Build an upstream with two paths; sparse-checkout will keep only
    // one of them.
    let mut init_opts = git2::RepositoryInitOptions::new();
    init_opts.initial_head("main");
    let repo = git2::Repository::init_opts(&work, &init_opts).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "A3").unwrap();
    cfg.set_str("user.email", "a3@aegis.test").unwrap();
    std::fs::create_dir_all(work.join("keep")).unwrap();
    std::fs::create_dir_all(work.join("prune")).unwrap();
    std::fs::write(work.join("keep/a.txt"), b"k\n").unwrap();
    std::fs::write(work.join("prune/b.txt"), b"p\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("keep/a.txt")).unwrap();
    index.add_path(std::path::Path::new("prune/b.txt")).unwrap();
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("A3", "a3@aegis.test").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let bare = tmp.path().join("fixture.git");
    let mut builder = git2::build::RepoBuilder::new();
    builder.bare(true);
    builder
        .clone(&format!("file://{}", work.display()), &bare)
        .unwrap();
    let url = format!("file://{}", bare.display());

    let target = tmp.path().join("cloned");
    let executor = test_executor();

    let binding = GitRepoBinding::new(
        TenantId::consumer(),
        None,
        url,
        GitRef::Branch("main".to_string()),
        Some(vec!["keep".to_string()]),
        VolumeId::new(),
        "sparse".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    );
    executor
        .clone_libgit2(&binding, &target, None, true)
        .await
        .expect("clone + sparse should succeed");

    assert!(
        target.join("keep/a.txt").exists(),
        "sparse-included path should be present"
    );
    assert!(
        !target.join("prune/b.txt").exists(),
        "sparse-excluded path should be pruned"
    );
}
