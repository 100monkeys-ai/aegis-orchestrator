// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Canvas Service + REST + SSE Integration Tests (ADR-106, Wave C2)
//!
//! Full-stack tests that exercise the [`CanvasService`] application service
//! through an in-memory repository harness plus an SSE-filtering test that
//! verifies the event-bus scoping used by
//! `GET /v1/canvas/sessions/:id/events`.
//!
//! Because the HTTP handlers are thin wrappers over the application service
//! (each one translates identity/JSON → `CanvasService` call → JSON
//! response), testing the service layer provides equivalent coverage to
//! spinning up axum. The SSE path is covered by directly driving the event
//! bus and asserting the filter semantics the handler relies on.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use aegis_orchestrator_core::application::canvas_service::{
    CanvasError, CanvasService, CreateCanvasSessionCommand, StandardCanvasService,
};
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::canvas::{
    CanvasEvent, CanvasSession, CanvasSessionId, CanvasSessionRepository, CanvasSessionStatus,
    ConversationId, WorkspaceMode,
};
use aegis_orchestrator_core::domain::git_repo::{
    CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository,
    GitRepoStatus,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::RepositoryError;
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
    VolumeStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use async_trait::async_trait;
use chrono::Utc;

// ============================================================================
// In-memory repositories and volume service (test harness)
// ============================================================================

/// Trivial in-memory [`CanvasSessionRepository`] — the production impl is a
/// Postgres-backed one; the service is repository-agnostic so this is
/// faithful to the real code path.
#[derive(Default, Clone)]
struct InMemoryCanvasRepo {
    sessions: Arc<RwLock<HashMap<CanvasSessionId, CanvasSession>>>,
}

#[async_trait]
impl CanvasSessionRepository for InMemoryCanvasRepo {
    async fn save(&self, session: &CanvasSession) -> Result<(), RepositoryError> {
        self.sessions
            .write()
            .unwrap()
            .insert(session.id, session.clone());
        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &CanvasSessionId,
    ) -> Result<Option<CanvasSession>, RepositoryError> {
        Ok(self.sessions.read().unwrap().get(id).cloned())
    }

    async fn find_by_conversation_id(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<CanvasSession>, RepositoryError> {
        Ok(self
            .sessions
            .read()
            .unwrap()
            .values()
            .find(|s| &s.conversation_id == conversation_id)
            .cloned())
    }

    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        include_archived: bool,
    ) -> Result<Vec<CanvasSession>, RepositoryError> {
        Ok(self
            .sessions
            .read()
            .unwrap()
            .values()
            .filter(|s| &s.tenant_id == tenant_id && (include_archived || !s.archived))
            .cloned()
            .collect())
    }

    async fn delete(&self, id: &CanvasSessionId) -> Result<(), RepositoryError> {
        self.sessions.write().unwrap().remove(id);
        Ok(())
    }
}

/// In-memory [`GitRepoBindingRepository`] seeded by the individual tests.
#[derive(Default, Clone)]
struct InMemoryGitRepoRepo {
    bindings: Arc<RwLock<HashMap<GitRepoBindingId, GitRepoBinding>>>,
}

impl InMemoryGitRepoRepo {
    fn insert(&self, binding: GitRepoBinding) {
        self.bindings.write().unwrap().insert(binding.id, binding);
    }
}

#[async_trait]
impl GitRepoBindingRepository for InMemoryGitRepoRepo {
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

/// Minimal [`VolumeService`] that records every create/delete call in a
/// shared log so tests can assert that the canvas service triggered the
/// right volume lifecycle transitions.
#[derive(Default)]
struct RecordingVolumeService {
    volumes: Arc<RwLock<HashMap<VolumeId, Volume>>>,
    create_log: Mutex<Vec<(String, StorageClass, VolumeOwnership)>>,
    delete_log: Mutex<Vec<VolumeId>>,
}

impl RecordingVolumeService {
    fn created(&self) -> Vec<(String, StorageClass, VolumeOwnership)> {
        self.create_log.lock().unwrap().clone()
    }

    fn deleted(&self) -> Vec<VolumeId> {
        self.delete_log.lock().unwrap().clone()
    }
}

#[async_trait]
impl VolumeService for RecordingVolumeService {
    async fn create_volume(
        &self,
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        size_limit_mb: u64,
        ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        self.create_log.lock().unwrap().push((
            name.clone(),
            storage_class.clone(),
            ownership.clone(),
        ));
        let id = VolumeId::new();
        let filer = FilerEndpoint::new("http://localhost:8888")?;
        let volume = Volume {
            id,
            name,
            tenant_id,
            storage_class,
            backend: VolumeBackend::SeaweedFS {
                filer_endpoint: filer,
                remote_path: format!("/aegis/volumes/{id}"),
            },
            size_limit_bytes: size_limit_mb * 1024 * 1024,
            status: VolumeStatus::Available,
            ownership,
            created_at: Utc::now(),
            attached_at: None,
            detached_at: None,
            expires_at: None,
            host_node_id: None,
        };
        self.volumes.write().unwrap().insert(id, volume);
        Ok(id)
    }

    async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
        self.volumes
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("not found"))
    }

    async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        Ok(self
            .volumes
            .read()
            .unwrap()
            .values()
            .filter(|v| v.tenant_id == tenant_id)
            .cloned()
            .collect())
    }

    async fn list_volumes_by_ownership(
        &self,
        ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(self
            .volumes
            .read()
            .unwrap()
            .values()
            .filter(|v| &v.ownership == ownership)
            .cloned()
            .collect())
    }

    async fn attach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
        _mount_point: std::path::PathBuf,
        _access_mode: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        unimplemented!("tests do not mount volumes")
    }

    async fn detach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn delete_volume(&self, volume_id: VolumeId) -> anyhow::Result<()> {
        self.delete_log.lock().unwrap().push(volume_id);
        self.volumes.write().unwrap().remove(&volume_id);
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

// ============================================================================
// Test harness constructor
// ============================================================================

struct Harness {
    service: StandardCanvasService,
    sessions: InMemoryCanvasRepo,
    volumes: Arc<RecordingVolumeService>,
    git_repos: InMemoryGitRepoRepo,
    event_bus: Arc<EventBus>,
}

fn build_harness() -> Harness {
    let sessions = InMemoryCanvasRepo::default();
    let volumes = Arc::new(RecordingVolumeService::default());
    let git_repos = InMemoryGitRepoRepo::default();
    let event_bus = Arc::new(EventBus::new(128));
    let service = StandardCanvasService::new(
        Arc::new(sessions.clone()),
        volumes.clone() as Arc<dyn VolumeService>,
        Arc::new(git_repos.clone()),
        event_bus.clone(),
    );
    Harness {
        service,
        sessions,
        volumes,
        git_repos,
        event_bus,
    }
}

fn ready_git_binding(tenant: TenantId) -> GitRepoBinding {
    let mut binding = GitRepoBinding::new(
        tenant,
        None,
        "https://github.com/octocat/Hello-World.git".to_string(),
        GitRef::default(),
        None,
        VolumeId::new(),
        "hello-world".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    );
    let _ = binding.take_events();
    binding.start_clone();
    binding.complete_clone("deadbeef".to_string(), 10);
    let _ = binding.take_events();
    binding
}

// ============================================================================
// POST /v1/canvas/sessions
// ============================================================================

#[tokio::test]
async fn create_session_ephemeral_free_tier_succeeds() {
    let h = build_harness();
    let tenant = TenantId::consumer();
    let cmd = CreateCanvasSessionCommand {
        tenant_id: tenant.clone(),
        owner: "user-1".to_string(),
        tier: ZaruTier::Free,
        conversation_id: ConversationId::new(),
        workspace_mode: WorkspaceMode::Ephemeral,
        name: None,
    };

    let session = h.service.create_session(cmd).await.expect("creates");

    assert_eq!(session.status, CanvasSessionStatus::Initializing);
    assert_eq!(session.tenant_id, tenant);
    assert!(matches!(session.workspace_mode, WorkspaceMode::Ephemeral));
    assert!(session.git_binding_id.is_none());

    // Volume service should have been asked for an ephemeral volume.
    let created = h.volumes.created();
    assert_eq!(created.len(), 1);
    assert!(matches!(created[0].1, StorageClass::Ephemeral { .. }));

    // Session should be persisted.
    assert!(h.sessions.find_by_id(&session.id).await.unwrap().is_some());
}

#[tokio::test]
async fn create_session_persistent_free_tier_rejected() {
    let h = build_harness();
    let cmd = CreateCanvasSessionCommand {
        tenant_id: TenantId::consumer(),
        owner: "user-1".to_string(),
        tier: ZaruTier::Free,
        conversation_id: ConversationId::new(),
        workspace_mode: WorkspaceMode::Persistent {
            volume_label: "proj".to_string(),
        },
        name: None,
    };

    let res = h.service.create_session(cmd).await;
    assert!(
        matches!(res, Err(CanvasError::NotAllowedByTier)),
        "expected NotAllowedByTier, got {res:?}"
    );
    assert!(
        h.volumes.created().is_empty(),
        "no volume should be provisioned on rejection"
    );
}

#[tokio::test]
async fn create_session_git_linked_rejects_non_ready_binding() {
    let h = build_harness();
    let tenant = TenantId::consumer();

    // Seed a binding that is still Pending.
    let mut binding = GitRepoBinding::new(
        tenant.clone(),
        None,
        "https://github.com/octocat/Hello-World.git".to_string(),
        GitRef::default(),
        None,
        VolumeId::new(),
        "hello-world".to_string(),
        CloneStrategy::Libgit2,
        false,
        None,
    );
    let _ = binding.take_events();
    let binding_id = binding.id;
    h.git_repos.insert(binding);

    let cmd = CreateCanvasSessionCommand {
        tenant_id: tenant,
        owner: "user-1".to_string(),
        tier: ZaruTier::Enterprise,
        conversation_id: ConversationId::new(),
        workspace_mode: WorkspaceMode::GitLinked { binding_id },
        name: None,
    };

    let res = h.service.create_session(cmd).await;
    assert!(
        matches!(res, Err(CanvasError::GitBindingNotReady)),
        "expected GitBindingNotReady, got {res:?}"
    );
}

#[tokio::test]
async fn create_session_git_linked_rejects_wrong_tenant() {
    let h = build_harness();
    let caller_tenant = TenantId::new("u-alice".to_string()).unwrap();
    let foreign_tenant = TenantId::new("u-bob".to_string()).unwrap();

    // Seed a binding owned by a different tenant.
    let binding = ready_git_binding(foreign_tenant);
    let binding_id = binding.id;
    h.git_repos.insert(binding);

    let cmd = CreateCanvasSessionCommand {
        tenant_id: caller_tenant,
        owner: "alice".to_string(),
        tier: ZaruTier::Enterprise,
        conversation_id: ConversationId::new(),
        workspace_mode: WorkspaceMode::GitLinked { binding_id },
        name: None,
    };

    let res = h.service.create_session(cmd).await;
    assert!(
        matches!(res, Err(CanvasError::GitBindingNotFound(_))),
        "expected GitBindingNotFound, got {res:?}"
    );
}

#[tokio::test]
async fn create_session_git_linked_enterprise_succeeds() {
    let h = build_harness();
    let tenant = TenantId::consumer();

    let binding = ready_git_binding(tenant.clone());
    let binding_id = binding.id;
    let expected_volume = binding.volume_id;
    h.git_repos.insert(binding);

    let cmd = CreateCanvasSessionCommand {
        tenant_id: tenant,
        owner: "user-1".to_string(),
        tier: ZaruTier::Enterprise,
        conversation_id: ConversationId::new(),
        workspace_mode: WorkspaceMode::GitLinked { binding_id },
        name: None,
    };

    let session = h
        .service
        .create_session(cmd)
        .await
        .expect("git-linked enterprise session");

    assert_eq!(session.workspace_volume_id, expected_volume);
    assert_eq!(session.git_binding_id, Some(binding_id));
    // Git-linked must reuse the existing volume — no new volume provisioned.
    assert!(h.volumes.created().is_empty());
}

/// Pro-tier users can create git-linked canvas sessions. ADR-106
/// §Sub-Decision 5's Git-Linked column reads "Yes" for both Pro and Business;
/// the Workspace-Modes column reads "Ephemeral + Persistent" but the
/// per-column "Yes" for Git-Linked is the authoritative gate. Pro and Business
/// both get all three workspace kinds.
#[tokio::test]
async fn create_session_git_linked_pro_tier_succeeds() {
    let h = build_harness();
    let tenant = TenantId::consumer();
    let binding = ready_git_binding(tenant.clone());
    let binding_id = binding.id;
    let expected_volume = binding.volume_id;
    h.git_repos.insert(binding);

    let cmd = CreateCanvasSessionCommand {
        tenant_id: tenant,
        owner: "user-1".to_string(),
        tier: ZaruTier::Pro,
        conversation_id: ConversationId::new(),
        workspace_mode: WorkspaceMode::GitLinked { binding_id },
        name: None,
    };

    let session = h
        .service
        .create_session(cmd)
        .await
        .expect("git-linked pro session");

    assert_eq!(session.workspace_volume_id, expected_volume);
    assert_eq!(session.git_binding_id, Some(binding_id));
    // Git-linked reuses the binding's volume — no new volume provisioned.
    assert!(h.volumes.created().is_empty());
}

// ============================================================================
// GET /v1/canvas/sessions
// ============================================================================

#[tokio::test]
async fn list_sessions_returns_only_callers_tenant() {
    let h = build_harness();
    let tenant_a = TenantId::new("u-alice".to_string()).unwrap();
    let tenant_b = TenantId::new("u-bob".to_string()).unwrap();

    // Alice creates two sessions.
    for _ in 0..2 {
        h.service
            .create_session(CreateCanvasSessionCommand {
                tenant_id: tenant_a.clone(),
                owner: "alice".to_string(),
                tier: ZaruTier::Free,
                conversation_id: ConversationId::new(),
                workspace_mode: WorkspaceMode::Ephemeral,
                name: None,
            })
            .await
            .unwrap();
    }
    // Bob creates one session.
    h.service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: tenant_b.clone(),
            owner: "bob".to_string(),
            tier: ZaruTier::Free,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::Ephemeral,
            name: None,
        })
        .await
        .unwrap();

    let alice_sessions = h
        .service
        .list_sessions(&tenant_a, "alice", false)
        .await
        .unwrap();
    assert_eq!(alice_sessions.len(), 2);
    assert!(alice_sessions.iter().all(|s| s.tenant_id == tenant_a));

    let bob_sessions = h
        .service
        .list_sessions(&tenant_b, "bob", false)
        .await
        .unwrap();
    assert_eq!(bob_sessions.len(), 1);
    assert_eq!(bob_sessions[0].tenant_id, tenant_b);
}

// ============================================================================
// GET /v1/canvas/sessions/:id
// ============================================================================

#[tokio::test]
async fn get_session_wrong_tenant_returns_not_found() {
    let h = build_harness();
    let tenant_a = TenantId::new("u-alice".to_string()).unwrap();
    let tenant_b = TenantId::new("u-bob".to_string()).unwrap();

    let session = h
        .service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: tenant_a.clone(),
            owner: "alice".to_string(),
            tier: ZaruTier::Free,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::Ephemeral,
            name: None,
        })
        .await
        .unwrap();

    // Bob tries to read Alice's session.
    let res = h.service.get_session(&session.id, &tenant_b, "bob").await;
    assert!(
        matches!(res, Err(CanvasError::NotFound(_))),
        "expected NotFound (info-leak protection), got {res:?}"
    );

    // Alice reads her own session successfully.
    let ok = h
        .service
        .get_session(&session.id, &tenant_a, "alice")
        .await
        .unwrap();
    assert_eq!(ok.id, session.id);
}

// ============================================================================
// DELETE /v1/canvas/sessions/:id
// ============================================================================

#[tokio::test]
async fn terminate_session_ephemeral_deletes_volume() {
    let h = build_harness();
    let tenant = TenantId::consumer();

    let session = h
        .service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: tenant.clone(),
            owner: "user-1".to_string(),
            tier: ZaruTier::Free,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::Ephemeral,
            name: None,
        })
        .await
        .unwrap();

    let volume_id = session.workspace_volume_id;

    h.service
        .terminate_session(&session.id, &tenant, "user-1")
        .await
        .unwrap();

    // Ephemeral volume MUST be deleted.
    let deleted = h.volumes.deleted();
    assert_eq!(deleted, vec![volume_id]);

    // Session persisted with Terminated status.
    let row = h.sessions.find_by_id(&session.id).await.unwrap().unwrap();
    assert_eq!(row.status, CanvasSessionStatus::Terminated);
}

#[tokio::test]
async fn terminate_session_git_linked_retains_volume() {
    let h = build_harness();
    let tenant = TenantId::consumer();
    let binding = ready_git_binding(tenant.clone());
    let binding_id = binding.id;
    h.git_repos.insert(binding);

    let session = h
        .service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: tenant.clone(),
            owner: "user-1".to_string(),
            tier: ZaruTier::Enterprise,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::GitLinked { binding_id },
            name: None,
        })
        .await
        .unwrap();

    h.service
        .terminate_session(&session.id, &tenant, "user-1")
        .await
        .unwrap();

    // Git-linked volumes MUST NOT be deleted on session terminate — the
    // GitRepoBinding lifecycle owns the volume independently.
    assert!(
        h.volumes.deleted().is_empty(),
        "git-linked canvas terminate must not delete the underlying repo volume"
    );
}

#[tokio::test]
async fn terminate_session_wrong_tenant_returns_not_found() {
    let h = build_harness();
    let tenant_a = TenantId::new("u-alice".to_string()).unwrap();
    let tenant_b = TenantId::new("u-bob".to_string()).unwrap();

    let session = h
        .service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: tenant_a.clone(),
            owner: "alice".to_string(),
            tier: ZaruTier::Free,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::Ephemeral,
            name: None,
        })
        .await
        .unwrap();

    let res = h
        .service
        .terminate_session(&session.id, &tenant_b, "bob")
        .await;
    assert!(
        matches!(res, Err(CanvasError::NotFound(_))),
        "expected NotFound, got {res:?}"
    );

    // Alice's session remains non-Terminated.
    let row = h.sessions.find_by_id(&session.id).await.unwrap().unwrap();
    assert_ne!(row.status, CanvasSessionStatus::Terminated);
}

/// VolumeService wrapper that delegates everything to the inner service
/// EXCEPT `delete_volume`, which always fails. Used by the
/// `terminate_persists_state_when_volume_delete_fails` regression test.
struct FailingDeleteVolumeService {
    inner: Arc<RecordingVolumeService>,
}

#[async_trait]
impl VolumeService for FailingDeleteVolumeService {
    async fn create_volume(
        &self,
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        size_limit_mb: u64,
        ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        self.inner
            .create_volume(name, tenant_id, storage_class, size_limit_mb, ownership)
            .await
    }
    async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
        self.inner.get_volume(id).await
    }
    async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        self.inner.list_volumes_by_tenant(tenant_id).await
    }
    async fn list_volumes_by_ownership(
        &self,
        ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        self.inner.list_volumes_by_ownership(ownership).await
    }
    async fn attach_volume(
        &self,
        volume_id: VolumeId,
        instance_id: InstanceId,
        mount_point: std::path::PathBuf,
        access_mode: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        self.inner
            .attach_volume(volume_id, instance_id, mount_point, access_mode)
            .await
    }
    async fn detach_volume(
        &self,
        volume_id: VolumeId,
        instance_id: InstanceId,
    ) -> anyhow::Result<()> {
        self.inner.detach_volume(volume_id, instance_id).await
    }
    async fn delete_volume(&self, _volume_id: VolumeId) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("simulated storage backend outage"))
    }
    async fn get_volume_usage(&self, volume_id: VolumeId) -> anyhow::Result<u64> {
        self.inner.get_volume_usage(volume_id).await
    }
    async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
        self.inner.cleanup_expired_volumes().await
    }
    async fn create_volumes_for_execution(
        &self,
        execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
        tenant_id: TenantId,
        volume_specs: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
        storage_mode: &str,
    ) -> anyhow::Result<Vec<Volume>> {
        self.inner
            .create_volumes_for_execution(execution_id, tenant_id, volume_specs, storage_mode)
            .await
    }
    async fn persist_external_volume(
        &self,
        volume_id: VolumeId,
        name: String,
        tenant_id: TenantId,
        remote_path: String,
        size_limit_bytes: u64,
        ownership: VolumeOwnership,
    ) -> anyhow::Result<()> {
        self.inner
            .persist_external_volume(
                volume_id,
                name,
                tenant_id,
                remote_path,
                size_limit_bytes,
                ownership,
            )
            .await
    }
}

/// Audit 002 §4.37.4 regression — when terminating a Canvas session whose
/// ephemeral workspace volume fails to delete (storage backend outage,
/// transient I/O error, etc.), the session row MUST still be persisted as
/// `Terminated` so the user-visible state reflects domain reality, and a
/// `WorkspaceVolumeOrphaned` event MUST be emitted carrying the volume id
/// so a janitor / retry path can finish the cleanup. The previous code
/// path saved the session AFTER the volume delete, so a delete failure
/// rolled back the entire terminate and orphaned the volume on the next
/// attempt.
#[tokio::test]
async fn terminate_persists_state_when_volume_delete_fails() {
    let inner_volumes = Arc::new(RecordingVolumeService::default());
    let failing = Arc::new(FailingDeleteVolumeService {
        inner: inner_volumes.clone(),
    });
    let sessions = InMemoryCanvasRepo::default();
    let git_repos = InMemoryGitRepoRepo::default();
    let event_bus = Arc::new(EventBus::new(128));

    let service = StandardCanvasService::new(
        Arc::new(sessions.clone()),
        failing as Arc<dyn VolumeService>,
        Arc::new(git_repos),
        event_bus.clone(),
    );

    let tenant = TenantId::consumer();
    let session = service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: tenant.clone(),
            owner: "user-1".to_string(),
            tier: ZaruTier::Free,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::Ephemeral,
            name: None,
        })
        .await
        .unwrap();
    let volume_id = session.workspace_volume_id;

    let mut receiver = event_bus.subscribe();

    let res = service
        .terminate_session(&session.id, &tenant, "user-1")
        .await;
    assert!(
        matches!(res, Err(CanvasError::VolumeProvisioningFailed(_))),
        "expected VolumeProvisioningFailed surfaced to the caller, got {res:?}"
    );

    // 1. Session row persisted as Terminated despite the cleanup failure.
    let row = sessions.find_by_id(&session.id).await.unwrap().unwrap();
    assert_eq!(
        row.status,
        CanvasSessionStatus::Terminated,
        "terminate must persist Terminated state even when volume cleanup fails"
    );

    // 2. Both SessionTerminated AND WorkspaceVolumeOrphaned events were
    //    published to the bus, with the orphan event carrying the volume id.
    let mut saw_terminated = false;
    let mut saw_orphan = false;
    for _ in 0..4 {
        match tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
            Ok(Ok(DomainEvent::Canvas(CanvasEvent::SessionTerminated { .. }))) => {
                saw_terminated = true;
            }
            Ok(Ok(DomainEvent::Canvas(CanvasEvent::WorkspaceVolumeOrphaned {
                volume_id: vid,
                session_id: sid,
                ..
            }))) => {
                assert_eq!(vid, volume_id);
                assert_eq!(sid, session.id);
                saw_orphan = true;
            }
            _ => break,
        }
        if saw_terminated && saw_orphan {
            break;
        }
    }
    assert!(saw_terminated, "SessionTerminated event must be published");
    assert!(
        saw_orphan,
        "WorkspaceVolumeOrphaned event must be published with the orphaned volume id"
    );
}

// ============================================================================
// Event bus: SessionCreated is published on create
// ============================================================================

#[tokio::test]
async fn create_session_publishes_session_created_event() {
    let h = build_harness();
    let mut receiver = h.event_bus.subscribe();

    let session = h
        .service
        .create_session(CreateCanvasSessionCommand {
            tenant_id: TenantId::consumer(),
            owner: "user-1".to_string(),
            tier: ZaruTier::Free,
            conversation_id: ConversationId::new(),
            workspace_mode: WorkspaceMode::Ephemeral,
            name: None,
        })
        .await
        .unwrap();

    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("event within 1s")
        .expect("event bus open");
    match event {
        DomainEvent::Canvas(CanvasEvent::SessionCreated {
            session_id,
            tenant_id,
            ..
        }) => {
            assert_eq!(session_id, session.id);
            assert_eq!(tenant_id, TenantId::consumer());
        }
        other => panic!("expected Canvas::SessionCreated, got {other:?}"),
    }
}

// ============================================================================
// SSE filter semantics — the handler subscribes to the event bus and only
// forwards CanvasEvents whose session_id matches the path parameter.
// ============================================================================

/// Mirrors `canvas_event_session_id` in `cli/src/daemon/handlers/canvas.rs`.
/// Duplicated here because that module lives in the `cli` crate and this
/// test suite sits in `orchestrator-core`; both implementations must stay
/// in sync.
fn canvas_event_session_id(event: &CanvasEvent) -> CanvasSessionId {
    match event {
        CanvasEvent::SessionCreated { session_id, .. }
        | CanvasEvent::FilesWrittenByAgent { session_id, .. }
        | CanvasEvent::GitCommitMade { session_id, .. }
        | CanvasEvent::GitPushed { session_id, .. }
        | CanvasEvent::SessionTerminated { session_id, .. } => *session_id,
    }
}

#[tokio::test]
async fn sse_filter_receives_matching_session_event() {
    let event_bus = Arc::new(EventBus::new(64));
    let subscribed_session = CanvasSessionId::new();

    // Subscribe first, then publish — the handler does exactly this.
    let mut receiver = event_bus.subscribe();

    let expected = CanvasEvent::FilesWrittenByAgent {
        session_id: subscribed_session,
        file_count: 3,
        written_at: Utc::now(),
    };
    event_bus.publish_canvas_event(expected.clone());

    let got = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match receiver.recv().await.unwrap() {
                DomainEvent::Canvas(e) if canvas_event_session_id(&e) == subscribed_session => {
                    break e;
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("matching event within 1s");

    match got {
        CanvasEvent::FilesWrittenByAgent {
            session_id,
            file_count,
            ..
        } => {
            assert_eq!(session_id, subscribed_session);
            assert_eq!(file_count, 3);
        }
        other => panic!("expected FilesWrittenByAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn sse_filter_drops_other_session_events() {
    let event_bus = Arc::new(EventBus::new(64));
    let subscribed_session = CanvasSessionId::new();
    let other_session = CanvasSessionId::new();

    let mut receiver = event_bus.subscribe();

    // Publish an event for the OTHER session first.
    event_bus.publish_canvas_event(CanvasEvent::FilesWrittenByAgent {
        session_id: other_session,
        file_count: 99,
        written_at: Utc::now(),
    });
    // Then the one our "subscriber" cares about.
    event_bus.publish_canvas_event(CanvasEvent::FilesWrittenByAgent {
        session_id: subscribed_session,
        file_count: 1,
        written_at: Utc::now(),
    });

    // Walk through all domain events, applying the same filter the SSE
    // handler applies. The first *matching* event MUST be the one scoped to
    // our session — the other-session one MUST NOT leak through.
    let matched = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match receiver.recv().await.unwrap() {
                DomainEvent::Canvas(e) => {
                    let sid = canvas_event_session_id(&e);
                    if sid == subscribed_session {
                        break e;
                    }
                    // Continue — the handler silently drops events for
                    // other sessions.
                    assert_eq!(
                        sid, other_session,
                        "unexpected canvas event with session_id {sid}"
                    );
                }
                _ => continue,
            }
        }
    })
    .await
    .expect("matching event within 1s");

    match matched {
        CanvasEvent::FilesWrittenByAgent {
            session_id,
            file_count,
            ..
        } => {
            assert_eq!(session_id, subscribed_session);
            assert_eq!(file_count, 1);
        }
        other => panic!("expected FilesWrittenByAgent, got {other:?}"),
    }
}

#[tokio::test]
async fn sse_filter_receives_session_terminated_event_last() {
    // When the SSE handler observes a SessionTerminated event for its
    // session_id it closes the stream gracefully. This test captures the
    // event-bus side of that contract.
    let event_bus = Arc::new(EventBus::new(64));
    let session_id = CanvasSessionId::new();

    let mut receiver = event_bus.subscribe();

    event_bus.publish_canvas_event(CanvasEvent::FilesWrittenByAgent {
        session_id,
        file_count: 2,
        written_at: Utc::now(),
    });
    event_bus.publish_canvas_event(CanvasEvent::SessionTerminated {
        session_id,
        terminated_at: Utc::now(),
    });

    let mut collected: Vec<CanvasEvent> = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match receiver.recv().await.unwrap() {
                DomainEvent::Canvas(e) if canvas_event_session_id(&e) == session_id => {
                    let is_term = matches!(e, CanvasEvent::SessionTerminated { .. });
                    collected.push(e);
                    if is_term {
                        break;
                    }
                }
                _ => continue,
            }
        }
    })
    .await;

    assert_eq!(collected.len(), 2);
    assert!(matches!(
        collected[0],
        CanvasEvent::FilesWrittenByAgent { .. }
    ));
    assert!(matches!(
        collected[1],
        CanvasEvent::SessionTerminated { .. }
    ));
}
