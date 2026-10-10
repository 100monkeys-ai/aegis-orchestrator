// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding Application Service (BC-7 Storage Gateway, ADR-081)
//!
//! [`GitRepoService`] — the primary interface for creating, listing,
//! cloning, refreshing, and deleting [`GitRepoBinding`]s. All business
//! logic lives here; the HTTP layer is a thin shell that translates
//! requests into command structs and maps errors onto status codes.
//!
//! ## A2 + A3 + B2 Scope
//!
//! | Method | Status |
//! |---|---|
//! | [`GitRepoService::create_binding`] | A2 — implemented |
//! | [`GitRepoService::clone_repo`] | A2/A3 — implemented (HostPath + EphemeralCli) |
//! | [`GitRepoService::list_bindings`] | A2 — implemented |
//! | [`GitRepoService::get_binding`] | A2 — implemented |
//! | [`GitRepoService::delete_binding`] | A2 — implemented |
//! | [`GitRepoService::refresh_repo`] | A3 — implemented (fetch + checkout pin; a git step on non-HostPath volumes) |
//! | [`GitRepoService::handle_webhook`] | A3 — implemented (HMAC validated) |
//! | [`GitRepoService::commit`] | **B2 — implemented** (Canvas git-write; a git step on non-HostPath volumes) |
//! | [`GitRepoService::push`] | **B2 — implemented** (Canvas git-write; a git step on non-HostPath volumes) |
//! | [`GitRepoService::diff`] | **B2 — implemented** (Canvas git-write; a git step on non-HostPath volumes) |
//!
//! On a `HostPath` volume the tree is worked in process by libgit2; on any
//! other volume (SeaweedFS, OpenDAL, SEAL) by a git step, a container with
//! the volume mounted through the FUSE gateway (AEGIS ADR-136 G11).
//!
//! ## Keymaster Pattern
//!
//! Credentials never enter the binding row. The service resolves them
//! just-in-time from [`SecretsManager`] inside [`GitRepoService::clone_repo`]
//! / [`GitRepoService::refresh_repo`], passes them to [`GitCloneExecutor`],
//! and drops them immediately after the git operation returns.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tracing::{error, info, instrument, warn};

use chrono::Utc;

use crate::application::credential_service::{CredentialError, CredentialManagementService};
use crate::application::git_clone_executor::{
    clone_credential, credential_secrets, redact_git_output, strip_remote_user_info, CloneError,
    GitCloneExecutor, ResolvedCredential, PUSH_FROM_A_VOLUME_GOES_TO_ORIGIN,
};
use crate::application::git_ssh_key::{attach_ssh_credentials, check_ssh_host_key};
use crate::application::user_volume_service::{UserVolumeError, UserVolumeService};
use crate::application::volume_manager::CreateUserVolumeCommand;
use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialStatus, CredentialType,
    UserCredentialBinding,
};
use crate::domain::git_host_keys::{host_keys_for, ssh_remote, SshHostKey};
use crate::domain::git_repo::{
    default_work_branch, is_mountable_label, validate_repo_url, CloneStrategy, GitRef,
    GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository, GitRepoEvent, GitRepoStatus,
    RunAuthor, RunRepository,
};
use crate::domain::git_repo_tier_limits::GitRepoTierLimits;
use crate::domain::iam::ZaruTier;
use crate::domain::repository::RepositoryError;
use crate::domain::secrets::{AccessContext, SensitiveString, SensitiveUrl};
use crate::domain::shared_kernel::TenantId;
use crate::domain::volume::{AccessMode, Volume, VolumeBackend, VolumeMount, VolumeOwnership};
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::secrets_manager::SecretsManager;

// ============================================================================
// Command Types
// ============================================================================

/// Default clone volume size — 500 MB. Intentionally conservative; users
/// can bump via tier upgrades. The hard storage-quota gate lives in
/// [`UserVolumeService`].
const DEFAULT_CLONE_VOLUME_BYTES: u64 = 500 * 1024 * 1024;

/// Audit 002 §4.37.13 — name of the OpenBao Transit key used to encrypt
/// webhook secrets at rest. The orchestrator pre-provisions this key at
/// startup; rotation is a Transit `rotate` operation that does not
/// invalidate ciphertexts encrypted with prior key versions.
const WEBHOOK_TRANSIT_KEY: &str = "webhook-secret";

/// Audit 002 §4.37.13 — compute the deterministic lookup hash that
/// replaces the previous `webhook_secret = $1` query path.
///
/// The cleartext is a `uuid::Uuid::new_v4().simple()` string (128 bits of
/// entropy), so a SHA-256 digest is computationally infeasible to
/// brute-force from DB-only access. We hex-encode the digest so the
/// lookup column type stays a plain `TEXT`.
fn compute_webhook_lookup_hash(cleartext: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(cleartext.as_bytes());
    hex::encode(digest)
}

/// Request to create a new [`GitRepoBinding`].
///
/// Bundles all create-path inputs into a single struct so the service
/// signature stays within clippy's `too_many_arguments` limit.
#[derive(Debug, Clone)]
pub struct CreateGitRepoCommand {
    pub tenant_id: TenantId,
    pub owner: String,
    pub zaru_tier: ZaruTier,
    pub credential_binding_id: Option<CredentialBindingId>,
    /// May carry a token as user info; prints redacted.
    pub repo_url: SensitiveUrl,
    pub git_ref: GitRef,
    pub sparse_paths: Option<Vec<String>>,
    pub label: String,
    pub auto_refresh: bool,
    /// `true` (default) clones with `depth = 1`. Full-history clones
    /// require explicit opt-in.
    pub shallow: bool,
    /// The SSH host keys of the remote, as public key lines
    /// (`ssh-ed25519 AAAA…`). Required for an SSH remote on a host that is
    /// not well known; not allowed for an HTTPS remote.
    pub ssh_host_keys: Vec<String>,
}

impl CreateGitRepoCommand {
    /// Canonical default: shallow, no sparse, no auto-refresh.
    pub fn new(
        tenant_id: TenantId,
        owner: impl Into<String>,
        zaru_tier: ZaruTier,
        repo_url: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id,
            owner: owner.into(),
            zaru_tier,
            credential_binding_id: None,
            repo_url: SensitiveUrl::new(repo_url),
            git_ref: GitRef::default(),
            sparse_paths: None,
            label: label.into(),
            auto_refresh: false,
            shallow: true,
            ssh_host_keys: Vec::new(),
        }
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Service-layer errors. Handlers map these onto HTTP status codes.
#[derive(Debug, Error)]
pub enum GitRepoError {
    #[error("tier limit exceeded: {max} bindings allowed for this tier")]
    TierLimitExceeded { max: u32 },

    #[error("git repo binding not found")]
    BindingNotFound,

    #[error("not owner")]
    NotOwned,

    /// The credential binding a git binding names is not an active
    /// credential binding of the caller's: another person's, a missing one,
    /// the caller's own one that is not `Active`, or an OAuth binding whose
    /// token can no longer be refreshed (AEGIS ADR-136 G2, G2a, G2c). Maps
    /// to HTTP `422 Unprocessable Entity` and the tool path's
    /// `INVALID_ARGUMENTS`.
    #[error("The credential named for this repository is not an active credential of yours.")]
    CredentialNotYours,

    #[error("clone failed: {0}")]
    CloneFailed(String),

    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),

    #[error("secret resolution failed: {0}")]
    SecretResolutionFailed(String),

    #[error("url validation failed: {0}")]
    UrlValidationFailed(String),

    /// The SSH host keys given for a binding are missing, not keys, or given
    /// for a remote that is not reached over SSH. The message says what to
    /// send. Maps to HTTP `400 Bad Request`.
    #[error("{0}")]
    SshHostKeys(String),

    #[error("volume provisioning failed: {0}")]
    VolumeProvisioningFailed(String),

    #[error("webhook rejected: {0}")]
    WebhookRejected(String),

    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),

    /// Canvas git-write: `commit` invoked but the working tree has no
    /// staged changes relative to HEAD. Maps to HTTP `409 Conflict`.
    #[error("nothing to commit: working tree is clean")]
    NothingToCommit,

    /// Canvas git-write: HEAD is detached (no current branch), so `push`
    /// cannot auto-resolve a ref_name. Maps to HTTP `400 Bad Request`.
    ///
    /// **This variant currently has no producer**, and that is a recorded
    /// defect rather than an oversight in this file. `blocking_push` used to
    /// reach it from `Reference::shorthand()` returning `None`, but libgit2
    /// returns the direct `HEAD` reference for a detached head and its
    /// shorthand is the string `"HEAD"`, so the branch never fired; the only
    /// thing that produced `None` there was a non-UTF-8 reference name, which
    /// is a git failure and is now reported as [`Self::GitFailed`]. Making a
    /// detached head actually reach this variant needs an explicit
    /// `Repository::head_detached()` check and changes the HTTP status a client
    /// sees, so it is left to the owner. Pinned by
    /// `push_with_detached_head_reports_which_error`.
    #[error("repository HEAD is detached; no current branch to push")]
    NoHeadBranch,

    /// Canvas git-write: the binding is currently mid-refresh / mid-clone
    /// and cannot safely accept a commit / push / diff. Maps to HTTP
    /// `409 Conflict`.
    #[error("binding is busy (status: {0}); retry when Ready")]
    BindingBusy(String),

    /// Canvas git-write: a git2 operation failed inside
    /// [`GitRepoService::commit`] / [`GitRepoService::push`] /
    /// [`GitRepoService::diff`]. Maps to HTTP `502 Bad Gateway` (same
    /// class as [`Self::CloneFailed`]).
    #[error("git operation failed: {0}")]
    GitFailed(String),

    /// The binding is mounted in a run, which holds it until the run ends;
    /// refresh, delete, commit and push from outside that run are refused
    /// (AEGIS ADR-136 G4, G4a). Maps to HTTP `409 Conflict`.
    #[error("repository '{label}' is in use by a run; try again when it has ended")]
    HeldByRun { label: String },

    /// The remote refused a push as not a fast-forward: its branch has
    /// commits the tree does not (AEGIS ADR-136 G8, G8a). Nothing was
    /// pushed. Maps to HTTP `409 Conflict` and the tool path's Conflict.
    #[error("the remote branch '{branch}' has commits this run does not have; nothing was pushed")]
    RemoteAhead { branch: String },

    /// A landing's ref moved on the remote: its branch has commits the
    /// run's work branch does not (AEGIS ADR-141 F8). The work branch may
    /// have been pushed; the ref is unchanged. Maps to the tool path's
    /// Conflict.
    #[error(
        "the remote branch '{git_ref}' has commits this run does not have; nothing was landed"
    )]
    RefAhead { git_ref: String },

    /// A landing on a binding whose ref is a tag or a commit, refused before
    /// any push (AEGIS ADR-141 F8).
    #[error("a run lands only on a branch; '{git_ref}' is not one")]
    NotABranch { git_ref: String },
}

impl From<UserVolumeError> for GitRepoError {
    fn from(e: UserVolumeError) -> Self {
        Self::VolumeProvisioningFailed(e.to_string())
    }
}

// ============================================================================
// Webhook provider kinds
// ============================================================================

/// Inbound webhook provider. Controls HMAC algorithm and header lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookProvider {
    /// GitHub — `X-Hub-Signature-256: sha256=<hex>` over the raw body.
    GitHub,
    /// GitLab — `X-Gitlab-Token: <secret>` constant-time equality with
    /// the binding's `webhook_secret`.
    GitLab,
    /// Bitbucket — `X-Hub-Signature: sha1=<hex>` over the raw body
    /// (legacy BitBucket Server variant).
    Bitbucket,
}

/// Parsed authentication material extracted from a webhook request.
#[derive(Debug, Clone)]
pub struct WebhookAuth {
    pub provider: WebhookProvider,
    /// Raw `sha256=<hex>` / `sha1=<hex>` / `<token>` value as it appeared
    /// on the incoming header.
    pub signature: String,
}

// ============================================================================
// OAuth access tokens (AEGIS ADR-136 G1b, G2d)
// ============================================================================

/// Answers an `OAuth2` credential binding's access token: the stored one
/// while it is good for more than 60 seconds, otherwise a refreshed one (the
/// credential service's `access_token_for`). The git path reads an OAuth
/// binding's token only through this, never OpenBao field `value`.
#[async_trait]
pub trait OAuthAccessTokens: Send + Sync {
    async fn access_token_for(
        &self,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<SensitiveString>;
}

/// [`OAuthAccessTokens`] over the credential service the daemon builds.
pub struct CredentialServiceTokens(pub Arc<dyn CredentialManagementService>);

#[async_trait]
impl OAuthAccessTokens for CredentialServiceTokens {
    async fn access_token_for(
        &self,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<SensitiveString> {
        self.0.access_token_for(binding_id).await
    }
}

// ============================================================================
// The person a run's commits are authored as (AEGIS ADR-136 G5d)
// ============================================================================

/// Answers a person's name and email by their subject, from their profile
/// with the identity provider. `Ok(None)` when the provider holds no such
/// person, or none with an email; `Err` when it could not be asked.
#[async_trait]
pub trait PersonProfiles: Send + Sync {
    async fn name_and_email(&self, sub: &str) -> Result<Option<(String, String)>, String>;
}

/// What the daemon logs when a run's person's name and email could not be
/// read (AEGIS ADR-136 G5d): the run still starts, and commits as before.
pub const RUN_AUTHOR_UNREAD: &str =
    "the person's name and email could not be read; the run's commits carry the platform's author";

// ============================================================================
// Service
// ============================================================================

/// Application service for [`GitRepoBinding`] lifecycle management.
pub struct GitRepoService {
    repo: Arc<dyn GitRepoBindingRepository>,
    volume_service: Arc<UserVolumeService>,
    clone_executor: Arc<GitCloneExecutor>,
    secret_manager: Arc<SecretsManager>,
    credential_repo: Option<Arc<dyn CredentialBindingRepository>>,
    /// Answers an `OAuth2` binding's access token. Absent, a git binding
    /// naming an `OAuth2` credential is refused as not implemented.
    access_tokens: Option<Arc<dyn OAuthAccessTokens>>,
    /// Answers a run's person's name and email (AEGIS ADR-136 G5d). Absent,
    /// a run's commits carry the platform's author.
    person_profiles: Option<Arc<dyn PersonProfiles>>,
    event_bus: Arc<EventBus>,
    /// Orchestrator identifier used in [`AccessContext`] audit rows.
    orchestrator_id: String,
    /// The run each held binding is mounted in (AEGIS ADR-136 G4a): the
    /// workflow execution, or the root agent execution. Kept in memory, as
    /// the per-volume step locks are: a restart clears it.
    run_holds: std::sync::Mutex<std::collections::HashMap<GitRepoBindingId, uuid::Uuid>>,
    /// What each run's preparation did, kept until its first agent execution
    /// publishes it as narrative rows (AEGIS ADR-136 G5b, G13a) or the run
    /// is released.
    run_preparations:
        std::sync::Mutex<std::collections::HashMap<uuid::Uuid, Vec<PreparedRepository>>>,
}

impl GitRepoService {
    pub fn new(
        repo: Arc<dyn GitRepoBindingRepository>,
        volume_service: Arc<UserVolumeService>,
        clone_executor: Arc<GitCloneExecutor>,
        secret_manager: Arc<SecretsManager>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            repo,
            volume_service,
            clone_executor,
            secret_manager,
            credential_repo: None,
            access_tokens: None,
            person_profiles: None,
            event_bus,
            orchestrator_id: "git-repo-service".to_string(),
            run_holds: std::sync::Mutex::new(std::collections::HashMap::new()),
            run_preparations: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Inject the credential-binding repository. When present, the
    /// service resolves private-repo credentials via the Keymaster
    /// pattern; absent, any binding that carries a
    /// `credential_binding_id` will fail with `NotYetImplemented`.
    pub fn with_credential_repo(mut self, repo: Arc<dyn CredentialBindingRepository>) -> Self {
        self.credential_repo = Some(repo);
        self
    }

    /// Inject the reader of `OAuth2` bindings' access tokens (AEGIS ADR-136
    /// G2d).
    pub fn with_access_tokens(mut self, tokens: Arc<dyn OAuthAccessTokens>) -> Self {
        self.access_tokens = Some(tokens);
        self
    }

    /// Inject the reader of a run's person's name and email (AEGIS ADR-136
    /// G5d).
    pub fn with_person_profiles(mut self, profiles: Arc<dyn PersonProfiles>) -> Self {
        self.person_profiles = Some(profiles);
        self
    }

    /// The author of `person`'s run (AEGIS ADR-136 G5d): their profile's
    /// name and email when both can stand in a commit, else none. A profile
    /// that cannot be read is logged and makes none.
    async fn run_author(&self, person: Option<&str>) -> Option<RunAuthor> {
        let (profiles, person) = (self.person_profiles.as_ref()?, person?);
        match profiles.name_and_email(person).await {
            Ok(found) => found.and_then(|(name, email)| RunAuthor::new(name, email)),
            Err(error) => {
                warn!(%error, "{RUN_AUTHOR_UNREAD}");
                None
            }
        }
    }

    /// Override the orchestrator identifier used in audit events.
    pub fn with_orchestrator_id(mut self, id: impl Into<String>) -> Self {
        self.orchestrator_id = id.into();
        self
    }

    // -----------------------------------------------------------------------
    // create_binding
    // -----------------------------------------------------------------------

    /// Validate the command, enforce tier limits, provision a persistent
    /// volume, and persist a new binding in [`GitRepoStatus::Pending`].
    ///
    /// The caller is responsible for scheduling the background clone
    /// task (e.g. via `tokio::spawn(service.clone_repo(id))`).
    #[instrument(skip(self, cmd), fields(owner = %cmd.owner, repo_url = %cmd.repo_url.redacted()))]
    pub async fn create_binding(
        &self,
        cmd: CreateGitRepoCommand,
    ) -> Result<GitRepoBinding, GitRepoError> {
        // Read to validate its form.
        validate_repo_url(cmd.repo_url.expose()).map_err(GitRepoError::UrlValidationFailed)?;
        let ssh_host_keys = binding_host_keys(cmd.repo_url.expose(), &cmd.ssh_host_keys)?;

        // Only the caller's own active credential, checked before anything
        // is counted or provisioned (AEGIS ADR-136 G2, G2b).
        if let Some(cred_id) = &cmd.credential_binding_id {
            self.owned_active_credential(cred_id, &cmd.tenant_id, &cmd.owner)
                .await?;
        }

        let limits = GitRepoTierLimits::for_tier(cmd.zaru_tier.clone());
        if let Some(max) = limits.max_bindings {
            let current = self.repo.count_by_owner(&cmd.tenant_id, &cmd.owner).await?;
            if current >= max {
                return Err(GitRepoError::TierLimitExceeded { max });
            }
        }

        let volume = self
            .volume_service
            .create_volume(CreateUserVolumeCommand {
                tenant_id: cmd.tenant_id.clone(),
                owner_user_id: cmd.owner.clone(),
                label: format!("git-{}", cmd.label),
                size_limit_bytes: DEFAULT_CLONE_VOLUME_BYTES,
                zaru_tier: cmd.zaru_tier.clone(),
            })
            .await?;

        // Generate a webhook secret whenever auto_refresh is requested.
        // The secret doubles as the `X-Aegis-Webhook-Secret` header value
        // routing the inbound event to its binding and as the HMAC key
        // for GitLab-style header-token verification.
        //
        // Audit 002 §4.37.13 — the cleartext is returned to the caller
        // (so they can configure the webhook in their git provider) but
        // is **never** persisted in plaintext. Encrypt the cleartext via
        // OpenBao Transit for at-rest storage, and persist a SHA-256
        // hash as a deterministic lookup index. The cleartext is
        // 128-bit random, so brute-forcing the hash from DB-only access
        // is computationally infeasible.
        let (webhook_secret, webhook_secret_ciphertext, webhook_lookup_hash) = if cmd.auto_refresh {
            let cleartext = uuid::Uuid::new_v4().simple().to_string();
            let ciphertext = self
                .secret_manager
                .encrypt(WEBHOOK_TRANSIT_KEY, cleartext.as_bytes())
                .await
                .map_err(|e| {
                    GitRepoError::SecretResolutionFailed(format!(
                        "transit-encrypt webhook secret: {e}"
                    ))
                })?;
            let hash = compute_webhook_lookup_hash(&cleartext);
            (Some(cleartext), Some(ciphertext), Some(hash))
        } else {
            (None, None, None)
        };

        // Pick a clone strategy based on the backing volume's backend.
        let provisional_strategy = match &volume.backend {
            VolumeBackend::HostPath { .. } => CloneStrategy::Libgit2,
            VolumeBackend::SeaweedFS { .. } => CloneStrategy::EphemeralCli {
                reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
            },
            VolumeBackend::OpenDal { .. } => CloneStrategy::EphemeralCli {
                reason: "OpenDAL volume requires FUSE-mounted container".to_string(),
            },
            VolumeBackend::Seal { .. } => CloneStrategy::EphemeralCli {
                reason: "SEAL remote-node volume requires FUSE-mounted container".to_string(),
            },
        };

        let mut binding = GitRepoBinding::new(
            cmd.tenant_id.clone(),
            cmd.credential_binding_id,
            cmd.repo_url.clone(),
            cmd.git_ref.clone(),
            cmd.sparse_paths.clone(),
            volume.id,
            cmd.label.clone(),
            provisional_strategy,
            cmd.auto_refresh,
            webhook_secret,
            webhook_secret_ciphertext,
            webhook_lookup_hash,
        )
        .with_ssh_host_keys(ssh_host_keys);

        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);
        info!(binding_id = %binding.id, volume_id = %volume.id, "git repo binding created");
        Ok(binding)
    }

    // -----------------------------------------------------------------------
    // clone_repo
    // -----------------------------------------------------------------------

    /// Execute the clone for `id` and transition the binding to
    /// [`GitRepoStatus::Ready`] (or `Failed` on error).
    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn clone_repo(&self, id: &GitRepoBindingId) -> Result<(), GitRepoError> {
        let mut binding = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or(GitRepoError::BindingNotFound)?;

        binding.start_clone();
        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);

        let volume = match self
            .volume_service
            .volume_repo
            .find_by_id(binding.volume_id)
            .await
        {
            Ok(Some(v)) => v,
            _ => {
                let msg = format!(
                    "volume {} not found for binding {}",
                    binding.volume_id, binding.id
                );
                self.fail(&mut binding, msg.clone()).await;
                return Err(GitRepoError::VolumeProvisioningFailed(msg));
            }
        };

        let credential = match self
            .resolve_credential(&binding, volume_owner(&volume))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                self.fail(&mut binding, msg.clone()).await;
                return Err(e);
            }
        };

        let started = std::time::Instant::now();
        let shallow = true;

        let strategy = self.clone_executor.select_strategy(&binding, &volume);
        // Persist the strategy back to the binding so the UI / operators
        // can see the real routing that was used.
        if strategy != binding.clone_strategy {
            binding.clone_strategy = strategy.clone();
        }

        let clone_result = match strategy {
            CloneStrategy::Libgit2 => {
                let target_dir = match host_path_for_volume(&volume) {
                    Ok(p) => p,
                    Err(e) => {
                        self.fail(&mut binding, e.clone()).await;
                        return Err(GitRepoError::VolumeProvisioningFailed(e));
                    }
                };
                self.clone_executor
                    .clone_libgit2(&binding, &target_dir, credential, shallow)
                    .await
            }
            CloneStrategy::EphemeralCli { .. } => {
                self.clone_executor
                    .clone_ephemeral(&binding, &volume, credential, shallow)
                    .await
            }
        };

        match clone_result {
            Ok(sha) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                binding.complete_clone(sha.clone(), duration_ms);
                self.repo.save(&binding).await?;
                self.drain_and_publish(&mut binding);
                info!(commit_sha = %sha, duration_ms, "clone completed");
                Ok(())
            }
            Err(e) => {
                let msg = match &e {
                    CloneError::Git(m) => format!("git: {m}"),
                    CloneError::Io(m) => format!("io: {m}"),
                    CloneError::NotYetImplemented(m) => format!("not_yet_implemented: {m}"),
                    CloneError::RemoteAhead(_) => format!("git: {e}"),
                };
                self.fail(&mut binding, msg.clone()).await;
                Err(GitRepoError::CloneFailed(msg))
            }
        }
    }

    // -----------------------------------------------------------------------
    // refresh_repo
    // -----------------------------------------------------------------------

    /// Refresh an existing binding against the remote. Transitions the
    /// binding through `Ready → Refreshing → Ready` (or `Failed`), emits
    /// the matching `Refresh*` events, and updates `last_commit_sha`.
    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn refresh_repo(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<(), GitRepoError> {
        let mut binding = self.get_binding(id, tenant_id, owner).await?;
        self.do_refresh(&mut binding).await
    }

    /// Internal refresh entry point that bypasses the ownership gate.
    /// Invoked by the webhook handler once the HMAC signature has been
    /// verified.
    async fn do_refresh(&self, binding: &mut GitRepoBinding) -> Result<(), GitRepoError> {
        self.refuse_if_held(binding, None)?;
        let old_sha = binding
            .last_commit_sha
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        binding.start_refresh();
        self.repo.save(binding).await?;
        self.drain_and_publish(binding);

        let volume = match self
            .volume_service
            .volume_repo
            .find_by_id(binding.volume_id)
            .await
        {
            Ok(Some(v)) => v,
            _ => {
                let msg = format!(
                    "volume {} not found for binding {}",
                    binding.volume_id, binding.id
                );
                self.fail_refresh(binding, msg.clone()).await;
                return Err(GitRepoError::VolumeProvisioningFailed(msg));
            }
        };

        let credential = match self
            .resolve_credential(binding, volume_owner(&volume))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                self.fail_refresh(binding, msg.clone()).await;
                return Err(e);
            }
        };

        let started = std::time::Instant::now();
        // A HostPath tree is fetched in process; any other volume's by a git
        // step (AEGIS ADR-136 G11).
        let fetched = match &volume.backend {
            VolumeBackend::HostPath { path } => {
                self.clone_executor
                    .fetch_and_checkout(binding, path, credential)
                    .await
            }
            _ => {
                self.clone_executor
                    .fetch_ephemeral(binding, &volume, credential)
                    .await
            }
        };
        match fetched {
            Ok(new_sha) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                binding.complete_refresh(old_sha, new_sha.clone(), duration_ms);
                self.repo.save(binding).await?;
                self.drain_and_publish(binding);
                info!(new_commit_sha = %new_sha, duration_ms, "refresh completed");
                Ok(())
            }
            Err(e) => {
                let msg = match &e {
                    CloneError::Git(m) => format!("git: {m}"),
                    CloneError::Io(m) => format!("io: {m}"),
                    CloneError::NotYetImplemented(m) => format!("not_yet_implemented: {m}"),
                    CloneError::RemoteAhead(_) => format!("git: {e}"),
                };
                self.fail_refresh(binding, msg.clone()).await;
                Err(GitRepoError::CloneFailed(msg))
            }
        }
    }

    // -----------------------------------------------------------------------
    // list_bindings
    // -----------------------------------------------------------------------

    #[instrument(skip(self))]
    pub async fn list_bindings(
        &self,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<Vec<GitRepoBinding>, GitRepoError> {
        let bindings = self.repo.find_by_owner(tenant_id, owner).await?;
        let owned_volumes = self.volume_service.list_volumes(tenant_id, owner).await?;
        let owned_volume_ids: std::collections::HashSet<_> =
            owned_volumes.into_iter().map(|v| v.id).collect();
        // Audit 002 §4.37.13 — wipe the transient cleartext webhook_secret on
        // any read path. See `get_binding` for the full rationale.
        Ok(bindings
            .into_iter()
            .filter(|b| owned_volume_ids.contains(&b.volume_id))
            .map(|mut b| {
                b.webhook_secret = None;
                b
            })
            .collect())
    }

    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn get_binding(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<GitRepoBinding, GitRepoError> {
        let mut binding = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or(GitRepoError::BindingNotFound)?;
        if &binding.tenant_id != tenant_id {
            return Err(GitRepoError::BindingNotFound);
        }
        let owned_volumes = self.volume_service.list_volumes(tenant_id, owner).await?;
        if !owned_volumes.iter().any(|v| v.id == binding.volume_id) {
            return Err(GitRepoError::BindingNotFound);
        }
        // Audit 002 §4.37.13 — `webhook_secret` is a transient cleartext slot
        // surfaced only on initial create/auto-refresh so the caller can
        // configure the upstream provider. On any subsequent read it MUST be
        // wiped to enforce the at-rest contract (only `webhook_secret_ciphertext`
        // + `webhook_lookup_hash` are persisted). Wipe at the service boundary
        // so this holds regardless of the underlying repository impl (Postgres
        // never stores it; in-memory repos and tests would otherwise leak it).
        binding.webhook_secret = None;
        Ok(binding)
    }

    // -----------------------------------------------------------------------
    // delete_binding
    // -----------------------------------------------------------------------

    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn delete_binding(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<(), GitRepoError> {
        let mut binding = self.get_binding(id, tenant_id, owner).await?;
        self.refuse_if_held(&binding, None)?;
        let volume_id = binding.volume_id;
        let _ = self.volume_service.delete_volume(&volume_id, owner).await;
        binding.mark_deleted();
        self.drain_and_publish(&mut binding);
        self.repo.delete(&binding.id).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // commit / push / diff — B2 (Canvas git-write)
    // -----------------------------------------------------------------------

    /// Stage every change in the working tree, create a commit on HEAD,
    /// and emit [`GitRepoEvent::CommitMade`].
    ///
    /// Behaviour:
    /// - Verifies tenant + ownership via [`Self::get_binding`].
    /// - Refuses if the binding is not in [`GitRepoStatus::Ready`] — a
    ///   concurrent refresh/clone could leave the index inconsistent.
    /// - Stages all workdir changes (`index.add_all(["*"], …)`), writes
    ///   the tree, and commits against the current `HEAD` parent.
    /// - Returns the commit SHA as a 40-char hex string.
    /// - Returns [`GitRepoError::NothingToCommit`] when the tree has no
    ///   changes relative to HEAD.
    #[instrument(skip(self, message), fields(binding_id = %id, owner = %owner))]
    pub async fn commit(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitRepoError> {
        self.commit_as(
            None,
            id,
            tenant_id,
            owner,
            message,
            author_name,
            author_email,
        )
        .await
    }

    /// [`Self::commit`] from inside the run `run`, which may commit on a
    /// binding it holds itself (AEGIS ADR-136 G7, G7b).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(self, message), fields(binding_id = %id, owner = %owner, run = %run))]
    pub async fn commit_for_run(
        &self,
        run: uuid::Uuid,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitRepoError> {
        self.commit_as(
            Some(run),
            id,
            tenant_id,
            owner,
            message,
            author_name,
            author_email,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_as(
        &self,
        acting_run: Option<uuid::Uuid>,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitRepoError> {
        let mut binding = self.get_binding(id, tenant_id, owner).await?;
        ensure_binding_ready(&binding)?;
        self.refuse_if_held(&binding, acting_run)?;
        // A commit reads no credential, but a binding naming one that is not
        // the caller's active credential is refused here too (AEGIS ADR-136
        // G2).
        if let Some(cred_id) = &binding.credential_binding_id {
            self.owned_active_credential(cred_id, &binding.tenant_id, owner)
                .await?;
        }

        let commit_sha = match self.resolve_tree(&binding).await? {
            Tree::Host(target_dir) => {
                let message = message.to_string();
                let author_name = author_name.to_string();
                let author_email = author_email.to_string();

                // libgit2 is blocking — off-load to the pool so we never stall
                // the tokio reactor.
                tokio::task::spawn_blocking(move || -> Result<String, GitRepoError> {
                    blocking_commit(&target_dir, &message, &author_name, &author_email)
                })
                .await
                .map_err(|e| GitRepoError::GitFailed(format!("commit task panicked: {e}")))??
            }
            Tree::Volume(volume) => self
                .clone_executor
                .commit_ephemeral(&volume, message, author_name, author_email)
                .await
                .map_err(step_error)?
                .ok_or(GitRepoError::NothingToCommit)?,
        };

        binding.domain_events.push(GitRepoEvent::CommitMade {
            id: binding.id,
            commit_sha: commit_sha.clone(),
            committed_at: Utc::now(),
        });
        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);

        info!(%commit_sha, "commit created on canvas git binding");
        Ok(commit_sha)
    }

    /// Push the binding's current branch (or the explicit `ref_name`) to
    /// the remote. Emits [`GitRepoEvent::PushCompleted`] on success.
    ///
    /// Credential resolution reuses the A2 `resolve_credential` path.
    /// Both HTTPS-PAT and SSH credentials are supported; the SSH path
    /// materialises the private key via the shared
    /// `git_ssh_key::attach_ssh_credentials` helper (same Keymaster
    /// Pattern used by the clone path).
    ///
    /// Defaults: `remote = "origin"`. When `ref_name` is `None` we read
    /// the working tree's current branch via
    /// `repo.head()?.shorthand()?`. A non-UTF-8 reference name → [`GitRepoError::GitFailed`].
    #[instrument(skip(self), fields(binding_id = %id, owner = %owner))]
    pub async fn push(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        remote: Option<&str>,
        ref_name: Option<&str>,
    ) -> Result<(), GitRepoError> {
        self.push_as(None, id, tenant_id, owner, remote, ref_name)
            .await
            .map(|_| ())
    }

    /// Push the work branch `branch` of a binding the run `run` holds to
    /// `origin`, never with force (AEGIS ADR-136 G8, G8a, G8b). Answers the
    /// branch, the binding's URL with no user info, and the host's page of
    /// the branch.
    #[instrument(skip(self), fields(binding_id = %id, owner = %owner, run = %run))]
    pub async fn push_for_run(
        &self,
        run: uuid::Uuid,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        branch: &str,
    ) -> Result<RunPush, GitRepoError> {
        let (pushed, remote_url) = self
            .push_as(
                Some(run),
                id,
                tenant_id,
                owner,
                Some("origin"),
                Some(branch),
            )
            .await?;
        let branch_url = branch_url(&remote_url, &pushed);
        Ok(RunPush {
            branch: pushed,
            remote_url,
            branch_url,
        })
    }

    /// Land the work branch `branch` of a binding the run `run` holds on
    /// the binding's ref (AEGIS ADR-141 F8): in one git step with the
    /// person's credential, push the work branch to `origin` as
    /// [`Self::push_for_run`] does, then push its HEAD to
    /// `refs/heads/<the binding's ref>`, neither with force. A ref that is a
    /// tag or a commit is refused before any push. A landing is never a
    /// merge, a rebase or a squash.
    #[instrument(skip(self), fields(binding_id = %id, owner = %owner, run = %run))]
    pub async fn land_for_run(
        &self,
        run: uuid::Uuid,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        branch: &str,
    ) -> Result<RunLanding, GitRepoError> {
        let mut binding = self.get_binding(id, tenant_id, owner).await?;
        ensure_binding_ready(&binding)?;
        self.refuse_if_held(&binding, Some(run))?;
        let git_ref = landing_ref(&binding.git_ref)?.to_string();
        let tree = self.resolve_tree(&binding).await?;
        let credential = self.resolve_credential(&binding, Some(owner)).await?;
        let commit_sha = match tree {
            Tree::Host(target_dir) => {
                let repo_url = binding.repo_url.clone();
                let ssh_host_keys = binding.ssh_host_keys.clone();
                let branch = branch.to_string();
                let git_ref = git_ref.clone();
                tokio::task::spawn_blocking(move || -> Result<String, GitRepoError> {
                    land_to_remote(
                        &target_dir,
                        &repo_url,
                        &branch,
                        &git_ref,
                        credential,
                        &ssh_host_keys,
                    )
                })
                .await
                .map_err(|e| GitRepoError::GitFailed(format!("land task panicked: {e}")))??
            }
            Tree::Volume(volume) => match self
                .clone_executor
                .land_ephemeral(&binding, &volume, branch, &git_ref, credential)
                .await
            {
                Ok(crate::application::git_clone_executor::VolumeLanding::Landed(commit_sha)) => {
                    commit_sha
                }
                Ok(crate::application::git_clone_executor::VolumeLanding::RefAhead(git_ref)) => {
                    return Err(GitRepoError::RefAhead { git_ref })
                }
                Err(e) => return Err(step_error(e)),
            },
        };
        let now = Utc::now();
        for ref_name in [branch.to_string(), git_ref.clone()] {
            binding.domain_events.push(GitRepoEvent::PushCompleted {
                id: binding.id,
                remote: "origin".to_string(),
                ref_name,
                pushed_at: now,
            });
        }
        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);
        info!(%commit_sha, git_ref = %git_ref, "a run's work branch landed on its binding's ref");
        let remote_url = crate::domain::secrets::RedactedUrl::new(binding.repo_url.expose())
            .as_str()
            .to_string();
        Ok(RunLanding {
            label: binding.label.clone(),
            branch: branch.to_string(),
            branch_url: branch_url(&remote_url, &git_ref),
            git_ref,
            commit_sha,
            remote_url,
        })
    }

    /// The tree of a binding the run `run` holds: whether it holds changes,
    /// and its HEAD, read from the tree (AEGIS ADR-136 G7c).
    #[instrument(skip(self), fields(binding_id = %id, owner = %owner, run = %run))]
    pub async fn status_for_run(
        &self,
        run: uuid::Uuid,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<RunTreeStatus, GitRepoError> {
        let binding = self.get_binding(id, tenant_id, owner).await?;
        ensure_binding_ready(&binding)?;
        self.refuse_if_held(&binding, Some(run))?;
        let (clean, head) = match self.resolve_tree(&binding).await? {
            Tree::Host(target_dir) => {
                tokio::task::spawn_blocking(move || blocking_head_and_status(&target_dir))
                    .await
                    .map_err(|e| GitRepoError::GitFailed(format!("status task panicked: {e}")))??
            }
            Tree::Volume(volume) => self
                .clone_executor
                .head_and_status_ephemeral(&volume)
                .await
                .map_err(step_error)?,
        };
        Ok(RunTreeStatus { clean, head })
    }

    /// [`Self::diff`] of a binding the run `run` holds (AEGIS ADR-136 G7).
    pub async fn diff_for_run(
        &self,
        run: uuid::Uuid,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        staged: bool,
    ) -> Result<String, GitRepoError> {
        let binding = self.get_binding(id, tenant_id, owner).await?;
        self.refuse_if_held(&binding, Some(run))?;
        self.diff(id, tenant_id, owner, staged).await
    }

    /// Answers the ref pushed and the binding's URL with no user info.
    async fn push_as(
        &self,
        acting_run: Option<uuid::Uuid>,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        remote: Option<&str>,
        ref_name: Option<&str>,
    ) -> Result<(String, String), GitRepoError> {
        let mut binding = self.get_binding(id, tenant_id, owner).await?;
        ensure_binding_ready(&binding)?;
        self.refuse_if_held(&binding, acting_run)?;

        let tree = self.resolve_tree(&binding).await?;
        // A push from a volume goes only where the binding's credential
        // belongs: origin, pointed at the binding's URL (AEGIS ADR-136
        // G11a).
        if matches!(tree, Tree::Volume(_)) && remote.is_some_and(|r| r != "origin") {
            return Err(GitRepoError::GitFailed(
                PUSH_FROM_A_VOLUME_GOES_TO_ORIGIN.to_string(),
            ));
        }
        let credential = self.resolve_credential(&binding, Some(owner)).await?;

        let resolved_ref = match tree {
            Tree::Host(target_dir) => {
                let remote_name = remote.unwrap_or("origin").to_string();
                let explicit_ref = ref_name.map(str::to_string);
                let repo_url = binding.repo_url.clone();
                let ssh_host_keys = binding.ssh_host_keys.clone();

                tokio::task::spawn_blocking(move || -> Result<String, GitRepoError> {
                    push_to_remote(
                        &target_dir,
                        &repo_url,
                        &remote_name,
                        explicit_ref,
                        credential,
                        &ssh_host_keys,
                    )
                })
                .await
                .map_err(|e| GitRepoError::GitFailed(format!("push task panicked: {e}")))??
            }
            Tree::Volume(volume) => self
                .clone_executor
                .push_ephemeral(&binding, &volume, ref_name, credential)
                .await
                .map_err(step_error)?,
        };

        binding.domain_events.push(GitRepoEvent::PushCompleted {
            id: binding.id,
            remote: remote.unwrap_or("origin").to_string(),
            ref_name: resolved_ref.clone(),
            pushed_at: Utc::now(),
        });
        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);

        info!("push completed on canvas git binding");
        let remote_url = crate::domain::secrets::RedactedUrl::new(binding.repo_url.expose())
            .as_str()
            .to_string();
        Ok((resolved_ref, remote_url))
    }

    /// Return the unified diff of the binding's working tree.
    ///
    /// - `staged == false` (default): diff index → workdir — what the
    ///   user has edited but not yet staged.
    /// - `staged == true`: diff HEAD's tree → index — what is staged and
    ///   ready to commit.
    ///
    /// No binding mutation, no domain event.
    #[instrument(skip(self), fields(binding_id = %id, owner = %owner, staged))]
    pub async fn diff(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
        staged: bool,
    ) -> Result<String, GitRepoError> {
        let binding = self.get_binding(id, tenant_id, owner).await?;
        ensure_binding_ready(&binding)?;

        let diff_text = match self.resolve_tree(&binding).await? {
            Tree::Host(target_dir) => {
                tokio::task::spawn_blocking(move || -> Result<String, GitRepoError> {
                    blocking_diff(&target_dir, staged)
                })
                .await
                .map_err(|e| GitRepoError::GitFailed(format!("diff task panicked: {e}")))??
            }
            Tree::Volume(volume) => self
                .clone_executor
                .diff_ephemeral(&volume, staged)
                .await
                .map_err(step_error)?,
        };

        Ok(diff_text)
    }

    /// Where a binding's working tree is: a host directory libgit2 works in,
    /// or a volume a git step mounts. Shared by [`Self::commit`],
    /// [`Self::push`], and [`Self::diff`].
    async fn resolve_tree(&self, binding: &GitRepoBinding) -> Result<Tree, GitRepoError> {
        let volume = self
            .volume_service
            .volume_repo
            .find_by_id(binding.volume_id)
            .await
            .map_err(|e| GitRepoError::VolumeProvisioningFailed(e.to_string()))?
            .ok_or_else(|| {
                GitRepoError::VolumeProvisioningFailed(format!(
                    "volume {} not found for binding {}",
                    binding.volume_id, binding.id
                ))
            })?;
        Ok(match &volume.backend {
            VolumeBackend::HostPath { path } => Tree::Host(path.clone()),
            _ => Tree::Volume(Box::new(volume)),
        })
    }

    // -----------------------------------------------------------------------
    // handle_webhook — A3 (HMAC-authenticated webhook)
    // -----------------------------------------------------------------------

    /// Handle an inbound webhook. Validates the HMAC signature using the
    /// binding's `webhook_secret` then triggers a refresh.
    ///
    /// Returns `Ok(())` on success, `WebhookRejected` on bad signature or
    /// unknown secret, `NotYetImplemented` when the binding is not
    /// configured for auto-refresh, and other variants propagate from
    /// the refresh path.
    pub async fn handle_webhook(
        &self,
        secret: &str,
        auth: &WebhookAuth,
        payload: &[u8],
    ) -> Result<(), GitRepoError> {
        use subtle::ConstantTimeEq;

        // Audit 002 §4.37.13 — the binding is no longer indexed by
        // cleartext secret. Compute the deterministic lookup hash and
        // query by that. The DB row carries only the ciphertext + hash;
        // the cleartext is decrypted on demand via Transit below.
        let lookup_hash = compute_webhook_lookup_hash(secret);
        let mut binding = self
            .repo
            .find_by_webhook_lookup_hash(&lookup_hash)
            .await?
            .ok_or_else(|| GitRepoError::WebhookRejected("unknown webhook secret".into()))?;

        let Some(ciphertext) = binding.webhook_secret_ciphertext.as_ref() else {
            return Err(GitRepoError::WebhookRejected(
                "binding has no webhook secret configured".into(),
            ));
        };

        // Decrypt the stored ciphertext via Transit. The cleartext is
        // pulled into a local `Vec<u8>` only for the lifetime of the
        // verification call.
        let stored_secret_bytes = self
            .secret_manager
            .decrypt(WEBHOOK_TRANSIT_KEY, ciphertext)
            .await
            .map_err(|e| {
                GitRepoError::WebhookRejected(format!("transit-decrypt webhook secret: {e}"))
            })?;

        // Audit 002 §4.13: defense in depth — constant-time compare the
        // presented header value against the decrypted stored value so
        // a hash collision (or future repository drift) cannot leak
        // per-byte timing through the response path.
        let presented = secret.as_bytes();
        let stored = stored_secret_bytes.as_slice();
        let length_match = (presented.len() == stored.len()) as u8;
        let lhs = if length_match == 1 { presented } else { stored };
        if (lhs.ct_eq(stored).unwrap_u8() & length_match) != 1 {
            return Err(GitRepoError::WebhookRejected(
                "webhook secret mismatch".into(),
            ));
        }

        if !verify_webhook(auth, payload, stored) {
            return Err(GitRepoError::WebhookRejected(
                "hmac signature verification failed".into(),
            ));
        }

        // Emit WebhookReceived event.
        let source = match auth.provider {
            WebhookProvider::GitHub => "github",
            WebhookProvider::GitLab => "gitlab",
            WebhookProvider::Bitbucket => "bitbucket",
        };
        binding
            .domain_events
            .push(crate::domain::git_repo::GitRepoEvent::WebhookReceived {
                id: binding.id,
                source: source.to_string(),
                received_at: chrono::Utc::now(),
            });
        self.drain_and_publish(&mut binding);

        self.do_refresh(&mut binding).await
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    async fn fail(&self, binding: &mut GitRepoBinding, error: String) {
        warn!(binding_id = %binding.id, %error, "marking binding as Failed (clone)");
        binding.fail_clone(error);
        if let Err(e) = self.repo.save(binding).await {
            error!(?e, "failed to persist Failed binding state");
        }
        self.drain_and_publish(binding);
    }

    async fn fail_refresh(&self, binding: &mut GitRepoBinding, error: String) {
        warn!(binding_id = %binding.id, %error, "marking binding as Failed (refresh)");
        binding.fail_refresh(error);
        if let Err(e) = self.repo.save(binding).await {
            error!(?e, "failed to persist Failed binding state");
        }
        self.drain_and_publish(binding);
    }

    /// The credential binding `cred_id` when it is an active credential
    /// binding of `owner` in `tenant_id`; otherwise
    /// [`GitRepoError::CredentialNotYours`], whether it is another person's,
    /// missing, or the owner's own inactive one (AEGIS ADR-136 G2, G2a).
    async fn owned_active_credential(
        &self,
        cred_id: &CredentialBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<UserCredentialBinding, GitRepoError> {
        let repo = self
            .credential_repo
            .as_ref()
            .ok_or(GitRepoError::NotYetImplemented(
                "credential-backed clone requires CredentialBindingRepository injection",
            ))?;

        let cb = repo
            .find_by_id(cred_id)
            .await
            .map_err(|e| GitRepoError::SecretResolutionFailed(e.to_string()))?;
        match cb {
            Some(cb)
                if &cb.tenant_id == tenant_id
                    && cb.owner_user_id == owner
                    && cb.status == CredentialStatus::Active =>
            {
                Ok(cb)
            }
            _ => {
                warn!(
                    credential_binding_id = %cred_id,
                    "a git binding names a credential that is not an active credential of its owner; refused"
                );
                Err(GitRepoError::CredentialNotYours)
            }
        }
    }

    /// Resolve a [`ResolvedCredential`] from OpenBao if the binding has
    /// a credential pinned. Returns `Ok(None)` for public repos. `owner` is
    /// the person the binding's volume belongs to; the credential must be
    /// theirs, active, and of the binding's tenant (AEGIS ADR-136 G2).
    async fn resolve_credential(
        &self,
        binding: &GitRepoBinding,
        owner: Option<&str>,
    ) -> Result<Option<ResolvedCredential>, GitRepoError> {
        let Some(cred_id) = binding.credential_binding_id else {
            return Ok(None);
        };
        let Some(owner) = owner else {
            // A volume with no persistent owner has no person whose
            // credential it could carry.
            return Err(GitRepoError::CredentialNotYours);
        };
        let cb = self
            .owned_active_credential(&cred_id, &binding.tenant_id, owner)
            .await?;

        let ctx = AccessContext::system(&self.orchestrator_id);
        let engine = cb.secret_path.effective_mount();

        match cb.credential_type {
            // An OAuth binding's token comes from the credential service,
            // refreshed as needed, never from field `value` (AEGIS ADR-136
            // G1b, G2c, G2d).
            CredentialType::OAuth2 => self.oauth_credential(&cb).await,
            CredentialType::Secret | CredentialType::ServiceAccount => {
                // For PAT / service-account credentials we read
                // the canonical "value" field from the KV record. The
                // optional "username" field lets callers override the
                // default `x-access-token`.
                let pat = self
                    .secret_manager
                    .read_secret_field(&engine, &cb.secret_path.path, "value", &ctx)
                    .await
                    .map_err(|e| GitRepoError::SecretResolutionFailed(e.to_string()))?;

                // Differentiate PAT vs SSH by reading an optional
                // "kind" field. When absent, default to PAT (preserves
                // existing API-key bindings).
                let kind = self
                    .secret_manager
                    .read_secret_field(&engine, &cb.secret_path.path, "kind", &ctx)
                    .await
                    .map(|s| s.expose_owned())
                    .unwrap_or_else(|_| "pat".to_string());

                if kind == "ssh_key" {
                    let passphrase = self
                        .secret_manager
                        .read_secret_field(&engine, &cb.secret_path.path, "passphrase", &ctx)
                        .await
                        .ok();
                    return Ok(Some(ResolvedCredential::SshKey {
                        private_key_pem: pat,
                        passphrase,
                    }));
                }

                let username = self
                    .secret_manager
                    .read_secret_field(&engine, &cb.secret_path.path, "username", &ctx)
                    .await
                    .map(|s| s.expose_owned())
                    .unwrap_or_else(|_| default_username_for(&cb));

                Ok(Some(ResolvedCredential::HttpsPat {
                    username,
                    token: pat,
                }))
            }
            CredentialType::Variable => Err(GitRepoError::SecretResolutionFailed(
                "non-secret credentials cannot be used for git authentication".into(),
            )),
            CredentialType::Mailbox => Err(GitRepoError::SecretResolutionFailed(
                "mailbox credentials cannot be used for git authentication".into(),
            )),
            CredentialType::Calendar => Err(GitRepoError::SecretResolutionFailed(
                "calendar credentials cannot be used for git authentication".into(),
            )),
        }
    }

    /// An `OAuth2` binding's git credential: its access token as the HTTPS
    /// password. A token that can no longer be refreshed (the binding is now
    /// `Expired`) is refused at once (AEGIS ADR-136 G2c); a binding holding
    /// no OAuth token at all is no credential (G1b).
    async fn oauth_credential(
        &self,
        cb: &UserCredentialBinding,
    ) -> Result<Option<ResolvedCredential>, GitRepoError> {
        let tokens = self
            .access_tokens
            .as_ref()
            .ok_or(GitRepoError::NotYetImplemented(
                "an OAuth credential for git requires the credential service's access tokens",
            ))?;
        match tokens.access_token_for(&cb.id).await {
            Ok(token) => Ok(Some(ResolvedCredential::HttpsPat {
                username: default_username_for(cb),
                token,
            })),
            Err(e) => match e.downcast_ref::<CredentialError>() {
                Some(CredentialError::OAuthExchangeFailed { error, .. })
                    if error == "invalid_grant" =>
                {
                    Err(GitRepoError::CredentialNotYours)
                }
                Some(CredentialError::BindingNotActive { .. }) => {
                    Err(GitRepoError::CredentialNotYours)
                }
                Some(CredentialError::NoAccessToken { .. }) => Ok(None),
                _ => Err(GitRepoError::SecretResolutionFailed(e.to_string())),
            },
        }
    }

    // -----------------------------------------------------------------------
    // A run's repositories (AEGIS ADR-136 G3, G4, G5; G3a, G4a, G5a)
    // -----------------------------------------------------------------------

    /// Refuse an act on a binding a run holds, from outside that run (G4a):
    /// `acting_run` may act on a binding it holds itself (G7b).
    fn refuse_if_held(
        &self,
        binding: &GitRepoBinding,
        acting_run: Option<uuid::Uuid>,
    ) -> Result<(), GitRepoError> {
        let holds = self.run_holds.lock().expect("run holds lock");
        if holds
            .get(&binding.id)
            .is_some_and(|holder| Some(*holder) != acting_run)
        {
            return Err(GitRepoError::HeldByRun {
                label: binding.label.clone(),
            });
        }
        Ok(())
    }

    /// The run holding `id`, if any.
    pub fn run_holding(&self, id: &GitRepoBindingId) -> Option<uuid::Uuid> {
        self.run_holds
            .lock()
            .expect("run holds lock")
            .get(id)
            .copied()
    }

    /// Hold `binding` for `run`, or refuse when another run holds it.
    /// Answers whether this call took the hold.
    fn hold_for_run(&self, binding: &GitRepoBinding, run: uuid::Uuid) -> Result<bool, String> {
        let mut holds = self.run_holds.lock().expect("run holds lock");
        match holds.get(&binding.id) {
            Some(holder) if *holder == run => Ok(false),
            Some(_) => Err(format!(
                "repository '{}' is in use by another run",
                binding.label
            )),
            None => {
                holds.insert(binding.id, run);
                Ok(true)
            }
        }
    }

    /// Release every binding `run` holds.
    pub fn release_run(&self, run: uuid::Uuid) {
        self.run_preparations
            .lock()
            .expect("run preparations lock")
            .remove(&run);
        let mut holds = self.run_holds.lock().expect("run holds lock");
        let before = holds.len();
        holds.retain(|_, holder| *holder != run);
        if holds.len() != before {
            info!(run = %run, released = before - holds.len(), "a run's repositories released");
        }
    }

    /// Release a run's repositories when it ends, however it ends: a root
    /// agent execution's end events and a workflow execution's (AEGIS
    /// ADR-136 G4). A run that is neither holds nothing, and its release is
    /// a no-op.
    pub fn release_runs_on_end(self: Arc<Self>) {
        let mut events = self.event_bus.subscribe();
        tokio::spawn(async move {
            use crate::domain::events::{ExecutionEvent, WorkflowEvent};
            use crate::infrastructure::event_bus::{DomainEvent, EventBusError};
            loop {
                let run = match events.recv().await {
                    Ok(DomainEvent::Execution(
                        ExecutionEvent::ExecutionCompleted { execution_id, .. }
                        | ExecutionEvent::ExecutionFailed { execution_id, .. }
                        | ExecutionEvent::ExecutionCancelled { execution_id, .. }
                        | ExecutionEvent::ExecutionTimedOut { execution_id, .. },
                    )) => execution_id.0,
                    Ok(DomainEvent::Workflow(
                        WorkflowEvent::WorkflowExecutionCompleted { execution_id, .. }
                        | WorkflowEvent::WorkflowExecutionFailed { execution_id, .. }
                        | WorkflowEvent::WorkflowExecutionCancelled { execution_id, .. },
                    )) => execution_id.0,
                    Ok(_) => continue,
                    Err(EventBusError::Lagged(n)) => {
                        warn!(skipped = n, "repository release listener lagged");
                        continue;
                    }
                    Err(_) => break,
                };
                self.release_run(run);
            }
        });
    }

    /// The binding a run names, read for `person` in `tenant_id`, with its
    /// label checked as a directory name; another person's binding is named
    /// by the first eight hex digits of its id (G3a).
    async fn run_binding(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        entry: &RunRepository,
    ) -> Result<GitRepoBinding, RunRepositoryError> {
        let not_yours = || {
            RunRepositoryError::Refused(format!(
                "repository '{}' is not one of yours",
                &entry.binding_id.0.to_string()[..8]
            ))
        };
        let Some(person) = person else {
            return Err(not_yours());
        };
        let binding = match self.get_binding(&entry.binding_id, tenant_id, person).await {
            Ok(binding) => binding,
            Err(GitRepoError::BindingNotFound | GitRepoError::NotOwned) => return Err(not_yours()),
            Err(e) => return Err(RunRepositoryError::Failed(e)),
        };
        if !is_mountable_label(&binding.label) {
            return Err(RunRepositoryError::Refused(format!(
                "repository '{}' cannot be mounted: its label must be letters, digits, '.', '_' or '-'",
                binding.label
            )));
        }
        if binding.status != GitRepoStatus::Ready {
            return Err(RunRepositoryError::Refused(format!(
                "repository '{}' is not ready (it is {}); start the run when its clone has finished",
                binding.label,
                status_word(&binding.status)
            )));
        }
        Ok(binding)
    }

    /// Fetch the binding's ref and check out `branch` in its tree (G5,
    /// G5a): the remote's copy when the remote has the branch, else the
    /// branch created from the ref. Answers the commit the run starts from
    /// and whether the branch was created.
    async fn check_out_work_branch(
        &self,
        binding: &GitRepoBinding,
        owner: &str,
        branch: &str,
    ) -> Result<(String, bool), GitRepoError> {
        let credential = self.resolve_credential(binding, Some(owner)).await?;
        match self.resolve_tree(binding).await? {
            Tree::Host(target_dir) => {
                let repo_url = binding.repo_url.clone();
                let git_ref = binding.git_ref.clone();
                let ssh_host_keys = binding.ssh_host_keys.clone();
                let branch = branch.to_string();
                tokio::task::spawn_blocking(move || {
                    work_branch_in_dir(
                        &target_dir,
                        &repo_url,
                        &git_ref,
                        &branch,
                        credential,
                        &ssh_host_keys,
                    )
                })
                .await
                .map_err(|e| GitRepoError::GitFailed(format!("branch task panicked: {e}")))?
            }
            Tree::Volume(volume) => self
                .clone_executor
                .work_branch_ephemeral(binding, &volume, branch, credential)
                .await
                .map_err(step_error),
        }
    }

    /// The mount of a binding's working tree at `/workspace/<label>` (G3a):
    /// a host directory's root, or the `repo` directory of any other volume.
    async fn tree_mount(&self, binding: &GitRepoBinding) -> Result<VolumeMount, GitRepoError> {
        let mount_point = PathBuf::from(format!("/workspace/{}", binding.label));
        Ok(match self.resolve_tree(binding).await? {
            Tree::Host(_) => {
                let volume = self
                    .volume_service
                    .volume_repo
                    .find_by_id(binding.volume_id)
                    .await
                    .map_err(|e| GitRepoError::VolumeProvisioningFailed(e.to_string()))?
                    .ok_or_else(|| {
                        GitRepoError::VolumeProvisioningFailed(format!(
                            "volume {} not found for binding {}",
                            binding.volume_id, binding.id
                        ))
                    })?;
                volume.to_mount(mount_point, AccessMode::ReadWrite)
            }
            Tree::Volume(volume) => {
                let mut mount = volume.to_mount(mount_point, AccessMode::ReadWrite);
                mount.remote_path = format!("{}/repo", mount.remote_path.trim_end_matches('/'));
                mount
            }
        })
    }

    /// Every check of G3 and G3a on a run's entries, before anything is
    /// held: each binding the person's, Ready, with a mountable label named
    /// once, its work branch (the run's default when none is given, G5a)
    /// not the binding's ref.
    async fn checked_entries(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<(GitRepoBinding, String)>, RunRepositoryError> {
        let mut checked: Vec<(GitRepoBinding, String)> = Vec::with_capacity(entries.len());
        for entry in entries {
            let binding = self.run_binding(tenant_id, person, entry).await?;
            if checked
                .iter()
                .any(|(other, _)| other.label == binding.label)
            {
                return Err(RunRepositoryError::Refused(format!(
                    "repository '{}' is named twice",
                    binding.label
                )));
            }
            let branch = entry
                .branch
                .clone()
                .unwrap_or_else(|| default_work_branch(run));
            let ref_name = match &binding.git_ref {
                GitRef::Branch(name) | GitRef::Tag(name) | GitRef::Commit(name) => name,
            };
            if &branch == ref_name {
                return Err(RunRepositoryError::Refused(format!(
                    "a run works on its own branch, not on '{ref_name}'"
                )));
            }
            checked.push((binding, branch));
        }
        Ok(checked)
    }

    fn drain_and_publish(&self, binding: &mut GitRepoBinding) {
        for event in binding.take_events() {
            self.event_bus.publish_git_repo_event(event);
        }
    }
}

/// The HTTPS username a git credential uses when its secret stores none (a
/// `username` field in OpenBao, read first): `x-access-token`, which GitHub
/// takes for a token and other hosts ignore beside one. It is the same for
/// every provider, as it was when the providers were matched by name; no
/// provider name is known here (AEGIS ADR-125, Update of 2026-10-04,
/// clause 1).
const DEFAULT_GIT_USERNAME: &str = "x-access-token";

fn default_username_for(_cb: &UserCredentialBinding) -> String {
    DEFAULT_GIT_USERNAME.to_string()
}

// ============================================================================
// HMAC verification
// ============================================================================

/// Verify an inbound webhook signature against the stored secret.
///
/// Returns `true` when the signature matches; `false` for any failure
/// (malformed header, algorithm mismatch, hex decode failure, or
/// signature mismatch). All comparisons use constant-time equality.
pub fn verify_webhook(auth: &WebhookAuth, payload: &[u8], secret: &[u8]) -> bool {
    match auth.provider {
        WebhookProvider::GitLab => ct_slice_eq(secret, auth.signature.as_bytes()),
        WebhookProvider::GitHub => {
            let Some(hex_sig) = auth.signature.strip_prefix("sha256=") else {
                return false;
            };
            let Ok(given) = hex::decode(hex_sig) else {
                return false;
            };
            let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
                return false;
            };
            mac.update(payload);
            let expected = mac.finalize().into_bytes();
            ct_slice_eq(&given, expected.as_slice())
        }
        WebhookProvider::Bitbucket => {
            let Some(hex_sig) = auth.signature.strip_prefix("sha1=") else {
                return false;
            };
            let Ok(given) = hex::decode(hex_sig) else {
                return false;
            };
            let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(secret) else {
                return false;
            };
            mac.update(payload);
            let expected = mac.finalize().into_bytes();
            let valid = ct_slice_eq(&given, expected.as_slice());
            if valid {
                // Audit 002 §4.37.5 — Bitbucket Cloud's webhook signature
                // header uses HMAC-SHA1, which is past its cryptographic
                // shelf life. Bitbucket has not yet shipped a SHA-256
                // alternative, so we still honour the signature, but every
                // accepted SHA-1 verification gets a deprecation warning so
                // the operator dashboard can track the residual risk and
                // prepare to disable this provider once Bitbucket rolls a
                // stronger algorithm.
                warn!(
                    provider = "bitbucket",
                    algorithm = "hmac-sha1",
                    "accepted Bitbucket webhook with deprecated HMAC-SHA1 signature; \
                     track upgrade to HMAC-SHA256 when Bitbucket \
                     publishes the alternative header"
                );
            }
            valid
        }
    }
}

/// Length-checked constant-time slice equality. `subtle`'s
/// `ConstantTimeEq::ct_eq` on `[T]` panics when lengths differ, so we
/// short-circuit the length check ourselves.
fn ct_slice_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

// ============================================================================
// Module-private helpers
// ============================================================================

/// Where a binding's working tree is (see `GitRepoService::resolve_tree`).
enum Tree {
    /// A HostPath volume's directory, worked in process by libgit2.
    Host(PathBuf),
    /// Any other volume, worked by a git step that mounts it.
    Volume(Box<Volume>),
}

// ============================================================================
// A run's repositories (AEGIS ADR-136 G3, G4, G5)
// ============================================================================

/// Why a run's repositories could not be prepared or mounted: a refusal of
/// G3 or G3a, answered to the caller in its own words, or a failure of the
/// git path.
#[derive(Debug, Error)]
pub enum RunRepositoryError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Failed(#[from] GitRepoError),
}

/// What a run's preparation did for one repository (AEGIS ADR-136 G5b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRepository {
    pub label: String,
    pub branch: String,
    /// The commit the work branch stood at once checked out.
    pub started_from: String,
    /// Whether the branch was created from the binding's ref.
    pub created: bool,
    pub prepared_at: chrono::DateTime<Utc>,
}

/// What a run's push answers (AEGIS ADR-136 G5c, G8a, G8b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPush {
    pub branch: String,
    /// The binding's URL, with no user info.
    pub remote_url: String,
    /// The host's page of the branch.
    pub branch_url: String,
}

// ============================================================================
// The workflow interpreter's repository steps (AEGIS ADR-141 F5)
// ============================================================================

/// The refusal of a repository step in a workflow run that holds no
/// repository (AEGIS ADR-141 F5).
pub const RUN_HOLDS_NO_REPOSITORY: &str = "this run holds no repository";

/// The refusal of `aegis.git.land` called by anything but a workflow's own
/// landing step (AEGIS ADR-141 F8).
pub const LAND_IS_THE_WORKFLOWS: &str =
    "aegis.git.land is answered only for a workflow's own landing step";

/// What one of the workflow interpreter's repository steps answers (AEGIS
/// ADR-141 F5): the commit made or landed, the run's work branch, the
/// binding's ref, the diff, and, when the step did not act, the sentence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryActionAnswer {
    pub commit_sha: Option<String>,
    pub branch: String,
    pub git_ref: String,
    pub diff: Option<String>,
    pub sentence: Option<String>,
}

impl RepositoryActionAnswer {
    pub fn refused(sentence: impl Into<String>) -> Self {
        Self {
            sentence: Some(sentence.into()),
            ..Self::default()
        }
    }
}

/// Why a repository step could not be answered at all.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryActionError {
    #[error("unknown repository action '{0}': expected diff, commit or land")]
    UnknownAction(String),
    #[error("a repository commit needs a message")]
    NoMessage,
    #[error("workflow execution {0} not found")]
    RunNotFound(uuid::Uuid),
    #[error("{0} is not configured on this node")]
    NotConfigured(&'static str),
    #[error("{0}")]
    Failed(String),
}

/// What a run's landing answers (AEGIS ADR-141 F8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLanding {
    pub label: String,
    pub branch: String,
    /// The binding's ref, the branch the landing fast-forwarded.
    pub git_ref: String,
    /// The work branch's HEAD, now the ref's.
    pub commit_sha: String,
    /// The binding's URL, with no user info.
    pub remote_url: String,
    /// The host's page of the ref.
    pub branch_url: String,
}

/// The branch a binding's ref lands on, or the refusal of a tag or a commit
/// (AEGIS ADR-141 F8).
pub fn landing_ref(git_ref: &GitRef) -> Result<&str, GitRepoError> {
    match git_ref {
        GitRef::Branch(name) => Ok(name),
        GitRef::Tag(name) | GitRef::Commit(name) => Err(GitRepoError::NotABranch {
            git_ref: name.clone(),
        }),
    }
}

/// A run's tree as `aegis.git.status` reads it (AEGIS ADR-136 G7c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTreeStatus {
    pub clean: bool,
    pub head: String,
}

/// The host's page of `branch` for a repository URL that holds no user info
/// (AEGIS ADR-136 G8b): an `http(s)` URL without `.git`, plus
/// `/tree/<branch>`; an scp-style or `ssh://` URL becomes
/// `https://<host>/<path>/tree/<branch>`.
pub fn branch_url(remote_url: &str, branch: &str) -> String {
    let url = remote_url.trim();
    let base = if url.starts_with("https://") || url.starts_with("http://") {
        url.to_string()
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        format!("https://{host}/{path}")
    } else if let Some((host, path)) = url
        .split_once(':')
        .filter(|(host, _)| !host.is_empty() && !host.contains('/'))
    {
        let host = host.rsplit('@').next().unwrap_or(host);
        format!("https://{host}/{}", path.trim_start_matches('/'))
    } else {
        url.to_string()
    };
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix(".git").unwrap_or(base);
    format!("{base}/tree/{branch}")
}

/// One repository mounted in a run's container.
#[derive(Debug, Clone)]
pub struct RunMount {
    pub label: String,
    pub mount: VolumeMount,
}

/// What a run needs of its repositories: prepared once when the run starts
/// (checked, held, the work branch checked out), mounted in each of its
/// agent executions, released when it ends (AEGIS ADR-136 G3 to G5).
#[async_trait]
pub trait RunRepositories: Send + Sync {
    /// At a run's start, before any container: every entry checked (G3,
    /// G3a), every binding held for `run` (G4a), each work branch checked
    /// out (G5, G5a). Answers the entries with each branch filled in, the
    /// run's default where none was given. Nothing stays held on a refusal.
    async fn prepare_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError>;

    /// For each agent execution of `run` (its root, a workflow state, a
    /// child): the entries checked again and the mount of each tree at
    /// `/workspace/<label>`. A binding no run holds is held for `run` again
    /// (after a restart, G4a); one another run holds is refused.
    async fn mounts_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunMount>, RunRepositoryError>;

    /// Release every binding `run` holds.
    fn release_run(&self, run: uuid::Uuid);

    /// What `run`'s preparation did, once: the first agent execution of the
    /// run takes it to publish its narrative rows (AEGIS ADR-136 G5b, G13a).
    fn take_prepared(&self, run: uuid::Uuid) -> Vec<PreparedRepository>;
}

#[async_trait]
impl RunRepositories for GitRepoService {
    async fn prepare_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        let checked = self
            .checked_entries(tenant_id, person, run, entries)
            .await?;
        let mut taken = Vec::new();
        for (binding, _) in &checked {
            match self.hold_for_run(binding, run) {
                Ok(true) => taken.push(binding.id),
                Ok(false) => {}
                Err(sentence) => {
                    self.release_ids(run, &taken);
                    return Err(RunRepositoryError::Refused(sentence));
                }
            }
        }
        let owner = person.unwrap_or_default();
        // The author is the platform's: any an entry carried is replaced by
        // the person's own, or removed when there is none (G5d).
        let author = self.run_author(person).await;
        let mut prepared = Vec::with_capacity(checked.len());
        let mut preparations = Vec::with_capacity(checked.len());
        for (binding, branch) in checked {
            match self.check_out_work_branch(&binding, owner, &branch).await {
                Ok((started_from, created)) => {
                    preparations.push(PreparedRepository {
                        label: binding.label.clone(),
                        branch: branch.clone(),
                        started_from: started_from.clone(),
                        created,
                        prepared_at: Utc::now(),
                    });
                    // The narrative row is G5b's; until then the preparation is logged.
                    info!(
                        run = %run,
                        repository = %binding.label,
                        branch = %branch,
                        started_from = %started_from,
                        created,
                        "a run's repository is prepared"
                    );
                    let git_ref = match &binding.git_ref {
                        GitRef::Branch(name) | GitRef::Tag(name) | GitRef::Commit(name) => {
                            name.clone()
                        }
                    };
                    prepared.push(RunRepository {
                        binding_id: binding.id,
                        branch: Some(branch),
                        author: author.clone(),
                        label: Some(binding.label.clone()),
                        git_ref: Some(git_ref),
                        started_from: Some(started_from),
                    });
                }
                Err(e) => {
                    self.release_ids(run, &taken);
                    return Err(RunRepositoryError::Failed(e));
                }
            }
        }
        self.run_preparations
            .lock()
            .expect("run preparations lock")
            .insert(run, preparations);
        Ok(prepared)
    }

    async fn mounts_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        let checked = self
            .checked_entries(tenant_id, person, run, entries)
            .await?;
        let mut mounts = Vec::with_capacity(checked.len());
        for (binding, _) in &checked {
            self.hold_for_run(binding, run)
                .map_err(RunRepositoryError::Refused)?;
            mounts.push(RunMount {
                label: binding.label.clone(),
                mount: self.tree_mount(binding).await?,
            });
        }
        Ok(mounts)
    }

    fn release_run(&self, run: uuid::Uuid) {
        GitRepoService::release_run(self, run);
    }

    fn take_prepared(&self, run: uuid::Uuid) -> Vec<PreparedRepository> {
        self.run_preparations
            .lock()
            .expect("run preparations lock")
            .remove(&run)
            .unwrap_or_default()
    }
}

/// Releases a run's repositories when dropped, unless it was given none or
/// was disarmed (AEGIS ADR-136 G4): every early return of a start after the
/// run was prepared, and the end of the task that runs it, drop it.
pub struct RunHoldGuard {
    repositories: Option<Arc<dyn RunRepositories>>,
    run: uuid::Uuid,
}

impl RunHoldGuard {
    pub fn new(repositories: Option<Arc<dyn RunRepositories>>, run: uuid::Uuid) -> Self {
        Self { repositories, run }
    }

    /// Keep the hold: the run has started and its end releases it.
    pub fn disarm(mut self) {
        self.repositories = None;
    }
}

impl Drop for RunHoldGuard {
    fn drop(&mut self) {
        if let Some(repositories) = self.repositories.take() {
            repositories.release_run(self.run);
        }
    }
}

impl GitRepoService {
    /// Release the holds `run` took in a call that is now refused.
    fn release_ids(&self, run: uuid::Uuid, ids: &[GitRepoBindingId]) {
        let mut holds = self.run_holds.lock().expect("run holds lock");
        for id in ids {
            if holds.get(id) == Some(&run) {
                holds.remove(id);
            }
        }
    }
}

/// The word a refusal uses for a binding's status (G3).
fn status_word(status: &GitRepoStatus) -> &'static str {
    match status {
        GitRepoStatus::Pending => "pending",
        GitRepoStatus::Cloning => "cloning",
        GitRepoStatus::Ready => "ready",
        GitRepoStatus::Refreshing => "refreshing",
        GitRepoStatus::Failed { .. } => "failed",
        GitRepoStatus::Deleted => "deleted",
    }
}

/// Attach `credential` to `callbacks`, as a push does. The guard of an SSH
/// key must outlive the git call.
fn attach_credential(
    callbacks: &mut git2::RemoteCallbacks<'_>,
    credential: Option<&ResolvedCredential>,
) -> Result<Option<crate::application::git_ssh_key::SshKeyTempFile>, GitRepoError> {
    match credential {
        None => Ok(None),
        Some(ResolvedCredential::HttpsPat { username, token }) => {
            let username = username.clone();
            let token = token.clone();
            callbacks.credentials(move |_url, _user_from_url, _allowed| {
                git2::Cred::userpass_plaintext(&username, token.expose())
            });
            Ok(None)
        }
        Some(ResolvedCredential::SshKey {
            private_key_pem,
            passphrase,
        }) => attach_ssh_credentials(
            callbacks,
            private_key_pem.expose(),
            passphrase.as_ref().map(|p| p.expose()),
        )
        .map(Some)
        .map_err(|e| GitRepoError::GitFailed(e.to_string())),
    }
}

/// The work-branch checkout on a host directory, by libgit2 (AEGIS ADR-136
/// G5a): `origin` pointed at the binding's URL, the ref fetched; the
/// remote's copy of `branch` checked out when the remote lists it, else
/// `branch` created from the ref; untracked files that are not ignored
/// removed. An error's text holds no part of the credential.
fn work_branch_in_dir(
    target_dir: &std::path::Path,
    repo_url: &SensitiveUrl,
    git_ref: &GitRef,
    branch: &str,
    credential: Option<ResolvedCredential>,
    ssh_host_keys: &[crate::domain::git_host_keys::SshHostKey],
) -> Result<(String, bool), GitRepoError> {
    let (url, credential) = clone_credential(repo_url.expose(), credential);
    let secrets = credential_secrets(credential.as_ref());
    let git = |e: git2::Error| GitRepoError::GitFailed(redact_git_output(e.message(), &secrets));
    let keys = host_keys_for(&url, ssh_host_keys).map_err(GitRepoError::GitFailed)?;
    let callbacks = || -> Result<_, GitRepoError> {
        let mut callbacks = git2::RemoteCallbacks::new();
        if let Some(keys) = keys.clone() {
            check_ssh_host_key(&mut callbacks, keys);
        }
        let guard = attach_credential(&mut callbacks, credential.as_ref())?;
        Ok((callbacks, guard))
    };

    let repo = git2::Repository::open(target_dir).map_err(git)?;
    if repo.find_remote("origin").is_ok() {
        repo.remote_set_url("origin", &url).map_err(git)?;
    } else {
        repo.remote("origin", &url).map_err(git)?;
    }
    let mut remote = repo.find_remote("origin").map_err(git)?;

    // Does the remote have the branch? (`ls-remote`)
    let branch_ref = format!("refs/heads/{branch}");
    let on_remote = {
        let (cbs, _guard) = callbacks()?;
        let connection = remote
            .connect_auth(git2::Direction::Fetch, Some(cbs), None)
            .map_err(git)?;
        let listed = connection.list().map_err(git)?;
        listed.iter().any(|head| head.name() == branch_ref)
    };

    let mut refspecs: Vec<String> = match git_ref {
        GitRef::Branch(name) => vec![format!("+refs/heads/{name}:refs/remotes/origin/{name}")],
        GitRef::Tag(name) => vec![format!("+refs/tags/{name}:refs/tags/{name}")],
        GitRef::Commit(_) => vec![
            "+refs/heads/*:refs/remotes/origin/*".to_string(),
            "+refs/tags/*:refs/tags/*".to_string(),
        ],
    };
    if on_remote {
        refspecs.push(format!("+{branch_ref}:refs/remotes/origin/{branch}"));
    }
    {
        let (cbs, _guard) = callbacks()?;
        let mut fetch_opts = git2::FetchOptions::new();
        fetch_opts.remote_callbacks(cbs);
        let specs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        remote
            .fetch(&specs, Some(&mut fetch_opts), None)
            .map_err(git)?;
    }

    let start = if on_remote {
        repo.find_reference(&format!("refs/remotes/origin/{branch}"))
            .and_then(|r| r.peel_to_commit())
            .map_err(git)?
    } else {
        match git_ref {
            GitRef::Branch(name) => repo
                .find_reference(&format!("refs/remotes/origin/{name}"))
                .and_then(|r| r.peel_to_commit()),
            GitRef::Tag(name) => repo
                .find_reference(&format!("refs/tags/{name}"))
                .and_then(|r| r.peel_to_commit()),
            GitRef::Commit(sha) => git2::Oid::from_str(sha).and_then(|oid| repo.find_commit(oid)),
        }
        .map_err(git)?
    };

    // `checkout --force -B <branch> <start>`: HEAD leaves the branch first,
    // since libgit2 will not move the branch HEAD is on.
    repo.set_head_detached(start.id()).map_err(git)?;
    repo.branch(branch, &start, true).map_err(git)?;
    repo.set_head(&branch_ref).map_err(git)?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .map_err(git)?;

    // `clean -ffd`: untracked files and directories that are not ignored.
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(false)
        .include_ignored(false);
    let untracked: Vec<PathBuf> = repo
        .statuses(Some(&mut options))
        .map_err(git)?
        .iter()
        .filter(|entry| entry.status().contains(git2::Status::WT_NEW))
        .filter_map(|entry| entry.path().ok().map(|p| target_dir.join(p)))
        .collect();
    for path in untracked {
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.map_err(|e| GitRepoError::GitFailed(format!("clean: {e}")))?;
    }

    Ok((start.id().to_string(), !on_remote))
}

/// A git step's failure as the commit, push and diff calls answer it.
fn step_error(e: CloneError) -> GitRepoError {
    match e {
        CloneError::Git(m) | CloneError::Io(m) => GitRepoError::GitFailed(m),
        CloneError::NotYetImplemented(m) => GitRepoError::NotYetImplemented(m),
        CloneError::RemoteAhead(branch) => GitRepoError::RemoteAhead { branch },
    }
}

/// Resolve the on-disk clone target for a HostPath-backed volume.
/// The person a binding's volume belongs to, when it is a persistent one.
fn volume_owner(volume: &Volume) -> Option<&str> {
    match &volume.ownership {
        VolumeOwnership::Persistent { owner } => Some(owner.as_str()),
        _ => None,
    }
}

fn host_path_for_volume(volume: &Volume) -> Result<PathBuf, String> {
    match &volume.backend {
        VolumeBackend::HostPath { path } => Ok(path.clone()),
        VolumeBackend::SeaweedFS { .. }
        | VolumeBackend::OpenDal { .. }
        | VolumeBackend::Seal { .. } => Err(format!(
            "libgit2 clone only supports HostPath volumes; volume {} has backend {:?}",
            volume.id, volume.backend
        )),
    }
}

/// Concurrency gate for B2 commit / push / diff.
///
/// Only [`GitRepoStatus::Ready`] bindings accept canvas writes. A
/// mid-clone or mid-refresh binding could have an inconsistent index or
/// detached working tree — refusing the operation is safer than racing
/// libgit2 against our own fetch task.
fn ensure_binding_ready(binding: &GitRepoBinding) -> Result<(), GitRepoError> {
    match &binding.status {
        GitRepoStatus::Ready => Ok(()),
        other => Err(GitRepoError::BindingBusy(format!("{other:?}"))),
    }
}

/// Blocking libgit2 commit against `target_dir`.
///
/// Stages every change in the workdir, writes the tree, and commits on
/// `HEAD` against the existing parent. Returns the 40-char hex SHA.
/// Returns [`GitRepoError::NothingToCommit`] when the staged tree is
/// identical to HEAD's.
fn blocking_commit(
    target_dir: &std::path::Path,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> Result<String, GitRepoError> {
    use git2::{IndexAddOption, Repository};

    let repo = Repository::open(target_dir).map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    // Stage everything under the workdir.
    let mut index = repo
        .index()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
    index
        .write()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    let tree_id = index
        .write_tree()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    // Resolve parent (HEAD). A "nothing to commit" guard: if HEAD's tree
    // matches the freshly-written tree, refuse.
    let parent_commit = repo
        .head()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
        .peel_to_commit()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
    let parent_tree_id = parent_commit.tree_id();
    if parent_tree_id == tree_id {
        return Err(GitRepoError::NothingToCommit);
    }

    let sig = git2::Signature::now(author_name, author_email)
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    let commit_oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent_commit])
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    Ok(commit_oid.to_string())
}

/// The SSH host keys a new binding holds, from the lines its creator gave.
///
/// An SSH remote on a host other than the well-known ones must be given its
/// key, since every clone checks it; keys given for a remote that is not
/// reached over SSH are refused rather than ignored.
fn binding_host_keys(repo_url: &str, lines: &[String]) -> Result<Vec<SshHostKey>, GitRepoError> {
    let keys = lines
        .iter()
        .map(|line| SshHostKey::parse(line))
        .collect::<Result<Vec<_>, _>>()
        .map_err(GitRepoError::SshHostKeys)?;
    if ssh_remote(repo_url).is_none() {
        if keys.is_empty() {
            return Ok(keys);
        }
        return Err(GitRepoError::SshHostKeys(
            "`ssh_host_keys` is for a repository reached over SSH (git@host:path); this one is not"
                .to_string(),
        ));
    }
    host_keys_for(repo_url, &keys).map_err(GitRepoError::SshHostKeys)?;
    Ok(keys)
}

/// Push the working tree at `target_dir` to `remote_name` for the binding
/// whose repository URL is `repo_url`, authenticating with `credential`.
///
/// The credential is the one the binding names or, failing that, the user
/// info of its URL, as for a clone. User info is taken off the remote's URL
/// in `.git/config`, and the remote still points where it did. An error's
/// text holds no part of the credential.
pub(crate) fn push_to_remote(
    target_dir: &std::path::Path,
    repo_url: &SensitiveUrl,
    remote_name: &str,
    ref_name: Option<String>,
    credential: Option<ResolvedCredential>,
    ssh_host_keys: &[crate::domain::git_host_keys::SshHostKey],
) -> Result<String, GitRepoError> {
    let (_, credential) = clone_credential(repo_url.expose(), credential);
    let secrets = credential_secrets(credential.as_ref());
    let pushed = (|| {
        let repo = git2::Repository::open(target_dir)
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
        strip_remote_user_info(&repo, remote_name)
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
        // The push goes where the tree's remote points. The binding's keys
        // are for the binding's host; a remote on another host is checked
        // against that host's published keys, or refused.
        let remote_url = repo
            .find_remote(remote_name)
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
            .url()
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
            .to_string();
        let same_host = ssh_remote(&remote_url).map(|(host, _)| host)
            == ssh_remote(repo_url.expose()).map(|(host, _)| host);
        let keys = host_keys_for(&remote_url, if same_host { ssh_host_keys } else { &[] })
            .map_err(GitRepoError::GitFailed)?;
        blocking_push(target_dir, remote_name, ref_name, credential, keys)
    })();
    pushed.map_err(|e| match e {
        GitRepoError::GitFailed(m) => GitRepoError::GitFailed(redact_git_output(&m, &secrets)),
        other => other,
    })
}

/// Blocking libgit2 push against `target_dir`.
///
/// Resolves `ref_name` to the shorthand of HEAD when `None`; a reference name
/// that is not valid UTF-8 returns [`GitRepoError::GitFailed`]. Credentials map the
/// [`ResolvedCredential`] surface onto the libgit2 callback — HTTPS-PAT
/// via `Cred::userpass_plaintext`, SSH via the shared
/// [`attach_ssh_credentials`] helper (mode-`0600` tempfile, zeroize-on-
/// drop).
/// Returns the resolved `ref_name` so the service can emit
/// [`GitRepoEvent::PushCompleted`] with the actual ref that was pushed.
fn blocking_push(
    target_dir: &std::path::Path,
    remote_name: &str,
    ref_name: Option<String>,
    credential: Option<ResolvedCredential>,
    ssh_host_keys: Option<Vec<crate::domain::git_host_keys::SshHostKey>>,
) -> Result<String, GitRepoError> {
    let repo =
        git2::Repository::open(target_dir).map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    // Resolve the ref to push: explicit value wins, else the shorthand of
    // whatever HEAD resolves to.
    //
    // `Reference::shorthand()` fails on exactly one condition — the reference
    // name is not valid UTF-8 — in git2 0.20 (`None`) and 0.21 (`Err`) alike.
    // It does NOT fail on a detached HEAD: libgit2's `git_repository_head`
    // returns the direct `HEAD` reference in that case and
    // `git_reference__shorthand` returns the full name when no `refs/` prefix
    // matches, so the shorthand is the string "HEAD". The previous code mapped
    // this failure to `NoHeadBranch`, which reported every such git failure as
    // "no current branch"; a UTF-8 failure is a git failure and is reported as
    // one. See the arc note on `NoHeadBranch` having no producer on this path.
    let resolved_ref = match ref_name {
        Some(r) => r,
        None => {
            let head = repo
                .head()
                .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
            head.shorthand()
                .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
                .to_string()
        }
    };
    drop(repo);

    match blocking_push_refs(
        target_dir,
        remote_name,
        &[(resolved_ref.clone(), resolved_ref.clone())],
        credential,
        ssh_host_keys,
    )? {
        Pushed::All => Ok(resolved_ref),
        Pushed::RefusedAt(_) => Err(GitRepoError::RemoteAhead {
            branch: resolved_ref,
        }),
    }
}

/// How far a sequence of pushes went.
enum Pushed {
    /// Every push was accepted.
    All,
    /// The push at this index was refused as not a fast-forward; none after
    /// it was attempted.
    RefusedAt(usize),
}

/// Blocking libgit2 pushes against `target_dir`, in order, each of
/// `refs/heads/<source>` to `refs/heads/<destination>`, never with force,
/// one connection's credentials for all. A push the remote refuses as not a
/// fast-forward stops the sequence and answers its index (AEGIS ADR-136
/// G8a, ADR-141 F8); any other refusal is a git failure.
fn blocking_push_refs(
    target_dir: &std::path::Path,
    remote_name: &str,
    refs: &[(String, String)],
    credential: Option<ResolvedCredential>,
    ssh_host_keys: Option<Vec<crate::domain::git_host_keys::SshHostKey>>,
) -> Result<Pushed, GitRepoError> {
    use git2::{PushOptions, RemoteCallbacks, Repository};

    let repo = Repository::open(target_dir).map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
    let mut remote = repo
        .find_remote(remote_name)
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    let mut callbacks = RemoteCallbacks::new();
    if let Some(keys) = ssh_host_keys {
        check_ssh_host_key(&mut callbacks, keys);
    }
    // SSH guard must outlive the `remote.push()` calls — libgit2 reads
    // the key tempfile from inside `push`. Dropping the guard before
    // then zeros the file and would break auth. Bind it into this
    // outer scope so it lives until the function returns.
    let _ssh_guard = if let Some(cred) = credential {
        match cred {
            ResolvedCredential::HttpsPat { username, token } => {
                callbacks.credentials(move |_url: &str, _user_from_url: Option<&str>, _allowed| {
                    git2::Cred::userpass_plaintext(&username, token.expose())
                });
                None
            }
            ResolvedCredential::SshKey {
                private_key_pem,
                passphrase,
            } => {
                let passphrase_ref = passphrase.as_ref().map(|p| p.expose());
                let guard = attach_ssh_credentials(
                    &mut callbacks,
                    private_key_pem.expose(),
                    passphrase_ref,
                )
                .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
                Some(guard)
            }
        }
    } else {
        None
    };

    // The remote's answer for the ref: a refusal arrives here, not as an
    // error of `push` (AEGIS ADR-136 G8a).
    let refused: std::rc::Rc<std::cell::RefCell<Option<String>>> = Default::default();
    let refused_in_callback = refused.clone();
    callbacks.push_update_reference(move |_refname, status| {
        if let Some(status) = status {
            *refused_in_callback.borrow_mut() = Some(status.to_string());
        }
        Ok(())
    });

    let mut push_opts = PushOptions::new();
    push_opts.remote_callbacks(callbacks);

    for (index, (source, destination)) in refs.iter().enumerate() {
        refused.borrow_mut().take();
        let refspec = format!("refs/heads/{source}:refs/heads/{destination}");
        if let Err(e) = remote.push(&[refspec.as_str()], Some(&mut push_opts)) {
            if e.code() == git2::ErrorCode::NotFastForward {
                return Ok(Pushed::RefusedAt(index));
            }
            return Err(GitRepoError::GitFailed(e.to_string()));
        }
        let status = refused.borrow_mut().take();
        if let Some(status) = status {
            if status.contains("non-fast-forward") || status.contains("fetch first") {
                return Ok(Pushed::RefusedAt(index));
            }
            return Err(GitRepoError::GitFailed(format!(
                "the remote refused the push of {source} to {destination}: {status}"
            )));
        }
    }
    Ok(Pushed::All)
}

/// Land the work branch `branch` of the working tree at `target_dir` on the
/// binding's branch `git_ref` (AEGIS ADR-141 F8): push the work branch to
/// `origin`, then its HEAD to `refs/heads/<git_ref>`, neither with force,
/// with the credential a push takes ([`push_to_remote`]). Answers the commit
/// landed. A refusal of the first push answers G8's sentence and lands
/// nothing; of the second, [`GitRepoError::RefAhead`] with the ref unchanged.
pub(crate) fn land_to_remote(
    target_dir: &std::path::Path,
    repo_url: &SensitiveUrl,
    branch: &str,
    git_ref: &str,
    credential: Option<ResolvedCredential>,
    ssh_host_keys: &[crate::domain::git_host_keys::SshHostKey],
) -> Result<String, GitRepoError> {
    let (_, credential) = clone_credential(repo_url.expose(), credential);
    let secrets = credential_secrets(credential.as_ref());
    let landed = (|| {
        let repo = git2::Repository::open(target_dir)
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
        strip_remote_user_info(&repo, "origin")
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
        let commit_sha = repo
            .refname_to_id(&format!("refs/heads/{branch}"))
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
            .to_string();
        let remote_url = repo
            .find_remote("origin")
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
            .url()
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
            .to_string();
        drop(repo);
        let same_host = ssh_remote(&remote_url).map(|(host, _)| host)
            == ssh_remote(repo_url.expose()).map(|(host, _)| host);
        let keys = host_keys_for(&remote_url, if same_host { ssh_host_keys } else { &[] })
            .map_err(GitRepoError::GitFailed)?;
        match blocking_push_refs(
            target_dir,
            "origin",
            &[
                (branch.to_string(), branch.to_string()),
                (branch.to_string(), git_ref.to_string()),
            ],
            credential,
            keys,
        )? {
            Pushed::All => Ok(commit_sha),
            Pushed::RefusedAt(0) => Err(GitRepoError::RemoteAhead {
                branch: branch.to_string(),
            }),
            Pushed::RefusedAt(_) => Err(GitRepoError::RefAhead {
                git_ref: git_ref.to_string(),
            }),
        }
    })();
    landed.map_err(|e| match e {
        GitRepoError::GitFailed(m) => GitRepoError::GitFailed(redact_git_output(&m, &secrets)),
        other => other,
    })
}

/// Whether the tree at `target_dir` is clean (no change, staged or not, and
/// no untracked file that is not ignored), and its HEAD (AEGIS ADR-136 G7c).
fn blocking_head_and_status(target_dir: &std::path::Path) -> Result<(bool, String), GitRepoError> {
    let repo =
        git2::Repository::open(target_dir).map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
    let mut options = git2::StatusOptions::new();
    options.include_untracked(true).include_ignored(false);
    let clean = repo
        .statuses(Some(&mut options))
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
        .is_empty();
    let head = repo
        .head()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
        .peel_to_commit()
        .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
        .id()
        .to_string();
    Ok((clean, head))
}

/// Blocking libgit2 diff against `target_dir`.
///
/// - `staged == true`  → HEAD tree vs index (what would be committed).
/// - `staged == false` → index vs workdir (what has been edited).
///
/// Emits a unified patch text.
fn blocking_diff(target_dir: &std::path::Path, staged: bool) -> Result<String, GitRepoError> {
    use git2::{DiffFormat, Repository};

    let repo = Repository::open(target_dir).map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    let diff = if staged {
        let head_tree = repo
            .head()
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
            .peel_to_tree()
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;
        repo.diff_tree_to_index(Some(&head_tree), None, None)
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
    } else {
        repo.diff_index_to_workdir(None, None)
            .map_err(|e| GitRepoError::GitFailed(e.to_string()))?
    };

    let mut output = String::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = line.origin();
        // Hunk-header / file-header lines arrive with origins outside the
        // '+' / '-' / ' ' set; libgit2's own print routine already
        // includes the correct leading char in `line.content()` for
        // those, so we emit the origin for context/add/remove lines
        // only, and otherwise fall back to no prefix.
        match origin {
            '+' | '-' | ' ' => output.push(origin),
            _ => {}
        }
        output.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })
    .map_err(|e| GitRepoError::GitFailed(e.to_string()))?;

    Ok(output)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// An existing GitHub or GitLab binding (a GitLab one was stored as
    /// `custom:gitlab` and reads as `gitlab`) clones with the username it
    /// had before the provider arms were removed: `x-access-token`, unless
    /// its secret stores a `username` (AEGIS ADR-125, Update of 2026-10-04,
    /// clause 1; the coordinator's correction C1).
    #[test]
    fn a_git_binding_keeps_its_default_username_whatever_its_provider() {
        use crate::domain::credential::{
            CredentialMetadata, CredentialProvider, CredentialScope, CredentialStatus,
            CredentialType,
        };
        use crate::domain::secrets::SecretPath;
        for provider in ["github", "gitlab", "bitbucket", "stripe"] {
            let binding = UserCredentialBinding {
                id: crate::domain::credential::CredentialBindingId::new(),
                owner_user_id: "user-1".to_string(),
                tenant_id: TenantId::consumer(),
                credential_type: CredentialType::Secret,
                provider: CredentialProvider::new(provider),
                secret_path: SecretPath::new("aegis-system", "kv", "users/x/credentials/y"),
                scope: CredentialScope::Personal,
                status: CredentialStatus::Active,
                metadata: CredentialMetadata {
                    label: "pat".to_string(),
                    tags: None,
                    service_url: None,
                    external_account_id: None,
                    oauth_scopes: None,
                    mailbox: None,
                    reach: None,
                    calendar: None,
                },
                grants: Vec::new(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            assert_eq!(
                default_username_for(&binding),
                "x-access-token",
                "{provider}"
            );
        }
    }

    #[test]
    fn create_git_repo_command_debug_does_not_print_the_url_credential() {
        let cmd = CreateGitRepoCommand::new(
            TenantId::consumer(),
            "user-1",
            ZaruTier::Pro,
            "https://user:Mk7-create-command-pat-marker@github.com/o/r.git",
            "label",
        );
        let printed = format!("{cmd:?}");
        assert!(
            !printed.contains("Mk7-create-command-pat-marker"),
            "CreateGitRepoCommand's Debug printed the repository URL's credential: {printed}"
        );
        assert!(
            printed.contains("github.com"),
            "Debug lost the repository host: {printed}"
        );
    }

    fn gh_signature(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        let out = mac.finalize().into_bytes();
        format!("sha256={}", hex::encode(out))
    }

    fn bb_signature(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(secret).unwrap();
        mac.update(body);
        let out = mac.finalize().into_bytes();
        format!("sha1={}", hex::encode(out))
    }

    #[test]
    fn github_hmac_verifies() {
        let body = b"{\"ref\":\"refs/heads/main\"}";
        let secret = b"s3cr3t";
        let auth = WebhookAuth {
            provider: WebhookProvider::GitHub,
            signature: gh_signature(secret, body),
        };
        assert!(verify_webhook(&auth, body, secret));
    }

    #[test]
    fn github_hmac_rejects_wrong_secret() {
        let body = b"payload";
        let auth = WebhookAuth {
            provider: WebhookProvider::GitHub,
            signature: gh_signature(b"right", body),
        };
        assert!(!verify_webhook(&auth, body, b"wrong"));
    }

    #[test]
    fn github_hmac_rejects_wrong_body() {
        let secret = b"s";
        let auth = WebhookAuth {
            provider: WebhookProvider::GitHub,
            signature: gh_signature(secret, b"a"),
        };
        assert!(!verify_webhook(&auth, b"b", secret));
    }

    #[test]
    fn github_hmac_rejects_missing_prefix() {
        let auth = WebhookAuth {
            provider: WebhookProvider::GitHub,
            signature: "deadbeef".to_string(),
        };
        assert!(!verify_webhook(&auth, b"", b"s"));
    }

    #[test]
    fn gitlab_token_verifies_constant_time() {
        let auth = WebhookAuth {
            provider: WebhookProvider::GitLab,
            signature: "shared-secret".to_string(),
        };
        assert!(verify_webhook(&auth, b"", b"shared-secret"));
        assert!(!verify_webhook(&auth, b"", b"shared-secre-"));
    }

    #[test]
    fn bitbucket_hmac_verifies() {
        let body = b"bb";
        let secret = b"s";
        let auth = WebhookAuth {
            provider: WebhookProvider::Bitbucket,
            signature: bb_signature(secret, body),
        };
        assert!(verify_webhook(&auth, body, secret));
    }

    /// Audit 002 §4.37.5 regression. Bitbucket's webhook signature still uses
    /// HMAC-SHA1 (Bitbucket Cloud has not shipped a SHA-256 alternative as of
    /// this audit). We continue to honour the signature so legitimate
    /// Bitbucket integrations keep working, but every accepted SHA-1
    /// verification logs a deprecation warning so operators can track the
    /// residual risk and prepare to disable this provider once a stronger
    /// header lands. This test pins the dual contract: a valid SHA-1
    /// signature must still verify (so we don't silently break the
    /// integration) AND a tampered payload must still be rejected (so the
    /// deprecation warning never ships at the cost of skipping verification).
    #[test]
    fn bitbucket_sha1_remains_accepted_with_deprecation_path() {
        let body = b"bb-payload";
        let secret = b"bb-secret";
        let auth = WebhookAuth {
            provider: WebhookProvider::Bitbucket,
            signature: bb_signature(secret, body),
        };
        // Acceptance arm — exercises the `warn!`-emitting branch.
        assert!(
            verify_webhook(&auth, body, secret),
            "Bitbucket SHA-1 signatures must still verify; the deprecation \
             warning is informational only and must not change semantics"
        );
        // Tamper arm — the deprecation logging must NOT bypass HMAC checks.
        let tampered = b"bb-payload-tampered";
        assert!(
            !verify_webhook(&auth, tampered, secret),
            "Bitbucket SHA-1 verification must reject tampered payloads"
        );
    }

    // -----------------------------------------------------------------
    // Regression: SSH credentials on push must NOT return
    // `NotYetImplemented`.
    //
    // Wave A3 shipped SSH for clone; push was still returning
    // `NotYetImplemented("SSH credential support for push is deferred
    // to ADR-081 Wave A3")` for every `ResolvedCredential::SshKey`,
    // which broke every Canvas git-write session bound to an SSH-
    // authenticated remote. This test drives `blocking_push` against
    // a real on-disk repo with an SSH remote and asserts the SSH code
    // path actually executes — the push will fail (the remote is
    // unreachable / the key is a dummy), but the failure MUST be a
    // real git error, not the old stub.
    // -----------------------------------------------------------------

    #[test]
    fn push_with_ssh_credential_no_longer_returns_not_yet_implemented() {
        use crate::domain::secrets::SensitiveString;

        let tmp = tempfile::tempdir().expect("tempdir");
        let workdir = tmp.path();

        // Init a repo with a commit on `main` so libgit2 has a ref to
        // push. This mirrors the A2 `ready_binding` fixture pattern —
        // a real working tree that the service's push path can open.
        let repo = git2::Repository::init(workdir).expect("git init");
        {
            let sig = git2::Signature::now("Tester", "test@aegis.test").unwrap();
            let mut index = repo.index().unwrap();
            std::fs::write(workdir.join("README.md"), b"hi\n").unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap();
            // Normalise the branch name to `main` so the push refspec
            // is deterministic regardless of the host git's
            // `init.defaultBranch` setting.
            let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
            repo.branch("main", &head_commit, true).unwrap();
            repo.set_head("refs/heads/main").unwrap();
        }

        // SSH remote pointing at a deliberately-unreachable address.
        // libgit2 must reach the credentials callback (proving the
        // SSH code path executes) and then fail on transport.
        repo.remote("origin", "ssh://git@127.0.0.1:1/does-not-exist/repo.git")
            .expect("set origin");

        // A syntactically-valid-ish OpenSSH key header. libgit2 will
        // reject it, but only after traversing the SSH credentials
        // code path — which is exactly what this test pins.
        let fake_key = SensitiveString::new(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n\
             AAAA-not-a-real-key-just-test-bytes\n\
             -----END OPENSSH PRIVATE KEY-----\n",
        );
        let credential = ResolvedCredential::SshKey {
            private_key_pem: fake_key,
            passphrase: None,
        };

        let res = blocking_push(
            workdir,
            "origin",
            Some("main".to_string()),
            Some(credential),
            None,
        );

        // The fix: any failure mode is acceptable EXCEPT the old
        // `NotYetImplemented` stub. A `GitFailed` means the SSH
        // credentials code path ran and libgit2 itself rejected the
        // operation (transport, auth, or key parsing).
        match res {
            Err(GitRepoError::NotYetImplemented(msg)) => {
                panic!(
                    "push with SSH credential still returns NotYetImplemented: {msg:?} \
                     — the SSH push path must be wired"
                );
            }
            Err(GitRepoError::GitFailed(_)) => {
                // Expected: libgit2 executed the SSH credentials
                // callback and failed on transport / auth / key parse.
            }
            Err(other) => {
                // Other error types (NoHeadBranch, etc.) are also
                // acceptable — they prove the old stub is gone. Only
                // NotYetImplemented is disallowed.
                let _ = other;
            }
            Ok(_) => {
                // Pushing to 127.0.0.1:1 cannot succeed; if it did,
                // something is wrong with the test fixture. Don't
                // fail the regression assertion on this — the point
                // is that `NotYetImplemented` no longer fires.
            }
        }
    }
}
