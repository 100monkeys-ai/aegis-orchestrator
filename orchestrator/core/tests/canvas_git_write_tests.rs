// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Canvas git-write Service + API Tests (BC-7, ADR-106 Wave B2)
//!
//! Covers the B2 service-layer surface added on top of ADR-081 A2:
//!
//! | Scenario | Test |
//! |---|---|
//! | Commit with staged changes returns SHA and emits `CommitMade` | `commit_with_changes_emits_event` |
//! | Commit with no changes → `NothingToCommit` | `commit_with_no_changes_returns_nothing_to_commit` |
//! | Commit with wrong owner → `BindingNotFound` | `commit_wrong_owner_returns_not_found` |
//! | Push against a local bare remote succeeds + emits `PushCompleted` | `push_to_local_bare_remote_succeeds` |
//! | Push with a bad remote bubbles `GitFailed` | `push_with_bad_remote_fails_cleanly` |
//! | Diff (workdir) returns non-empty patch after edit | `diff_returns_unified_patch_for_workdir_change` |
//! | Diff `staged=true` differs from `staged=false` when index ≠ workdir | `diff_staged_differs_from_workdir_when_index_ahead` |
//! | API-style: commit wrong owner → NotFound (leaks nothing) | `api_commit_wrong_owner_returns_not_found` |
//! | API-style: push wrong owner → NotFound | `api_push_wrong_owner_returns_not_found` |
//! | API-style: diff wrong owner → NotFound | `api_diff_wrong_owner_returns_not_found` |
//!
//! The fixture mirrors `git_repo_service_tests.rs` exactly: in-memory
//! [`GitRepoBindingRepository`], [`MockVolumeService`] producing
//! HostPath volumes, and a real libgit2 working-copy that we populate
//! directly (bypassing the A2 clone executor — B2 operates on any
//! `Ready` binding regardless of how it got there).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;

use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{
    CreateGitRepoCommand, GitRepoError, GitRepoService,
};
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::events::VolumeEvent;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::{RepositoryError, VolumeRepository};
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership, VolumeStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

// ===========================================================================
// In-memory GitRepoBindingRepository — mirrored from service_tests (each
// integration test file is its own binary target, so no shared module).
// ===========================================================================

#[derive(Default)]
struct InMemoryGitRepoBindingRepository {
    bindings: RwLock<HashMap<GitRepoBindingId, GitRepoBinding>>,
}

#[async_trait]
impl GitRepoBindingRepository for InMemoryGitRepoBindingRepository {
    async fn save(&self, binding: &GitRepoBinding) -> Result<(), RepositoryError> {
        self.bindings
            .write()
            .unwrap()
            .insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &GitRepoBindingId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self.bindings.read().unwrap().get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<Vec<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .filter(|b| &b.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn find_by_volume_id(
        &self,
        volume_id: &VolumeId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .find(|b| &b.volume_id == volume_id)
            .cloned())
    }
    async fn find_by_webhook_lookup_hash(
        &self,
        _hash: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(None)
    }
    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<u32, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .filter(|b| &b.tenant_id == tenant_id)
            .count() as u32)
    }
    async fn delete(&self, id: &GitRepoBindingId) -> Result<(), RepositoryError> {
        self.bindings.write().unwrap().remove(id);
        Ok(())
    }
}

// ===========================================================================
// Mock VolumeService producing HostPath volumes.
// ===========================================================================

struct MockVolumeService {
    repo: Arc<InMemoryVolumeRepository>,
    event_bus: Arc<EventBus>,
    root: PathBuf,
}

#[async_trait]
impl VolumeService for MockVolumeService {
    async fn create_volume(
        &self,
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        size_limit_mb: u64,
        ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        let vid = VolumeId::new();
        let dir = self.root.join(vid.0.to_string());
        std::fs::create_dir_all(&dir)?;
        let vol = Volume {
            id: vid,
            name,
            tenant_id,
            storage_class,
            backend: VolumeBackend::HostPath { path: dir },
            size_limit_bytes: size_limit_mb * 1024 * 1024,
            status: VolumeStatus::Available,
            ownership,
            created_at: chrono::Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };
        self.repo.save(&vol).await?;
        Ok(vid)
    }
    async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
        self.repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("not found"))
    }
    async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        Ok(self.repo.find_by_tenant(tenant_id).await?)
    }
    async fn list_volumes_by_ownership(
        &self,
        ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(self.repo.find_by_ownership(ownership).await?)
    }
    async fn attach_volume(
        &self,
        _vid: VolumeId,
        _iid: InstanceId,
        _m: PathBuf,
        _a: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        unimplemented!()
    }
    async fn detach_volume(&self, _vid: VolumeId, _iid: InstanceId) -> anyhow::Result<()> {
        unimplemented!()
    }
    async fn delete_volume(&self, volume_id: VolumeId) -> anyhow::Result<()> {
        self.repo.delete(volume_id).await?;
        self.event_bus
            .publish_volume_event(VolumeEvent::VolumeDeleted {
                volume_id,
                deleted_at: chrono::Utc::now(),
            });
        Ok(())
    }
    async fn get_volume_usage(&self, _v: VolumeId) -> anyhow::Result<u64> {
        Ok(0)
    }
    async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn create_volumes_for_execution(
        &self,
        _eid: aegis_orchestrator_core::domain::execution::ExecutionId,
        _tid: TenantId,
        _vs: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
        _m: &str,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn persist_external_volume(
        &self,
        _vid: VolumeId,
        _n: String,
        _t: TenantId,
        _p: String,
        _s: u64,
        _o: VolumeOwnership,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

struct NoopPublisher;
#[async_trait]
impl EventPublisher for NoopPublisher {
    async fn publish_storage_event(
        &self,
        _e: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

fn stub_storage_provider() -> Arc<dyn aegis_orchestrator_core::domain::storage::StorageProvider> {
    use aegis_orchestrator_core::domain::storage::{
        DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    struct Unused;
    #[async_trait]
    impl StorageProvider for Unused {
        async fn create_directory(&self, _p: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete_directory(&self, _p: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_quota(&self, _p: &str, _b: u64) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_usage(&self, _p: &str) -> Result<u64, StorageError> {
            Ok(0)
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn open_file(&self, _p: &str, _m: OpenMode) -> Result<FileHandle, StorageError> {
            unreachable!()
        }
        async fn read_at(
            &self,
            _h: &FileHandle,
            _o: u64,
            _l: usize,
        ) -> Result<Vec<u8>, StorageError> {
            unreachable!()
        }
        async fn write_at(
            &self,
            _h: &FileHandle,
            _o: u64,
            _d: &[u8],
        ) -> Result<usize, StorageError> {
            unreachable!()
        }
        async fn close_file(&self, _h: &FileHandle) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn stat(&self, _p: &str) -> Result<FileAttributes, StorageError> {
            unreachable!()
        }
        async fn readdir(&self, _p: &str) -> Result<Vec<DirEntry>, StorageError> {
            unreachable!()
        }
        async fn create_file(&self, _p: &str, _m: u32) -> Result<FileHandle, StorageError> {
            unreachable!()
        }
        async fn delete_file(&self, _p: &str) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn rename(&self, _f: &str, _t: &str) -> Result<(), StorageError> {
            unreachable!()
        }
    }
    Arc::new(Unused)
}

// ===========================================================================
// Fixture
// ===========================================================================

fn captured_events(bus: &EventBus) -> Arc<Mutex<Vec<DomainEvent>>> {
    let captured = Arc::new(Mutex::new(Vec::<DomainEvent>::new()));
    let mut rx = bus.subscribe();
    let captured_clone = captured.clone();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            captured_clone.lock().unwrap().push(event);
        }
    });
    captured
}

// ===========================================================================
// Working-tree helpers (populate the HostPath volume with a real repo
// so commit/push/diff have something to operate on).
// ===========================================================================

/// Initialise a real libgit2 working tree at the binding's HostPath
/// with a single `README.md` file + initial commit. Returns the target
/// dir. The repo has `user.name` / `user.email` configured so commit
/// signatures resolve even when the test env has no git config.
fn init_working_repo(workdir: &Path) {
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main");
    let repo = git2::Repository::init_opts(workdir, &opts).unwrap();

    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "B2 Fixture").unwrap();
    cfg.set_str("user.email", "b2@aegis.test").unwrap();

    std::fs::write(workdir.join("README.md"), b"initial\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = git2::Signature::now("B2 Fixture", "b2@aegis.test").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
        .unwrap();
}

/// Clone `url` as a bare repo into `target` (used to set up a local
/// bare remote the push test can target without the network).
fn clone_as_bare(url: &str, target: &Path) {
    let mut builder = git2::build::RepoBuilder::new();
    builder.bare(true);
    builder.clone(url, target).unwrap();
}

// -- fixture that exposes the binding repo for direct overwrite so --
// -- tests can mark bindings Ready after populating disk --

struct Fixture {
    service: GitRepoService,
    volume_repo: Arc<InMemoryVolumeRepository>,
    binding_repo: Arc<InMemoryGitRepoBindingRepository>,
    event_bus: Arc<EventBus>,
    _tmp: tempfile::TempDir,
}

fn build_fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let event_bus = Arc::new(EventBus::new(64));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());

    let mock_volume_service = Arc::new(MockVolumeService {
        repo: volume_repo.clone(),
        event_bus: event_bus.clone(),
        root: tmp.path().to_path_buf(),
    });

    let user_volume_service = Arc::new(UserVolumeService::new(
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        mock_volume_service as Arc<dyn VolumeService>,
        event_bus.clone(),
        aegis_orchestrator_core::domain::volume::StorageTierLimits::default(),
    ));

    let secrets_manager = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        event_bus.clone(),
    ));

    let fsal = Arc::new(AegisFSAL::new(
        stub_storage_provider(),
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoopPublisher),
    ));

    let clone_executor = Arc::new(GitCloneExecutor::new(
        secrets_manager.clone(),
        fsal,
        None::<Arc<EphemeralCliEngine>>,
    ));

    let binding_repo = Arc::new(InMemoryGitRepoBindingRepository::default());
    let binding_repo_trait = binding_repo.clone() as Arc<dyn GitRepoBindingRepository>;

    let service = GitRepoService::new(
        binding_repo_trait,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus.clone(),
    );

    Fixture {
        service,
        volume_repo,
        binding_repo,
        event_bus,
        _tmp: tmp,
    }
}

async fn workdir_of(fx: &Fixture, binding: &GitRepoBinding) -> PathBuf {
    let vol = fx
        .volume_repo
        .find_by_id(binding.volume_id)
        .await
        .unwrap()
        .unwrap();
    match vol.backend {
        VolumeBackend::HostPath { path } => path,
        other => panic!("test fixture must produce HostPath volumes, got {other:?}"),
    }
}

/// Create a binding, init a real git repo on the volume, transition
/// to Ready in memory and persist it back through the binding-repo
/// handle so the service observes `status == Ready`.
async fn ready_binding(fx: &Fixture, owner: &str, label: &str) -> (GitRepoBinding, PathBuf) {
    let mut binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            TenantId::consumer(),
            owner,
            ZaruTier::Pro,
            "https://github.com/a/canvas.git",
            label,
        ))
        .await
        .unwrap();

    let workdir = workdir_of(fx, &binding).await;
    init_working_repo(&workdir);

    binding.complete_clone("0".repeat(40), 0);
    // Drop the buffered events so tests only see what B2 emits next.
    binding.domain_events.clear();
    fx.binding_repo.save(&binding).await.unwrap();

    (binding, workdir)
}

// ===========================================================================
// Tests — commit
// ===========================================================================

#[tokio::test]
async fn commit_with_changes_emits_event() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();
    let events = captured_events(&fx.event_bus);

    let (binding, workdir) = ready_binding(&fx, "alice", "canvas-1").await;
    // Edit working tree.
    std::fs::write(workdir.join("README.md"), b"edited by alice\n").unwrap();

    let sha = fx
        .service
        .commit(
            &binding.id,
            &tenant,
            "alice",
            "chore: edit README",
            "Alice",
            "alice@aegis.test",
        )
        .await
        .expect("commit should succeed");

    assert_eq!(sha.len(), 40, "commit returned a 40-char hex SHA");

    // Give the broadcast bus a tick to flush.
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let got = events.lock().unwrap().clone();
    let has_commit_made = got.iter().any(|e| {
        matches!(
            e,
            DomainEvent::GitRepo(
                aegis_orchestrator_core::domain::events::GitRepoEvent::CommitMade { .. }
            )
        )
    });
    assert!(has_commit_made, "CommitMade not in event stream: {got:?}");
}

#[tokio::test]
async fn commit_with_no_changes_returns_nothing_to_commit() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();

    let (binding, _workdir) = ready_binding(&fx, "alice", "canvas-clean").await;

    let res = fx
        .service
        .commit(
            &binding.id,
            &tenant,
            "alice",
            "no-op",
            "Alice",
            "alice@aegis.test",
        )
        .await;
    assert!(
        matches!(res, Err(GitRepoError::NothingToCommit)),
        "expected NothingToCommit, got {res:?}"
    );
}

#[tokio::test]
async fn commit_wrong_owner_returns_not_found() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();

    let (binding, workdir) = ready_binding(&fx, "alice", "canvas-hidden").await;
    std::fs::write(workdir.join("README.md"), b"malicious\n").unwrap();

    let res = fx
        .service
        .commit(
            &binding.id,
            &tenant,
            "mallory",
            "steal",
            "Mallory",
            "mallory@evil.test",
        )
        .await;
    assert!(
        matches!(res, Err(GitRepoError::BindingNotFound)),
        "non-owner must see NotFound to avoid leaking existence: got {res:?}"
    );
}

// ===========================================================================
// Tests — push
// ===========================================================================

#[tokio::test]
async fn push_to_local_bare_remote_succeeds() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();
    let events = captured_events(&fx.event_bus);

    let (binding, workdir) = ready_binding(&fx, "alice", "canvas-push").await;

    // Build a local bare remote from the working repo's first commit,
    // then wire it as `origin` on the workdir so `git push` has a
    // real target. file://… URLs work with libgit2.
    let bare_dir = fx._tmp.path().join("fixture.git");
    let src_url = format!("file://{}", workdir.display());
    clone_as_bare(&src_url, &bare_dir);
    let bare_url = format!("file://{}", bare_dir.display());

    let repo = git2::Repository::open(&workdir).unwrap();
    // Remove pre-existing `origin` if any (none expected, but be defensive).
    let _ = repo.remote_delete("origin");
    repo.remote("origin", &bare_url).unwrap();

    // Edit + commit so the push has something beyond the bare's HEAD.
    std::fs::write(workdir.join("README.md"), b"post-clone edit\n").unwrap();
    let _ = fx
        .service
        .commit(
            &binding.id,
            &tenant,
            "alice",
            "edit README",
            "Alice",
            "alice@aegis.test",
        )
        .await
        .unwrap();

    fx.service
        .push(&binding.id, &tenant, "alice", None, None)
        .await
        .expect("push to local bare remote should succeed");

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let got = events.lock().unwrap().clone();
    let has_push_completed = got.iter().any(|e| {
        matches!(
            e,
            DomainEvent::GitRepo(
                aegis_orchestrator_core::domain::events::GitRepoEvent::PushCompleted { .. }
            )
        )
    });
    assert!(
        has_push_completed,
        "PushCompleted not in event stream: {got:?}"
    );

    // The bare repo's `refs/heads/main` should now point at the
    // workdir's HEAD.
    let bare = git2::Repository::open(&bare_dir).unwrap();
    let bare_head = bare
        .find_reference("refs/heads/main")
        .unwrap()
        .target()
        .unwrap()
        .to_string();
    let work_head = repo.head().unwrap().target().unwrap().to_string();
    assert_eq!(bare_head, work_head, "bare remote advanced to new HEAD");
}

#[tokio::test]
async fn push_with_bad_remote_fails_cleanly() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();

    let (binding, workdir) = ready_binding(&fx, "alice", "canvas-bad-remote").await;
    // Wire `origin` to a non-existent path — libgit2 refuses to resolve.
    let repo = git2::Repository::open(&workdir).unwrap();
    repo.remote("origin", "file:///aegis/does/not/exist/for/b2/tests.git")
        .unwrap();

    let res = fx
        .service
        .push(&binding.id, &tenant, "alice", None, None)
        .await;
    assert!(
        matches!(res, Err(GitRepoError::GitFailed(_))),
        "bad remote must surface as GitFailed, got {res:?}"
    );
}

// ===========================================================================
// Tests — diff
// ===========================================================================

#[tokio::test]
async fn diff_returns_unified_patch_for_workdir_change() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();

    let (binding, workdir) = ready_binding(&fx, "alice", "canvas-diff").await;
    std::fs::write(workdir.join("README.md"), b"changed content\n").unwrap();

    let diff = fx
        .service
        .diff(&binding.id, &tenant, "alice", false)
        .await
        .expect("workdir diff should succeed");

    assert!(!diff.is_empty(), "workdir diff must be non-empty");
    assert!(
        diff.contains("README.md"),
        "diff should mention the edited file: {diff}"
    );
    assert!(
        diff.contains("+changed content") || diff.contains("-initial"),
        "diff should include add/remove markers for the edited line: {diff}"
    );
}

#[tokio::test]
async fn diff_staged_differs_from_workdir_when_index_ahead() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();

    let (binding, workdir) = ready_binding(&fx, "alice", "canvas-diff-staged").await;

    // Stage a change (so index ≠ HEAD) then immediately edit again
    // (so workdir ≠ index). staged=true should surface the first
    // edit; staged=false should surface the second.
    std::fs::write(workdir.join("README.md"), b"staged change\n").unwrap();
    let repo = git2::Repository::open(&workdir).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    std::fs::write(workdir.join("README.md"), b"workdir overrides\n").unwrap();

    let staged_diff = fx
        .service
        .diff(&binding.id, &tenant, "alice", true)
        .await
        .unwrap();
    let workdir_diff = fx
        .service
        .diff(&binding.id, &tenant, "alice", false)
        .await
        .unwrap();

    assert!(!staged_diff.is_empty(), "staged diff is non-empty");
    assert!(!workdir_diff.is_empty(), "workdir diff is non-empty");
    assert_ne!(
        staged_diff, workdir_diff,
        "staged and workdir diffs must differ when index is ahead of workdir"
    );
    assert!(
        staged_diff.contains("staged change"),
        "staged diff should contain staged text: {staged_diff}"
    );
    assert!(
        workdir_diff.contains("workdir overrides"),
        "workdir diff should contain the latest edit: {workdir_diff}"
    );
}

// ===========================================================================
// API-boundary tests — confirm ownership gate pattern carried forward
// ===========================================================================

#[tokio::test]
async fn api_commit_wrong_owner_returns_not_found() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();
    let (binding, workdir) = ready_binding(&fx, "alice", "api-commit").await;
    std::fs::write(workdir.join("README.md"), b"edited\n").unwrap();

    let res = fx
        .service
        .commit(&binding.id, &tenant, "mallory", "pwn", "M", "m@evil.test")
        .await;
    assert!(matches!(res, Err(GitRepoError::BindingNotFound)));
}

#[tokio::test]
async fn api_push_wrong_owner_returns_not_found() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();
    let (binding, _) = ready_binding(&fx, "alice", "api-push").await;

    let res = fx
        .service
        .push(&binding.id, &tenant, "mallory", None, None)
        .await;
    assert!(matches!(res, Err(GitRepoError::BindingNotFound)));
}

#[tokio::test]
async fn api_diff_wrong_owner_returns_not_found() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();
    let (binding, _) = ready_binding(&fx, "alice", "api-diff").await;

    let res = fx
        .service
        .diff(&binding.id, &tenant, "mallory", false)
        .await;
    assert!(matches!(res, Err(GitRepoError::BindingNotFound)));
}

// ===========================================================================
// BindingBusy gate
// ===========================================================================

#[tokio::test]
async fn commit_on_non_ready_binding_rejects_as_binding_busy() {
    // A fresh binding is in Pending status — commit/push/diff must refuse.
    let fx = build_fixture();
    let tenant = TenantId::consumer();

    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            tenant.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/b.git",
            "pending",
        ))
        .await
        .unwrap();

    let res = fx
        .service
        .commit(&binding.id, &tenant, "alice", "x", "A", "a@aegis.test")
        .await;
    assert!(
        matches!(res, Err(GitRepoError::BindingBusy(_))),
        "Pending binding must reject commit with BindingBusy, got {res:?}"
    );
}
