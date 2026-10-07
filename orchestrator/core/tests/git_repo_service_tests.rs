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

use aegis_orchestrator_core::application::credential_service::CredentialError;
use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{
    CreateGitRepoCommand, GitRepoError, GitRepoService, OAuthAccessTokens,
};
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::events::VolumeEvent;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::{RepositoryError, VolumeRepository};
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::secrets::{SecretPath, SecretStore, SensitiveString};
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

// ===========================================================================
// Fixture
// ===========================================================================

struct Fixture {
    service: GitRepoService,
    event_bus: Arc<EventBus>,
    volume_repo: Arc<InMemoryVolumeRepository>,
    repo: Arc<dyn GitRepoBindingRepository>,
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
    build_fixture_with(Arc::new(TestSecretStore::new()), None, None)
}

/// The fixture with a credential-binding repository, an access-token
/// reader and the secret store the service reads credentials from.
fn build_fixture_with(
    store: Arc<TestSecretStore>,
    credentials: Option<Arc<Bindings>>,
    tokens: Option<Arc<dyn OAuthAccessTokens>>,
) -> Fixture {
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

    let secrets_manager = Arc::new(SecretsManager::from_store(store, event_bus.clone()));

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

    let mut service = GitRepoService::new(
        repo.clone(),
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus.clone(),
    );
    if let Some(credentials) = credentials {
        service = service.with_credential_repo(credentials as Arc<dyn CredentialBindingRepository>);
    }
    if let Some(tokens) = tokens {
        service = service.with_access_tokens(tokens);
    }

    Fixture {
        service,
        event_bus,
        volume_repo,
        repo,
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
        binding.repo_url.expose(),
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

/// An SSH repository's host key is checked on every clone, so a binding to
/// an SSH host must have one: GitHub, GitLab and Bitbucket have their
/// published keys; any other host must be given its key, and a binding
/// without one is refused with a sentence that says what to add.
#[tokio::test]
async fn create_binding_needs_the_host_key_of_an_ssh_host() {
    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfuCHKVTjquxvt6CM6tdG4SLp1Btn/nOeHHE5UOzRdf";
    let fx = build_fixture();
    // Each binding gets a volume named after its label, so each needs its own.
    let made = std::cell::Cell::new(0);
    let command = |url: &str, keys: &[&str]| {
        made.set(made.get() + 1);
        let mut cmd = CreateGitRepoCommand::new(
            TenantId::consumer(),
            "user-ssh",
            ZaruTier::Enterprise,
            url,
            format!("ssh-{}", made.get()),
        );
        cmd.ssh_host_keys = keys.iter().map(|k| k.to_string()).collect();
        cmd
    };

    let refused = fx
        .service
        .create_binding(command("git@git.example.invalid:o/r.git", &[]))
        .await;
    let message = match refused {
        Ok(_) => panic!("an SSH binding to a host with no known key was created"),
        Err(e) => e.to_string(),
    };
    assert!(
        message.contains("git.example.invalid") && message.contains("ssh_host_keys"),
        "the refusal does not say what to add: {message}"
    );

    for (url, keys, why) in [
        (
            "git@git.example.invalid:o/r.git",
            &["ssh-ed25519 not-a-key"][..],
            "a host key that is not one",
        ),
        (
            "https://git.example.invalid/o/r.git",
            &[KEY][..],
            "a host key for an HTTPS repository",
        ),
    ] {
        assert!(
            fx.service.create_binding(command(url, keys)).await.is_err(),
            "a binding with {why} was created"
        );
    }

    let given = fx
        .service
        .create_binding(command("git@git.example.invalid:o/r.git", &[KEY]))
        .await
        .expect("an SSH binding with its host key is created");
    assert_eq!(
        given
            .ssh_host_keys
            .iter()
            .map(|k| k.to_line())
            .collect::<Vec<_>>(),
        vec![KEY.to_string()],
        "the binding holds the key it was given"
    );
    let known = fx
        .service
        .create_binding(command("git@github.com:o/r.git", &[]))
        .await
        .expect("an SSH binding to GitHub needs no key");
    assert!(known.ssh_host_keys.is_empty());
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
async fn refresh_repo_fails_without_cloned_volume() {
    // In A3 refresh_repo is implemented, but a freshly-created binding
    // has no cloned working tree yet — fetch_and_checkout against an
    // empty directory returns CloneError::Git, which the service maps to
    // CloneFailed. (The full happy-path is covered by
    // git_clone_executor_tests::fetch_and_checkout_branch_fast_forwards.)
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
        matches!(res, Err(GitRepoError::CloneFailed(_))),
        "refresh against an uncloned binding should surface CloneFailed; got {res:?}"
    );
}

#[tokio::test]
async fn refresh_repo_refuses_non_owner() {
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

    let res = fx.service.refresh_repo(&binding.id, &t, "mallory").await;
    assert!(
        matches!(res, Err(GitRepoError::BindingNotFound)),
        "non-owner refresh must 404, got {res:?}"
    );
}

#[tokio::test]
async fn handle_webhook_rejects_unknown_secret() {
    use aegis_orchestrator_core::application::git_repo_service::{WebhookAuth, WebhookProvider};
    let fx = build_fixture();
    let auth = WebhookAuth {
        provider: WebhookProvider::GitLab,
        signature: "anything".to_string(),
    };
    let res = fx
        .service
        .handle_webhook("no-such-secret", &auth, b"")
        .await;
    assert!(
        matches!(res, Err(GitRepoError::WebhookRejected(_))),
        "unknown secret should 401, got {res:?}"
    );
}

#[tokio::test]
async fn handle_webhook_rejects_bad_signature() {
    use aegis_orchestrator_core::application::git_repo_service::{WebhookAuth, WebhookProvider};
    // Direct manipulation: insert a binding with a webhook_secret and
    // then submit a webhook with the wrong signature.
    let fx = build_fixture();
    let t = TenantId::consumer();
    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand {
            tenant_id: t.clone(),
            owner: "alice".to_string(),
            zaru_tier: ZaruTier::Pro,
            credential_binding_id: None,
            repo_url: "https://github.com/a/one.git".into(),
            git_ref: Default::default(),
            sparse_paths: None,
            label: "alice-one".to_string(),
            auto_refresh: true,
            shallow: true,
            ssh_host_keys: Vec::new(),
        })
        .await
        .unwrap();

    let secret = binding
        .webhook_secret
        .clone()
        .expect("auto_refresh → secret")
        .expose_owned();

    let auth = WebhookAuth {
        provider: WebhookProvider::GitHub,
        signature: "sha256=deadbeef".to_string(),
    };
    let res = fx.service.handle_webhook(&secret, &auth, b"payload").await;
    assert!(
        matches!(res, Err(GitRepoError::WebhookRejected(_))),
        "bad sig should 401, got {res:?}"
    );
}

#[tokio::test]
async fn handle_webhook_accepts_valid_github_signature() {
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
            repo_url: "https://github.com/a/one.git".into(),
            git_ref: Default::default(),
            sparse_paths: None,
            label: "alice-one".to_string(),
            auto_refresh: true,
            shallow: true,
            ssh_host_keys: Vec::new(),
        })
        .await
        .unwrap();
    let secret = binding.webhook_secret.clone().unwrap().expose_owned();

    let body = b"{\"ref\":\"refs/heads/main\"}";
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let sig_hex = hex::encode(mac.finalize().into_bytes());
    let auth = WebhookAuth {
        provider: WebhookProvider::GitHub,
        signature: format!("sha256={sig_hex}"),
    };

    // Signature is valid; refresh will then fail because there's no
    // cloned working tree yet — we only assert that the error is NOT
    // WebhookRejected (i.e. authentication passed).
    let res = fx.service.handle_webhook(&secret, &auth, body).await;
    match res {
        Err(GitRepoError::CloneFailed(_))
        | Err(GitRepoError::VolumeProvisioningFailed(_))
        | Ok(()) => {} // all acceptable — hmac verification succeeded
        Err(GitRepoError::WebhookRejected(m)) => {
            panic!("valid signature should NOT be rejected, got: {m}");
        }
        Err(other) => panic!("unexpected error variant: {other:?}"),
    }
}

/// Audit 002 §4.37.13 regression — `git_repo_bindings.webhook_secret` was
/// stored as cleartext, exposing every binding to a single DB-only
/// compromise. After the fix:
///
/// 1. The persisted aggregate carries `webhook_secret_ciphertext` (Transit
///    ciphertext) and `webhook_lookup_hash` (deterministic SHA-256 of the
///    cleartext) — never the cleartext itself.
/// 2. The cleartext is returned to the caller in-memory (so the user can
///    configure their git provider) but a re-load from the repository
///    surfaces `webhook_secret == None`.
/// 3. The webhook verification path uses the lookup hash to find the
///    binding and the ciphertext (decrypted on demand) to recompute HMAC.
#[tokio::test]
async fn webhook_secret_is_persisted_as_ciphertext_plus_lookup_hash_only() {
    let fx = build_fixture();
    let t = TenantId::consumer();

    let binding = fx
        .service
        .create_binding(CreateGitRepoCommand {
            tenant_id: t.clone(),
            owner: "alice".to_string(),
            zaru_tier: ZaruTier::Pro,
            credential_binding_id: None,
            repo_url: "https://github.com/a/audit13.git".into(),
            git_ref: Default::default(),
            sparse_paths: None,
            label: "audit13".to_string(),
            auto_refresh: true,
            shallow: true,
            ssh_host_keys: Vec::new(),
        })
        .await
        .unwrap();

    // Cleartext is returned to the caller (so they can configure the
    // webhook in their git provider).
    let cleartext = binding
        .webhook_secret
        .clone()
        .expect("auto_refresh → cleartext returned to caller")
        .expose_owned();

    // Persistent state carries the ciphertext + hash, NOT the cleartext.
    assert!(
        binding.webhook_secret_ciphertext.is_some(),
        "ciphertext column must be populated when auto_refresh is on"
    );
    assert!(
        binding.webhook_lookup_hash.is_some(),
        "lookup-hash column must be populated when auto_refresh is on"
    );
    assert_ne!(
        binding.webhook_secret_ciphertext.as_deref(),
        Some(cleartext.as_str()),
        "ciphertext MUST differ from cleartext — the audit fix is moot otherwise"
    );
    assert_ne!(
        binding.webhook_lookup_hash.as_deref(),
        Some(cleartext.as_str()),
        "lookup hash MUST differ from cleartext (it is a SHA-256 digest)"
    );

    // Re-load from the repository (via the service): cleartext field is wiped.
    let reloaded = fx
        .service
        .get_binding(&binding.id, &t, "alice")
        .await
        .expect("binding persisted");
    assert!(
        reloaded.webhook_secret.is_none(),
        "DB hydration MUST NOT carry the cleartext — it is a transient field, \
         wiped on reload to enforce the at-rest contract"
    );
    assert!(
        reloaded.webhook_secret_ciphertext.is_some(),
        "ciphertext survives reload"
    );
    assert!(
        reloaded.webhook_lookup_hash.is_some(),
        "lookup hash survives reload"
    );

    // Webhook verification still works end-to-end: the URL header carries
    // the cleartext, the service hashes it to find the binding, decrypts
    // to recover the cleartext, and validates HMAC.
    use aegis_orchestrator_core::application::git_repo_service::{WebhookAuth, WebhookProvider};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let body = b"audit-13-payload";
    let mut mac = Hmac::<Sha256>::new_from_slice(cleartext.as_bytes()).unwrap();
    mac.update(body);
    let sig_hex = hex::encode(mac.finalize().into_bytes());
    let auth = WebhookAuth {
        provider: WebhookProvider::GitHub,
        signature: format!("sha256={sig_hex}"),
    };
    let res = fx.service.handle_webhook(&cleartext, &auth, body).await;
    // Authentication MUST pass; the binding has no working tree yet so
    // the refresh side may fail — that's not the concern of this test.
    assert!(
        !matches!(res, Err(GitRepoError::WebhookRejected(_))),
        "valid signature MUST NOT be rejected after the encrypt-at-rest \
         migration; got {res:?}"
    );

    // A bogus secret (not registered) MUST be rejected — the lookup
    // hash will not match any binding.
    let res_bogus = fx
        .service
        .handle_webhook("000000000000000000000000bogus000", &auth, body)
        .await;
    assert!(
        matches!(res_bogus, Err(GitRepoError::WebhookRejected(_))),
        "unknown secret MUST be rejected by lookup-hash miss; got {res_bogus:?}"
    );
}

// ===========================================================================
// A git binding may name only its owner's active credential (AEGIS ADR-136
// G1b, G2, G2a to G2d)
// ===========================================================================

const SENTENCE: &str =
    "The credential named for this repository is not an active credential of yours.";
const OWNER: &str = "git-owner-sub";
const OTHER: &str = "another-person-sub";
const DECOY_VALUE: &str = "Mk12-decoy-stored-value";
const ANSWERED_TOKEN: &str = "Mk12-answered-access-token";

#[derive(Default)]
struct Bindings(RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

#[async_trait]
impl CredentialBindingRepository for Bindings {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.0.write().unwrap().insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.0.read().unwrap().get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self
            .0
            .read()
            .unwrap()
            .values()
            .filter(|b| &b.tenant_id == tenant_id && b.owner_user_id == owner_user_id)
            .cloned()
            .collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _: &TenantId,
        _: &str,
        _: &CredentialProvider,
        _: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.0.write().unwrap().remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        _: &str,
        _: &CredentialBindingId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(
        &self,
        _: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<u64> {
        Ok(0)
    }
}

/// What the stub access-token reader answers.
enum TokenAnswer {
    Token,
    /// The refresh was answered `invalid_grant`: the binding is now Expired.
    CannotRefresh,
    /// The binding holds no OAuth token at all.
    NoToken,
}

/// Records every binding it is asked about and answers as told.
struct RecordingTokens {
    answer: TokenAnswer,
    asked: Mutex<Vec<CredentialBindingId>>,
}

impl RecordingTokens {
    fn answering(answer: TokenAnswer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl OAuthAccessTokens for RecordingTokens {
    async fn access_token_for(
        &self,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<SensitiveString> {
        self.asked.lock().unwrap().push(*binding_id);
        match self.answer {
            TokenAnswer::Token => Ok(SensitiveString::new(ANSWERED_TOKEN)),
            TokenAnswer::CannotRefresh => Err(CredentialError::OAuthExchangeFailed {
                error: "invalid_grant".to_string(),
                description: None,
            }
            .into()),
            TokenAnswer::NoToken => Err(CredentialError::NoAccessToken {
                binding_id: binding_id.to_string(),
            }
            .into()),
        }
    }
}

/// A credential binding of `owner` in the consumer tenant, its secret
/// stored with a decoy `value`.
async fn credential(
    credentials: &Bindings,
    store: &TestSecretStore,
    owner: &str,
    credential_type: CredentialType,
    status: CredentialStatus,
) -> CredentialBindingId {
    let id = CredentialBindingId::new();
    let tenant = TenantId::consumer();
    let now = chrono::Utc::now();
    let secret_path = SecretPath::for_tenant(
        tenant.clone(),
        "kv",
        format!("users/{owner}/credentials/{}", id.0),
    );
    let mut secret = HashMap::new();
    secret.insert("value".to_string(), SensitiveString::new(DECOY_VALUE));
    store
        .write(&secret_path.effective_mount(), &secret_path.path, secret)
        .await
        .expect("store the secret");
    credentials
        .save(&UserCredentialBinding {
            id,
            owner_user_id: owner.to_string(),
            tenant_id: tenant,
            credential_type,
            provider: CredentialProvider::new("github"),
            secret_path,
            scope: CredentialScope::Personal,
            status,
            metadata: CredentialMetadata {
                label: "GitHub".to_string(),
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox: None,
                reach: None,
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("save the credential binding");
    id
}

struct CredentialFixture {
    fx: Fixture,
    credentials: Arc<Bindings>,
    store: Arc<TestSecretStore>,
}

fn credential_fixture(tokens: Option<Arc<dyn OAuthAccessTokens>>) -> CredentialFixture {
    let store = Arc::new(TestSecretStore::new());
    let credentials = Arc::new(Bindings::default());
    let fx = build_fixture_with(store.clone(), Some(credentials.clone()), tokens);
    CredentialFixture {
        fx,
        credentials,
        store,
    }
}

fn create_command(credential: CredentialBindingId) -> CreateGitRepoCommand {
    let mut cmd = CreateGitRepoCommand::new(
        TenantId::consumer(),
        OWNER,
        ZaruTier::Pro,
        "https://git.example.invalid/o/r.git",
        format!("repo-{}", GitRepoBindingId::new().0.simple()),
    );
    cmd.credential_binding_id = Some(credential);
    cmd
}

/// A binding of `OWNER` created with no credential, then pointed at
/// `credential` in the store, as a binding created before the check was
/// built would be; `ready` marks it cloned so commit and push reach it.
async fn binding_naming(
    fx: &Fixture,
    credential: CredentialBindingId,
    ready: bool,
) -> GitRepoBinding {
    // Each binding's volume is named after its label, so each gets its own.
    let label = format!("repo-{}", GitRepoBindingId::new().0.simple());
    let mut binding = fx
        .service
        .create_binding(CreateGitRepoCommand::new(
            TenantId::consumer(),
            OWNER,
            ZaruTier::Enterprise,
            "https://git.example.invalid/o/r.git",
            label,
        ))
        .await
        .expect("a binding with no credential is created");
    binding.credential_binding_id = Some(credential);
    if ready {
        binding.complete_clone("0".repeat(40), 1);
    }
    let _ = binding.take_events();
    fx.repo.save(&binding).await.unwrap();
    binding
}

/// `None` when `result` is the refusal; else the failure, in words.
fn not_yours<T: std::fmt::Debug>(what: &str, result: Result<T, GitRepoError>) -> Option<String> {
    match result {
        Err(GitRepoError::CredentialNotYours) => None,
        other => Some(format!(
            "{what}: expected the refusal \"{SENTENCE}\", got {other:?}"
        )),
    }
}

fn assert_not_yours<T: std::fmt::Debug>(what: &str, result: Result<T, GitRepoError>) {
    if let Some(failure) = not_yours(what, result) {
        panic!("{failure}");
    }
}

fn assert_none_failed(failures: Vec<Option<String>>) {
    let failures: Vec<String> = failures.into_iter().flatten().collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// G2, G2a, G2b: another person's active credential, a missing one and the
/// caller's own inactive one are refused at create with the sentence, and
/// nothing is provisioned.
#[tokio::test]
async fn create_refuses_a_credential_that_is_not_an_active_credential_of_the_callers() {
    let cf = credential_fixture(None);
    let others = credential(
        &cf.credentials,
        &cf.store,
        OTHER,
        CredentialType::Secret,
        CredentialStatus::Active,
    )
    .await;
    let own_revoked = credential(
        &cf.credentials,
        &cf.store,
        OWNER,
        CredentialType::Secret,
        CredentialStatus::Revoked,
    )
    .await;
    let own_expired = credential(
        &cf.credentials,
        &cf.store,
        OWNER,
        CredentialType::OAuth2,
        CredentialStatus::Expired,
    )
    .await;
    let missing = CredentialBindingId::new();

    let mut failures = Vec::new();
    for (what, id) in [
        ("another person's active credential", others),
        ("a missing credential", missing),
        ("the caller's own revoked credential", own_revoked),
        ("the caller's own expired credential", own_expired),
    ] {
        match cf.fx.service.create_binding(create_command(id)).await {
            Err(e @ GitRepoError::CredentialNotYours) => {
                if e.to_string() != SENTENCE {
                    failures.push(format!("{what}: refused with \"{e}\", not \"{SENTENCE}\""));
                }
            }
            other => failures.push(format!(
                "{what}: expected the refusal \"{SENTENCE}\" at create, got {other:?}"
            )),
        }
    }
    let volumes = cf
        .fx
        .volume_repo
        .find_by_tenant(TenantId::consumer())
        .await
        .unwrap();
    if !volumes.is_empty() {
        failures.push(format!(
            "a refused create provisioned {} volume(s)",
            volumes.len()
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// G2: the caller's own active credential is admitted at create.
#[tokio::test]
async fn create_admits_the_callers_own_active_credential() {
    let cf = credential_fixture(None);
    let own = credential(
        &cf.credentials,
        &cf.store,
        OWNER,
        CredentialType::Secret,
        CredentialStatus::Active,
    )
    .await;
    let binding = cf
        .fx
        .service
        .create_binding(create_command(own))
        .await
        .expect("the caller's own active credential is admitted");
    assert_eq!(binding.credential_binding_id, Some(own));
}

/// G2b: with no credential repository wired, a binding naming a credential
/// keeps today's not-implemented refusal.
#[tokio::test]
async fn create_naming_a_credential_with_no_credential_repository_is_not_implemented() {
    let fx = build_fixture();
    let res = fx
        .service
        .create_binding(create_command(CredentialBindingId::new()))
        .await;
    assert!(
        matches!(res, Err(GitRepoError::NotYetImplemented(_))),
        "expected NotYetImplemented with no credential repository, got {res:?}"
    );
}

/// Clone, refresh, commit and push of four bindings of `OWNER` naming
/// `credential`, each answer checked for the refusal; every failure kept.
async fn clone_refresh_commit_and_push(
    fx: &Fixture,
    credential: CredentialBindingId,
) -> Vec<Option<String>> {
    let tenant = TenantId::consumer();
    let cloning = binding_naming(fx, credential, false).await;
    let refreshing = binding_naming(fx, credential, true).await;
    let committing = binding_naming(fx, credential, true).await;
    let pushing = binding_naming(fx, credential, true).await;
    vec![
        not_yours("clone", fx.service.clone_repo(&cloning.id).await),
        not_yours(
            "refresh",
            fx.service
                .refresh_repo(&refreshing.id, &tenant, OWNER)
                .await,
        ),
        not_yours(
            "commit",
            fx.service
                .commit(&committing.id, &tenant, OWNER, "m", "A", "a@example.com")
                .await,
        ),
        not_yours(
            "push",
            fx.service
                .push(&pushing.id, &tenant, OWNER, None, Some("main"))
                .await,
        ),
    ]
}

/// G2: another person's active credential is refused at clone, refresh,
/// commit and push, before any git operation.
#[tokio::test]
async fn clone_refresh_commit_and_push_refuse_another_persons_credential() {
    let cf = credential_fixture(None);
    let others = credential(
        &cf.credentials,
        &cf.store,
        OTHER,
        CredentialType::Secret,
        CredentialStatus::Active,
    )
    .await;
    assert_none_failed(clone_refresh_commit_and_push(&cf.fx, others).await);
}

/// G2: a missing credential and the caller's own inactive one are refused
/// at clone, refresh, commit and push the same way.
#[tokio::test]
async fn clone_refresh_commit_and_push_refuse_a_missing_or_inactive_credential() {
    let cf = credential_fixture(None);
    let revoked = credential(
        &cf.credentials,
        &cf.store,
        OWNER,
        CredentialType::Secret,
        CredentialStatus::Revoked,
    )
    .await;
    let mut failures = clone_refresh_commit_and_push(&cf.fx, revoked).await;
    failures.extend(clone_refresh_commit_and_push(&cf.fx, CredentialBindingId::new()).await);
    assert_none_failed(failures);
}

/// G2: the caller's own active credential is admitted at clone, which goes
/// on to the git host.
#[tokio::test]
async fn clone_admits_the_callers_own_active_credential() {
    let cf = credential_fixture(None);
    let own = credential(
        &cf.credentials,
        &cf.store,
        OWNER,
        CredentialType::Secret,
        CredentialStatus::Active,
    )
    .await;
    let cloning = binding_naming(&cf.fx, own, false).await;
    let res = cf.fx.service.clone_repo(&cloning.id).await;
    assert!(
        matches!(res, Err(GitRepoError::CloneFailed(_))),
        "the caller's own active credential was not admitted to the clone (the host does not resolve, so it fails there): {res:?}"
    );
}

// ---------------------------------------------------------------------------
// A loopback git host that asks for Basic credentials and records them
// ---------------------------------------------------------------------------

/// Answers the first request with `401` and a Basic challenge, and every
/// request that carries `Authorization` with `404`, recording the decoded
/// `user:password` of each. Returns the address and the record.
async fn basic_auth_recorder() -> (String, Arc<Mutex<Vec<String>>>, Arc<Mutex<usize>>) {
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let requests = Arc::new(Mutex::new(0usize));
    let (seen_bg, requests_bg) = (seen.clone(), requests.clone());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (seen, requests) = (seen_bg.clone(), requests_bg.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                *requests.lock().unwrap() += 1;
                let head = String::from_utf8_lossy(&buf).to_string();
                let auth = head.lines().find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_string())
                });
                let response: &[u8] = match auth {
                    Some(value) => {
                        let decoded = value
                            .strip_prefix("Basic ")
                            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                            .map(|b| String::from_utf8_lossy(&b).to_string())
                            .unwrap_or(value);
                        seen.lock().unwrap().push(decoded);
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    }
                    None => b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                };
                let _ = socket.write_all(response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/o/r.git"), seen, requests)
}

/// An OAuth binding of `OWNER`'s whose clone goes to the loopback host.
async fn oauth_binding_to(
    cf: &CredentialFixture,
    url: &str,
) -> (GitRepoBinding, CredentialBindingId) {
    let oauth = credential(
        &cf.credentials,
        &cf.store,
        OWNER,
        CredentialType::OAuth2,
        CredentialStatus::Active,
    )
    .await;
    let mut binding = binding_naming(&cf.fx, oauth, false).await;
    // The create path admits only https remotes; the loopback host is
    // plain http, so the stored binding is pointed at it directly.
    binding.repo_url = aegis_orchestrator_core::domain::secrets::SensitiveUrl::new(url);
    cf.fx.repo.save(&binding).await.unwrap();
    (binding, oauth)
}

/// G1b, G2d: an OAuth binding's clone carries the access token the
/// credential service answers, never the stored `value`.
#[tokio::test]
async fn an_oauth_bindings_clone_carries_the_answered_access_token_never_the_stored_value() {
    let tokens = RecordingTokens::answering(TokenAnswer::Token);
    let cf = credential_fixture(Some(tokens.clone() as Arc<dyn OAuthAccessTokens>));
    let (url, seen, _) = basic_auth_recorder().await;
    let (binding, oauth) = oauth_binding_to(&cf, &url).await;

    let res = cf.fx.service.clone_repo(&binding.id).await;

    let mut failures = Vec::new();
    if !matches!(res, Err(GitRepoError::CloneFailed(_))) {
        failures.push(format!(
            "the loopback host answers 404 after the credential, so the clone fails: {res:?}"
        ));
    }
    let asked = tokens.asked.lock().unwrap().clone();
    if asked != vec![oauth] {
        failures.push(format!(
            "the access-token reader was not asked for the binding's token: asked {asked:?}"
        ));
    }
    let seen = seen.lock().unwrap().clone();
    if seen.is_empty() {
        failures.push("the clone sent no credential to the git host".to_string());
    }
    for sent in &seen {
        if sent.contains(DECOY_VALUE) {
            failures.push(format!(
                "the clone sent the stored value, not the answered access token: {sent}"
            ));
        } else if sent != &format!("x-access-token:{ANSWERED_TOKEN}") {
            failures.push(format!(
                "the clone sent a credential other than the answered access token: {sent}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// G2c: a binding whose token can no longer be refreshed is refused at
/// once with the sentence, and nothing reaches the git host.
#[tokio::test]
async fn an_oauth_binding_that_cannot_be_refreshed_is_refused_at_once() {
    let tokens = RecordingTokens::answering(TokenAnswer::CannotRefresh);
    let cf = credential_fixture(Some(tokens.clone() as Arc<dyn OAuthAccessTokens>));
    let (url, _, requests) = basic_auth_recorder().await;
    let (binding, _) = oauth_binding_to(&cf, &url).await;

    assert_not_yours("clone", cf.fx.service.clone_repo(&binding.id).await);
    assert_eq!(
        *requests.lock().unwrap(),
        0,
        "a binding that cannot be refreshed reached the git host"
    );
}

/// G1b as G2c reads it: a binding holding no OAuth token at all is no
/// credential, so the clone goes out with none.
#[tokio::test]
async fn an_oauth_binding_holding_no_token_is_no_credential() {
    let tokens = RecordingTokens::answering(TokenAnswer::NoToken);
    let cf = credential_fixture(Some(tokens.clone() as Arc<dyn OAuthAccessTokens>));
    let (url, seen, requests) = basic_auth_recorder().await;
    let (binding, _) = oauth_binding_to(&cf, &url).await;

    let res = cf.fx.service.clone_repo(&binding.id).await;
    assert!(
        matches!(res, Err(GitRepoError::CloneFailed(_))),
        "a clone with no credential fails at the host's challenge: {res:?}"
    );
    assert!(
        *requests.lock().unwrap() >= 1,
        "the clone never reached the git host"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "a binding with no token sent a credential: {:?}",
        seen.lock().unwrap()
    );
}
