// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `GitRepoService` Integration Tests (BC-7, ADR-081 Wave A2)
//!
//! Covers the A2 service-layer surface:
//!
//! | Scenario | Test |
//! |---|---|
//! | Tier-limit enforcement | `create_binding_rejects_over_tier_limit` |
//! | URL validator wired at service boundary | `create_binding_rejects_invalid_url` |
//! | Happy-path create succeeds | `create_binding_succeeds_under_tier_limit` |
//! | List only returns caller's bindings | `list_bindings_filters_by_owner` |
//! | `delete_binding` ownership gate | `delete_binding_refuses_non_owner` |
//! | `delete_binding` emits `BindingDeleted` | `delete_binding_emits_deleted_event` |
//! | `refresh_repo` A3 stub | `refresh_repo_returns_not_yet_implemented` |
//!
//! All tests use in-memory repositories and a mocked clone executor so
//! no external git server is required.

use std::collections::HashMap;
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
// In-memory GitRepoBindingRepository
// ===========================================================================

#[derive(Default)]
struct InMemoryGitRepoBindingRepository {
    bindings: RwLock<HashMap<GitRepoBindingId, GitRepoBinding>>,
}

#[async_trait]
impl GitRepoBindingRepository for InMemoryGitRepoBindingRepository {
    async fn save(&self, binding: &GitRepoBinding) -> Result<(), RepositoryError> {
        let mut map = self.bindings.write().unwrap();
        map.insert(binding.id, binding.clone());
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
        // Tenant-scoped per ADR-081 A1; owner filtering happens at the
        // service layer by cross-referencing the volume repo.
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
    async fn find_by_webhook_secret(
        &self,
        secret: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .find(|b| b.webhook_secret.as_deref() == Some(secret))
            .cloned())
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
// Mock VolumeService producing HostPath volumes (A2 executor target)
// ===========================================================================

struct MockVolumeService {
    repo: Arc<InMemoryVolumeRepository>,
    event_bus: Arc<EventBus>,
    /// Parent directory under which every created volume gets a
    /// dedicated subdir — lets clone tests resolve a real on-disk
    /// target.
    root: std::path::PathBuf,
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
        _volume_id: VolumeId,
        _instance_id: InstanceId,
        _mount_point: std::path::PathBuf,
        _access_mode: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        unimplemented!()
    }
    async fn detach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
    ) -> anyhow::Result<()> {
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
    async fn get_volume_usage(&self, _volume_id: VolumeId) -> anyhow::Result<u64> {
        Ok(0)
    }
    async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn create_volumes_for_execution(
        &self,
        _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        _tenant_id: TenantId,
        _volume_specs: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
        _storage_mode: &str,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn persist_external_volume(
        &self,
        _volume_id: VolumeId,
        _name: String,
        _tenant_id: TenantId,
        _remote_path: String,
        _size_limit_bytes: u64,
        _ownership: VolumeOwnership,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Noop FSAL publisher
// ===========================================================================

struct NoopPublisher;
#[async_trait]
impl EventPublisher for NoopPublisher {
    async fn publish_storage_event(
        &self,
        _e: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

// ===========================================================================
// Fixture
// ===========================================================================

struct Fixture {
    service: GitRepoService,
    event_bus: Arc<EventBus>,
    _tmp: tempfile::TempDir,
}

fn stub_storage_provider() -> Arc<dyn aegis_orchestrator_core::domain::storage::StorageProvider> {
    use aegis_orchestrator_core::domain::storage::{
        DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    struct Unused;
    #[async_trait]
    impl StorageProvider for Unused {
        async fn create_directory(&self, _path: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete_directory(&self, _path: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_quota(&self, _path: &str, _bytes: u64) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_usage(&self, _path: &str) -> Result<u64, StorageError> {
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

    let repo =
        Arc::new(InMemoryGitRepoBindingRepository::default()) as Arc<dyn GitRepoBindingRepository>;

    let service = GitRepoService::new(
        repo,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus.clone(),
    );

    Fixture {
        service,
        event_bus,
        _tmp: tmp,
    }
}

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

// Note: we don't drive the background clone in these tests — they
// exercise create/list/delete / error paths only. The full clone flow
// is covered by `git_clone_executor_tests.rs` and
// `git_repo_api_tests.rs`.

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn create_binding_succeeds_under_tier_limit() {
    let fx = build_fixture();
    let cmd = CreateGitRepoCommand::new(
        TenantId::consumer(),
        "user-1",
        ZaruTier::Pro,
        "https://github.com/octocat/Hello-World.git",
        "hello",
    );
    let binding = fx
        .service
        .create_binding(cmd)
        .await
        .expect("should succeed");
    assert_eq!(
        binding.repo_url,
        "https://github.com/octocat/Hello-World.git"
    );
    assert_eq!(binding.label, "hello");
}

#[tokio::test]
async fn create_binding_rejects_over_tier_limit() {
    let fx = build_fixture();
    // Free tier = 1 binding max.
    let first = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            TenantId::consumer(),
            "user-free",
            ZaruTier::Free,
            "https://github.com/a/b.git",
            "first",
        ))
        .await;
    assert!(first.is_ok());

    let second = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            TenantId::consumer(),
            "user-free",
            ZaruTier::Free,
            "https://github.com/a/c.git",
            "second",
        ))
        .await;
    assert!(
        matches!(second, Err(GitRepoError::TierLimitExceeded { max: 1 })),
        "got {second:?}"
    );
}

#[tokio::test]
async fn create_binding_rejects_invalid_url() {
    let fx = build_fixture();
    let res = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            TenantId::consumer(),
            "user-x",
            ZaruTier::Pro,
            "file:///tmp/attack.git", // rejected per ADR-081 §Security
            "bad",
        ))
        .await;
    assert!(
        matches!(res, Err(GitRepoError::UrlValidationFailed(_))),
        "got {res:?}"
    );
}

#[tokio::test]
async fn list_bindings_filters_by_owner() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    fx.service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/one.git",
            "alice-one",
        ))
        .await
        .unwrap();
    fx.service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "bob",
            ZaruTier::Pro,
            "https://github.com/b/two.git",
            "bob-one",
        ))
        .await
        .unwrap();

    let alice_bindings = fx.service.list_bindings(&t, "alice").await.unwrap();
    let bob_bindings = fx.service.list_bindings(&t, "bob").await.unwrap();

    assert_eq!(alice_bindings.len(), 1);
    assert_eq!(alice_bindings[0].label, "alice-one");
    assert_eq!(bob_bindings.len(), 1);
    assert_eq!(bob_bindings[0].label, "bob-one");
}

#[tokio::test]
async fn delete_binding_refuses_non_owner() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/one.git",
            "alice-one",
        ))
        .await
        .unwrap();

    let res = fx.service.delete_binding(&binding.id, &t, "mallory").await;
    assert!(
        matches!(res, Err(GitRepoError::BindingNotFound)),
        "non-owner must see NotFound, got {res:?}"
    );
}

#[tokio::test]
async fn delete_binding_emits_deleted_event() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let events = captured_events(&fx.event_bus);

    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/one.git",
            "alice-one",
        ))
        .await
        .unwrap();

    fx.service
        .delete_binding(&binding.id, &t, "alice")
        .await
        .expect("owner delete should succeed");

    // Give the event bus a moment to drain the tokio broadcast.
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    let got = events.lock().unwrap().clone();
    let has_deleted = got.iter().any(|e| {
        matches!(
            e,
            DomainEvent::GitRepo(
                aegis_orchestrator_core::domain::events::GitRepoEvent::BindingDeleted { .. }
            )
        )
    });
    assert!(
        has_deleted,
        "expected BindingDeleted in event stream: {got:?}"
    );
}

#[tokio::test]
async fn refresh_repo_returns_not_yet_implemented() {
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/one.git",
            "alice-one",
        ))
        .await
        .unwrap();

    let res = fx.service.refresh_repo(&binding.id, &t, "alice").await;
    assert!(
        matches!(res, Err(GitRepoError::NotYetImplemented(_))),
        "A2 refresh must stub; got {res:?}"
    );
}
