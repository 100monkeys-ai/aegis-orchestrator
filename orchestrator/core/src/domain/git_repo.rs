// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding Domain (BC-7 Storage Gateway, ADR-081)
//!
//! Domain model for [`GitRepoBinding`] — a user's binding of a git repository
//! to an AEGIS volume. Links an optional [`CredentialBindingId`] (for private
//! repos) with a repository URL, a pinned [`GitRef`], optional
//! [`sparse_paths`](GitRepoBinding::sparse_paths), and a [`VolumeId`] that
//! ultimately holds the cloned working tree.
//!
//! The binding defines the *intent* ("I want this repo as a volume"). The
//! orchestrator handles the *execution* (clone/pull) via the `GitRepoService`
//! application service (Wave A2). Agents see the result as a mounted volume at
//! `/workspace/{label}` and never execute `git clone` or handle credentials —
//! this preserves the Keymaster Pattern (ADR-034).
//!
//! ## Type Map
//!
//! | Type | Role |
//! |------|------|
//! | [`GitRepoBindingId`] | UUID newtype — aggregate root identity |
//! | [`GitRef`] | Branch / Tag / Commit pinning |
//! | [`GitRepoStatus`] | Clone / refresh lifecycle state machine |
//! | [`CloneStrategy`] | Libgit2 primary, EphemeralCli fallback |
//! | [`GitRepoEvent`] | Domain events published to the event bus |
//! | [`GitRepoBinding`] | Aggregate root |
//! | [`GitRepoBindingRepository`] | Repository trait (Postgres impl in infrastructure) |
//!
//! See [`crate::domain::git_repo_tier_limits`] for per-[`ZaruTier`] gating.

use crate::domain::credential::CredentialBindingId;
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::{TenantId, VolumeId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// Value Object — Identity
// ============================================================================

/// Unique identifier for a [`GitRepoBinding`] aggregate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitRepoBindingId(pub Uuid);

impl GitRepoBindingId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for GitRepoBindingId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for GitRepoBindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Value Object — GitRef
// ============================================================================

/// Git reference pinning mode for a [`GitRepoBinding`] (ADR-081 §Sub-Decision 4).
///
/// - [`GitRef::Branch`] — tracks HEAD of the branch; refresh fetches and fast-forwards.
/// - [`GitRef::Tag`] — fixed tag; refresh is a no-op unless the tag was force-pushed.
/// - [`GitRef::Commit`] — pinned to exact SHA; refresh is always a no-op after clone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitRef {
    Branch(String),
    Tag(String),
    Commit(String),
}

impl Default for GitRef {
    fn default() -> Self {
        Self::Branch("main".to_string())
    }
}

// ============================================================================
// Value Object — GitRepoStatus
// ============================================================================

/// Lifecycle state of a [`GitRepoBinding`] (ADR-081 §Sub-Decision 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitRepoStatus {
    /// Binding row created, clone not yet started.
    Pending,
    /// Background clone task is running.
    Cloning,
    /// Clone or refresh completed successfully; volume is mounted and current.
    Ready,
    /// Background fetch+checkout is running.
    Refreshing,
    /// Clone or refresh failed. The `error` field carries the user-visible message.
    Failed { error: String },
    /// Binding (and associated volume) has been deleted.
    Deleted,
}

// ============================================================================
// Value Object — CloneStrategy
// ============================================================================

/// Which git implementation the orchestrator uses to clone/fetch the binding
/// (ADR-081 §Sub-Decision 2).
///
/// [`CloneStrategy::Libgit2`] is the default — in-process clones with no
/// container overhead. [`CloneStrategy::EphemeralCli`] is the fallback for
/// LFS / submodule / custom-git-config edge cases; it routes through the
/// `EphemeralCliTool` (ADR-053). The `reason` string records *why* the
/// fallback was selected so we can audit and improve the heuristic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloneStrategy {
    Libgit2,
    EphemeralCli { reason: String },
}

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events published by [`GitRepoBinding`] state transitions (ADR-081
/// §Domain Events, plus Wave B2 `CommitMade` / `PushCompleted`).
///
/// Emitted by the aggregate during state changes and drained by the
/// application service via [`GitRepoBinding::take_events`] for publication to
/// the event bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GitRepoEvent {
    BindingCreated {
        id: GitRepoBindingId,
        repo_url: String,
        git_ref: GitRef,
        volume_id: VolumeId,
        created_at: DateTime<Utc>,
    },
    CloneStarted {
        id: GitRepoBindingId,
        volume_id: VolumeId,
        strategy: CloneStrategy,
        started_at: DateTime<Utc>,
    },
    CloneCompleted {
        id: GitRepoBindingId,
        commit_sha: String,
        duration_ms: u64,
        completed_at: DateTime<Utc>,
    },
    CloneFailed {
        id: GitRepoBindingId,
        error: String,
        failed_at: DateTime<Utc>,
    },
    RefreshStarted {
        id: GitRepoBindingId,
        started_at: DateTime<Utc>,
    },
    RefreshCompleted {
        id: GitRepoBindingId,
        old_commit_sha: String,
        new_commit_sha: String,
        duration_ms: u64,
        completed_at: DateTime<Utc>,
    },
    RefreshFailed {
        id: GitRepoBindingId,
        error: String,
        failed_at: DateTime<Utc>,
    },
    WebhookReceived {
        id: GitRepoBindingId,
        source: String,
        received_at: DateTime<Utc>,
    },
    BindingDeleted {
        id: GitRepoBindingId,
        volume_id: VolumeId,
        deleted_at: DateTime<Utc>,
    },
    /// Canvas git-write: a commit was created on the bound working tree
    /// (Wave B2 — extends ADR-081 for the Vibe-Code Canvas).
    CommitMade {
        id: GitRepoBindingId,
        commit_sha: String,
        committed_at: DateTime<Utc>,
    },
    /// Canvas git-write: a push to the remote completed successfully
    /// (Wave B2 — extends ADR-081 for the Vibe-Code Canvas).
    PushCompleted {
        id: GitRepoBindingId,
        remote: String,
        ref_name: String,
        pushed_at: DateTime<Utc>,
    },
}

// ============================================================================
// Aggregate Root — GitRepoBinding
// ============================================================================

/// Aggregate root for a user's binding of a git repository to an AEGIS volume
/// (ADR-081 §Domain Model).
///
/// ## Invariants
///
/// - `credential_binding_id` is required for private repos, `None` for public repos.
/// - If set, `credential_binding_id` MUST reference an active
///   `UserCredentialBinding` with `CredentialType::Secret` (for PATs) or a
///   future SSH-key credential type.
/// - `volume_id` MUST reference a `Volume` with SeaweedFS backend and
///   `VolumeOwnership::Persistent`.
/// - `repo_url` MUST pass [`validate_repo_url`] (HTTPS or SSH, no IP hosts).
/// - Only the owning tenant can modify or delete.
#[derive(Debug, Clone)]
pub struct GitRepoBinding {
    pub id: GitRepoBindingId,
    pub tenant_id: TenantId,
    pub credential_binding_id: Option<CredentialBindingId>,
    pub repo_url: String,
    pub git_ref: GitRef,
    pub sparse_paths: Option<Vec<String>>,
    pub volume_id: VolumeId,
    pub label: String,
    pub status: GitRepoStatus,
    pub clone_strategy: CloneStrategy,
    pub last_cloned_at: Option<DateTime<Utc>>,
    pub last_commit_sha: Option<String>,
    pub auto_refresh: bool,
    pub webhook_secret: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Event buffer. Drained by [`take_events`](Self::take_events) at the
    /// aggregate boundary and published to the event bus.
    pub domain_events: Vec<GitRepoEvent>,
}

impl GitRepoBinding {
    /// Construct a new binding in [`GitRepoStatus::Pending`] state and buffer
    /// a [`GitRepoEvent::BindingCreated`] event.
    ///
    /// The caller is responsible for validating `repo_url` with
    /// [`validate_repo_url`] before invoking this constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tenant_id: TenantId,
        credential_binding_id: Option<CredentialBindingId>,
        repo_url: String,
        git_ref: GitRef,
        sparse_paths: Option<Vec<String>>,
        volume_id: VolumeId,
        label: String,
        clone_strategy: CloneStrategy,
        auto_refresh: bool,
        webhook_secret: Option<String>,
    ) -> Self {
        let id = GitRepoBindingId::new();
        let now = Utc::now();
        let mut binding = Self {
            id,
            tenant_id,
            credential_binding_id,
            repo_url: repo_url.clone(),
            git_ref: git_ref.clone(),
            sparse_paths,
            volume_id,
            label,
            status: GitRepoStatus::Pending,
            clone_strategy,
            last_cloned_at: None,
            last_commit_sha: None,
            auto_refresh,
            webhook_secret,
            created_at: now,
            updated_at: now,
            domain_events: Vec::new(),
        };
        binding.domain_events.push(GitRepoEvent::BindingCreated {
            id,
            repo_url,
            git_ref,
            volume_id,
            created_at: now,
        });
        binding
    }

    /// Transition `Pending → Cloning`. Emits [`GitRepoEvent::CloneStarted`].
    pub fn start_clone(&mut self) {
        let now = Utc::now();
        self.status = GitRepoStatus::Cloning;
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::CloneStarted {
            id: self.id,
            volume_id: self.volume_id,
            strategy: self.clone_strategy.clone(),
            started_at: now,
        });
    }

    /// Transition `Cloning → Ready` with the resolved HEAD `sha`. Records
    /// `last_cloned_at` and `last_commit_sha`. Emits
    /// [`GitRepoEvent::CloneCompleted`].
    pub fn complete_clone(&mut self, sha: String, duration_ms: u64) {
        let now = Utc::now();
        self.status = GitRepoStatus::Ready;
        self.last_cloned_at = Some(now);
        self.last_commit_sha = Some(sha.clone());
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::CloneCompleted {
            id: self.id,
            commit_sha: sha,
            duration_ms,
            completed_at: now,
        });
    }

    /// Transition `Cloning → Failed { error }`. Emits
    /// [`GitRepoEvent::CloneFailed`].
    pub fn fail_clone(&mut self, error: String) {
        let now = Utc::now();
        self.status = GitRepoStatus::Failed {
            error: error.clone(),
        };
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::CloneFailed {
            id: self.id,
            error,
            failed_at: now,
        });
    }

    /// Transition `Ready → Refreshing`. Emits [`GitRepoEvent::RefreshStarted`].
    pub fn start_refresh(&mut self) {
        let now = Utc::now();
        self.status = GitRepoStatus::Refreshing;
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::RefreshStarted {
            id: self.id,
            started_at: now,
        });
    }

    /// Transition `Refreshing → Ready` with the new HEAD `new_sha`. Updates
    /// `last_commit_sha`. Emits [`GitRepoEvent::RefreshCompleted`].
    pub fn complete_refresh(&mut self, old_sha: String, new_sha: String, duration_ms: u64) {
        let now = Utc::now();
        self.status = GitRepoStatus::Ready;
        self.last_cloned_at = Some(now);
        self.last_commit_sha = Some(new_sha.clone());
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::RefreshCompleted {
            id: self.id,
            old_commit_sha: old_sha,
            new_commit_sha: new_sha,
            duration_ms,
            completed_at: now,
        });
    }

    /// Transition `Refreshing → Failed { error }`. Emits
    /// [`GitRepoEvent::RefreshFailed`].
    pub fn fail_refresh(&mut self, error: String) {
        let now = Utc::now();
        self.status = GitRepoStatus::Failed {
            error: error.clone(),
        };
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::RefreshFailed {
            id: self.id,
            error,
            failed_at: now,
        });
    }

    /// Transition any state → `Deleted`. Emits [`GitRepoEvent::BindingDeleted`].
    ///
    /// The associated volume is cascaded by the infrastructure layer's
    /// `ON DELETE CASCADE` and the application service.
    pub fn mark_deleted(&mut self) {
        let now = Utc::now();
        self.status = GitRepoStatus::Deleted;
        self.updated_at = now;
        self.domain_events.push(GitRepoEvent::BindingDeleted {
            id: self.id,
            volume_id: self.volume_id,
            deleted_at: now,
        });
    }

    /// Drain and return the buffered [`GitRepoEvent`]s. The application
    /// service calls this at the aggregate boundary to publish them.
    pub fn take_events(&mut self) -> Vec<GitRepoEvent> {
        std::mem::take(&mut self.domain_events)
    }
}

// ============================================================================
// Repository Trait
// ============================================================================

/// Persistence interface for the [`GitRepoBinding`] aggregate root (ADR-081
/// §Repository Trait).
///
/// Implemented by the infrastructure layer (PostgreSQL, see
/// `infrastructure::repositories::postgres_git_repo`). All queries are
/// tenant-scoped to enforce the multi-tenant data isolation boundary.
#[async_trait]
pub trait GitRepoBindingRepository: Send + Sync {
    /// Upsert a [`GitRepoBinding`] (insert or update by primary key).
    async fn save(&self, binding: &GitRepoBinding) -> Result<(), RepositoryError>;

    /// Load a binding by its aggregate id, or `None` if not found.
    async fn find_by_id(
        &self,
        id: &GitRepoBindingId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError>;

    /// Return all bindings owned by `owner` within `tenant_id`.
    ///
    /// In ADR-081 Phase 1 the binding is owned by the tenant; the `owner`
    /// string parameter is retained for API symmetry with the credential /
    /// volume repositories and is matched against the tenant's owner user
    /// claim at the service layer.
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<Vec<GitRepoBinding>, RepositoryError>;

    /// Lookup a binding by its backing [`VolumeId`] (1:1 in ADR-081).
    async fn find_by_volume_id(
        &self,
        volume_id: &VolumeId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError>;

    /// Lookup a binding by its `webhook_secret` path parameter.
    ///
    /// Used by the unauthenticated webhook endpoint to route an inbound push
    /// event to its binding before validating the HMAC signature.
    async fn find_by_webhook_secret(
        &self,
        secret: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError>;

    /// Count the number of bindings owned by `owner` within `tenant_id`.
    ///
    /// Used by the tier-limit enforcement in
    /// [`crate::domain::git_repo_tier_limits::GitRepoTierLimits`].
    async fn count_by_owner(
        &self,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<u32, RepositoryError>;

    /// Permanently delete a binding. The associated volume is cascaded via
    /// the `ON DELETE CASCADE` on the `volume_id` foreign key.
    async fn delete(&self, id: &GitRepoBindingId) -> Result<(), RepositoryError>;
}

// ============================================================================
// URL Validation (ADR-081 §Security Considerations)
// ============================================================================

/// Validate a git repository URL per ADR-081 §Security.
///
/// Accepts only:
/// - `https://host/path` where `host` is a hostname (not a raw IP address).
/// - `git@host:path` SSH-style shortcut.
///
/// Rejects all other schemes (`file://`, `ftp://`, `http://`, …) to prevent
/// SSRF and exfiltration attacks. Also rejects IP-address hosts since allowing
/// them would bypass the hostname allow-list of downstream policies.
pub fn validate_repo_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("repo URL must not be empty".to_string());
    }

    // SSH shortcut form: user@host:path — only `git@` is accepted.
    if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest
            .split_once(':')
            .ok_or_else(|| "SSH URL must be of the form git@host:path".to_string())?;
        if host.is_empty() || path.is_empty() {
            return Err("SSH URL host and path must both be non-empty".to_string());
        }
        if is_ip_address(host) {
            return Err("IP addresses are not allowed in repo URLs".to_string());
        }
        return Ok(());
    }

    // HTTPS form.
    if let Some(rest) = url.strip_prefix("https://") {
        // Strip optional userinfo (`user:pass@`) before extracting the host.
        let after_userinfo = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
        let host = after_userinfo
            .split(|c: char| c == '/' || c == ':')
            .next()
            .unwrap_or("");
        if host.is_empty() {
            return Err("HTTPS URL must have a non-empty host".to_string());
        }
        if is_ip_address(host) {
            return Err("IP addresses are not allowed in repo URLs".to_string());
        }
        return Ok(());
    }

    Err(format!(
        "repo URL must use https:// or git@ scheme; got: {url}"
    ))
}

/// Return `true` if `host` parses as a bare IPv4 or IPv6 address.
///
/// IPv6 hosts in URLs are normally wrapped in `[…]` per RFC-3986; we accept
/// either the wrapped or unwrapped form here because both are hostile for
/// the purposes of ADR-081's SSRF guard.
fn is_ip_address(host: &str) -> bool {
    let stripped = host.trim_start_matches('[').trim_end_matches(']');
    stripped.parse::<std::net::IpAddr>().is_ok()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_binding() -> GitRepoBinding {
        GitRepoBinding::new(
            TenantId::consumer(),
            None,
            "https://github.com/octocat/Hello-World.git".to_string(),
            GitRef::default(),
            None,
            VolumeId::new(),
            "hello-world".to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
        )
    }

    #[test]
    fn git_ref_default_is_main_branch() {
        assert_eq!(GitRef::default(), GitRef::Branch("main".to_string()));
    }

    #[test]
    fn new_binding_is_pending_with_created_event() {
        let mut binding = sample_binding();
        assert_eq!(binding.status, GitRepoStatus::Pending);
        let events = binding.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], GitRepoEvent::BindingCreated { .. }));
    }

    #[test]
    fn start_clone_transitions_and_emits_event() {
        let mut binding = sample_binding();
        let _ = binding.take_events(); // drain BindingCreated
        binding.start_clone();
        assert_eq!(binding.status, GitRepoStatus::Cloning);
        let events = binding.take_events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], GitRepoEvent::CloneStarted { .. }));
    }

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_repo_url("https://github.com/octocat/Hello-World.git").is_ok());
    }

    #[test]
    fn validate_url_accepts_git_ssh() {
        assert!(validate_repo_url("git@github.com:octocat/Hello-World.git").is_ok());
    }

    #[test]
    fn validate_url_rejects_plain_http() {
        assert!(validate_repo_url("http://github.com/foo/bar").is_err());
    }

    #[test]
    fn validate_url_rejects_ip_host() {
        assert!(validate_repo_url("https://192.168.1.1/foo").is_err());
    }
}
