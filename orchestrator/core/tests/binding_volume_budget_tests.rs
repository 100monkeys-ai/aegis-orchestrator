// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A repository binding's clone volume and the chat-attachments budget
//!
//! A person's storage budget is split: the bindings' allowance (the tier's
//! clone volume size times its `max_bindings`) and the chat-attachments
//! budget (the rest). A binding's clone volume counts against the allowance,
//! never against the chat budget, so a person whose `chat-attachments`
//! volume was given the tier's whole budget can still create a binding: the
//! chat volume's stored size is shrunk to the chat budget first (AEGIS
//! ADR-136 Update G2c).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{
    CreateGitRepoCommand, GitRepoError, GitRepoService,
};
use aegis_orchestrator_core::application::user_volume_service::{
    UserVolumeService, CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES, CHAT_ATTACHMENTS_VOLUME_NAME,
};
use aegis_orchestrator_core::application::volume_manager::{
    CreateUserVolumeCommand, VolumeService,
};
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
    AccessMode, StorageClass, StorageTierLimits, Volume, VolumeBackend, VolumeMount,
    VolumeOwnership, VolumeStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

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
    async fn find_by_webhook_lookup_hash(
        &self,
        hash: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .find(|b| b.webhook_lookup_hash.as_deref() == Some(hash))
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

struct Fixture {
    service: GitRepoService,
    volumes: Arc<UserVolumeService>,
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
    let volumes = Arc::new(UserVolumeService::new(
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        mock_volume_service as Arc<dyn VolumeService>,
        event_bus.clone(),
        StorageTierLimits::default(),
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
    let service = GitRepoService::new(
        Arc::new(InMemoryGitRepoBindingRepository::default()) as Arc<dyn GitRepoBindingRepository>,
        volumes.clone(),
        clone_executor,
        secrets_manager,
        event_bus,
    );
    Fixture {
        service,
        volumes,
        volume_repo,
        _tmp: tmp,
    }
}

impl Fixture {
    /// Give `owner` a `chat-attachments` volume of `bytes`, as the sizing
    /// before this change allocated it (the tier's whole budget).
    async fn full_chat_volume(&self, owner: &str, tier: ZaruTier, bytes: u64) {
        self.volumes
            .create_volume(CreateUserVolumeCommand {
                tenant_id: TenantId::consumer(),
                owner_user_id: owner.to_string(),
                label: CHAT_ATTACHMENTS_VOLUME_NAME.to_string(),
                size_limit_bytes: bytes,
                zaru_tier: tier,
            })
            .await
            .expect("the chat-attachments volume as it was allocated");
    }

    async fn volume_named(&self, owner: &str, name: &str) -> Volume {
        self.volume_repo
            .find_by_owner(&TenantId::consumer(), owner)
            .await
            .unwrap()
            .into_iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("{owner} has no volume named {name}"))
    }

    async fn bind(
        &self,
        owner: &str,
        tier: ZaruTier,
        label: &str,
    ) -> Result<GitRepoBinding, GitRepoError> {
        self.service
            .create_binding(CreateGitRepoCommand::new(
                TenantId::consumer(),
                owner,
                tier,
                format!("https://github.com/octocat/{label}.git"),
                label,
            ))
            .await
    }
}

/// Pro: 10 GiB, 5 bindings of 500 MiB; the chat budget is the rest.
const PRO_CHAT_BUDGET: u64 = 10 * GIB - 5 * 500 * MIB;
/// Free: 500 MiB, 1 binding; the clone volume is half the budget.
const FREE_CLONE: u64 = 250 * MIB;
const FREE_CHAT_BUDGET: u64 = 500 * MIB - FREE_CLONE;

#[tokio::test]
async fn a_pro_person_with_a_full_chat_volume_creates_a_binding_and_the_chat_volume_is_shrunk() {
    let fx = build_fixture();
    fx.full_chat_volume("pro-1", ZaruTier::Pro, 10 * GIB).await;

    let binding = fx.bind("pro-1", ZaruTier::Pro, "hello").await;
    assert!(binding.is_ok(), "the binding was refused: {binding:?}");

    let chat = fx.volume_named("pro-1", CHAT_ATTACHMENTS_VOLUME_NAME).await;
    assert_eq!(chat.size_limit_bytes, PRO_CHAT_BUDGET);
    let clone = fx.volume_named("pro-1", "git-hello").await;
    assert_eq!(clone.size_limit_bytes, 500 * MIB);
}

#[tokio::test]
async fn a_free_person_with_a_full_chat_volume_creates_a_binding_and_the_chat_volume_is_shrunk() {
    let fx = build_fixture();
    fx.full_chat_volume("free-1", ZaruTier::Free, 500 * MIB)
        .await;

    let binding = fx.bind("free-1", ZaruTier::Free, "hello").await;
    assert!(binding.is_ok(), "the binding was refused: {binding:?}");

    let chat = fx
        .volume_named("free-1", CHAT_ATTACHMENTS_VOLUME_NAME)
        .await;
    assert_eq!(chat.size_limit_bytes, FREE_CHAT_BUDGET);
    let clone = fx.volume_named("free-1", "git-hello").await;
    assert_eq!(clone.size_limit_bytes, FREE_CLONE);
}

#[tokio::test]
async fn a_second_binding_changes_the_chat_volume_no_further() {
    let fx = build_fixture();
    fx.full_chat_volume("pro-2", ZaruTier::Pro, 10 * GIB).await;
    fx.bind("pro-2", ZaruTier::Pro, "first")
        .await
        .expect("the first binding");
    let after_first = fx.volume_named("pro-2", CHAT_ATTACHMENTS_VOLUME_NAME).await;

    fx.bind("pro-2", ZaruTier::Pro, "second")
        .await
        .expect("the second binding");
    let after_second = fx.volume_named("pro-2", CHAT_ATTACHMENTS_VOLUME_NAME).await;

    assert_eq!(after_first.size_limit_bytes, PRO_CHAT_BUDGET);
    assert_eq!(after_second.size_limit_bytes, after_first.size_limit_bytes);
}

#[tokio::test]
async fn a_person_at_max_bindings_is_refused_with_the_existing_sentence() {
    let fx = build_fixture();
    fx.full_chat_volume("pro-3", ZaruTier::Pro, 10 * GIB).await;
    for i in 0..5 {
        fx.bind("pro-3", ZaruTier::Pro, &format!("repo-{i}"))
            .await
            .unwrap_or_else(|e| panic!("binding {i} was refused: {e}"));
    }
    let sixth = fx.bind("pro-3", ZaruTier::Pro, "repo-5").await;
    match sixth {
        Err(e @ GitRepoError::TierLimitExceeded { max: 5 }) => assert_eq!(
            e.to_string(),
            "tier limit exceeded: 5 bindings allowed for this tier"
        ),
        other => panic!("the sixth binding was not refused by the tier limit: {other:?}"),
    }
}

#[tokio::test]
async fn a_new_chat_volume_is_provisioned_at_the_chat_budget() {
    let fx = build_fixture();
    let tenant = TenantId::consumer();
    for (owner, tier, budget) in [
        ("free-4", ZaruTier::Free, FREE_CHAT_BUDGET),
        ("pro-4", ZaruTier::Pro, PRO_CHAT_BUDGET),
        (
            "ent-4",
            ZaruTier::Enterprise,
            CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES,
        ),
    ] {
        fx.volumes
            .find_or_provision_chat_attachments(&tenant, owner, &tier)
            .await
            .unwrap_or_else(|e| panic!("{owner}: {e}"));
        let chat = fx.volume_named(owner, CHAT_ATTACHMENTS_VOLUME_NAME).await;
        assert_eq!(chat.size_limit_bytes, budget, "{owner}");
    }
    // Then a binding fits beside it, with nothing shrunk.
    fx.bind("pro-4", ZaruTier::Pro, "hello")
        .await
        .expect("a binding beside a chat volume at the budget");
    let chat = fx.volume_named("pro-4", CHAT_ATTACHMENTS_VOLUME_NAME).await;
    assert_eq!(chat.size_limit_bytes, PRO_CHAT_BUDGET);
}
