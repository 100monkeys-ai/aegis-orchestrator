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
//! See [`crate::domain::git_repo_tier_limits`] for per-[`crate::domain::iam::ZaruTier`] gating.

use crate::domain::credential::CredentialBindingId;
use crate::domain::git_host_keys::SshHostKey;
use crate::domain::repository::RepositoryError;
use crate::domain::secrets::{RedactedUrl, SensitiveString, SensitiveUrl};
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
        /// The binding's URL without its user info.
        repo_url: RedactedUrl,
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
///
/// `repo_url` may carry a token as user info
/// (`https://user:token@host/repo.git`) and `webhook_secret` is the cleartext
/// secret; both are held in types that print redacted, so the derived
/// `Debug` is safe wherever a binding is recorded (`#[instrument]`).
#[derive(Debug, Clone)]
pub struct GitRepoBinding {
    pub id: GitRepoBindingId,
    pub tenant_id: TenantId,
    pub credential_binding_id: Option<CredentialBindingId>,
    pub repo_url: SensitiveUrl,
    pub git_ref: GitRef,
    pub sparse_paths: Option<Vec<String>>,
    pub volume_id: VolumeId,
    pub label: String,
    pub status: GitRepoStatus,
    pub clone_strategy: CloneStrategy,
    pub last_cloned_at: Option<DateTime<Utc>>,
    pub last_commit_sha: Option<String>,
    pub auto_refresh: bool,
    /// Audit 002 §4.37.13 — cleartext webhook secret. **Transient** — not
    /// stored in the database. The aggregate carries the cleartext only
    /// during in-memory operations: at create time so the application
    /// service can return it to the caller, and at verify time after the
    /// service has decrypted `webhook_secret_ciphertext` via Transit.
    /// Persistence ignores this field; the repository hydrates it as
    /// `None` on every read and the application service decrypts on
    /// demand when verifying webhook deliveries.
    pub webhook_secret: Option<SensitiveString>,
    /// Audit 002 §4.37.13 — Transit ciphertext of the cleartext webhook
    /// secret. Persisted at rest under
    /// `git_repo_bindings.webhook_secret_ciphertext`. Decrypted via
    /// `SecretsManager::decrypt` on every webhook verification call.
    pub webhook_secret_ciphertext: Option<String>,
    /// Audit 002 §4.37.13 — deterministic HMAC-SHA256 of the cleartext
    /// webhook secret used as a lookup index. Replaces the previous
    /// `webhook_secret = $1` query path. The hash is irreversible
    /// without the HMAC key, so DB-only access cannot recover the
    /// cleartext. Stored under `git_repo_bindings.webhook_lookup_hash`.
    pub webhook_lookup_hash: Option<String>,
    /// The SSH host keys given for this repository's remote when the binding
    /// was created. Empty for an HTTPS remote and for an SSH remote on a
    /// well-known host, whose published keys are used
    /// ([`crate::domain::git_host_keys::host_keys_for`]).
    pub ssh_host_keys: Vec<SshHostKey>,
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
        repo_url: impl Into<SensitiveUrl>,
        git_ref: GitRef,
        sparse_paths: Option<Vec<String>>,
        volume_id: VolumeId,
        label: String,
        clone_strategy: CloneStrategy,
        auto_refresh: bool,
        // Audit 002 §4.37.13 — the constructor receives the cleartext
        // (transient — returned to the caller for git-provider setup,
        // never persisted) plus the encrypted/hashed pair (persisted at
        // rest in place of the cleartext).
        webhook_secret: Option<String>,
        webhook_secret_ciphertext: Option<String>,
        webhook_lookup_hash: Option<String>,
    ) -> Self {
        let repo_url = repo_url.into();
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
            webhook_secret: webhook_secret.map(SensitiveString::new),
            webhook_secret_ciphertext,
            webhook_lookup_hash,
            ssh_host_keys: Vec::new(),
            created_at: now,
            updated_at: now,
            domain_events: Vec::new(),
        };
        binding.domain_events.push(GitRepoEvent::BindingCreated {
            id,
            repo_url: RedactedUrl::from(&repo_url),
            git_ref,
            volume_id,
            created_at: now,
        });
        binding
    }

    /// The SSH host keys of the repository's remote, given when the binding
    /// is created.
    pub fn with_ssh_host_keys(mut self, keys: Vec<SshHostKey>) -> Self {
        self.ssh_host_keys = keys;
        self
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

    /// Lookup a binding by the deterministic HMAC-SHA256 hash of its
    /// webhook secret (Audit 002 §4.37.13). Replaces the previous
    /// `find_by_webhook_secret(cleartext)` query.
    ///
    /// Used by the unauthenticated webhook endpoint to route an inbound
    /// push event to its binding before validating the HMAC signature.
    /// The hash is computed by the application layer from the URL path
    /// parameter using the same fixed key the binding was registered
    /// with; an attacker with DB-only access cannot reverse the hash to
    /// recover the cleartext.
    async fn find_by_webhook_lookup_hash(
        &self,
        hash: &str,
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
// A run's repositories (AEGIS ADR-136 G3, G3a, G5a)
// ============================================================================

/// The reserved key of an execution's `input` that carries the run's
/// repositories, `[{"binding_id": "<git repository binding id>", "branch":
/// "<work branch>"?}]` (AEGIS ADR-136 G3). Like `contexts`, it is the
/// platform's and never the agent's: the input schema and the rendered prompt
/// never see it, and an agent state or a child takes its run's.
pub const REPOSITORIES_INPUT_KEY: &str = "repositories";

/// The refusal of a `repositories` value of any other shape (AEGIS ADR-136
/// G3).
pub const REPOSITORIES_SHAPE: &str =
    "'repositories' must be a list of objects naming a binding_id or a url and, optionally, a ref and a branch";

/// One repository a run is given: the binding, the work branch the run
/// works on, and the person its commits are authored as. A start with no
/// `branch` fills in the run's default once (G5a), so every state and child
/// of the run reads the same branch. `author` is the platform's, written
/// when the run's repositories are prepared and never taken from a caller
/// (AEGIS ADR-136 G5d); so are `label`, `ref` (the binding's) and
/// `started_from` (the commit the work branch started from), which the
/// workflow's repository entry reads (AEGIS ADR-141 F3), and `tree_reset`,
/// `purge`, `test_globs` and `tests`: how the preparation leaves the tree
/// and the tracked test files it found (AEGIS ADR-141's Update of
/// 2026-10-14, R1 and R5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRepository {
    pub binding_id: GitRepoBindingId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<RunAuthor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_from: Option<String>,
    /// How the preparation resets the tree; none given is [`TreeReset::None`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_reset: Option<TreeReset>,
    /// The glob patterns [`TreeReset::List`] removes untracked and ignored
    /// paths by, as git's pathspec matches them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge: Option<Vec<String>>,
    /// The glob patterns the preparation lists [`Self::tests`] by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_globs: Option<Vec<String>>,
    /// The tracked files matching `test_globs` once the tree is prepared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<Vec<String>>,
}

impl RunRepository {
    /// Whether the entry names a field only the platform writes: `author`
    /// (AEGIS ADR-136 G5d), `label`, `ref`, `started_from` (AEGIS ADR-141
    /// F3), `tree_reset`, `purge`, `test_globs` or `tests` (its Update of
    /// 2026-10-14, R1 and R5). A starting tool's caller and a schedule never
    /// name one.
    pub fn names_a_platform_field(&self) -> bool {
        self.author.is_some()
            || self.label.is_some()
            || self.git_ref.is_some()
            || self.started_from.is_some()
            || self.tree_reset.is_some()
            || self.purge.is_some()
            || self.test_globs.is_some()
            || self.tests.is_some()
    }
}

/// How a run's preparation leaves its repository's tree after the work
/// branch is checked out (AEGIS ADR-141's Update of 2026-10-14, R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TreeReset {
    /// `git reset --hard` to the start, then `git clean -ffdx`: every
    /// untracked and ignored path removed.
    Hard,
    /// `git clean -ffd`, then `git clean -ffdx` limited to each `purge`
    /// pattern.
    List,
    /// `git clean -ffd`: untracked paths that are not ignored removed.
    #[default]
    None,
}

impl TreeReset {
    /// The input key a run names it by.
    pub const INPUT_KEY: &'static str = "tree_reset";

    /// Read a run's `tree_reset`, refused in the Update's words when it is
    /// not one of the three.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hard" => Ok(Self::Hard),
            "list" => Ok(Self::List),
            "none" => Ok(Self::None),
            other => Err(tree_reset_refusal(other)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::List => "list",
            Self::None => "none",
        }
    }
}

/// The input key a run names its purge patterns by.
pub const PURGE_INPUT_KEY: &str = "purge";

/// The input key a run names its test globs by.
pub const TEST_GLOBS_INPUT_KEY: &str = "test_globs";

fn tree_reset_refusal(value: &str) -> String {
    format!("tree_reset must be one of hard, list or none; this run names '{value}'")
}

/// Refuse a purge pattern that names a path outside the repository: an
/// absolute one, one holding a `..` component, or an empty one.
pub fn check_purge_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() || pattern.starts_with('/') || pattern.split('/').any(|part| part == "..")
    {
        return Err(format!(
            "a purge pattern names a path inside the repository; '{pattern}' does not"
        ));
    }
    Ok(())
}

/// What a run's preparation does to its repositories' trees, read from the
/// run's input before anything is held (AEGIS ADR-141's Update of
/// 2026-10-14, R1 and R5).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunTreePreparation {
    pub tree_reset: TreeReset,
    pub purge: Vec<String>,
    /// `None` when neither the run nor its workflow names test globs: no
    /// test files are listed.
    pub test_globs: Option<Vec<String>>,
}

impl RunTreePreparation {
    /// Read `tree_reset`, `purge` and `test_globs` from the run's `input`,
    /// each falling back to the default `input_schema` declares for its key,
    /// then to `none`, no patterns and no test globs. An unknown
    /// `tree_reset`, or a pattern naming a path outside the repository, is
    /// refused with its sentence.
    pub fn from_input(
        input: &serde_json::Value,
        input_schema: Option<&serde_json::Value>,
    ) -> Result<Self, String> {
        let value = |key: &str| {
            input.get(key).cloned().or_else(|| {
                input_schema
                    .and_then(|schema| schema.get("properties"))
                    .and_then(|properties| properties.get(key))
                    .and_then(|property| property.get("default"))
                    .cloned()
            })
        };
        let tree_reset = match value(TreeReset::INPUT_KEY) {
            None | Some(serde_json::Value::Null) => TreeReset::None,
            Some(serde_json::Value::String(name)) => TreeReset::parse(&name)?,
            Some(other) => return Err(tree_reset_refusal(&other.to_string())),
        };
        let patterns = |key: &str, noun: &str| -> Result<Option<Vec<String>>, String> {
            let outside = |pattern: &str| {
                format!("{noun} names a path inside the repository; '{pattern}' does not")
            };
            match value(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(serde_json::Value::Array(items)) => items
                    .into_iter()
                    .map(|item| match item {
                        serde_json::Value::String(pattern) => check_purge_pattern(&pattern)
                            .map(|()| pattern.clone())
                            .map_err(|_| outside(&pattern)),
                        other => Err(outside(&other.to_string())),
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Some),
                Some(other) => Err(outside(&other.to_string())),
            }
        };
        Ok(Self {
            tree_reset,
            purge: patterns(PURGE_INPUT_KEY, "a purge pattern")?.unwrap_or_default(),
            test_globs: patterns(TEST_GLOBS_INPUT_KEY, "a test glob")?,
        })
    }

    /// Write this preparation onto a run's entry, replacing whatever it
    /// carried; `tests` is left to the preparation. Today's step (`none`,
    /// no patterns, no test globs) leaves the entry as it was before these
    /// fields existed.
    pub fn apply_to(&self, entry: &mut RunRepository) {
        entry.tree_reset = (self.tree_reset != TreeReset::None).then_some(self.tree_reset);
        entry.purge = (!self.purge.is_empty()).then(|| self.purge.clone());
        entry.test_globs = self.test_globs.clone();
        entry.tests = None;
    }
}

/// The person a run's commits are authored as: `name <email>` (AEGIS
/// ADR-136 G5d).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAuthor {
    pub name: String,
    pub email: String,
}

impl RunAuthor {
    /// An author, when both parts can stand in a commit's `name <email>`:
    /// not empty, and holding no `<`, `>` or control character.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Option<Self> {
        let (name, email) = (name.into(), email.into());
        (is_author_part(&name) && is_author_part(&email)).then_some(Self { name, email })
    }
}

/// Whether `part` can be a commit author's name or email: not empty, and
/// holding no `<`, `>` or control character, which git refuses or strips.
fn is_author_part(part: &str) -> bool {
    !part.trim().is_empty() && !part.chars().any(|c| c == '<' || c == '>' || c.is_control())
}

/// Read a `repositories` value as G3 admits it: a list of objects whose only
/// keys are `binding_id` (a UUID), optionally `branch` (a name git accepts
/// for a branch) and, as the platform stores them, `author` (an object of
/// exactly `name` and `email`, each fit for a commit, G5d) and `label`,
/// `ref` and `started_from` (strings, AEGIS ADR-141 F3), `tree_reset` (one
/// of `hard`, `list` or `none`) and `purge`, `test_globs` and `tests` (lists
/// of strings, its Update of 2026-10-14). Anything else answers
/// [`REPOSITORIES_SHAPE`].
pub fn parse_run_repositories(
    value: &serde_json::Value,
) -> Result<Vec<RunRepository>, &'static str> {
    let serde_json::Value::Array(items) = value else {
        return Err(REPOSITORIES_SHAPE);
    };
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let serde_json::Value::Object(map) = item else {
            return Err(REPOSITORIES_SHAPE);
        };
        if map.keys().any(|key| {
            !matches!(
                key.as_str(),
                "binding_id"
                    | "branch"
                    | "author"
                    | "label"
                    | "ref"
                    | "started_from"
                    | "tree_reset"
                    | "purge"
                    | "test_globs"
                    | "tests"
            )
        }) {
            return Err(REPOSITORIES_SHAPE);
        }
        let binding_id = match map.get("binding_id") {
            Some(serde_json::Value::String(id)) => {
                Uuid::parse_str(id).map_err(|_| REPOSITORIES_SHAPE)?
            }
            _ => return Err(REPOSITORIES_SHAPE),
        };
        let branch = match map.get("branch") {
            None => None,
            Some(serde_json::Value::String(branch)) if is_branch_name(branch) => {
                Some(branch.clone())
            }
            Some(_) => return Err(REPOSITORIES_SHAPE),
        };
        let author = match map.get("author") {
            None => None,
            Some(serde_json::Value::Object(author))
                if author.len() == 2
                    && author.contains_key("name")
                    && author.contains_key("email") =>
            {
                match (author.get("name"), author.get("email")) {
                    (
                        Some(serde_json::Value::String(name)),
                        Some(serde_json::Value::String(email)),
                    ) => Some(
                        RunAuthor::new(name.clone(), email.clone()).ok_or(REPOSITORIES_SHAPE)?,
                    ),
                    _ => return Err(REPOSITORIES_SHAPE),
                }
            }
            Some(_) => return Err(REPOSITORIES_SHAPE),
        };
        let text = |key: &str| match map.get(key) {
            None => Ok(None),
            Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
            Some(_) => Err(REPOSITORIES_SHAPE),
        };
        let texts = |key: &str| match map.get(key) {
            None => Ok(None),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    serde_json::Value::String(value) => Ok(value.clone()),
                    _ => Err(REPOSITORIES_SHAPE),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(_) => Err(REPOSITORIES_SHAPE),
        };
        entries.push(RunRepository {
            binding_id: GitRepoBindingId(binding_id),
            branch,
            author,
            label: text("label")?,
            git_ref: text("ref")?,
            started_from: text("started_from")?,
            tree_reset: match map.get(TreeReset::INPUT_KEY) {
                None => None,
                Some(serde_json::Value::String(name)) => {
                    Some(TreeReset::parse(name).map_err(|_| REPOSITORIES_SHAPE)?)
                }
                Some(_) => return Err(REPOSITORIES_SHAPE),
            },
            purge: texts(PURGE_INPUT_KEY)?,
            test_globs: texts(TEST_GLOBS_INPUT_KEY)?,
            tests: texts("tests")?,
        });
    }
    Ok(entries)
}

/// One entry of a run's `repositories` as a caller may give it: a binding
/// by its id (AEGIS ADR-136 G3), or a repository by its URL, whose binding
/// the platform finds or makes before the run starts (AEGIS ADR-141 F2a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestedRepository {
    Binding(RunRepository),
    Url(RepositoryByUrl),
}

/// A repository named by its URL: `{"url": "<repository URL>", "ref":
/// "<branch>"?, "branch": "<work branch>"?}`, `ref` `main` when not given
/// (AEGIS ADR-141 F2a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryByUrl {
    pub url: String,
    pub git_ref: String,
    pub branch: Option<String>,
}

/// Read a `repositories` value as a caller may give it: every entry G3
/// admits, and beside them entries naming a repository by `url` with an
/// optional `ref` (a branch name, default `main`) and `branch`. An entry
/// naming both `binding_id` and `url`, a URL that is not an `https://` or
/// `git@host:path` one or that carries user info, or anything else answers
/// [`REPOSITORIES_SHAPE`].
pub fn parse_requested_repositories(
    value: &serde_json::Value,
) -> Result<Vec<RequestedRepository>, &'static str> {
    let serde_json::Value::Array(items) = value else {
        return Err(REPOSITORIES_SHAPE);
    };
    let mut entries = Vec::with_capacity(items.len());
    for item in items {
        let serde_json::Value::Object(map) = item else {
            return Err(REPOSITORIES_SHAPE);
        };
        if map.contains_key("url") {
            entries.push(RequestedRepository::Url(parse_repository_by_url(map)?));
        } else {
            let mut entry = parse_run_repositories(&serde_json::Value::Array(vec![item.clone()]))?;
            entries.push(RequestedRepository::Binding(entry.remove(0)));
        }
    }
    Ok(entries)
}

/// One entry naming a repository by its URL: only `url`, `ref` and
/// `branch`, the URL one a binding may name, with no user info, and naming
/// a repository.
fn parse_repository_by_url(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<RepositoryByUrl, &'static str> {
    if map
        .keys()
        .any(|key| !matches!(key.as_str(), "url" | "ref" | "branch"))
    {
        return Err(REPOSITORIES_SHAPE);
    }
    let url = match map.get("url") {
        Some(serde_json::Value::String(url))
            if validate_repo_url(url).is_ok()
                && !carries_user_info(url)
                && repository_name(url).is_some() =>
        {
            url.clone()
        }
        _ => return Err(REPOSITORIES_SHAPE),
    };
    let branch_name = |key: &str| match map.get(key) {
        None => Ok(None),
        Some(serde_json::Value::String(name)) if is_branch_name(name) => Ok(Some(name.clone())),
        Some(_) => Err(REPOSITORIES_SHAPE),
    };
    Ok(RepositoryByUrl {
        url,
        git_ref: branch_name("ref")?.unwrap_or_else(|| "main".to_string()),
        branch: branch_name("branch")?,
    })
}

/// Whether an `https://` URL carries user info (`user:token@`), which a run's
/// entry, kept in its input, must not.
fn carries_user_info(url: &str) -> bool {
    url.strip_prefix("https://")
        .map(|rest| rest.split('/').next().unwrap_or("").contains('@'))
        .unwrap_or(false)
}

/// The host, in lower case, and the path of a repository URL, without user
/// info, a trailing `/` or `.git`; `None` for a URL of no known form.
fn repository_location(url: &str) -> Option<(&'static str, String, String)> {
    let url = url.trim();
    let (form, host, path) = if let Some(rest) = url.strip_prefix("https://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit('@').next().unwrap_or(authority);
        ("https", host, path)
    } else {
        let (host, path) = url.strip_prefix("git@")?.split_once(':')?;
        ("ssh", host, path)
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    Some((form, host.to_ascii_lowercase(), path.to_string()))
}

/// The name of the repository at `url`: its last path segment without
/// `.git`, when that can name a directory (AEGIS ADR-141 F2a).
pub fn repository_name(url: &str) -> Option<String> {
    let (_, _, path) = repository_location(url)?;
    let name = path.rsplit('/').next()?;
    is_mountable_label(name).then(|| name.to_string())
}

/// Whether two repository URLs name one repository: the same host, in any
/// case, and the same path, with or without user info, a trailing `/` or
/// `.git`.
pub fn same_repository(a: &str, b: &str) -> bool {
    match (repository_location(a), repository_location(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Whether `name` is a name git accepts for a branch (`refs/heads/<name>`).
pub fn is_branch_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && git2::Reference::is_valid_name(&format!("refs/heads/{name}"))
}

/// Whether `label` can name the directory `/workspace/<label>` (AEGIS
/// ADR-136 G3a): letters, digits, '.', '_' or '-', and not `.` or `..`.
pub fn is_mountable_label(label: &str) -> bool {
    !label.is_empty()
        && label != "."
        && label != ".."
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The run's default work branch (AEGIS ADR-136 G5, G5a): `aegis/` and the
/// first eight characters of the run's execution id.
pub fn default_work_branch(run: Uuid) -> String {
    format!("aegis/{}", &run.to_string()[..8])
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
        // Bracketed IPv6 literal per RFC-3986: `[…]` may contain `:` and
        // must be handled before the generic `/:`-split. Anything else is
        // a hostname or IPv4 literal where the first `/` or `:` ends it.
        let host = if let Some(bracket_body) = after_userinfo.strip_prefix('[') {
            let end = bracket_body
                .find(']')
                .ok_or_else(|| "HTTPS URL has unterminated IPv6 bracket".to_string())?;
            &bracket_body[..end]
        } else {
            after_userinfo.split(['/', ':']).next().unwrap_or("")
        };
        if host.is_empty() {
            return Err("HTTPS URL must have a non-empty host".to_string());
        }
        if is_ip_address(host) {
            return Err("IP addresses are not allowed in repo URLs".to_string());
        }
        return Ok(());
    }

    // The URL is not repeated: it may hold a credential. A scheme is named
    // only when it is one a repository URL is known to use.
    const KNOWN_SCHEMES: &[&str] = &[
        "http", "ftp", "ftps", "file", "ssh", "git", "git+ssh", "ssh+git", "svn", "rsync",
    ];
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .filter(|scheme| KNOWN_SCHEMES.contains(&scheme.as_str()));
    Err(match scheme {
        Some(scheme) => {
            format!("repo URL must use https:// or git@host:path; this one uses {scheme}://")
        }
        None => "repo URL must use https:// or git@host:path".to_string(),
    })
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

    /// A `BindingCreated` event as it is serialised. Captured from the
    /// derived serde form while `repo_url` was a `String`.
    const BINDING_CREATED_FIXTURE: &str = r#"{"BindingCreated":{"id":"00000000-0000-0000-0000-000000000001","repo_url":"https://user:Mk7-git-pat-marker@github.com/o/r.git","git_ref":{"Branch":"main"},"volume_id":"00000000-0000-0000-0000-000000000002","created_at":"2026-09-28T00:00:00Z"}}"#;

    #[test]
    fn git_repo_event_debug_does_not_print_the_url_credential() {
        let event: GitRepoEvent = serde_json::from_str(BINDING_CREATED_FIXTURE).unwrap();
        let printed = format!("{event:?}");
        assert!(
            !printed.contains("Mk7-git-pat-marker"),
            "GitRepoEvent's Debug printed the repository URL's credential: {printed}"
        );
        assert!(
            printed.contains("github.com"),
            "Debug lost the repository host: {printed}"
        );
    }

    /// The event's wire form is the fixture's with only the URL's user info
    /// gone, also for an event read back from the old form.
    #[test]
    fn git_repo_event_wire_form_drops_only_the_user_info() {
        let event: GitRepoEvent = serde_json::from_str(BINDING_CREATED_FIXTURE).unwrap();
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            BINDING_CREATED_FIXTURE.replace("user:Mk7-git-pat-marker@", "")
        );
    }

    #[test]
    fn git_repo_binding_debug_does_not_print_the_url_credential_or_webhook_secret() {
        let binding = GitRepoBinding::new(
            TenantId::consumer(),
            None,
            "https://user:Mk7-git-pat-marker@github.com/o/r.git".to_string(),
            GitRef::default(),
            None,
            VolumeId::new(),
            "label".to_string(),
            CloneStrategy::Libgit2,
            true,
            Some("Mk7-webhook-secret-marker".to_string()),
            Some("vault:v1:ciphertext".to_string()),
            Some("lookup-hash".to_string()),
        );
        let printed = format!("{binding:?}");
        for marker in ["Mk7-git-pat-marker", "Mk7-webhook-secret-marker"] {
            assert!(
                !printed.contains(marker),
                "GitRepoBinding's Debug printed a credential: {printed}"
            );
        }
        assert!(
            printed.contains("github.com"),
            "Debug lost the repository host: {printed}"
        );
    }

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
            None,
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

    /// An event published for a binding whose URL holds user info shows none
    /// of it once serialised, the form every subscriber and stream reads.
    #[test]
    fn binding_created_event_serialises_the_url_without_user_info() {
        let mut binding = GitRepoBinding::new(
            TenantId::system(),
            None,
            "https://Mk7-event-user:Mk7-event-token@git.example.invalid/o/r.git".to_string(),
            GitRef::Branch("main".to_string()),
            None,
            VolumeId::new(),
            "event-url".to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
            None,
            None,
        );
        let events = binding.take_events();
        let json = serde_json::to_string(&events).unwrap();
        let bus_json = serde_json::to_string(
            &crate::infrastructure::event_bus::DomainEvent::GitRepo(events[0].clone()),
        )
        .unwrap();
        for (form, text) in [("event", &json), ("bus event", &bus_json)] {
            assert!(
                !text.contains("Mk7-event"),
                "the serialised {form} holds the URL's user info"
            );
            assert!(
                text.contains("git.example.invalid/o/r.git"),
                "the serialised {form} lost the repository's address"
            );
        }
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

    /// A URL that is refused is not repeated in the error, which the caller
    /// and an agent read: it may hold a credential, as user info or as a
    /// token pasted where the URL belongs.
    #[test]
    fn a_refused_url_is_not_repeated_in_the_error() {
        for url in [
            "http://someone:Mk6-password-marker@git.example.invalid/o/r.git",
            "ftp://Mk6-token-marker@git.example.invalid/o/r.git",
            "Mk6-bare-token-marker",
            "ghp_Mk6-pasted-token-marker",
        ] {
            let error = validate_repo_url(url).expect_err("the URL is refused");
            assert!(
                !error.contains("Mk6"),
                "the refusal repeats the URL it refused: {error}"
            );
        }
    }

    const RUN_BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";

    /// AEGIS ADR-136 G5d: an entry's stored author is read with it.
    #[test]
    fn a_stored_author_is_read_with_the_entry() {
        let value = serde_json::json!([{
            "binding_id": RUN_BINDING,
            "branch": "aegis/1234abcd",
            "author": { "name": "Ada Lovelace", "email": "ada@example.com" }
        }]);
        let entries = parse_run_repositories(&value)
            .unwrap_or_else(|e| panic!("an entry with a stored author was refused: {e}"));
        assert_eq!(
            entries[0].author,
            RunAuthor::new("Ada Lovelace", "ada@example.com"),
            "the entry's author was not read"
        );
        assert_eq!(
            serde_json::to_value(&entries).unwrap(),
            value,
            "the entry does not write back as it was read"
        );
    }

    /// AEGIS ADR-136 G5d: a malformed author is refused with G3's sentence,
    /// every shape of it reported before failing.
    #[test]
    fn a_malformed_author_is_refused_with_the_sentence() {
        let mut admitted = Vec::new();
        for author in [
            serde_json::json!("Ada <ada@example.com>"),
            serde_json::json!(null),
            serde_json::json!({ "name": "Ada" }),
            serde_json::json!({ "email": "ada@example.com" }),
            serde_json::json!({ "name": "Ada", "email": "ada@example.com", "extra": 1 }),
            serde_json::json!({ "name": "", "email": "ada@example.com" }),
            serde_json::json!({ "name": "  ", "email": "ada@example.com" }),
            serde_json::json!({ "name": "Ada", "email": "" }),
            serde_json::json!({ "name": "Ada <x>", "email": "ada@example.com" }),
            serde_json::json!({ "name": "Ada", "email": "<ada@example.com>" }),
            serde_json::json!({ "name": "Ada\nEvil", "email": "ada@example.com" }),
            serde_json::json!({ "name": 7, "email": "ada@example.com" }),
        ] {
            let value = serde_json::json!([{ "binding_id": RUN_BINDING, "author": author }]);
            if parse_run_repositories(&value) != Err(REPOSITORIES_SHAPE) {
                admitted.push(author);
            }
        }
        assert!(
            admitted.is_empty(),
            "a malformed author was not refused with G3's sentence: {admitted:?}"
        );
    }

    #[test]
    fn an_author_part_holding_angle_brackets_or_control_characters_makes_no_author() {
        assert!(RunAuthor::new("Ada Lovelace", "ada@example.com").is_some());
        for (name, email) in [
            ("", "ada@example.com"),
            ("Ada", " "),
            ("Ada <a>", "ada@example.com"),
            ("Ada", "ada@example.com>"),
            ("Ada\u{7}", "ada@example.com"),
        ] {
            assert_eq!(
                RunAuthor::new(name, email),
                None,
                "{name:?} <{email:?}> made an author"
            );
        }
    }

    /// AEGIS ADR-141 F2a: an entry naming a repository by URL is admitted
    /// beside one naming a binding, its ref `main` when not given.
    #[test]
    fn a_repository_named_by_url_is_admitted_with_its_ref_defaulting_to_main() {
        let value = serde_json::json!([
            { "binding_id": RUN_BINDING },
            { "url": "https://github.com/100monkeys-ai/aegis-examples.git" },
            { "url": "git@github.com:100monkeys-ai/zaru-client.git", "ref": "develop", "branch": "feature/x" }
        ]);
        let entries = parse_requested_repositories(&value)
            .unwrap_or_else(|e| panic!("the entries naming repositories by URL were refused: {e}"));
        assert_eq!(
            entries[1..],
            [
                RequestedRepository::Url(RepositoryByUrl {
                    url: "https://github.com/100monkeys-ai/aegis-examples.git".to_string(),
                    git_ref: "main".to_string(),
                    branch: None,
                }),
                RequestedRepository::Url(RepositoryByUrl {
                    url: "git@github.com:100monkeys-ai/zaru-client.git".to_string(),
                    git_ref: "develop".to_string(),
                    branch: Some("feature/x".to_string()),
                }),
            ],
            "the URL entries were not read as given"
        );
        assert!(
            matches!(&entries[0], RequestedRepository::Binding(entry) if entry.binding_id.0.to_string() == RUN_BINDING),
            "the binding entry beside them was not read: {:?}",
            entries[0]
        );
    }

    /// AEGIS ADR-141 F2a: an entry naming both a binding and a URL, or a URL
    /// of any other shape, is refused with the sentence; a prepared list
    /// carries bindings only.
    #[test]
    fn a_url_entry_of_any_other_shape_is_refused_with_the_sentence() {
        let mut admitted = Vec::new();
        for entry in [
            serde_json::json!({ "binding_id": RUN_BINDING, "url": "https://github.com/o/r.git" }),
            serde_json::json!({ "url": "http://github.com/o/r.git" }),
            serde_json::json!({ "url": "https://x-access-token:Mk9@github.com/o/r.git" }),
            serde_json::json!({ "url": "https://github.com/" }),
            serde_json::json!({ "url": 7 }),
            serde_json::json!({ "url": "https://github.com/o/r.git", "ref": "a..b" }),
            serde_json::json!({ "url": "https://github.com/o/r.git", "ref": 7 }),
            serde_json::json!({ "url": "https://github.com/o/r.git", "branch": "a..b" }),
            serde_json::json!({ "url": "https://github.com/o/r.git", "label": "r" }),
            serde_json::json!({ "url": "https://github.com/o/r.git", "author": { "name": "A", "email": "a@b" } }),
        ] {
            if parse_requested_repositories(&serde_json::json!([entry])) != Err(REPOSITORIES_SHAPE)
            {
                admitted.push(entry);
            }
        }
        assert!(
            admitted.is_empty(),
            "an entry was not refused with the sentence: {admitted:?}"
        );
        assert_eq!(
            parse_run_repositories(&serde_json::json!([{ "url": "https://github.com/o/r.git" }])),
            Err(REPOSITORIES_SHAPE),
            "a prepared list admitted an entry naming a URL"
        );
        assert_eq!(
            REPOSITORIES_SHAPE,
            "'repositories' must be a list of objects naming a binding_id or a url and, optionally, a ref and a branch"
        );
    }

    /// AEGIS ADR-141 F2a: the label of a binding made for a URL is the
    /// repository's name, and two spellings of one URL are one repository.
    #[test]
    fn a_repository_url_names_its_repository_and_matches_its_other_spellings() {
        for (url, name) in [
            (
                "https://github.com/100monkeys-ai/aegis-examples.git",
                "aegis-examples",
            ),
            (
                "https://github.com/100monkeys-ai/aegis-examples",
                "aegis-examples",
            ),
            (
                "https://github.com/100monkeys-ai/aegis-examples/",
                "aegis-examples",
            ),
            (
                "git@github.com:100monkeys-ai/zaru-client.git",
                "zaru-client",
            ),
        ] {
            assert_eq!(repository_name(url).as_deref(), Some(name), "{url}");
        }
        assert!(same_repository(
            "https://GitHub.com/100monkeys-ai/aegis-examples.git",
            "https://github.com/100monkeys-ai/aegis-examples/"
        ));
        assert!(same_repository(
            "https://x-access-token:Mk9@github.com/o/r.git",
            "https://github.com/o/r"
        ));
        assert!(!same_repository(
            "https://github.com/o/r.git",
            "https://github.com/o/r2.git"
        ));
        assert!(!same_repository(
            "https://github.com/o/r.git",
            "https://gitlab.com/o/r.git"
        ));
    }

    /// What `GET /v1/storage/git` answers for a binding's `git_ref`: the
    /// derive's external tag, the variant's name keyed to its value.
    #[test]
    fn a_git_ref_serializes_as_its_variant_name_keyed_to_its_value() {
        for (git_ref, wire) in [
            (
                GitRef::Branch("main".to_string()),
                serde_json::json!({ "Branch": "main" }),
            ),
            (
                GitRef::Tag("v1.0.0".to_string()),
                serde_json::json!({ "Tag": "v1.0.0" }),
            ),
            (
                GitRef::Commit("0123abc".to_string()),
                serde_json::json!({ "Commit": "0123abc" }),
            ),
        ] {
            assert_eq!(serde_json::to_value(&git_ref).unwrap(), wire, "{git_ref:?}");
        }
    }
}
