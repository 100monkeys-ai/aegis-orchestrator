// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Credential Management Application Service (BC-11, ADR-078)
//!
//! Defines [`CredentialManagementService`] — the primary interface for managing
//! user-owned third-party credential bindings, and
//! [`StandardCredentialManagementService`] — the production implementation backed
//! by [`CredentialBindingRepository`] and [`SecretsManager`].
//!
//! ## Responsibilities
//!
//! - Store API-key credentials securely in OpenBao and record the binding in Postgres
//! - Create an SMTP-with-IMAP mailbox binding after a live check of both
//!   servers (AEGIS ADR-125 D1)
//! - Hand out a mailbox's OAuth access token, refreshing it when it is
//!   within 60 seconds of expiry (ADR-125 D3)
//! - Initiate and complete OAuth2 PKCE flows, managing pending state lifecycle
//! - Rotate credential values in OpenBao without changing the binding id
//! - Add / revoke grants that control which agents and workflows may use a credential
//! - Revoke entire credential bindings and purge the secret from OpenBao
//! - Publish [`CredentialEvent`]s for audit, observability, and Cortex learning
//!
//! ## Bounded Context
//!
//! BC-11 Secrets & Identity Management (ADR-078).

use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrantId, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    MailboxSettings, OAuthPendingState, UserCredentialBinding,
};
use crate::domain::events::CredentialEvent;
use crate::domain::node_config::{resolve_env_value, OAuthProviderEntry};
use crate::domain::secrets::{AccessContext, SecretPath, SensitiveString, SensitiveUrl};
use crate::domain::team::{MembershipRepository, MembershipStatus, TeamId};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::mail::{CheckFailureKind, MailboxProbe, SessionMailboxProbe};
use crate::infrastructure::secrets_manager::SecretsManager;
use anyhow::anyhow;
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use url::Url;

/// An access token whose `expires_at` is nearer than this is refreshed
/// before it is handed out (ADR-125 D3).
pub const ACCESS_TOKEN_REFRESH_MARGIN_SECS: i64 = 60;

// ============================================================================
// OAuth Provider Configuration
// ============================================================================

/// Per-provider OAuth 2.0 client configuration required to drive an
/// authorization-code + PKCE flow (RFC 6749 §4.1, RFC 7636).
///
/// `client_secret` is optional: public clients (per RFC 6749 §2.1) omit it and
/// rely on PKCE for proof of possession.
///
/// `authorization_url` and `redirect_uri_allowlist` are mandatory and validated
/// at registry construction time via [`validate_oauth_provider_registry`]:
/// placeholder hosts (`oauth.placeholder`, anything containing the literal
/// substring `"placeholder"`) are rejected, and the allowlist must be
/// non-empty. This is the fix for security audit 002 §4.11 and §4.18.
#[derive(Debug, Clone)]
pub struct OAuthProviderConfig {
    /// Provider's authorization endpoint URL (where the user-agent is sent
    /// to begin the flow). MUST be HTTPS and MUST NOT be a placeholder.
    /// A URL can carry a credential, so it prints redacted.
    pub authorization_url: SensitiveUrl,
    /// Provider's token endpoint URL. MUST be HTTPS (localhost exempted for dev).
    /// Prints redacted.
    pub token_url: SensitiveUrl,
    /// OAuth 2.0 `client_id` registered with the provider.
    pub client_id: String,
    /// OAuth 2.0 `client_secret` for confidential clients. `None` for public clients.
    pub client_secret: Option<SensitiveString>,
    /// Exact-match allowlist of `redirect_uri` values the application is
    /// permitted to use. The caller-supplied `redirect_uri` is rejected
    /// unless it appears verbatim in this list.
    pub redirect_uri_allowlist: Vec<String>,
    /// Scopes requested, sent space-separated as `scope` on the
    /// authorization URL; none sends no `scope` (ADR-125 D2).
    pub scopes: Vec<String>,
    /// Further authorization-request parameters, appended in key order
    /// (e.g. `access_type=offline`, `prompt=consent`).
    pub extra_authorization_params: BTreeMap<String, String>,
}

/// Registry mapping `CredentialProvider` → `OAuthProviderConfig`.
///
/// Shared across the service; loaded at startup from platform configuration.
/// Validate the registry with [`validate_oauth_provider_registry`] before
/// wrapping it in `Arc` and handing it to the service.
pub type OAuthProviderRegistry = HashMap<CredentialProvider, OAuthProviderConfig>;

/// Errors produced by [`validate_oauth_provider_registry`] at startup.
///
/// Boot MUST fail when any of these are returned — see security audit 002
/// §4.11 (placeholder URLs leak PKCE state) and §4.18 (open redirect via
/// arbitrary `redirect_uri`).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OAuthRegistryError {
    #[error("OAuth provider {provider}: authorization_url must not be empty")]
    EmptyAuthorizationUrl { provider: String },
    #[error(
        "OAuth provider {provider}: authorization_url is a placeholder ({url}) — \
         configure a real provider endpoint before booting"
    )]
    PlaceholderAuthorizationUrl { provider: String, url: String },
    #[error("OAuth provider {provider}: authorization_url must be HTTPS: {url}")]
    InsecureAuthorizationUrl { provider: String, url: String },
    #[error("OAuth provider {provider}: authorization_url is unparseable: {detail}")]
    UnparseableAuthorizationUrl { provider: String, detail: String },
    #[error("OAuth provider {provider}: redirect_uri_allowlist must contain at least one entry")]
    EmptyRedirectAllowlist { provider: String },
    #[error("OAuth provider {provider}: redirect_uri_allowlist entry is unparseable: {detail}")]
    UnparseableRedirectUri { provider: String, detail: String },
    #[error("OAuth provider {provider}: {detail}")]
    InvalidEntry { provider: String, detail: String },
    #[error("OAuth provider {provider} is configured more than once")]
    DuplicateProvider { provider: String },
    #[error("OAuth provider {provider}: {field} could not be resolved: {detail}")]
    UnresolvedValue {
        provider: String,
        field: &'static str,
        detail: String,
    },
    #[error("OAuth provider {provider}: client_id must not be empty")]
    EmptyClientId { provider: String },
}

/// Build the registry from the node configuration's `spec.oauth_providers`
/// (AEGIS ADR-125 D2): each entry validated, its `env:` values resolved, and
/// the whole registry validated by [`validate_oauth_provider_registry`].
/// The daemon refuses to boot on any error.
pub fn oauth_provider_registry_from_config(
    entries: &[OAuthProviderEntry],
) -> Result<OAuthProviderRegistry, OAuthRegistryError> {
    let mut registry = OAuthProviderRegistry::new();
    for entry in entries {
        let name = entry.provider.trim().to_string();
        entry
            .validate()
            .map_err(|e| OAuthRegistryError::InvalidEntry {
                provider: name.clone(),
                detail: e.to_string(),
            })?;
        let provider = CredentialProvider::new(name.clone());
        if registry.contains_key(&provider) {
            return Err(OAuthRegistryError::DuplicateProvider { provider: name });
        }
        let client_id = resolve_env_value(entry.client_id.trim()).map_err(|e| {
            OAuthRegistryError::UnresolvedValue {
                provider: name.clone(),
                field: "client_id",
                detail: e.to_string(),
            }
        })?;
        if client_id.trim().is_empty() {
            return Err(OAuthRegistryError::EmptyClientId { provider: name });
        }
        let client_secret = match &entry.client_secret {
            None => None,
            Some(raw) => {
                let value = resolve_env_value(raw.expose().trim()).map_err(|e| {
                    OAuthRegistryError::UnresolvedValue {
                        provider: name.clone(),
                        field: "client_secret",
                        detail: e.to_string(),
                    }
                })?;
                if value.is_empty() {
                    return Err(OAuthRegistryError::InvalidEntry {
                        provider: name,
                        detail: "client_secret resolves to an empty value".to_string(),
                    });
                }
                Some(SensitiveString::new(value))
            }
        };
        registry.insert(
            provider,
            OAuthProviderConfig {
                authorization_url: entry.authorization_url.clone(),
                token_url: entry.token_url.clone(),
                client_id: client_id.trim().to_string(),
                client_secret,
                redirect_uri_allowlist: entry.redirect_uri_allowlist.clone(),
                scopes: entry.scopes.clone(),
                extra_authorization_params: entry.extra_authorization_params.clone(),
            },
        );
    }
    validate_oauth_provider_registry(&registry)?;
    Ok(registry)
}

/// Validate every entry in the registry before the service is constructed.
///
/// Refuses to return `Ok` if any provider has a placeholder authorization
/// URL or an empty allowlist. Callers MUST propagate this error and refuse
/// to boot — there is no safe fallback.
pub fn validate_oauth_provider_registry(
    registry: &OAuthProviderRegistry,
) -> Result<(), OAuthRegistryError> {
    for (provider, cfg) in registry.iter() {
        let provider_str = provider.to_string();

        // Read to validate its form; any URL an error carries is redacted.
        let authorization_url = cfg.authorization_url.expose();
        if authorization_url.is_empty() {
            return Err(OAuthRegistryError::EmptyAuthorizationUrl {
                provider: provider_str,
            });
        }
        // Hard refusal of any URL that contains the literal substring
        // "placeholder" anywhere — this catches the legacy
        // `oauth.placeholder` host and any developer copy-paste of the
        // default value.
        if authorization_url.contains("placeholder") {
            return Err(OAuthRegistryError::PlaceholderAuthorizationUrl {
                provider: provider_str,
                url: cfg.authorization_url.redacted(),
            });
        }
        let parsed = Url::parse(authorization_url).map_err(|e| {
            OAuthRegistryError::UnparseableAuthorizationUrl {
                provider: provider_str.clone(),
                detail: e.to_string(),
            }
        })?;
        if parsed.scheme() != "https" {
            return Err(OAuthRegistryError::InsecureAuthorizationUrl {
                provider: provider_str,
                url: cfg.authorization_url.redacted(),
            });
        }

        if cfg.redirect_uri_allowlist.is_empty() {
            return Err(OAuthRegistryError::EmptyRedirectAllowlist {
                provider: provider_str,
            });
        }
        for entry in &cfg.redirect_uri_allowlist {
            Url::parse(entry).map_err(|e| OAuthRegistryError::UnparseableRedirectUri {
                provider: provider_str.clone(),
                detail: format!("{entry}: {e}"),
            })?;
        }
    }
    Ok(())
}

// ============================================================================
// Error types
// ============================================================================

/// Typed errors produced by the credential service during OAuth exchange and
/// related operations. Wrapped in `anyhow::Result` at the trait boundary.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The provider returned an RFC 6749 §5.2 error response (e.g. `invalid_grant`).
    #[error("OAuth token exchange rejected by provider: {error}{}",
        .description.as_ref().map(|d| format!(" — {d}")).unwrap_or_default())]
    OAuthExchangeFailed {
        error: String,
        description: Option<String>,
    },
    /// The provider's `token_url` is not HTTPS (and not `http://localhost`).
    /// The URL prints redacted.
    #[error("OAuth token_url must use HTTPS (or http://localhost for dev): {0}")]
    InsecureTokenUrl(SensitiveUrl),
    /// The provider's `token_url` cannot be parsed. Carries the parser's
    /// message, which does not repeat the URL.
    #[error("OAuth token_url is unparseable: {0}")]
    UnparseableTokenUrl(String),
    /// No `OAuthProviderConfig` was registered for the provider.
    #[error("No OAuth provider configuration registered for: {0}")]
    ProviderNotConfigured(String),
    /// Transport-level failure reaching the token endpoint.
    #[error("OAuth token endpoint transport error: {0}")]
    HttpError(String),
    /// Provider returned a malformed or unparseable token response.
    #[error("OAuth token response was malformed: {0}")]
    InvalidResponse(String),
    /// Caller-supplied `redirect_uri` was not in the per-provider allowlist
    /// (security audit 002 §4.18 — open-redirect prevention).
    #[error("OAuth redirect_uri not in provider allowlist")]
    RedirectUriNotAllowlisted,
    /// The IMAP or the SMTP session of a mailbox check did not complete;
    /// `reply` is the server's answer, the password redacted (ADR-125 D1).
    #[error("mailbox_unreachable: the {protocol} server answered: {reply}")]
    MailboxUnreachable { protocol: String, reply: String },
    /// The mailbox check's guard refused a host or port before any
    /// connection: only the mail ports and public unicast addresses are
    /// reached (ADR-125 D1, its server-side request forgery rule). `field`
    /// is the setting refused; `reason` the sentence naming it.
    #[error("mailbox_host_not_allowed: {reason}")]
    MailboxHostNotAllowed { field: String, reason: String },
    /// Mailbox settings no session could use.
    #[error("invalid mailbox settings: {0}")]
    InvalidMailboxSettings(String),
    /// The binding is not `Active` (expired, revoked or pending), so no
    /// token is handed out for it.
    #[error("credential binding {binding_id} is not active (status: {status})")]
    BindingNotActive { binding_id: String, status: String },
    /// The binding holds no OAuth access token to hand out or refresh.
    #[error("credential binding {binding_id} holds no OAuth access token")]
    NoAccessToken { binding_id: String },
}

/// Enforce HTTPS on the token URL per RFC 6749 §3.1.2.1, with a development
/// exemption for `http://localhost` / `http://127.0.0.1`.
fn ensure_secure_token_url(token_url: &str) -> Result<(), CredentialError> {
    let parsed =
        Url::parse(token_url).map_err(|e| CredentialError::UnparseableTokenUrl(e.to_string()))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            if host == "localhost" || host == "127.0.0.1" || host == "::1" {
                Ok(())
            } else {
                Err(CredentialError::InsecureTokenUrl(SensitiveUrl::new(
                    token_url,
                )))
            }
        }
        _ => Err(CredentialError::InsecureTokenUrl(SensitiveUrl::new(
            token_url,
        ))),
    }
}

// ============================================================================
// Wire-format types for the token endpoint (RFC 6749 §5.1 / §5.2)
// ============================================================================

/// RFC 6749 §5.1 successful token response. The tokens are held in
/// `SensitiveString`, so the derived `Debug` prints them redacted.
#[derive(Debug, serde::Deserialize)]
struct OAuthTokenResponse {
    access_token: SensitiveString,
    #[allow(dead_code)]
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<SensitiveString>,
    scope: Option<String>,
    /// The OpenID Connect ID token, present when `openid` was requested.
    id_token: Option<SensitiveString>,
}

/// RFC 6749 §5.2 error response.
#[derive(Debug, serde::Deserialize)]
struct OAuthErrorResponse {
    error: String,
    error_description: Option<String>,
}

// ============================================================================
// Command types
// ============================================================================

/// Command object for [`CredentialManagementService::store_api_key`].
///
/// Bundles all parameters to keep the method signature within clippy's
/// `too_many_arguments` limit (max 7).
#[derive(Debug)]
pub struct StoreApiKeyCommand {
    pub owner_user_id: String,
    pub tenant_id: TenantId,
    pub provider: CredentialProvider,
    pub label: String,
    pub scope: CredentialScope,
    pub api_key_value: SensitiveString,
    pub credential_type: CredentialType,
}

/// Command object for [`CredentialManagementService::create_imap_mailbox`]
/// (AEGIS ADR-125 D1).
#[derive(Debug)]
pub struct CreateImapMailboxCommand {
    pub owner_user_id: String,
    pub tenant_id: TenantId,
    /// Display label; the address when absent.
    pub label: Option<String>,
    pub scope: CredentialScope,
    /// The non-secret settings, stored as the binding's metadata.
    pub settings: MailboxSettings,
    /// The password both servers accept; stored only in OpenBao, under the
    /// field `password`.
    pub password: SensitiveString,
}

// ============================================================================
// Return type for OAuth initiation
// ============================================================================

/// Return value of [`CredentialManagementService::initiate_oauth_connection`].
#[derive(Debug)]
pub struct OAuthInitiation {
    /// The provider's authorization URL the client must redirect to.
    /// Prints redacted; serialises as the bare string.
    pub authorization_url: SensitiveUrl,
    /// The opaque CSRF/state token — the client MUST pass this back at callback.
    pub state: String,
}

// ============================================================================
// Who may reach a binding by id
// ============================================================================

/// The caller of a by-id credential operation, as the presentation layer
/// derived it from the authenticated identity.
///
/// A binding is reachable by (a) its owner; (b) where its scope is
/// `team:<uuid>`, an active member of that team to read it, and an active
/// member with a membership-managing role (owner or admin) to rotate it,
/// change its grants or revoke it; (c) an operator — every role reads, only
/// `aegis:admin` and `aegis:operator` write (ADR-073 §3e). Every other
/// caller gets exactly the answer for a binding that does not exist, so an
/// id cannot be probed. Security audit 003, finding F-1; ADR-078.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialActor {
    /// A consumer or tenant-realm user acting as themselves.
    User {
        /// Keycloak `sub` of the caller.
        user_id: String,
        /// The caller's own tenant, as derived from its identity.
        tenant_id: TenantId,
    },
    /// A platform operator. `may_write` is false for `aegis:readonly`.
    Operator { may_write: bool },
}

/// What a by-id operation does to a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingAccess {
    /// Read the binding's metadata or its grants.
    Read,
    /// Rotate the secret, change the grants, or revoke the binding.
    Manage,
}

// ============================================================================
// Service Trait
// ============================================================================

/// Primary interface for managing user-owned third-party credential bindings
/// (BC-11 Secrets & Identity Management, ADR-078).
///
/// All methods are async and return `anyhow::Result` so that database,
/// validation, and secret-store errors propagate cleanly to callers.
///
/// # See Also
///
/// `StandardCredentialManagementService` — the production implementation.
#[async_trait]
pub trait CredentialManagementService: Send + Sync {
    /// Store an API key in OpenBao and create a new [`UserCredentialBinding`].
    ///
    /// Returns the [`CredentialBindingId`] of the newly created binding.
    async fn store_api_key(&self, cmd: StoreApiKeyCommand) -> anyhow::Result<CredentialBindingId>;

    /// Create an `imap` mailbox binding (AEGIS ADR-125 D1) after an IMAP
    /// session (LOGIN, SELECT INBOX, LOGOUT) and an SMTP session (EHLO,
    /// AUTH, QUIT) both succeed with the supplied settings and password.
    /// Either refusal is [`CredentialError::MailboxUnreachable`] and nothing
    /// is stored. The password is written only to OpenBao, under `password`.
    async fn create_imap_mailbox(
        &self,
        cmd: CreateImapMailboxCommand,
    ) -> anyhow::Result<UserCredentialBinding>;

    /// The binding's OAuth access token (ADR-125 D3): the stored one when
    /// its `expires_at` is more than 60 seconds away, otherwise a fresh one
    /// from the provider's token endpoint, written back to the same OpenBao
    /// path. A refresh answered `invalid_grant` (or impossible for want of a
    /// refresh token) sets the binding `Expired` and publishes
    /// [`CredentialEvent::CredentialExpired`].
    async fn access_token_for(
        &self,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<SensitiveString>;

    /// Begin an OAuth2 PKCE authorisation flow for `provider`.
    ///
    /// Creates a pending binding row, stores the PKCE verifier, and returns the
    /// constructed authorization URL + opaque state token.
    async fn initiate_oauth_connection(
        &self,
        owner_user_id: &str,
        tenant_id: &TenantId,
        provider: CredentialProvider,
        redirect_uri: String,
    ) -> anyhow::Result<OAuthInitiation>;

    /// Complete an OAuth2 PKCE flow using the `code` and `state` returned by the
    /// provider's callback.
    ///
    /// Looks up the pending state, simulates token exchange, stores the token in
    /// OpenBao, and transitions the binding to `Active`.
    async fn complete_oauth_connection(
        &self,
        state: &str,
        code: &str,
    ) -> anyhow::Result<CredentialBindingId>;

    /// Rotate the underlying secret value in OpenBao for an existing binding.
    ///
    /// The [`CredentialBindingId`] is stable; only the stored secret value changes.
    async fn rotate_credential(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        new_value: SensitiveString,
    ) -> anyhow::Result<()>;

    /// Grant `target` access to use the credential.
    ///
    /// Returns the new [`CredentialGrantId`].
    async fn add_grant(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        target: GrantTarget,
        granted_by: String,
    ) -> anyhow::Result<CredentialGrantId>;

    /// Revoke a single grant by id.
    async fn revoke_grant(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        grant_id: &CredentialGrantId,
    ) -> anyhow::Result<()>;

    /// Revoke the entire binding: clears all grants, deletes the secret from
    /// OpenBao, and marks the binding `Revoked`.
    async fn revoke_binding(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<()>;

    /// List all bindings owned by `owner_user_id` within `tenant_id`.
    async fn list_bindings(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>>;

    /// Load a single binding by id, or `None` if it does not exist or
    /// `actor` may not read it (see [`CredentialActor`]).
    async fn get_binding(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>>;
}

// ============================================================================
// Helper — build the OpenBao secret path for a user credential
// ============================================================================

fn user_credential_path(
    tenant_id: &TenantId,
    owner_user_id: &str,
    binding_id: &CredentialBindingId,
) -> SecretPath {
    SecretPath::for_tenant(
        tenant_id.clone(),
        "kv",
        format!(
            "users/{}/{}/credentials/{}",
            tenant_id.as_str(),
            owner_user_id,
            binding_id.0
        ),
    )
}

// ============================================================================
// Concrete Service Implementation
// ============================================================================

/// Production implementation of [`CredentialManagementService`].
///
/// Wires together:
/// - [`CredentialBindingRepository`] — Postgres persistence
/// - [`SecretsManager`] — OpenBao read/write
/// - [`EventBus`] — domain event publication
pub struct StandardCredentialManagementService {
    repo: Arc<dyn CredentialBindingRepository>,
    secrets: Arc<SecretsManager>,
    event_bus: Arc<EventBus>,
    http: reqwest::Client,
    oauth_providers: Arc<OAuthProviderRegistry>,
    /// Answers "is this caller an active member (or manager) of the team a
    /// `team:<uuid>` binding is scoped to". `None` denies every team-scope
    /// reach, leaving owner and operator access.
    membership_repo: Option<Arc<dyn MembershipRepository>>,
    /// The live IMAP and SMTP check run before an `imap` mailbox is stored.
    mailbox_probe: Arc<dyn MailboxProbe>,
}

impl StandardCredentialManagementService {
    /// Production constructor.
    ///
    /// Builds a default `reqwest::Client` and accepts the `OAuthProviderRegistry`
    /// loaded from platform configuration.
    pub fn new(
        repo: Arc<dyn CredentialBindingRepository>,
        secrets: Arc<SecretsManager>,
        event_bus: Arc<EventBus>,
        oauth_providers: Arc<OAuthProviderRegistry>,
    ) -> Self {
        Self {
            repo,
            secrets,
            event_bus,
            // Audit 002 §4.37.9 — explicit timeout. A naked
            // `reqwest::Client::new()` inherits no implicit total/connect
            // timeout, so a frozen OAuth provider hangs the credential
            // service indefinitely.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("default reqwest client must build"),
            oauth_providers,
            membership_repo: None,
            mailbox_probe: Arc::new(SessionMailboxProbe::tls()),
        }
    }

    /// Test / advanced constructor that takes an explicit `reqwest::Client`.
    ///
    /// Used to point the service at a mockito server for integration tests.
    pub fn with_http_client(
        repo: Arc<dyn CredentialBindingRepository>,
        secrets: Arc<SecretsManager>,
        event_bus: Arc<EventBus>,
        oauth_providers: Arc<OAuthProviderRegistry>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            repo,
            secrets,
            event_bus,
            http,
            oauth_providers,
            membership_repo: None,
            mailbox_probe: Arc::new(SessionMailboxProbe::tls()),
        }
    }

    /// Replace the mailbox check (tests point it at loopback stand-ins).
    pub fn with_mailbox_probe(mut self, probe: Arc<dyn MailboxProbe>) -> Self {
        self.mailbox_probe = probe;
        self
    }

    /// Wire the team-membership repository that team-scoped bindings are
    /// authorised against (ADR-111 memberships).
    pub fn with_membership_repo(mut self, repo: Arc<dyn MembershipRepository>) -> Self {
        self.membership_repo = Some(repo);
        self
    }

    /// Whether `actor` may perform `access` on `binding`.
    async fn may_access(
        &self,
        actor: &CredentialActor,
        binding: &UserCredentialBinding,
        access: BindingAccess,
    ) -> anyhow::Result<bool> {
        let (user_id, tenant_id) = match actor {
            CredentialActor::Operator { may_write } => {
                return Ok(access == BindingAccess::Read || *may_write)
            }
            CredentialActor::User { user_id, tenant_id } => (user_id, tenant_id),
        };
        if &binding.owner_user_id == user_id && &binding.tenant_id == tenant_id {
            return Ok(true);
        }
        let CredentialScope::Team { team_id } = binding.scope else {
            return Ok(false);
        };
        let Some(memberships) = self.membership_repo.as_ref() else {
            return Ok(false);
        };
        let member = memberships
            .find_by_team(&TeamId(team_id))
            .await
            .map_err(|e| anyhow!("team membership lookup failed: {e}"))?
            .into_iter()
            .find(|m| &m.user_id == user_id && m.status == MembershipStatus::Active);
        Ok(match (member, access) {
            (Some(_), BindingAccess::Read) => true,
            (Some(m), BindingAccess::Manage) => m.role.can_manage_membership(),
            (None, _) => false,
        })
    }

    /// Load `binding_id` for `actor`. A binding that does not exist and one
    /// the actor may not reach produce the same error, word for word.
    async fn load_for(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        access: BindingAccess,
    ) -> anyhow::Result<UserCredentialBinding> {
        let not_found = || anyhow!("Credential binding not found: {}", binding_id);
        let binding = self
            .repo
            .find_by_id(binding_id)
            .await?
            .ok_or_else(not_found)?;
        if self.may_access(actor, &binding, access).await? {
            Ok(binding)
        } else {
            Err(not_found())
        }
    }

    /// RFC 6749 §4.1.3 + RFC 7636 authorization-code-for-token exchange.
    ///
    /// Posts to `provider.token_url` and returns the parsed token response.
    /// Never logs the `code`, `code_verifier`, `client_secret`, or any returned
    /// token material.
    async fn exchange_authorization_code(
        &self,
        provider: &CredentialProvider,
        code: &str,
        pending: &OAuthPendingState,
    ) -> Result<OAuthTokenResponse, CredentialError> {
        self.post_token_request(
            provider,
            "authorization-code exchange",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &pending.redirect_uri),
                ("code_verifier", &pending.pkce_verifier),
            ],
        )
        .await
    }

    /// RFC 6749 §6 refresh-token grant (ADR-125 D3).
    async fn refresh_access_token(
        &self,
        provider: &CredentialProvider,
        refresh_token: &SensitiveString,
    ) -> Result<OAuthTokenResponse, CredentialError> {
        self.post_token_request(
            provider,
            "refresh-token grant",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.expose()),
            ],
        )
        .await
    }

    /// Post `grant` to the provider's token endpoint with the client's
    /// credentials, as `application/x-www-form-urlencoded`. `client_secret`
    /// is included only for confidential clients. Logs the provider, the
    /// redacted URL and the outcome; never a code, verifier, secret or token.
    async fn post_token_request(
        &self,
        provider: &CredentialProvider,
        purpose: &'static str,
        grant: &[(&str, &str)],
    ) -> Result<OAuthTokenResponse, CredentialError> {
        let cfg = self
            .oauth_providers
            .get(provider)
            .ok_or_else(|| CredentialError::ProviderNotConfigured(provider.to_string()))?;

        // Read to check its scheme before any request is sent.
        ensure_secure_token_url(cfg.token_url.expose())?;

        let mut form: Vec<(&str, &str)> = grant.to_vec();
        form.push(("client_id", &cfg.client_id));
        let secret_holder;
        if let Some(s) = &cfg.client_secret {
            secret_holder = s.expose().to_string();
            form.push(("client_secret", &secret_holder));
        }

        tracing::info!(
            provider = %provider,
            token_url = %cfg.token_url.redacted(),
            "Posting OAuth {purpose} to provider token endpoint"
        );

        let resp = self
            .http
            // Read to send the request to the token endpoint.
            .post(cfg.token_url.expose())
            .header("Accept", "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|e| CredentialError::HttpError(e.to_string()))?;

        let status = resp.status();

        if status.is_success() {
            let body = resp
                .text()
                .await
                .map_err(|e| CredentialError::HttpError(e.to_string()))?;
            let token: OAuthTokenResponse = serde_json::from_str(&body).map_err(|e| {
                // Do NOT include the raw body in the error — it contains the token.
                CredentialError::InvalidResponse(format!("deserialisation failed: {e}"))
            })?;
            tracing::info!(
                provider = %provider,
                scope = ?token.scope,
                expires_in = ?token.expires_in,
                has_refresh_token = token.refresh_token.is_some(),
                "OAuth {purpose} succeeded"
            );
            Ok(token)
        } else if status.is_client_error() {
            // RFC 6749 §5.2: expect a JSON error object with `error` + optional
            // `error_description`. Fall back to a generic variant if the
            // provider returns something non-conforming.
            let body = resp.text().await.unwrap_or_default();
            match serde_json::from_str::<OAuthErrorResponse>(&body) {
                Ok(err) => {
                    tracing::warn!(
                        provider = %provider,
                        error = %err.error,
                        "Provider rejected OAuth {purpose}"
                    );
                    Err(CredentialError::OAuthExchangeFailed {
                        error: err.error,
                        description: err.error_description,
                    })
                }
                Err(_) => Err(CredentialError::OAuthExchangeFailed {
                    error: format!("http_{}", status.as_u16()),
                    description: None,
                }),
            }
        } else {
            Err(CredentialError::HttpError(format!(
                "unexpected status {} from token endpoint",
                status
            )))
        }
    }

    /// Mark `binding` `Expired` and publish [`CredentialEvent::CredentialExpired`].
    async fn expire(&self, mut binding: UserCredentialBinding) -> anyhow::Result<()> {
        binding.status = CredentialStatus::Expired;
        binding.updated_at = Utc::now();
        self.repo.save(&binding).await?;
        tracing::warn!(
            binding_id = %binding.id,
            provider = %binding.provider,
            "OAuth token can no longer be refreshed; binding set Expired"
        );
        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialExpired {
                binding_id: binding.id,
                tenant_id: binding.tenant_id,
            });
        Ok(())
    }
}

/// The email address in an OpenID Connect ID token's claims. The token came
/// straight from the token endpoint over TLS, which OpenID Connect Core
/// §3.1.3.7 accepts in place of validating its signature; an address the
/// provider marks unverified is refused.
fn email_from_id_token(id_token: &SensitiveString) -> Result<String, CredentialError> {
    let claims = id_token
        .expose()
        .split('.')
        .nth(1)
        .ok_or_else(|| CredentialError::InvalidResponse("id_token is not a JWT".to_string()))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(claims.trim_end_matches('='))
        .map_err(|e| CredentialError::InvalidResponse(format!("id_token claims: {e}")))?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| CredentialError::InvalidResponse(format!("id_token claims: {e}")))?;
    if claims.get("email_verified") == Some(&serde_json::Value::Bool(false)) {
        return Err(CredentialError::InvalidResponse(
            "the id_token's email is not verified".to_string(),
        ));
    }
    claims
        .get("email")
        .and_then(|e| e.as_str())
        .filter(|e| e.contains('@'))
        .map(str::to_string)
        .ok_or_else(|| {
            CredentialError::InvalidResponse("the id_token carries no email claim".to_string())
        })
}

#[async_trait]
impl CredentialManagementService for StandardCredentialManagementService {
    // -----------------------------------------------------------------------
    // store_api_key
    // -----------------------------------------------------------------------

    async fn store_api_key(&self, cmd: StoreApiKeyCommand) -> anyhow::Result<CredentialBindingId> {
        let StoreApiKeyCommand {
            owner_user_id,
            tenant_id,
            provider,
            label,
            scope,
            api_key_value,
            credential_type,
        } = cmd;
        let binding_id = CredentialBindingId::new();
        let secret_path = user_credential_path(&tenant_id, &owner_user_id, &binding_id);

        // Write the raw API key to OpenBao under the binding's path.
        let mut secret_data = HashMap::new();
        secret_data.insert("value".to_string(), api_key_value);
        self.secrets
            .write_secret(
                &secret_path.effective_mount(),
                &secret_path.path,
                secret_data,
                &AccessContext::system("aegis-credential-service"),
            )
            .await?;

        let now = Utc::now();
        let binding = UserCredentialBinding {
            id: binding_id,
            owner_user_id: owner_user_id.to_string(),
            tenant_id: tenant_id.clone(),
            credential_type: credential_type.clone(),
            provider: provider.clone(),
            secret_path,
            scope,
            status: CredentialStatus::Active,
            metadata: CredentialMetadata {
                label,
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox: None,
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        };

        self.repo.save(&binding).await?;

        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialCreated {
                binding_id,
                owner_user_id: owner_user_id.to_string(),
                tenant_id: tenant_id.clone(),
                provider,
                credential_type,
            });

        Ok(binding_id)
    }

    // -----------------------------------------------------------------------
    // create_imap_mailbox (ADR-125 D1)
    // -----------------------------------------------------------------------

    async fn create_imap_mailbox(
        &self,
        cmd: CreateImapMailboxCommand,
    ) -> anyhow::Result<UserCredentialBinding> {
        let CreateImapMailboxCommand {
            owner_user_id,
            tenant_id,
            label,
            scope,
            settings,
            password,
        } = cmd;
        settings
            .validate()
            .map_err(CredentialError::InvalidMailboxSettings)?;
        if password.is_empty() {
            return Err(CredentialError::InvalidMailboxSettings(
                "password must not be empty".into(),
            )
            .into());
        }

        // The live check, before anything is stored.
        if let Err(failure) = self.mailbox_probe.check(&settings, &password).await {
            if let CheckFailureKind::HostNotAllowed { field } = failure.kind {
                tracing::warn!(
                    address = %settings.address,
                    field,
                    reason = %failure.reply,
                    "Mailbox check refused an endpoint outside the rule; no connection made"
                );
                return Err(CredentialError::MailboxHostNotAllowed {
                    field: field.to_string(),
                    reason: failure.reply,
                }
                .into());
            }
            tracing::info!(
                address = %settings.address,
                protocol = %failure.protocol,
                reply = %failure.reply,
                "Mailbox check failed; no binding stored"
            );
            return Err(CredentialError::MailboxUnreachable {
                protocol: failure.protocol.to_string(),
                reply: failure.reply,
            }
            .into());
        }

        let binding_id = CredentialBindingId::new();
        let secret_path = user_credential_path(&tenant_id, &owner_user_id, &binding_id);
        let mut secret_data = HashMap::new();
        secret_data.insert("password".to_string(), password);
        self.secrets
            .write_secret(
                &secret_path.effective_mount(),
                &secret_path.path,
                secret_data,
                &AccessContext::system("aegis-credential-service"),
            )
            .await?;

        let now = Utc::now();
        let binding = UserCredentialBinding {
            id: binding_id,
            owner_user_id: owner_user_id.clone(),
            tenant_id: tenant_id.clone(),
            credential_type: CredentialType::Mailbox,
            provider: CredentialProvider::imap(),
            secret_path,
            scope,
            status: CredentialStatus::Active,
            metadata: CredentialMetadata {
                label: label.unwrap_or_else(|| settings.address.clone()),
                tags: None,
                service_url: None,
                external_account_id: Some(settings.address.clone()),
                oauth_scopes: None,
                mailbox: Some(settings),
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        self.repo.save(&binding).await?;

        tracing::info!(
            binding_id = %binding_id,
            address = ?binding.metadata.external_account_id,
            "Mailbox checks passed; imap binding stored"
        );
        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialCreated {
                binding_id,
                owner_user_id,
                tenant_id,
                provider: CredentialProvider::imap(),
                credential_type: CredentialType::Mailbox,
            });

        Ok(binding)
    }

    // -----------------------------------------------------------------------
    // access_token_for (ADR-125 D3)
    // -----------------------------------------------------------------------

    async fn access_token_for(
        &self,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<SensitiveString> {
        let binding = self
            .repo
            .find_by_id(binding_id)
            .await?
            .ok_or_else(|| anyhow!("Credential binding not found: {}", binding_id))?;
        if binding.status != CredentialStatus::Active {
            return Err(CredentialError::BindingNotActive {
                binding_id: binding_id.to_string(),
                status: format!("{:?}", binding.status).to_lowercase(),
            }
            .into());
        }

        let ctx = AccessContext::system("aegis-credential-service");
        let mount = binding.secret_path.effective_mount();
        let mut stored = self
            .secrets
            .read_secret(&mount, &binding.secret_path.path, &ctx)
            .await?;
        let access_token =
            stored
                .get("access_token")
                .cloned()
                .ok_or_else(|| CredentialError::NoAccessToken {
                    binding_id: binding_id.to_string(),
                })?;

        // A token stored without `expires_at` was issued without
        // `expires_in` and is used as it is.
        let fresh = match stored.get("expires_at") {
            None => true,
            Some(at) => chrono::DateTime::parse_from_rfc3339(at.expose())
                .map(|at| {
                    at.with_timezone(&Utc)
                        > Utc::now() + chrono::Duration::seconds(ACCESS_TOKEN_REFRESH_MARGIN_SECS)
                })
                .unwrap_or(false),
        };
        if fresh {
            return Ok(access_token);
        }

        let Some(refresh_token) = stored.get("refresh_token").cloned() else {
            // Nothing to refresh with: the user must reconnect.
            self.expire(binding).await?;
            return Err(CredentialError::OAuthExchangeFailed {
                error: "invalid_grant".to_string(),
                description: Some("no refresh token is held for this binding".to_string()),
            }
            .into());
        };

        let refreshed = match self
            .refresh_access_token(&binding.provider, &refresh_token)
            .await
        {
            Ok(token) => token,
            Err(CredentialError::OAuthExchangeFailed { error, description })
                if error == "invalid_grant" =>
            {
                self.expire(binding).await?;
                return Err(CredentialError::OAuthExchangeFailed { error, description }.into());
            }
            Err(other) => return Err(other.into()),
        };

        stored.insert("access_token".to_string(), refreshed.access_token.clone());
        match refreshed.expires_in {
            Some(expires_in) => {
                let expires_at = Utc::now() + chrono::Duration::seconds(expires_in as i64);
                stored.insert(
                    "expires_at".to_string(),
                    SensitiveString::new(expires_at.to_rfc3339()),
                );
            }
            None => {
                stored.remove("expires_at");
            }
        }
        if let Some(rotated) = refreshed.refresh_token {
            stored.insert("refresh_token".to_string(), rotated);
        }
        if let Some(scope) = refreshed.scope {
            stored.insert("scope".to_string(), SensitiveString::new(scope));
        }
        self.secrets
            .write_secret(&mount, &binding.secret_path.path, stored, &ctx)
            .await?;
        tracing::info!(
            binding_id = %binding_id,
            provider = %binding.provider,
            "OAuth access token refreshed and stored"
        );
        Ok(refreshed.access_token)
    }

    // -----------------------------------------------------------------------
    // initiate_oauth_connection
    // -----------------------------------------------------------------------

    async fn initiate_oauth_connection(
        &self,
        owner_user_id: &str,
        tenant_id: &TenantId,
        provider: CredentialProvider,
        redirect_uri: String,
    ) -> anyhow::Result<OAuthInitiation> {
        // Resolve the provider's configuration up front. Refuse to start the
        // flow if the provider is unconfigured — this is the fix for
        // security audit 002 §4.11 (no more `oauth.placeholder` host).
        let cfg = self
            .oauth_providers
            .get(&provider)
            .ok_or_else(|| CredentialError::ProviderNotConfigured(provider.to_string()))?;

        // §4.18: the caller-supplied `redirect_uri` MUST be an exact match
        // against one of the provider's allowlisted entries. Anything else
        // is a potential open-redirect / token-leak vector.
        if !cfg
            .redirect_uri_allowlist
            .iter()
            .any(|allowed| allowed == &redirect_uri)
        {
            return Err(CredentialError::RedirectUriNotAllowlisted.into());
        }

        // Generate a cryptographically random state token using two UUIDs concatenated.
        let state = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );

        // PKCE: generate a random 128-char verifier (only URL-safe chars are needed).
        let code_verifier = format!(
            "{}{}{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple(),
        );

        // Compute S256 code challenge: BASE64URL(SHA256(code_verifier))
        let digest = Sha256::digest(code_verifier.as_bytes());
        let code_challenge = URL_SAFE_NO_PAD.encode(digest);

        let binding_id = CredentialBindingId::new();
        let now = Utc::now();
        // Every OAuth binding is of type `oauth2`, whatever its provider's
        // name (ADR-125, Update of 2026-10-04, clause 1).
        let credential_type = CredentialType::OAuth2;

        let binding = UserCredentialBinding {
            id: binding_id,
            owner_user_id: owner_user_id.to_string(),
            tenant_id: tenant_id.clone(),
            credential_type,
            provider: provider.clone(),
            // Placeholder path — updated to real path once the flow completes.
            secret_path: SecretPath::new("PENDING_OAUTH", "PENDING_OAUTH", "PENDING_OAUTH"),
            scope: CredentialScope::Personal,
            status: CredentialStatus::PendingOAuth,
            metadata: CredentialMetadata {
                label: format!("{} OAuth connection", provider),
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox: None,
            },
            grants: Vec::new(),
            created_at: now,
            updated_at: now,
        };

        self.repo.save(&binding).await?;
        self.repo
            .save_oauth_state(&state, &binding_id, &code_verifier, &redirect_uri)
            .await?;

        // Build the authorization URL by parsing the validated provider
        // base and appending properly-encoded query parameters via
        // `Url::query_pairs_mut` — never via `format!` (security audit 002
        // §4.11: caller-supplied `redirect_uri` must be percent-encoded).
        // Read to build the redirect the client follows.
        let mut auth_url = Url::parse(cfg.authorization_url.expose()).map_err(|e| {
            anyhow!(
                "configured authorization_url for {} is unparseable: {}",
                provider,
                e
            )
        })?;
        auth_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &cfg.client_id)
            .append_pair("state", &state)
            .append_pair("code_challenge", &code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("redirect_uri", &redirect_uri);
        // ADR-125 D2: the configured scopes, space-separated, and every
        // extra parameter (e.g. Google's `access_type` and `prompt`).
        {
            let mut query = auth_url.query_pairs_mut();
            if !cfg.scopes.is_empty() {
                query.append_pair("scope", &cfg.scopes.join(" "));
            }
            for (key, value) in &cfg.extra_authorization_params {
                query.append_pair(key, value);
            }
        }

        Ok(OAuthInitiation {
            authorization_url: SensitiveUrl::new(auth_url.to_string()),
            state,
        })
    }

    // -----------------------------------------------------------------------
    // complete_oauth_connection
    // -----------------------------------------------------------------------

    async fn complete_oauth_connection(
        &self,
        state: &str,
        code: &str,
    ) -> anyhow::Result<CredentialBindingId> {
        let pending: OAuthPendingState = self
            .repo
            .find_oauth_state(state)
            .await?
            .ok_or_else(|| anyhow!("OAuth state invalid or expired"))?;

        // Reject states older than 10 minutes.
        let age = Utc::now().signed_duration_since(pending.created_at);
        if age.num_minutes() > 10 {
            self.repo.delete_oauth_state(state).await?;
            return Err(anyhow!("OAuth state invalid or expired"));
        }

        let mut binding = self
            .repo
            .find_by_id(&pending.binding_id)
            .await?
            .ok_or_else(|| anyhow!("Credential binding not found for pending OAuth state"))?;

        // §4.18 (defence in depth): re-validate the persisted redirect_uri
        // against the current allowlist. If the operator has tightened the
        // allowlist since the flow started, reject the callback rather than
        // proceeding with a now-disallowed URI.
        if let Some(cfg) = self.oauth_providers.get(&binding.provider) {
            if !cfg
                .redirect_uri_allowlist
                .iter()
                .any(|allowed| allowed == &pending.redirect_uri)
            {
                self.repo.delete_oauth_state(state).await?;
                return Err(CredentialError::RedirectUriNotAllowlisted.into());
            }
        }

        // RFC 6749 §4.1.3 + RFC 7636: exchange the authorization code + PKCE
        // verifier for an access token at the provider's token endpoint. A
        // single attempt only — authorization codes are single-use, so retries
        // are unsafe.
        let token_response = self
            .exchange_authorization_code(&binding.provider, code, &pending)
            .await?;

        let secret_path =
            user_credential_path(&binding.tenant_id, &binding.owner_user_id, &binding.id);

        // A `mailbox` binding in its OAuth form, pending since before ADR-125's
        // Update of 2026-10-04, records its address and the scopes granted
        // (ADR-125 D1); without the address it cannot be used, so the
        // callback fails before anything is stored. Any other OAuth binding
        // records the scopes granted (its scopes are its registry entry's,
        // the Update's clause 1) and, where the token response carries an
        // id_token naming an email, that account; no provider is named.
        if binding.credential_type != CredentialType::Mailbox {
            binding.metadata.oauth_scopes = Some(match &token_response.scope {
                Some(scope) => scope.split_whitespace().map(str::to_string).collect(),
                None => self
                    .oauth_providers
                    .get(&binding.provider)
                    .map(|c| c.scopes.clone())
                    .unwrap_or_default(),
            });
            if let Some(address) = token_response
                .id_token
                .as_ref()
                .and_then(|t| email_from_id_token(t).ok())
            {
                binding.metadata.external_account_id = Some(address);
            }
        }
        if binding.credential_type == CredentialType::Mailbox {
            let id_token = token_response.id_token.as_ref().ok_or_else(|| {
                CredentialError::InvalidResponse(
                    "the token response carries no id_token; request the openid and email scopes"
                        .to_string(),
                )
            })?;
            let address = email_from_id_token(id_token)?;
            // RFC 6749 §5.1: an absent `scope` means the scopes requested.
            let granted: Vec<String> = match &token_response.scope {
                Some(scope) => scope.split_whitespace().map(str::to_string).collect(),
                None => self
                    .oauth_providers
                    .get(&binding.provider)
                    .map(|c| c.scopes.clone())
                    .unwrap_or_default(),
            };
            binding.metadata.label = address.clone();
            binding.metadata.external_account_id = Some(address);
            binding.metadata.oauth_scopes = Some(granted);
        }

        // Persist the tokens returned by the provider. Compute an absolute
        // `expires_at` so the refresh path doesn't need clock math on read.
        let mut secret_data = HashMap::new();
        secret_data.insert("access_token".to_string(), token_response.access_token);
        if let Some(refresh_token) = token_response.refresh_token {
            secret_data.insert("refresh_token".to_string(), refresh_token);
        }
        if let Some(expires_in) = token_response.expires_in {
            let expires_at = Utc::now() + chrono::Duration::seconds(expires_in as i64);
            secret_data.insert(
                "expires_at".to_string(),
                SensitiveString::new(expires_at.to_rfc3339()),
            );
        }
        if let Some(scope) = token_response.scope {
            secret_data.insert("scope".to_string(), SensitiveString::new(scope));
        }

        self.secrets
            .write_secret(
                &secret_path.effective_mount(),
                &secret_path.path,
                secret_data,
                &AccessContext::system("aegis-credential-service"),
            )
            .await?;

        binding.secret_path = secret_path;
        binding.status = CredentialStatus::Active;
        binding.updated_at = Utc::now();

        self.repo.save(&binding).await?;
        self.repo.delete_oauth_state(state).await?;

        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialCreated {
                binding_id: binding.id,
                owner_user_id: binding.owner_user_id.clone(),
                tenant_id: binding.tenant_id.clone(),
                provider: binding.provider.clone(),
                credential_type: binding.credential_type.clone(),
            });

        Ok(binding.id)
    }

    // -----------------------------------------------------------------------
    // rotate_credential
    // -----------------------------------------------------------------------

    async fn rotate_credential(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        new_value: SensitiveString,
    ) -> anyhow::Result<()> {
        let binding = self
            .load_for(actor, binding_id, BindingAccess::Manage)
            .await?;

        let mut secret_data = HashMap::new();
        secret_data.insert("value".to_string(), new_value);
        self.secrets
            .write_secret(
                &binding.secret_path.effective_mount(),
                &binding.secret_path.path,
                secret_data,
                &AccessContext::system("aegis-credential-service"),
            )
            .await?;

        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialRotated {
                binding_id: *binding_id,
                tenant_id: binding.tenant_id,
            });

        Ok(())
    }

    // -----------------------------------------------------------------------
    // add_grant
    // -----------------------------------------------------------------------

    async fn add_grant(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        target: GrantTarget,
        granted_by: String,
    ) -> anyhow::Result<CredentialGrantId> {
        let mut binding = self
            .load_for(actor, binding_id, BindingAccess::Manage)
            .await?;

        let grant_id = binding.add_grant(target.clone(), granted_by.clone());
        self.repo.save(&binding).await?;

        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialGranted {
                binding_id: *binding_id,
                grant_id,
                target,
                granted_by,
            });

        Ok(grant_id)
    }

    // -----------------------------------------------------------------------
    // revoke_grant
    // -----------------------------------------------------------------------

    async fn revoke_grant(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
        grant_id: &CredentialGrantId,
    ) -> anyhow::Result<()> {
        let mut binding = self
            .load_for(actor, binding_id, BindingAccess::Manage)
            .await?;

        if !binding.revoke_grant(grant_id) {
            return Err(anyhow!("Grant not found: {}", grant_id));
        }

        self.repo.save(&binding).await?;

        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialGrantRevoked {
                binding_id: *binding_id,
                grant_id: *grant_id,
            });

        Ok(())
    }

    // -----------------------------------------------------------------------
    // revoke_binding
    // -----------------------------------------------------------------------

    async fn revoke_binding(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<()> {
        let mut binding = self
            .load_for(actor, binding_id, BindingAccess::Manage)
            .await?;

        let tenant_id = binding.tenant_id.clone();

        binding.revoke();
        self.repo.save(&binding).await?;

        // Delete the secret from OpenBao — ignore NotFound errors (already gone).
        let _ = self
            .secrets
            .delete_secret(
                &binding.secret_path.effective_mount(),
                &binding.secret_path.path,
                &AccessContext::system("aegis-credential-service"),
            )
            .await;

        self.repo.delete(binding_id).await?;

        self.event_bus
            .publish_credential_event(CredentialEvent::CredentialRevoked {
                binding_id: *binding_id,
                tenant_id,
            });

        Ok(())
    }

    // -----------------------------------------------------------------------
    // list_bindings
    // -----------------------------------------------------------------------

    async fn list_bindings(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        self.repo.find_by_owner(tenant_id, owner_user_id).await
    }

    // -----------------------------------------------------------------------
    // get_binding
    // -----------------------------------------------------------------------

    async fn get_binding(
        &self,
        actor: &CredentialActor,
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        let Some(binding) = self.repo.find_by_id(binding_id).await? else {
            return Ok(None);
        };
        if self
            .may_access(actor, &binding, BindingAccess::Read)
            .await?
        {
            Ok(Some(binding))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn oauth_provider_config_debug_does_not_print_a_credential_in_its_urls() {
        let cfg = OAuthProviderConfig {
            authorization_url: "https://user:Mk7-authorization-url-marker@idp.example/authorize"
                .into(),
            token_url: "https://user:Mk7-oauth-token-url-marker@idp.example/token".into(),
            client_id: "client".to_string(),
            client_secret: Some(SensitiveString::new("Mk7-oauth-client-secret-marker")),
            redirect_uri_allowlist: vec!["https://app.example/cb".to_string()],
            scopes: Vec::new(),
            extra_authorization_params: BTreeMap::new(),
        };
        let printed = format!("{cfg:?}");
        for marker in [
            "Mk7-authorization-url-marker",
            "Mk7-oauth-token-url-marker",
            "Mk7-oauth-client-secret-marker",
        ] {
            assert!(
                !printed.contains(marker),
                "OAuthProviderConfig's Debug printed a credential: {printed}"
            );
        }
        assert!(
            printed.contains("idp.example"),
            "Debug lost the host: {printed}"
        );
    }

    #[test]
    fn oauth_token_response_debug_does_not_print_the_tokens() {
        let response: OAuthTokenResponse = serde_json::from_str(
            r#"{"access_token":"Mk7-oauth-access-token-marker","token_type":"bearer","expires_in":3600,"refresh_token":"Mk7-oauth-refresh-token-marker","scope":"repo"}"#,
        )
        .unwrap();
        let printed = format!("{response:?}");
        for marker in [
            "Mk7-oauth-access-token-marker",
            "Mk7-oauth-refresh-token-marker",
        ] {
            assert!(
                !printed.contains(marker),
                "OAuthTokenResponse's Debug printed a token: {printed}"
            );
        }
        assert!(printed.contains("repo"), "Debug lost the scope: {printed}");
    }

    #[test]
    fn oauth_initiation_debug_does_not_print_a_credential_in_the_url() {
        let initiation = OAuthInitiation {
            authorization_url: "https://user:Mk7-initiation-url-marker@idp.example/authorize?x=1"
                .into(),
            state: "state-1".to_string(),
        };
        let printed = format!("{initiation:?}");
        assert!(
            !printed.contains("Mk7-initiation-url-marker"),
            "OAuthInitiation's Debug printed a credential: {printed}"
        );
    }

    #[test]
    fn insecure_token_url_error_does_not_print_the_url_credential() {
        let err =
            ensure_secure_token_url("http://user:Mk7-insecure-token-url-marker@evil.example/token")
                .expect_err("an http token URL off localhost is refused");
        let printed = format!("{err} {err:?}");
        assert!(
            !printed.contains("Mk7-insecure-token-url-marker"),
            "the insecure token URL error printed the URL's credential: {printed}"
        );
        assert!(
            printed.contains("evil.example"),
            "the error lost the host it refused: {printed}"
        );
    }
}
