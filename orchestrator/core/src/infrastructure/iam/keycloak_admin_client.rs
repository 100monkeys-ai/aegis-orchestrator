// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Keycloak Admin REST API Client (ADR-097)
//!
//! HTTP client for Keycloak Admin REST API operations, used by
//! [`crate::application::tenant_provisioning::TenantProvisioningService`]
//! to stamp `tenant_id` user attributes on newly registered consumer users.

use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::RwLock;

/// Configuration for Keycloak Admin REST API access (ADR-097).
#[derive(Debug, Clone)]
pub struct KeycloakAdminConfig {
    pub host: String,
    pub admin_username: String,
    pub admin_password: String,
}

/// HTTP client for Keycloak Admin REST API operations.
pub struct KeycloakAdminClient {
    http: Client,
    config: KeycloakAdminConfig,
    cached_token: RwLock<Option<CachedToken>>,
}

struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum KeycloakAdminError {
    #[error("failed to obtain admin token: {0}")]
    TokenError(String),
    #[error("failed to set user attribute: {status} {body}")]
    AttributeError { status: u16, body: String },
    #[error("realm operation failed: {status} {body}")]
    RealmError { status: u16, body: String },
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

impl KeycloakAdminClient {
    pub fn new(config: KeycloakAdminConfig) -> Self {
        Self {
            http: Client::new(),
            config,
            cached_token: RwLock::new(None),
        }
    }

    /// Obtain an admin access token, using cache if valid.
    async fn get_admin_token(&self) -> Result<String, KeycloakAdminError> {
        // Check cache
        if let Ok(guard) = self.cached_token.read() {
            if let Some(cached) = guard.as_ref() {
                if cached.expires_at > Utc::now() + Duration::seconds(30) {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Fetch new token
        let url = format!(
            "{}/realms/master/protocol/openid-connect/token",
            self.config.host
        );
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("grant_type", "password"),
                ("client_id", "admin-cli"),
                ("username", &self.config.admin_username),
                ("password", &self.config.admin_password),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::TokenError(format!(
                "HTTP {status}: {body}"
            )));
        }

        let token_resp: TokenResponse = resp.json().await?;
        let expires_at = Utc::now() + Duration::seconds(token_resp.expires_in);

        // Cache
        if let Ok(mut guard) = self.cached_token.write() {
            *guard = Some(CachedToken {
                access_token: token_resp.access_token.clone(),
                expires_at,
            });
        }

        Ok(token_resp.access_token)
    }

    /// Create a new Keycloak realm for an enterprise tenant (ADR-056).
    ///
    /// Idempotent: a 409 Conflict response (realm already exists) is treated as success.
    pub async fn create_realm(&self, realm_name: &str) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms", self.config.host);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "realm": realm_name,
                "enabled": true
            }))
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 409 {
            return Ok(());
        }

        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(KeycloakAdminError::RealmError { status: code, body })
    }

    /// Delete a Keycloak realm (rollback / deprovisioning).
    ///
    /// A 404 response (realm not found) is treated as idempotent success.
    pub async fn delete_realm(&self, realm_name: &str) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms/{}", self.config.host, realm_name);

        let resp = self.http.delete(&url).bearer_auth(&token).send().await?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }

        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(KeycloakAdminError::RealmError { status: code, body })
    }

    /// Set a user attribute on a Keycloak user in the given realm.
    pub async fn set_user_attribute(
        &self,
        realm: &str,
        user_id: &str,
        attribute: &str,
        value: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}",
            self.config.host, realm, user_id
        );

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "attributes": {
                    attribute: [value]
                }
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::AttributeError { status, body });
        }

        Ok(())
    }
}
