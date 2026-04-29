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
    OAuthPendingState, UserCredentialBinding,
};
use crate::domain::events::CredentialEvent;
use crate::domain::secrets::{AccessContext, SecretPath, SensitiveString};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::secrets_manager::SecretsManager;
use anyhow::anyhow;
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

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
    pub authorization_url: String,
    /// Provider's token endpoint URL. MUST be HTTPS (localhost exempted for dev).
    pub token_url: String,
    /// OAuth 2.0 `client_id` registered with the provider.
    pub client_id: String,
    /// OAuth 2.0 `client_secret` for confidential clients. `None` for public clients.
    pub client_secret: Option<SensitiveString>,
    /// Exact-match allowlist of `redirect_uri` values the application is
    /// permitted to use. The caller-supplied `redirect_uri` is rejected
    /// unless it appears verbatim in this list.
    pub redirect_uri_allowlist: Vec<String>,
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

        if cfg.authorization_url.is_empty() {
            return Err(OAuthRegistryError::EmptyAuthorizationUrl {
                provider: provider_str,
            });
        }
        // Hard refusal of any URL that contains the literal substring
        // "placeholder" anywhere — this catches the legacy
        // `oauth.placeholder` host and any developer copy-paste of the
        // default value.
        if cfg.authorization_url.contains("placeholder") {
            return Err(OAuthRegistryError::PlaceholderAuthorizationUrl {
                provider: provider_str,
                url: cfg.authorization_url.clone(),
            });
        }
        let parsed = Url::parse(&cfg.authorization_url).map_err(|e| {
            OAuthRegistryError::UnparseableAuthorizationUrl {
                provider: provider_str.clone(),
                detail: e.to_string(),
            }
        })?;
        if parsed.scheme() != "https" {
            return Err(OAuthRegistryError::InsecureAuthorizationUrl {
                provider: provider_str,
                url: cfg.authorization_url.clone(),
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
    #[error("OAuth token_url must use HTTPS (or http://localhost for dev): {0}")]
    InsecureTokenUrl(String),
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
}

/// Enforce HTTPS on the token URL per RFC 6749 §3.1.2.1, with a development
/// exemption for `http://localhost` / `http://127.0.0.1`.
fn ensure_secure_token_url(token_url: &str) -> Result<(), CredentialError> {
    let parsed = Url::parse(token_url)
        .map_err(|e| CredentialError::InsecureTokenUrl(format!("unparseable: {e}")))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            if host == "localhost" || host == "127.0.0.1" || host == "::1" {
                Ok(())
            } else {
                Err(CredentialError::InsecureTokenUrl(token_url.to_string()))
            }
        }
        _ => Err(CredentialError::InsecureTokenUrl(token_url.to_string())),
    }
}

// ============================================================================
// Wire-format types for the token endpoint (RFC 6749 §5.1 / §5.2)
// ============================================================================

/// RFC 6749 §5.1 successful token response.
///
/// NEVER derive `Debug` on a value populated from a live response: `Debug` is
/// derived here because the struct is only used transiently inside the
/// exchange function and never logged. Values are moved directly into
/// `SensitiveString` before being stored or returned.
#[derive(Debug, serde::Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    scope: Option<String>,
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

// ============================================================================
// Return type for OAuth initiation
// ============================================================================

/// Return value of [`CredentialManagementService::initiate_oauth_connection`].
#[derive(Debug)]
pub struct OAuthInitiation {
    /// The provider's authorization URL the client must redirect to.
    pub authorization_url: String,
    /// The opaque CSRF/state token — the client MUST pass this back at callback.
    pub state: String,
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
        binding_id: &CredentialBindingId,
        new_value: SensitiveString,
    ) -> anyhow::Result<()>;

    /// Grant `target` access to use the credential.
    ///
    /// Returns the new [`CredentialGrantId`].
    async fn add_grant(
        &self,
        binding_id: &CredentialBindingId,
        target: GrantTarget,
        granted_by: String,
    ) -> anyhow::Result<CredentialGrantId>;

    /// Revoke a single grant by id.
    async fn revoke_grant(
        &self,
        binding_id: &CredentialBindingId,
        grant_id: &CredentialGrantId,
    ) -> anyhow::Result<()>;

    /// Revoke the entire binding: clears all grants, deletes the secret from
    /// OpenBao, and marks the binding `Revoked`.
    async fn revoke_binding(&self, binding_id: &CredentialBindingId) -> anyhow::Result<()>;

    /// List all bindings owned by `owner_user_id` within `tenant_id`.
    async fn list_bindings(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>>;

    /// Load a single binding by id, or `None` if not found.
    async fn get_binding(
        &self,
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
        let cfg = self
            .oauth_providers
            .get(provider)
            .ok_or_else(|| CredentialError::ProviderNotConfigured(provider.to_string()))?;

        ensure_secure_token_url(&cfg.token_url)?;

        // Build application/x-www-form-urlencoded body per RFC 6749 §4.1.3.
        // `client_secret` is included only for confidential clients.
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &pending.redirect_uri),
            ("client_id", &cfg.client_id),
            ("code_verifier", &pending.pkce_verifier),
        ];
        let secret_holder;
        if let Some(s) = &cfg.client_secret {
            secret_holder = s.expose().to_string();
            form.push(("client_secret", &secret_holder));
        }

        tracing::info!(
            provider = %provider,
            token_url = %cfg.token_url,
            "Posting OAuth authorization-code exchange to provider token endpoint"
        );

        let resp = self
            .http
            .post(&cfg.token_url)
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
                "OAuth authorization-code exchange succeeded"
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
                        "Provider rejected OAuth authorization-code exchange"
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

        let binding = UserCredentialBinding {
            id: binding_id,
            owner_user_id: owner_user_id.to_string(),
            tenant_id: tenant_id.clone(),
            credential_type: CredentialType::OAuth2,
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
        let mut auth_url = Url::parse(&cfg.authorization_url).map_err(|e| {
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

        Ok(OAuthInitiation {
            authorization_url: auth_url.to_string(),
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

        // Persist the tokens returned by the provider. Compute an absolute
        // `expires_at` so the refresh path doesn't need clock math on read.
        let mut secret_data = HashMap::new();
        secret_data.insert(
            "access_token".to_string(),
            SensitiveString::new(token_response.access_token),
        );
        if let Some(refresh_token) = token_response.refresh_token {
            secret_data.insert(
                "refresh_token".to_string(),
                SensitiveString::new(refresh_token),
            );
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
                credential_type: CredentialType::OAuth2,
            });

        Ok(binding.id)
    }

    // -----------------------------------------------------------------------
    // rotate_credential
    // -----------------------------------------------------------------------

    async fn rotate_credential(
        &self,
        binding_id: &CredentialBindingId,
        new_value: SensitiveString,
    ) -> anyhow::Result<()> {
        let binding = self
            .repo
            .find_by_id(binding_id)
            .await?
            .ok_or_else(|| anyhow!("Credential binding not found: {}", binding_id))?;

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
        binding_id: &CredentialBindingId,
        target: GrantTarget,
        granted_by: String,
    ) -> anyhow::Result<CredentialGrantId> {
        let mut binding = self
            .repo
            .find_by_id(binding_id)
            .await?
            .ok_or_else(|| anyhow!("Credential binding not found: {}", binding_id))?;

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
        binding_id: &CredentialBindingId,
        grant_id: &CredentialGrantId,
    ) -> anyhow::Result<()> {
        let mut binding = self
            .repo
            .find_by_id(binding_id)
            .await?
            .ok_or_else(|| anyhow!("Credential binding not found: {}", binding_id))?;

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

    async fn revoke_binding(&self, binding_id: &CredentialBindingId) -> anyhow::Result<()> {
        let mut binding = self
            .repo
            .find_by_id(binding_id)
            .await?
            .ok_or_else(|| anyhow!("Credential binding not found: {}", binding_id))?;

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
        binding_id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        self.repo.find_by_id(binding_id).await
    }
}
