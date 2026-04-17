// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Vibe-Code Canvas Domain (BC-7 Storage Gateway, ADR-106)
//!
//! Domain model for [`CanvasSession`] — the aggregate that ties a VibeCode-mode
//! `ChatConversation` to a workspace volume (ephemeral, persistent, or
//! git-linked via a [`GitRepoBindingId`]). The Canvas is the server-side
//! anchor for "The Studio" — the browser-based vibe-code surface described in
//! ADR-106.
//!
//! The aggregate is an *intent + lifecycle* object: it records which
//! conversation the canvas belongs to, which volume backs it, how the volume
//! was provisioned, and whether the session is still active. File writes and
//! git operations are performed through orthogonal subsystems (the AegisFSAL
//! volume path and the `GitRepoBinding` aggregate respectively); this
//! aggregate only emits `FilesWrittenByAgent` / `GitCommitMade` /
//! `GitPushed` *audit markers* into the domain event stream so the Zaru
//! Client Studio can visualize activity in real time.
//!
//! ## Type Map
//!
//! | Type | Role |
//! |------|------|
//! | [`CanvasSessionId`] | UUID newtype — aggregate root identity |
//! | [`ConversationId`] | UUID newtype — conversation this canvas is bound to |
//! | [`WorkspaceMode`] | Ephemeral / Persistent / GitLinked backing mode |
//! | [`WorkspaceModeKind`] | Payload-stripped discriminant used by tier gating |
//! | [`CanvasSessionStatus`] | Initializing / Ready / Idle / Terminated |
//! | [`CanvasTierLimits`] | Per-[`ZaruTier`] allowed workspace modes (ADR-106 §Sub-Decision 5) |
//! | [`CanvasEvent`] | Domain events published to the event bus |
//! | [`CanvasSession`] | Aggregate root |
//! | [`CanvasSessionRepository`] | Repository trait (Postgres impl in infrastructure) |

use crate::domain::git_repo::GitRepoBindingId;
use crate::domain::iam::ZaruTier;
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::{TenantId, VolumeId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Value Object — Identity
// ============================================================================

/// Unique identifier for a [`CanvasSession`] aggregate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanvasSessionId(pub Uuid);

impl CanvasSessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CanvasSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CanvasSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for the `ChatConversation` a [`CanvasSession`] is bound
/// to (ADR-106 §Domain Model).
///
/// The full `ChatConversation` aggregate lives in the Zaru Client domain; on
/// the orchestrator side we only need its stable id to associate a canvas
/// session with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub Uuid);

impl ConversationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConversationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Value Object — WorkspaceMode
// ============================================================================

/// Backing mode for the canvas workspace volume (ADR-106 §Sub-Decision 3).
///
/// - [`WorkspaceMode::Ephemeral`] — volume is auto-cleaned on session
///   termination and does not count against the user's persistent quota.
/// - [`WorkspaceMode::Persistent`] — volume survives across sessions under the
///   given `volume_label`; counts against [ADR-079] per-user volume quota.
/// - [`WorkspaceMode::GitLinked`] — the workspace volume is the working tree
///   of the referenced [`crate::domain::git_repo::GitRepoBinding`] (ADR-081).
///   The agent edits the cloned repository directly; commit/push routes
///   through the canvas SEAL tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceMode {
    Ephemeral,
    Persistent { volume_label: String },
    GitLinked { binding_id: GitRepoBindingId },
}

impl WorkspaceMode {
    /// Payload-stripped discriminant used by tier-limit tables.
    pub fn kind(&self) -> WorkspaceModeKind {
        match self {
            WorkspaceMode::Ephemeral => WorkspaceModeKind::Ephemeral,
            WorkspaceMode::Persistent { .. } => WorkspaceModeKind::Persistent,
            WorkspaceMode::GitLinked { .. } => WorkspaceModeKind::GitLinked,
        }
    }
}

/// Payload-stripped discriminant of [`WorkspaceMode`].
///
/// Tier gating is expressed as a set of allowed *kinds* (ADR-106 §Sub-Decision
/// 5) — the payloads carried by the full enum (`volume_label`, `binding_id`)
/// don't make sense in a "which variants are allowed" list, so we project to
/// this sibling enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkspaceModeKind {
    Ephemeral,
    Persistent,
    GitLinked,
}

// ============================================================================
// Value Object — CanvasSessionStatus
// ============================================================================

/// Lifecycle state of a [`CanvasSession`] (ADR-106 §Domain Model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanvasSessionStatus {
    /// Workspace volume is being provisioned (either fresh ephemeral create,
    /// persistent lookup, or initial git clone).
    Initializing,
    /// Volume is mounted; the browser Studio can read/write through the SDK.
    Ready,
    /// No activity within the idle-timeout window.
    Idle,
    /// Session ended; ephemeral volumes have been released.
    Terminated,
}

// ============================================================================
// Value Object — CanvasTierLimits
// ============================================================================

/// Per-[`ZaruTier`] gating for [`CanvasSession`] creation (ADR-106
/// §Sub-Decision 5).
///
/// | Tier | Allowed Modes | Git-Linked |
/// |------|---------------|------------|
/// | Free | `[Ephemeral]` | No |
/// | Pro | `[Ephemeral, Persistent]` | Yes |
/// | Business | `[Ephemeral, Persistent]` | Yes |
/// | Enterprise | `[Ephemeral, Persistent, GitLinked]` | Yes |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasTierLimits {
    /// Which [`WorkspaceModeKind`]s the tier is permitted to request at
    /// session creation.
    pub allowed_workspace_modes: Vec<WorkspaceModeKind>,
    /// Whether a git-linked canvas is available. Mirrors the right-hand column
    /// of ADR-106 §Sub-Decision 5.
    pub git_linked: bool,
}

impl CanvasTierLimits {
    /// Resolve the [`CanvasTierLimits`] for a [`ZaruTier`] per ADR-106
    /// §Sub-Decision 5.
    pub fn for_tier(tier: ZaruTier) -> Self {
        match tier {
            ZaruTier::Free => Self {
                allowed_workspace_modes: vec![WorkspaceModeKind::Ephemeral],
                git_linked: false,
            },
            ZaruTier::Pro => Self {
                allowed_workspace_modes: vec![
                    WorkspaceModeKind::Ephemeral,
                    WorkspaceModeKind::Persistent,
                    WorkspaceModeKind::GitLinked,
                ],
                git_linked: true,
            },
            ZaruTier::Business => Self {
                allowed_workspace_modes: vec![
                    WorkspaceModeKind::Ephemeral,
                    WorkspaceModeKind::Persistent,
                    WorkspaceModeKind::GitLinked,
                ],
                git_linked: true,
            },
            ZaruTier::Enterprise => Self {
                allowed_workspace_modes: vec![
                    WorkspaceModeKind::Ephemeral,
                    WorkspaceModeKind::Persistent,
                    WorkspaceModeKind::GitLinked,
                ],
                git_linked: true,
            },
        }
    }

    /// Return `true` if the given [`WorkspaceMode`] is permitted by this tier.
    ///
    /// Combines the kind allow-list with the `git_linked` flag so a caller
    /// can't slip a `GitLinked` mode past tiers whose kind list happens to
    /// include `GitLinked` but whose `git_linked` flag is `false` (this
    /// combination is impossible for the ADR-defined tiers, but the defensive
    /// check keeps future extension safe).
    pub fn is_allowed(&self, mode: &WorkspaceMode) -> bool {
        let kind = mode.kind();
        if !self.allowed_workspace_modes.contains(&kind) {
            return false;
        }
        if matches!(kind, WorkspaceModeKind::GitLinked) && !self.git_linked {
            return false;
        }
        true
    }
}

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events published by [`CanvasSession`] state transitions (ADR-106
/// §Domain Events).
///
/// Emitted by the aggregate during state changes and drained by the
/// application service via [`CanvasSession::take_events`] for publication to
/// the event bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CanvasEvent {
    SessionCreated {
        session_id: CanvasSessionId,
        conversation_id: ConversationId,
        workspace_mode: WorkspaceMode,
        tenant_id: TenantId,
        created_at: DateTime<Utc>,
    },
    FilesWrittenByAgent {
        session_id: CanvasSessionId,
        file_count: u32,
        written_at: DateTime<Utc>,
    },
    GitCommitMade {
        session_id: CanvasSessionId,
        commit_sha: String,
        committed_at: DateTime<Utc>,
    },
    GitPushed {
        session_id: CanvasSessionId,
        remote: String,
        ref_name: String,
        pushed_at: DateTime<Utc>,
    },
    SessionTerminated {
        session_id: CanvasSessionId,
        terminated_at: DateTime<Utc>,
    },
}

// ============================================================================
// Aggregate Root — CanvasSession
// ============================================================================

/// Aggregate root for a Vibe-Code Canvas session (ADR-106 §Domain Model).
///
/// ## Invariants
///
/// - A [`WorkspaceMode::GitLinked`] session MUST have `git_binding_id ==
///   Some(binding_id)` matching the variant payload.
/// - A non-git session MUST have `git_binding_id == None`.
/// - A [`WorkspaceMode::Persistent`] session MUST carry a non-empty
///   `volume_label`.
/// - `tenant_id` is included for repository tenant-scoping (not strictly
///   required by ADR-106's struct definition, which omits it — but the ADR's
///   ownership rules make tenant isolation a hard requirement and every
///   repository query below is tenant-scoped).
#[derive(Debug, Clone)]
pub struct CanvasSession {
    pub id: CanvasSessionId,
    pub tenant_id: TenantId,
    pub conversation_id: ConversationId,
    pub workspace_volume_id: VolumeId,
    pub git_binding_id: Option<GitRepoBindingId>,
    pub workspace_mode: WorkspaceMode,
    pub status: CanvasSessionStatus,
    /// Optional human-readable name for the session. When `None`, the UI
    /// may auto-generate a name from the first user message or a timestamp.
    pub name: Option<String>,
    /// Soft-archive flag. Archived sessions are excluded from the default
    /// list view but remain queryable with `include_archived`.
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    /// Event buffer. Drained by [`take_events`](Self::take_events) at the
    /// aggregate boundary and published to the event bus.
    pub domain_events: Vec<CanvasEvent>,
}

impl CanvasSession {
    /// Construct a new canvas session in [`CanvasSessionStatus::Initializing`]
    /// state and buffer a [`CanvasEvent::SessionCreated`] event.
    ///
    /// Validates the two cross-field invariants:
    ///
    /// - [`WorkspaceMode::GitLinked { binding_id }`](WorkspaceMode::GitLinked)
    ///   ⇔ `git_binding_id == Some(binding_id)`.
    /// - [`WorkspaceMode::Persistent { volume_label }`](WorkspaceMode::Persistent)
    ///   requires a non-empty `volume_label`.
    pub fn new(
        tenant_id: TenantId,
        conversation_id: ConversationId,
        workspace_volume_id: VolumeId,
        workspace_mode: WorkspaceMode,
        git_binding_id: Option<GitRepoBindingId>,
        name: Option<String>,
    ) -> Result<Self, String> {
        match (&workspace_mode, git_binding_id) {
            (WorkspaceMode::GitLinked { binding_id }, Some(provided)) => {
                if *binding_id != provided {
                    return Err(format!(
                        "git_binding_id ({provided}) does not match \
                         WorkspaceMode::GitLinked binding_id ({binding_id})"
                    ));
                }
            }
            (WorkspaceMode::GitLinked { .. }, None) => {
                return Err(
                    "WorkspaceMode::GitLinked requires git_binding_id to be Some".to_string(),
                );
            }
            (WorkspaceMode::Ephemeral | WorkspaceMode::Persistent { .. }, Some(_)) => {
                return Err(
                    "git_binding_id must be None for non-GitLinked workspace modes".to_string(),
                );
            }
            (WorkspaceMode::Persistent { volume_label }, None) => {
                if volume_label.trim().is_empty() {
                    return Err(
                        "WorkspaceMode::Persistent requires a non-empty volume_label".to_string(),
                    );
                }
            }
            (WorkspaceMode::Ephemeral, None) => {}
        }

        let id = CanvasSessionId::new();
        let now = Utc::now();
        let mut session = Self {
            id,
            tenant_id: tenant_id.clone(),
            conversation_id,
            workspace_volume_id,
            git_binding_id,
            workspace_mode: workspace_mode.clone(),
            status: CanvasSessionStatus::Initializing,
            name,
            archived: false,
            created_at: now,
            last_active_at: now,
            domain_events: Vec::new(),
        };
        session.domain_events.push(CanvasEvent::SessionCreated {
            session_id: id,
            conversation_id,
            workspace_mode,
            tenant_id,
            created_at: now,
        });
        Ok(session)
    }

    /// Transition status → [`CanvasSessionStatus::Ready`] and touch
    /// `last_active_at`.
    pub fn mark_ready(&mut self) {
        self.status = CanvasSessionStatus::Ready;
        self.last_active_at = Utc::now();
    }

    /// Update the session's display name.
    pub fn rename(&mut self, name: Option<String>) {
        self.name = name;
        self.last_active_at = Utc::now();
    }

    /// Set or clear the archived flag.
    pub fn set_archived(&mut self, archived: bool) {
        self.archived = archived;
        self.last_active_at = Utc::now();
    }

    /// Transition status → [`CanvasSessionStatus::Idle`] and touch
    /// `last_active_at`.
    pub fn mark_idle(&mut self) {
        self.status = CanvasSessionStatus::Idle;
        self.last_active_at = Utc::now();
    }

    /// Emit a [`CanvasEvent::FilesWrittenByAgent`] audit marker and touch
    /// `last_active_at`.
    pub fn mark_files_written(&mut self, count: u32) {
        let now = Utc::now();
        self.last_active_at = now;
        self.domain_events.push(CanvasEvent::FilesWrittenByAgent {
            session_id: self.id,
            file_count: count,
            written_at: now,
        });
    }

    /// Emit a [`CanvasEvent::GitCommitMade`] audit marker and touch
    /// `last_active_at`.
    pub fn mark_commit(&mut self, commit_sha: String) {
        let now = Utc::now();
        self.last_active_at = now;
        self.domain_events.push(CanvasEvent::GitCommitMade {
            session_id: self.id,
            commit_sha,
            committed_at: now,
        });
    }

    /// Emit a [`CanvasEvent::GitPushed`] audit marker and touch
    /// `last_active_at`.
    pub fn mark_push(&mut self, remote: String, ref_name: String) {
        let now = Utc::now();
        self.last_active_at = now;
        self.domain_events.push(CanvasEvent::GitPushed {
            session_id: self.id,
            remote,
            ref_name,
            pushed_at: now,
        });
    }

    /// Transition status → [`CanvasSessionStatus::Terminated`] and emit a
    /// [`CanvasEvent::SessionTerminated`] event.
    pub fn terminate(&mut self) {
        let now = Utc::now();
        self.status = CanvasSessionStatus::Terminated;
        self.last_active_at = now;
        self.domain_events.push(CanvasEvent::SessionTerminated {
            session_id: self.id,
            terminated_at: now,
        });
    }

    /// Drain and return the buffered [`CanvasEvent`]s. The application
    /// service calls this at the aggregate boundary to publish them.
    pub fn take_events(&mut self) -> Vec<CanvasEvent> {
        std::mem::take(&mut self.domain_events)
    }
}

// ============================================================================
// Repository Trait
// ============================================================================

/// Persistence interface for the [`CanvasSession`] aggregate root (ADR-106
/// §Domain Model).
///
/// Implemented by the infrastructure layer (PostgreSQL, see
/// `infrastructure::repositories::postgres_canvas`). All queries are
/// tenant-scoped to enforce the multi-tenant data isolation boundary.
#[async_trait]
pub trait CanvasSessionRepository: Send + Sync {
    /// Upsert a [`CanvasSession`] (insert or update by primary key).
    async fn save(&self, session: &CanvasSession) -> Result<(), RepositoryError>;

    /// Load a session by its aggregate id, or `None` if not found.
    async fn find_by_id(
        &self,
        id: &CanvasSessionId,
    ) -> Result<Option<CanvasSession>, RepositoryError>;

    /// Return the session bound to `conversation_id`, or `None` if the
    /// conversation is not in VibeCode mode / has no canvas.
    async fn find_by_conversation_id(
        &self,
        conversation_id: &ConversationId,
    ) -> Result<Option<CanvasSession>, RepositoryError>;

    /// Return sessions owned by `tenant_id`.
    ///
    /// When `include_archived` is `false` (the default for list endpoints),
    /// only non-archived sessions are returned. When `true`, all sessions
    /// including archived ones are returned.
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        include_archived: bool,
    ) -> Result<Vec<CanvasSession>, RepositoryError>;

    /// Permanently delete a session row.
    async fn delete(&self, id: &CanvasSessionId) -> Result<(), RepositoryError>;
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ephemeral_session() -> CanvasSession {
        CanvasSession::new(
            TenantId::consumer(),
            ConversationId::new(),
            VolumeId::new(),
            WorkspaceMode::Ephemeral,
            None,
            None,
        )
        .expect("valid ephemeral session")
    }

    #[test]
    fn new_ephemeral_session_is_initializing_with_created_event() {
        let mut session = ephemeral_session();
        assert_eq!(session.status, CanvasSessionStatus::Initializing);
        let events = session.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], CanvasEvent::SessionCreated { .. }));
    }

    #[test]
    fn mark_ready_updates_status() {
        let mut session = ephemeral_session();
        session.mark_ready();
        assert_eq!(session.status, CanvasSessionStatus::Ready);
    }

    #[test]
    fn persistent_requires_nonempty_label() {
        let err = CanvasSession::new(
            TenantId::consumer(),
            ConversationId::new(),
            VolumeId::new(),
            WorkspaceMode::Persistent {
                volume_label: String::new(),
            },
            None,
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn git_linked_requires_matching_binding_id() {
        let b = GitRepoBindingId::new();
        let ok = CanvasSession::new(
            TenantId::consumer(),
            ConversationId::new(),
            VolumeId::new(),
            WorkspaceMode::GitLinked { binding_id: b },
            Some(b),
            None,
        );
        assert!(ok.is_ok());

        let mismatch = CanvasSession::new(
            TenantId::consumer(),
            ConversationId::new(),
            VolumeId::new(),
            WorkspaceMode::GitLinked { binding_id: b },
            Some(GitRepoBindingId::new()),
            None,
        );
        assert!(mismatch.is_err());

        let missing = CanvasSession::new(
            TenantId::consumer(),
            ConversationId::new(),
            VolumeId::new(),
            WorkspaceMode::GitLinked { binding_id: b },
            None,
            None,
        );
        assert!(missing.is_err());
    }

    #[test]
    fn new_session_with_name() {
        let session = CanvasSession::new(
            TenantId::consumer(),
            ConversationId::new(),
            VolumeId::new(),
            WorkspaceMode::Ephemeral,
            None,
            Some("My Project".to_string()),
        )
        .expect("valid session with name");
        assert_eq!(session.name, Some("My Project".to_string()));
        assert!(!session.archived);
    }

    #[test]
    fn rename_and_archive() {
        let mut session = ephemeral_session();
        assert_eq!(session.name, None);
        assert!(!session.archived);

        session.rename(Some("Renamed".to_string()));
        assert_eq!(session.name, Some("Renamed".to_string()));

        session.set_archived(true);
        assert!(session.archived);

        session.set_archived(false);
        assert!(!session.archived);
    }
}
