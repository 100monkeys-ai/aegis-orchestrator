// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding API-Boundary Tests (BC-7, ADR-081 Wave A2)
//!
//! These tests exercise the contract the HTTP handlers depend on —
//! error variants, ownership invariants, and redaction semantics — at
//! the [`GitRepoService`] boundary. They are intentionally
//! co-located with the domain / service tests rather than cli/tests so
//! they live alongside the service they validate.
//!
//! Handler-layer integration (JWT extraction, routing, serialization
//! shape) lives in `cli/src/daemon/handlers/git_repo.rs` and is covered
//! at the daemon smoke-test level.
//!
//! | HTTP behaviour | Covered here by |
//! |---|---|
//! | `POST /v1/storage/git` returns 201 and a redacted body | `post_create_returns_redacted_binding` |
//! | `GET /v1/storage/git` returns only caller's bindings | `get_list_tenant_scoped` |
//! | `GET /v1/storage/git/:id` with wrong owner → 404 | `get_binding_wrong_owner_returns_not_found` |
//! | `DELETE /v1/storage/git/:id` cleans up volume | `delete_binding_cleans_up_volume` |
//! | `POST /v1/storage/git/:id/refresh` → 404 for non-owner | `refresh_refuses_non_owner` |
//! | `POST /v1/webhooks/git/:secret` rejects bad hmac | `webhook_rejects_bad_signature` |
//! | `POST /v1/webhooks/git/:secret` accepts valid hmac | `webhook_accepts_valid_signature` |
//!
//! [`GitRepoService`]: aegis_orchestrator_core::application::git_repo_service::GitRepoService

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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
    GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository, GitRepoStatus,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::{RepositoryError, VolumeRepository};
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership, VolumeStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

// ===========================================================================
// In-memory GitRepoBindingRepository (duplicated from service_tests; keeps
// each test file self-contained and avoids a shared `tests/common/` module
// which Cargo treats as an integration target on its own).
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
        _volume_id: &VolumeId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(None)
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
// Mock VolumeService producing HostPath volumes (so service wiring works
// end-to-end without a live SeaweedFS).
// ===========================================================================

struct MockVolumeService {
    repo: Arc<InMemoryVolumeRepository>,
    event_bus: Arc<EventBus>,
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
        _vid: VolumeId,
        _iid: InstanceId,
        _m: std::path::PathBuf,
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

struct Fixture {
    service: GitRepoService,
    volume_repo: Arc<InMemoryVolumeRepository>,
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

    let binding_repo =
        Arc::new(InMemoryGitRepoBindingRepository::default()) as Arc<dyn GitRepoBindingRepository>;

    let service = GitRepoService::new(
        binding_repo,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus,
    );

    Fixture {
        service,
        volume_repo,
        _tmp: tmp,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn post_create_returns_redacted_binding() {
    // POST /v1/storage/git → 201 with a GitRepoBinding. The handler
    // wraps this in a `redacted_binding()` shape that omits the
    // webhook_secret; here we assert the aggregate returned from the
    // service carries no webhook_secret by default (so the handler's
    // redaction has nothing unsafe to redact).
    let fx = build_fixture();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            TenantId::consumer(),
            "user-1",
            ZaruTier::Pro,
            "https://github.com/a/b.git",
            "demo",
        ))
        .await
        .unwrap();

    assert_eq!(binding.status, GitRepoStatus::Pending);
    assert!(binding.webhook_secret.is_none());
    // The binding volume is wired and findable.
    assert!(fx
        .volume_repo
        .find_by_id(binding.volume_id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn get_list_tenant_scoped() {
    // GET /v1/storage/git → only the caller's bindings appear, even
    // when another user in the same tenant has bindings too.
    let fx = build_fixture();
    let t = TenantId::consumer();
    fx.service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/1.git",
            "a1",
        ))
        .await
        .unwrap();
    fx.service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "bob",
            ZaruTier::Pro,
            "https://github.com/b/1.git",
            "b1",
        ))
        .await
        .unwrap();

    let alice = fx.service.list_bindings(&t, "alice").await.unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].label, "a1");
}

#[tokio::test]
async fn get_binding_wrong_owner_returns_not_found() {
    // GET /v1/storage/git/:id with a different tenant-user must 404 —
    // never 403. Avoids leaking existence of other users' bindings.
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/1.git",
            "a1",
        ))
        .await
        .unwrap();

    let res = fx.service.get_binding(&binding.id, &t, "mallory").await;
    assert!(matches!(res, Err(GitRepoError::BindingNotFound)));
}

#[tokio::test]
async fn delete_binding_cleans_up_volume() {
    // DELETE /v1/storage/git/:id — after success, the backing volume
    // is gone from the repo.
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/1.git",
            "a1",
        ))
        .await
        .unwrap();

    let volume_id = binding.volume_id;
    assert!(fx
        .volume_repo
        .find_by_id(volume_id)
        .await
        .unwrap()
        .is_some());

    fx.service
        .delete_binding(&binding.id, &t, "alice")
        .await
        .unwrap();

    assert!(fx
        .volume_repo
        .find_by_id(volume_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn refresh_refuses_non_owner() {
    // POST /v1/storage/git/:id/refresh — non-owner sees 404 (same
    // ownership gate pattern as delete).
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            t.clone(),
            "alice",
            ZaruTier::Pro,
            "https://github.com/a/1.git",
            "a1",
        ))
        .await
        .unwrap();

    let res = fx.service.refresh_repo(&binding.id, &t, "mallory").await;
    assert!(matches!(res, Err(GitRepoError::BindingNotFound)));
}

#[tokio::test]
async fn webhook_rejects_bad_signature() {
    use aegis_orchestrator_core::application::git_repo_service::{WebhookAuth, WebhookProvider};
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand {
            tenant_id: t.clone(),
            owner: "alice".to_string(),
            zaru_tier: ZaruTier::Pro,
            credential_binding_id: None,
            repo_url: "https://github.com/a/1.git".to_string(),
            git_ref: Default::default(),
            sparse_paths: None,
            label: "a1".to_string(),
            auto_refresh: true,
            shallow: true,
        })
        .await
        .unwrap();
    let secret = binding.webhook_secret.clone().unwrap();
    let auth = WebhookAuth {
        provider: WebhookProvider::GitHub,
        signature: "sha256=deadbeef".to_string(),
    };
    let res = fx.service.handle_webhook(&secret, &auth, b"").await;
    assert!(matches!(res, Err(GitRepoError::WebhookRejected(_))));
}

#[tokio::test]
async fn webhook_accepts_valid_signature() {
    use aegis_orchestrator_core::application::git_repo_service::{WebhookAuth, WebhookProvider};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand {
            tenant_id: t.clone(),
            owner: "alice".to_string(),
            zaru_tier: ZaruTier::Pro,
            credential_binding_id: None,
            repo_url: "https://github.com/a/1.git".to_string(),
            git_ref: Default::default(),
            sparse_paths: None,
            label: "a1".to_string(),
            auto_refresh: true,
            shallow: true,
        })
        .await
        .unwrap();
    let secret = binding.webhook_secret.clone().unwrap();
    let body = b"payload";
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let auth = WebhookAuth {
        provider: WebhookProvider::GitHub,
        signature: sig,
    };
    // HMAC verification succeeds, refresh then fails because there's no
    // cloned tree — we only assert the authentication layer accepted it.
    let res = fx.service.handle_webhook(&secret, &auth, body).await;
    match res {
        Err(GitRepoError::WebhookRejected(m)) => {
            panic!("valid sig must not be rejected: {m}")
        }
        _ => {}
    }
}
