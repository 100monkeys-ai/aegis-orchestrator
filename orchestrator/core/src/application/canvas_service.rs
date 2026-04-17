// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Vibe-Code Canvas Service (BC-7 Storage Gateway, ADR-106)
//!
//! Application service that orchestrates the [`CanvasSession`] aggregate
//! lifecycle for The Studio (ADR-106). Wires the Canvas domain aggregate
//! ([`crate::domain::canvas`]) to:
//!
//! - [`CanvasSessionRepository`] — aggregate persistence
//! - [`VolumeService`] — workspace volume provisioning (ephemeral / persistent)
//! - [`GitRepoBindingRepository`] — resolves a caller's git binding for
//!   [`WorkspaceMode::GitLinked`] sessions
//! - [`EventBus`] — publishes [`CanvasEvent`]s
//!
//! ## Bounded Context
//!
//! BC-7 Storage Gateway, ADR-106 §Application Service.

use std::sync::Arc;

use async_trait::async_trait;

use crate::application::volume_manager::VolumeService;
use crate::domain::canvas::{
    CanvasEvent, CanvasSession, CanvasSessionId, CanvasSessionRepository, CanvasTierLimits,
    ConversationId, WorkspaceMode,
};
use crate::domain::git_repo::{GitRepoBindingId, GitRepoBindingRepository, GitRepoStatus};
use crate::domain::iam::ZaruTier;
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::{TenantId, VolumeId};
use crate::domain::volume::{StorageClass, VolumeOwnership};
use crate::infrastructure::event_bus::EventBus;

// ============================================================================
// Errors
// ============================================================================

/// Error type returned by [`CanvasService`] methods (ADR-106 §Application
/// Service).
#[derive(Debug, thiserror::Error)]
pub enum CanvasError {
    /// The requested session does not exist.
    #[error("canvas session not found: {0}")]
    NotFound(CanvasSessionId),
    /// The caller does not own the requested session. Returned as
    /// [`CanvasError::NotFound`] through the HTTP layer to avoid information
    /// leaks (ADR-106 §Security Considerations).
    #[error("canvas session not owned by caller")]
    NotOwned,
    /// The [`ZaruTier`] of the caller does not permit the requested
    /// [`WorkspaceMode`] (ADR-106 §Sub-Decision 5).
    #[error("workspace mode not allowed for tier")]
    NotAllowedByTier,
    /// The referenced [`GitRepoBinding`](crate::domain::git_repo::GitRepoBinding)
    /// does not exist or does not belong to the caller's tenant.
    #[error("git repo binding not found: {0}")]
    GitBindingNotFound(GitRepoBindingId),
    /// The referenced [`GitRepoBinding`](crate::domain::git_repo::GitRepoBinding)
    /// is not in [`GitRepoStatus::Ready`]; the caller must wait for the clone
    /// to complete before binding a canvas session.
    #[error("git repo binding not ready")]
    GitBindingNotReady,
    /// Volume provisioning failed at the infrastructure layer.
    #[error("volume provisioning failed: {0}")]
    VolumeProvisioningFailed(String),
    /// Persistence-layer error surfaced from the repository.
    #[error("repository error: {0}")]
    RepositoryError(#[from] RepositoryError),
    /// The [`CreateCanvasSessionCommand`] failed validation in the aggregate
    /// constructor (e.g. empty persistent label, GitLinked/binding_id
    /// mismatch).
    #[error("invalid command: {0}")]
    InvalidCommand(String),
}

// ============================================================================
// Command
// ============================================================================

/// Command to create a new [`CanvasSession`] (ADR-106 §Application Service).
///
/// Constructed by the HTTP handler from the authenticated caller's identity
/// (`tenant_id`, `owner`, `tier`) plus the JSON body (`conversation_id`,
/// `workspace_mode`, optional `git_binding_id`).
#[derive(Debug, Clone)]
pub struct CreateCanvasSessionCommand {
    /// Tenant that will own the session.
    pub tenant_id: TenantId,
    /// Caller's user subject (Keycloak `sub`). Used as the ownership anchor
    /// for any volumes provisioned for the session.
    pub owner: String,
    /// Caller's [`ZaruTier`] — resolved from the OIDC `zaru_tier` claim. Used
    /// to look up [`CanvasTierLimits`].
    pub tier: ZaruTier,
    /// Conversation the session is bound to (ADR-106 §Domain Model).
    pub conversation_id: ConversationId,
    /// Requested backing mode. Payload-bearing variants (`Persistent`,
    /// `GitLinked`) carry their sub-fields here.
    pub workspace_mode: WorkspaceMode,
    /// Optional human-readable name for the session.
    pub name: Option<String>,
}

/// Command to update a [`CanvasSession`]'s mutable fields (name, archived).
#[derive(Debug, Clone)]
pub struct UpdateCanvasSessionCommand {
    /// Session to update.
    pub session_id: CanvasSessionId,
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Caller's user subject.
    pub owner: String,
    /// If `Some`, sets the session name (pass `Some(None)` to clear).
    pub name: Option<Option<String>>,
    /// If `Some`, sets the archived flag.
    pub archived: Option<bool>,
}

// ============================================================================
// Service Trait
// ============================================================================

/// Primary interface for managing [`CanvasSession`] aggregates (ADR-106).
#[async_trait]
pub trait CanvasService: Send + Sync {
    /// Create a new canvas session. Provisions the backing workspace volume
    /// according to the requested [`WorkspaceMode`] and enforces
    /// [`CanvasTierLimits`].
    async fn create_session(
        &self,
        cmd: CreateCanvasSessionCommand,
    ) -> Result<CanvasSession, CanvasError>;

    /// List the caller's canvas sessions. When `include_archived` is false
    /// (default), archived sessions are excluded.
    async fn list_sessions(
        &self,
        tenant_id: &TenantId,
        owner: &str,
        include_archived: bool,
    ) -> Result<Vec<CanvasSession>, CanvasError>;

    /// Update a session's name and/or archived flag.
    async fn update_session(
        &self,
        cmd: UpdateCanvasSessionCommand,
    ) -> Result<CanvasSession, CanvasError>;

    /// Load a single session by id; verifies tenant ownership. Returns
    /// [`CanvasError::NotFound`] for missing or mis-tenanted rows (see
    /// ADR-106 §Security Considerations — no information leak).
    async fn get_session(
        &self,
        id: &CanvasSessionId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<CanvasSession, CanvasError>;

    /// Terminate a session. Transitions status to
    /// [`crate::domain::canvas::CanvasSessionStatus::Terminated`], publishes
    /// [`CanvasEvent::SessionTerminated`], and deletes the backing volume
    /// when the session is [`WorkspaceMode::Ephemeral`]. Persistent and
    /// git-linked volumes are retained.
    async fn terminate_session(
        &self,
        id: &CanvasSessionId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<(), CanvasError>;
}

// ============================================================================
// Concrete Service Implementation
// ============================================================================

/// Production implementation of [`CanvasService`] (ADR-106 §Application
/// Service).
///
/// Wires the [`CanvasSessionRepository`], the [`VolumeService`] used for
/// ephemeral/persistent workspace provisioning, the
/// [`GitRepoBindingRepository`] used to validate git-linked bindings, and
/// the [`EventBus`] used to publish domain events.
pub struct StandardCanvasService {
    session_repo: Arc<dyn CanvasSessionRepository>,
    volume_service: Arc<dyn VolumeService>,
    git_repo_repo: Arc<dyn GitRepoBindingRepository>,
    event_bus: Arc<EventBus>,
}

impl StandardCanvasService {
    pub fn new(
        session_repo: Arc<dyn CanvasSessionRepository>,
        volume_service: Arc<dyn VolumeService>,
        git_repo_repo: Arc<dyn GitRepoBindingRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            session_repo,
            volume_service,
            git_repo_repo,
            event_bus,
        }
    }

    /// Default size (MB) for canvas ephemeral workspace volumes. Generous
    /// enough to scaffold a multi-file browser app but well below the Free
    /// tier per-volume quota (`TierStorageLimit.max_file_size_bytes`).
    const EPHEMERAL_SIZE_MB: u64 = 256;

    /// Default size (MB) for canvas persistent workspace volumes.
    const PERSISTENT_SIZE_MB: u64 = 512;

    /// Default TTL for ephemeral canvas workspaces. Matches the orchestrator's
    /// default ephemeral-volume TTL (24h); the session-terminate path
    /// short-circuits the TTL by deleting the volume immediately when the
    /// user closes the session.
    const EPHEMERAL_TTL_HOURS: i64 = 24;

    /// Provision the workspace volume for a new [`CanvasSession`] and return
    /// its id. Dispatches on [`WorkspaceMode`]:
    ///
    /// - [`WorkspaceMode::Ephemeral`] → new ephemeral SeaweedFS volume owned
    ///   by the caller.
    /// - [`WorkspaceMode::Persistent`] → new persistent volume labeled from
    ///   the variant payload.
    /// - [`WorkspaceMode::GitLinked`] → reuses the existing `GitRepoBinding`
    ///   volume; verifies the binding belongs to the caller's tenant and is
    ///   in [`GitRepoStatus::Ready`].
    async fn provision_workspace_volume(
        &self,
        tenant_id: &TenantId,
        owner: &str,
        workspace_mode: &WorkspaceMode,
    ) -> Result<VolumeId, CanvasError> {
        match workspace_mode {
            WorkspaceMode::Ephemeral => {
                let label = format!("canvas-ephemeral-{}", uuid::Uuid::new_v4());
                self.volume_service
                    .create_volume(
                        label,
                        tenant_id.clone(),
                        StorageClass::ephemeral_hours(Self::EPHEMERAL_TTL_HOURS),
                        Self::EPHEMERAL_SIZE_MB,
                        VolumeOwnership::persistent(owner.to_string()),
                    )
                    .await
                    .map_err(|e| CanvasError::VolumeProvisioningFailed(e.to_string()))
            }
            WorkspaceMode::Persistent { volume_label } => self
                .volume_service
                .create_volume(
                    volume_label.clone(),
                    tenant_id.clone(),
                    StorageClass::persistent(),
                    Self::PERSISTENT_SIZE_MB,
                    VolumeOwnership::persistent(owner.to_string()),
                )
                .await
                .map_err(|e| CanvasError::VolumeProvisioningFailed(e.to_string())),
            WorkspaceMode::GitLinked { binding_id } => {
                let binding = self
                    .git_repo_repo
                    .find_by_id(binding_id)
                    .await?
                    .ok_or(CanvasError::GitBindingNotFound(*binding_id))?;
                // Tenant isolation: a caller cannot bind a canvas to another
                // tenant's repository even if they know the binding id.
                if binding.tenant_id != *tenant_id {
                    return Err(CanvasError::GitBindingNotFound(*binding_id));
                }
                // The binding must have completed its initial clone.
                if binding.status != GitRepoStatus::Ready {
                    return Err(CanvasError::GitBindingNotReady);
                }
                Ok(binding.volume_id)
            }
        }
    }

    /// Drain the aggregate's buffered domain events and publish each to the
    /// event bus.
    fn publish_events(&self, events: Vec<CanvasEvent>) {
        for event in events {
            self.event_bus.publish_canvas_event(event);
        }
    }
}

#[async_trait]
impl CanvasService for StandardCanvasService {
    async fn create_session(
        &self,
        cmd: CreateCanvasSessionCommand,
    ) -> Result<CanvasSession, CanvasError> {
        // Tier gating (ADR-106 §Sub-Decision 5). The aggregate constructor
        // handles structural invariants; the tier check gates *allowed
        // combinations*.
        let limits = CanvasTierLimits::for_tier(cmd.tier);
        if !limits.is_allowed(&cmd.workspace_mode) {
            return Err(CanvasError::NotAllowedByTier);
        }

        // Workspace volume provisioning.
        let workspace_volume_id = self
            .provision_workspace_volume(&cmd.tenant_id, &cmd.owner, &cmd.workspace_mode)
            .await?;

        // The GitLinked workspace mode carries its binding_id inside the
        // enum payload; the aggregate's cross-field invariant requires the
        // constructor's `git_binding_id` argument to match. For non-git
        // modes it must be None.
        let git_binding_id = match &cmd.workspace_mode {
            WorkspaceMode::GitLinked { binding_id } => Some(*binding_id),
            _ => None,
        };

        let mut session = CanvasSession::new(
            cmd.tenant_id.clone(),
            cmd.conversation_id,
            workspace_volume_id,
            cmd.workspace_mode,
            git_binding_id,
            cmd.name,
        )
        .map_err(CanvasError::InvalidCommand)?;

        self.session_repo.save(&session).await?;

        let events = session.take_events();
        self.publish_events(events);

        Ok(session)
    }

    async fn list_sessions(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
        include_archived: bool,
    ) -> Result<Vec<CanvasSession>, CanvasError> {
        // ADR-106: canvas sessions are tenant-scoped. Per-user tenants
        // (ADR-097) collapse "owner" and "tenant" to the same boundary for
        // consumer users. Tenant-wide teams (Business/Enterprise) legitimately
        // share sessions across seats within the colony.
        Ok(self
            .session_repo
            .find_by_owner(tenant_id, include_archived)
            .await?)
    }

    async fn update_session(
        &self,
        cmd: UpdateCanvasSessionCommand,
    ) -> Result<CanvasSession, CanvasError> {
        let mut session = self
            .session_repo
            .find_by_id(&cmd.session_id)
            .await?
            .ok_or(CanvasError::NotFound(cmd.session_id))?;

        if session.tenant_id != cmd.tenant_id {
            return Err(CanvasError::NotFound(cmd.session_id));
        }

        if let Some(name) = cmd.name {
            session.rename(name);
        }
        if let Some(archived) = cmd.archived {
            session.set_archived(archived);
        }

        self.session_repo.save(&session).await?;
        Ok(session)
    }

    async fn get_session(
        &self,
        id: &CanvasSessionId,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<CanvasSession, CanvasError> {
        let session = self
            .session_repo
            .find_by_id(id)
            .await?
            .ok_or(CanvasError::NotFound(*id))?;

        if session.tenant_id != *tenant_id {
            // Collapse to NotFound at the HTTP boundary to avoid disclosing
            // the existence of other tenants' sessions (ADR-106 §Security).
            return Err(CanvasError::NotFound(*id));
        }

        Ok(session)
    }

    async fn terminate_session(
        &self,
        id: &CanvasSessionId,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<(), CanvasError> {
        let mut session = self
            .session_repo
            .find_by_id(id)
            .await?
            .ok_or(CanvasError::NotFound(*id))?;

        if session.tenant_id != *tenant_id {
            return Err(CanvasError::NotFound(*id));
        }

        session.terminate();

        // Ephemeral volumes are released on terminate — there is no value in
        // retaining a random workspace after the session closes. Persistent
        // and git-linked volumes survive because the user explicitly chose
        // them and may mount them in a new session.
        if matches!(session.workspace_mode, WorkspaceMode::Ephemeral) {
            // Migration 020 uses `ON DELETE SET NULL` on workspace_volume_id
            // so deleting the volume does not cascade-delete the session row
            // — the historical record is preserved with a NULL volume
            // pointer.
            self.volume_service
                .delete_volume(session.workspace_volume_id)
                .await
                .map_err(|e| CanvasError::VolumeProvisioningFailed(e.to_string()))?;
        }

        self.session_repo.save(&session).await?;

        let events = session.take_events();
        self.publish_events(events);

        Ok(())
    }
}
