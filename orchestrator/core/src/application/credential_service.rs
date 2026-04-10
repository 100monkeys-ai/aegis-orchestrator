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
}

impl StandardCredentialManagementService {
    pub fn new(
        repo: Arc<dyn CredentialBindingRepository>,
        secrets: Arc<SecretsManager>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            repo,
            secrets,
            event_bus,
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

        let authorization_url = format!(
            "https://oauth.placeholder/{}/authorize?state={}&code_challenge={}&code_challenge_method=S256&redirect_uri={}",
            provider, state, code_challenge, redirect_uri
        );

        Ok(OAuthInitiation {
            authorization_url,
            state,
        })
    }

    // -----------------------------------------------------------------------
    // complete_oauth_connection
    // -----------------------------------------------------------------------

    async fn complete_oauth_connection(
        &self,
        state: &str,
        _code: &str,
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

        // TODO: call provider token endpoint using `_code` and `pending.pkce_verifier`
        // to exchange the authorization code for an access token + optional refresh token.

        let secret_path =
            user_credential_path(&binding.tenant_id, &binding.owner_user_id, &binding.id);

        // Write a placeholder token value to OpenBao at the real path.
        let mut secret_data = HashMap::new();
        secret_data.insert(
            "access_token".to_string(),
            SensitiveString::new("PENDING_REAL_TOKEN"),
        );
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
