// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding Application Service (BC-7 Storage Gateway, ADR-081)
//!
//! [`GitRepoService`] — the primary interface for creating, listing,
//! cloning, refreshing, and deleting [`GitRepoBinding`]s. All business
//! logic lives here; the HTTP layer is a thin shell that translates
//! requests into command structs and maps errors onto status codes.
//!
//! ## A3 Scope (Phases 2 / 3 / 4)
//!
//! | Method | Status |
//! |---|---|
//! | [`GitRepoService::create_binding`] | implemented |
//! | [`GitRepoService::clone_repo`] | implemented (HostPath + EphemeralCli) |
//! | [`GitRepoService::list_bindings`] | implemented |
//! | [`GitRepoService::get_binding`] | implemented |
//! | [`GitRepoService::delete_binding`] | implemented |
//! | [`GitRepoService::refresh_repo`] | implemented (fetch + checkout pin) |
//! | [`GitRepoService::handle_webhook`] | implemented (HMAC validated) |
//!
//! ## Keymaster Pattern
//!
//! Credentials never enter the binding row. The service resolves them
//! just-in-time from [`SecretsManager`] inside [`Self::clone_repo`] /
//! [`Self::refresh_repo`], passes them to [`GitCloneExecutor`], and drops
//! them immediately after the git operation returns.

use std::path::PathBuf;
use std::sync::Arc;

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tracing::{error, info, instrument, warn};

use crate::application::git_clone_executor::{CloneError, GitCloneExecutor, ResolvedCredential};
use crate::application::user_volume_service::{UserVolumeError, UserVolumeService};
use crate::application::volume_manager::CreateUserVolumeCommand;
use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialStatus, CredentialType,
    UserCredentialBinding,
};
use crate::domain::git_repo::{
    validate_repo_url, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository,
};
use crate::domain::git_repo_tier_limits::GitRepoTierLimits;
use crate::domain::iam::ZaruTier;
use crate::domain::repository::RepositoryError;
use crate::domain::secrets::AccessContext;
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

    #[error("webhook rejected: {0}")]
    WebhookRejected(String),

    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),
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
// Service
// ============================================================================

/// Application service for [`GitRepoBinding`] lifecycle management.
pub struct GitRepoService {
    repo: Arc<dyn GitRepoBindingRepository>,
    volume_service: Arc<UserVolumeService>,
    clone_executor: Arc<GitCloneExecutor>,
    secret_manager: Arc<SecretsManager>,
    credential_repo: Option<Arc<dyn CredentialBindingRepository>>,
    event_bus: Arc<EventBus>,
    /// Orchestrator identifier used in [`AccessContext`] audit rows.
    orchestrator_id: String,
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
            event_bus,
            orchestrator_id: "git-repo-service".to_string(),
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
    #[instrument(skip(self, cmd), fields(owner = %cmd.owner, repo_url = %cmd.repo_url))]
    pub async fn create_binding(
        &self,
        cmd: CreateGitRepoCommand,
    ) -> Result<GitRepoBinding, GitRepoError> {
        validate_repo_url(&cmd.repo_url).map_err(GitRepoError::UrlValidationFailed)?;

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
        // The secret doubles as the URL path parameter and as the HMAC
        // key for GitLab-style header-token verification.
        let webhook_secret = if cmd.auto_refresh {
            Some(uuid::Uuid::new_v4().simple().to_string())
        } else {
            None
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
        );

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

        let credential = match self.resolve_credential(&binding).await {
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
                    .clone(&binding, &target_dir, credential, shallow)
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

        let credential = match self.resolve_credential(binding).await {
            Ok(c) => c,
            Err(e) => {
                let msg = e.to_string();
                self.fail_refresh(binding, msg.clone()).await;
                return Err(e);
            }
        };

        let target_dir = match host_path_for_volume(&volume) {
            Ok(p) => p,
            Err(e) => {
                // SeaweedFS / OpenDAL / SEAL refresh is currently only
                // supported through a fresh ephemeral clone. For Phase 3
                // we mark this as NotYetImplemented rather than silently
                // dropping to a different code path.
                let msg = format!("refresh via ephemeral CLI not yet implemented: {e}");
                self.fail_refresh(binding, msg.clone()).await;
                return Err(GitRepoError::NotYetImplemented(
                    "refresh for non-HostPath volumes requires ephemeral-cli re-clone (ADR-081 Phase 5)",
                ));
            }
        };

        let started = std::time::Instant::now();
        match self
            .clone_executor
            .fetch_and_checkout(binding, &target_dir, credential)
            .await
        {
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
        Ok(bindings
            .into_iter()
            .filter(|b| owned_volume_ids.contains(&b.volume_id))
            .collect())
    }

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
        let owned_volumes = self.volume_service.list_volumes(tenant_id, owner).await?;
        if !owned_volumes.iter().any(|v| v.id == binding.volume_id) {
            return Err(GitRepoError::BindingNotFound);
        }
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
        let volume_id = binding.volume_id;
        let _ = self.volume_service.delete_volume(&volume_id, owner).await;
        binding.mark_deleted();
        self.drain_and_publish(&mut binding);
        self.repo.delete(&binding.id).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // handle_webhook
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
        let mut binding = self
            .repo
            .find_by_webhook_secret(secret)
            .await?
            .ok_or_else(|| GitRepoError::WebhookRejected("unknown webhook secret".into()))?;

        let Some(stored_secret) = binding.webhook_secret.as_ref() else {
            return Err(GitRepoError::WebhookRejected(
                "binding has no webhook secret configured".into(),
            ));
        };

        if !verify_webhook(auth, payload, stored_secret.as_bytes()) {
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

    /// Resolve a [`ResolvedCredential`] from OpenBao if the binding has
    /// a credential pinned. Returns `Ok(None)` for public repos.
    async fn resolve_credential(
        &self,
        binding: &GitRepoBinding,
    ) -> Result<Option<ResolvedCredential>, GitRepoError> {
        let Some(cred_id) = binding.credential_binding_id else {
            return Ok(None);
        };

        let repo = self
            .credential_repo
            .as_ref()
            .ok_or(GitRepoError::NotYetImplemented(
                "credential-backed clone requires CredentialBindingRepository injection",
            ))?;

        let cb = repo
            .find_by_id(&cred_id)
            .await
            .map_err(|e| GitRepoError::SecretResolutionFailed(e.to_string()))?
            .ok_or_else(|| {
                GitRepoError::SecretResolutionFailed(format!(
                    "credential binding {cred_id} not found"
                ))
            })?;

        // Tenant isolation — the credential must belong to the same
        // tenant as the git repo binding.
        if cb.tenant_id != binding.tenant_id {
            return Err(GitRepoError::SecretResolutionFailed(
                "credential binding tenant mismatch".into(),
            ));
        }

        if cb.status != CredentialStatus::Active {
            return Err(GitRepoError::SecretResolutionFailed(format!(
                "credential binding {cred_id} is not active (status={:?})",
                cb.status
            )));
        }

        let ctx = AccessContext::system(&self.orchestrator_id);
        let engine = cb.secret_path.effective_mount();

        match cb.credential_type {
            CredentialType::Secret | CredentialType::OAuth2 | CredentialType::ServiceAccount => {
                // For PAT / OAuth / service-account credentials we read
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
        }
    }

    fn drain_and_publish(&self, binding: &mut GitRepoBinding) {
        for event in binding.take_events() {
            self.event_bus.publish_git_repo_event(event);
        }
    }
}

fn default_username_for(cb: &UserCredentialBinding) -> String {
    use crate::domain::credential::CredentialProvider;
    match &cb.provider {
        CredentialProvider::GitHub => "x-access-token".to_string(),
        CredentialProvider::Custom(_) => "x-access-token".to_string(),
        _ => "x-access-token".to_string(),
    }
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
            ct_slice_eq(&given, expected.as_slice())
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

/// Resolve the on-disk clone target for a HostPath-backed volume.
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
}
