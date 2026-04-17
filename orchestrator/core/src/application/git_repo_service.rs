// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding Application Service (BC-7 Storage Gateway, ADR-081)
//!
//! [`GitRepoService`] — the primary interface for creating, listing,
//! cloning, and deleting [`GitRepoBinding`]s. All business logic lives
//! here; the HTTP layer is a thin shell that translates requests into
//! command structs and maps errors onto status codes.
//!
//! ## A2 Scope (Phase 1)
//!
//! | Method | Status |
//! |---|---|
//! | [`GitRepoService::create_binding`] | implemented |
//! | [`GitRepoService::clone_repo`] | implemented |
//! | [`GitRepoService::list_bindings`] | implemented |
//! | [`GitRepoService::get_binding`] | implemented |
//! | [`GitRepoService::delete_binding`] | implemented |
//! | [`GitRepoService::refresh_repo`] | **A3 stub** — returns `NotYetImplemented` |
//! | [`GitRepoService::handle_webhook`] | **A3 stub** — returns `NotYetImplemented` |
//!
//! ## Keymaster Pattern
//!
//! Credentials never enter the binding row. The service resolves them
//! just-in-time from [`SecretsManager`] inside [`Self::clone_repo`],
//! passes them to [`GitCloneExecutor`], and drops them immediately after
//! the git operation returns.

use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;
use tracing::{error, info, instrument, warn};

use crate::application::git_clone_executor::{CloneError, GitCloneExecutor, ResolvedCredential};
use crate::application::user_volume_service::{UserVolumeError, UserVolumeService};
use crate::application::volume_manager::CreateUserVolumeCommand;
use crate::domain::credential::CredentialBindingId;
use crate::domain::git_repo::{
    validate_repo_url, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository,
};
use crate::domain::git_repo_tier_limits::GitRepoTierLimits;
use crate::domain::iam::ZaruTier;
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::TenantId;
use crate::domain::volume::{Volume, VolumeBackend};
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::secrets_manager::SecretsManager;

// ============================================================================
// Command Types
// ============================================================================

/// Default clone volume size — 500 MB. Intentionally conservative; users
/// can bump via tier upgrades. The hard storage-quota gate lives in
/// [`UserVolumeService`].
const DEFAULT_CLONE_VOLUME_BYTES: u64 = 500 * 1024 * 1024;

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
    pub repo_url: String,
    pub git_ref: GitRef,
    pub sparse_paths: Option<Vec<String>>,
    pub label: String,
    pub auto_refresh: bool,
    /// `true` (default) clones with `depth = 1`. Full-history clones
    /// require explicit opt-in.
    pub shallow: bool,
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
            repo_url: repo_url.into(),
            git_ref: GitRef::default(),
            sparse_paths: None,
            label: label.into(),
            auto_refresh: false,
            shallow: true,
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

    #[error("clone failed: {0}")]
    CloneFailed(String),

    #[error("repository error: {0}")]
    Repository(#[from] RepositoryError),

    #[error("secret resolution failed: {0}")]
    SecretResolutionFailed(String),

    #[error("url validation failed: {0}")]
    UrlValidationFailed(String),

    #[error("volume provisioning failed: {0}")]
    VolumeProvisioningFailed(String),

    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),
}

impl From<UserVolumeError> for GitRepoError {
    fn from(e: UserVolumeError) -> Self {
        Self::VolumeProvisioningFailed(e.to_string())
    }
}

// ============================================================================
// Service
// ============================================================================

/// Application service for [`GitRepoBinding`] lifecycle management.
///
/// Wired via [`GitRepoService::new`] with:
/// - `repo` — Postgres (or in-memory for tests) binding repository
/// - `volume_service` — quota-enforcing persistent volume provisioner
/// - `clone_executor` — libgit2-backed clone / fetch primitive
/// - `secret_manager` — OpenBao-backed credential resolver
/// - `event_bus` — publishes domain events after aggregate commits
pub struct GitRepoService {
    repo: Arc<dyn GitRepoBindingRepository>,
    volume_service: Arc<UserVolumeService>,
    clone_executor: Arc<GitCloneExecutor>,
    /// Retained for A3's full credential-resolution flow; unused on
    /// A2's public-only clone path.
    #[allow(dead_code)]
    secret_manager: Arc<SecretsManager>,
    event_bus: Arc<EventBus>,
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
            event_bus,
        }
    }

    // -----------------------------------------------------------------------
    // create_binding
    // -----------------------------------------------------------------------

    /// Validate the command, enforce tier limits, provision a persistent
    /// volume, and persist a new binding in [`GitRepoStatus::Pending`].
    ///
    /// The caller is responsible for scheduling the background clone
    /// task (e.g. via `tokio::spawn(service.clone_repo(id))`).
    #[instrument(skip(self, cmd), fields(owner = %cmd.owner, repo_url = %cmd.repo_url))]
    pub async fn create_binding(
        &self,
        cmd: CreateGitRepoCommand,
    ) -> Result<GitRepoBinding, GitRepoError> {
        // 1) URL validation (ADR-081 §Security).
        validate_repo_url(&cmd.repo_url).map_err(GitRepoError::UrlValidationFailed)?;

        // 2) Tier limit check.
        let limits = GitRepoTierLimits::for_tier(cmd.zaru_tier.clone());
        if let Some(max) = limits.max_bindings {
            let current = self.repo.count_by_owner(&cmd.tenant_id, &cmd.owner).await?;
            if current >= max {
                return Err(GitRepoError::TierLimitExceeded { max });
            }
        }

        // 3) Provision a persistent volume for the binding.
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

        // 4) Construct the aggregate in Pending state.
        let mut binding = GitRepoBinding::new(
            cmd.tenant_id.clone(),
            cmd.credential_binding_id,
            cmd.repo_url.clone(),
            cmd.git_ref.clone(),
            cmd.sparse_paths.clone(),
            volume.id,
            cmd.label.clone(),
            CloneStrategy::Libgit2,
            cmd.auto_refresh,
            None, // webhook_secret — A3 wires this when auto_refresh enabled.
        );

        // 5) Persist + publish.
        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);
        info!(binding_id = %binding.id, volume_id = %volume.id, "git repo binding created");
        Ok(binding)
    }

    // -----------------------------------------------------------------------
    // clone_repo
    // -----------------------------------------------------------------------

    /// Execute the libgit2 clone for `id` and transition the binding to
    /// [`GitRepoStatus::Ready`] (or `Failed` on error).
    ///
    /// Typically invoked via `tokio::spawn` right after
    /// [`Self::create_binding`] — the HTTP response returns the Pending
    /// binding immediately while this task runs in the background.
    ///
    /// The shallow flag comes from the binding's clone strategy
    /// derivation — for A2 we always shallow-clone.
    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn clone_repo(&self, id: &GitRepoBindingId) -> Result<(), GitRepoError> {
        let mut binding = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or(GitRepoError::BindingNotFound)?;

        // Transition Pending → Cloning.
        binding.start_clone();
        self.repo.save(&binding).await?;
        self.drain_and_publish(&mut binding);

        // Resolve target directory from the volume backend. A2 only
        // supports HostPath-backed volumes for direct libgit2 writes.
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

        let target_dir = match host_path_for_volume(&volume) {
            Ok(p) => p,
            Err(e) => {
                self.fail(&mut binding, e.clone()).await;
                return Err(GitRepoError::VolumeProvisioningFailed(e));
            }
        };

        // Resolve credential (Keymaster: just-in-time, tightly scoped).
        let credential = match self.resolve_credential(&binding).await {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                self.fail(&mut binding, msg.clone()).await;
                return Err(e);
            }
        };

        let started = std::time::Instant::now();
        let shallow = true; // A2: always shallow. A3 honours CreateGitRepoCommand::shallow.

        match self
            .clone_executor
            .clone(&binding, &target_dir, credential, shallow)
            .await
        {
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
                };
                self.fail(&mut binding, msg.clone()).await;
                Err(GitRepoError::CloneFailed(msg))
            }
        }
    }

    // -----------------------------------------------------------------------
    // refresh_repo — A3 stub
    // -----------------------------------------------------------------------

    /// **A3 STUB.** Refresh an existing binding against the remote.
    ///
    /// Validates ownership and returns
    /// [`GitRepoError::NotYetImplemented`] — A3 implements the fetch +
    /// checkout flow and the associated event emissions.
    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn refresh_repo(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<(), GitRepoError> {
        // Ownership gate via get_binding (returns 404 for non-owners).
        let _binding = self.get_binding(id, tenant_id, owner).await?;
        Err(GitRepoError::NotYetImplemented(
            "refresh_repo is deferred to ADR-081 Wave A3",
        ))
    }

    // -----------------------------------------------------------------------
    // list_bindings
    // -----------------------------------------------------------------------

    /// List all bindings owned by `owner` within `tenant_id`.
    ///
    /// A1's repository is tenant-scoped (not owner-scoped). A2 filters
    /// bindings by cross-referencing each `volume_id` against the
    /// caller's volume list, which carries the real ownership stamp
    /// (`VolumeOwnership::Persistent.owner`).
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
        Ok(bindings
            .into_iter()
            .filter(|b| owned_volume_ids.contains(&b.volume_id))
            .collect())
    }

    /// Load a single binding, verifying tenant + ownership.
    ///
    /// Returns [`GitRepoError::BindingNotFound`] — **not** `NotOwned` —
    /// when the binding exists but belongs to a different owner. This
    /// prevents leaking the existence of other users' bindings through
    /// the REST surface (ADR-081 §Security).
    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn get_binding(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<GitRepoBinding, GitRepoError> {
        let binding = self
            .repo
            .find_by_id(id)
            .await?
            .ok_or(GitRepoError::BindingNotFound)?;
        if &binding.tenant_id != tenant_id {
            return Err(GitRepoError::BindingNotFound);
        }
        // Ownership gate: the binding's volume must be owned by `owner`.
        let owned_volumes = self.volume_service.list_volumes(tenant_id, owner).await?;
        if !owned_volumes.iter().any(|v| v.id == binding.volume_id) {
            return Err(GitRepoError::BindingNotFound);
        }
        Ok(binding)
    }

    // -----------------------------------------------------------------------
    // delete_binding
    // -----------------------------------------------------------------------

    /// Delete the binding and its backing volume. Emits
    /// [`crate::domain::git_repo::GitRepoEvent::BindingDeleted`].
    #[instrument(skip(self), fields(binding_id = %id))]
    pub async fn delete_binding(
        &self,
        id: &GitRepoBindingId,
        tenant_id: &TenantId,
        owner: &str,
    ) -> Result<(), GitRepoError> {
        // Ownership gate via get_binding (returns 404 for non-owners).
        let mut binding = self.get_binding(id, tenant_id, owner).await?;

        // Cascade: volume delete first (so a partial failure surfaces
        // as VolumeProvisioningFailed without orphaning the binding
        // row).
        let volume_id = binding.volume_id;
        let _ = self.volume_service.delete_volume(&volume_id, owner).await;

        binding.mark_deleted();
        self.drain_and_publish(&mut binding);
        self.repo.delete(&binding.id).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // handle_webhook — A3 stub
    // -----------------------------------------------------------------------

    /// **A3 STUB.** Handle an inbound webhook from a git provider.
    pub async fn handle_webhook(
        &self,
        _secret: &str,
        _signature: &str,
        _payload: &[u8],
    ) -> Result<(), GitRepoError> {
        Err(GitRepoError::NotYetImplemented(
            "handle_webhook is deferred to ADR-081 Wave A3",
        ))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Transition the binding to `Failed { error }`, persist, and emit
    /// events. Errors inside this helper are logged but never mask the
    /// original failure reason.
    async fn fail(&self, binding: &mut GitRepoBinding, error: String) {
        warn!(binding_id = %binding.id, %error, "marking binding as Failed");
        binding.fail_clone(error);
        if let Err(e) = self.repo.save(binding).await {
            error!(?e, "failed to persist Failed binding state");
        }
        self.drain_and_publish(binding);
    }

    /// Resolve a [`ResolvedCredential`] from OpenBao if the binding has
    /// a credential pinned. Returns `Ok(None)` for public repos.
    ///
    /// Called inside [`Self::clone_repo`] so the token lives on the
    /// stack for the duration of the git operation only.
    ///
    /// **A2 NOTE:** full credential-binding lookup requires wiring a
    /// [`crate::domain::credential::CredentialBindingRepository`] into
    /// this service so we can resolve `credential_binding_id →
    /// SecretPath` verbatim. A2 ships **public-repo clone only**; when
    /// a binding carries a credential id, we return
    /// [`GitRepoError::NotYetImplemented`] and mark the binding
    /// `Failed`. A3 extends this with the full OpenBao lookup + SSH
    /// key handling per ADR-081 §Security.
    ///
    /// TODO(A3): inject `Arc<dyn CredentialBindingRepository>` and
    /// resolve `binding.credential_binding_id` → `UserCredentialBinding`
    /// → `secret_path`, then call
    /// `secret_manager.read_secret_field(effective_mount, path, "value",
    /// …)` verbatim. Branch on `CredentialType::Secret` (→ HttpsPat) vs
    /// `CredentialType::SshKey` (→ SshKey + `scopeguard` temp file).
    async fn resolve_credential(
        &self,
        binding: &GitRepoBinding,
    ) -> Result<Option<ResolvedCredential>, GitRepoError> {
        let Some(_cred_id) = binding.credential_binding_id else {
            return Ok(None);
        };

        // Intentionally fail loudly in A2 when a credential is required.
        // A3 will replace this with a real lookup.
        let _ = self.secret_manager.clone(); // keep the field alive for A3.
        Err(GitRepoError::NotYetImplemented(
            "credential-backed clone (private repos) is deferred to ADR-081 Wave A3",
        ))
    }

    /// Drain buffered aggregate events and publish each to the event bus.
    fn drain_and_publish(&self, binding: &mut GitRepoBinding) {
        for event in binding.take_events() {
            self.event_bus.publish_git_repo_event(event);
        }
    }
}

// ============================================================================
// Module-private helpers
// ============================================================================

/// Resolve the on-disk clone target for a volume's backend.
///
/// A2 only supports [`VolumeBackend::HostPath`] — the direct libgit2
/// write path. Remote SeaweedFS and cross-node SEAL backends will be
/// handled via the `EphemeralCliEngine` fallback in A3.
fn host_path_for_volume(volume: &Volume) -> Result<PathBuf, String> {
    match &volume.backend {
        VolumeBackend::HostPath { path } => Ok(path.clone()),
        VolumeBackend::SeaweedFS { .. }
        | VolumeBackend::OpenDal { .. }
        | VolumeBackend::Seal { .. } => Err(format!(
            "A2 git clone only supports HostPath volumes; volume {} has backend {:?}",
            volume.id, volume.backend
        )),
    }
}
