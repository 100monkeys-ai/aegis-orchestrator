// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Keycloak Admin REST API Client (ADR-097)
//!
//! HTTP client for Keycloak Admin REST API operations, used by
//! [`crate::application::tenant_provisioning::TenantProvisioningService`]
//! to stamp `tenant_id` user attributes on newly registered consumer users.

use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// A Keycloak user as returned by the Admin REST API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KeycloakUser {
    pub id: String,
    pub email: Option<String>,
    #[serde(rename = "firstName")]
    pub first_name: Option<String>,
    #[serde(rename = "lastName")]
    pub last_name: Option<String>,
    #[serde(rename = "createdTimestamp")]
    pub created_timestamp: i64,
    pub attributes: Option<std::collections::HashMap<String, Vec<String>>>,
}

/// SAML IdP configuration for a Keycloak realm.
#[derive(Debug, Clone)]
pub struct SamlIdpConfig {
    pub entity_id: String,
    pub sso_url: String,
    pub certificate: String,
}

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

    /// List all users in a realm.
    pub async fn list_realm_users(
        &self,
        realm: &str,
    ) -> Result<Vec<KeycloakUser>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms/{}/users", self.config.host, realm);

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let users: Vec<KeycloakUser> = resp.json().await?;
        Ok(users)
    }

    /// Create a user in a realm and send required actions (verify email + set password).
    /// Returns the newly created user fetched by the Location header.
    pub async fn invite_user(
        &self,
        realm: &str,
        email: &str,
        role: &str,
    ) -> Result<KeycloakUser, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms/{}/users", self.config.host, realm);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "email": email,
                "username": email,
                "enabled": true,
                "requiredActions": ["UPDATE_PASSWORD", "VERIFY_EMAIL"],
                "attributes": {
                    "aegis_role": [role]
                }
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        // Keycloak returns the new user URL in Location header
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| KeycloakAdminError::RealmError {
                status: 0,
                body: "No Location header in create-user response".into(),
            })?;

        // Fetch the created user
        let token2 = self.get_admin_token().await?;
        let user_resp = self.http.get(location).bearer_auth(&token2).send().await?;

        if !user_resp.status().is_success() {
            let status = user_resp.status().as_u16();
            let body = user_resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let user: KeycloakUser = user_resp.json().await?;
        Ok(user)
    }

    /// Delete a user from a realm.
    pub async fn remove_user(&self, realm: &str, user_id: &str) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}",
            self.config.host, realm, user_id
        );

        let resp = self.http.delete(&url).bearer_auth(&token).send().await?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            return Ok(());
        }

        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(KeycloakAdminError::RealmError { status: code, body })
    }

    /// Assign a realm role to a user.
    pub async fn assign_realm_role(
        &self,
        realm: &str,
        user_id: &str,
        role: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;

        // Fetch the role representation
        let role_url = format!("{}/admin/realms/{}/roles/{}", self.config.host, realm, role);
        let role_resp = self.http.get(&role_url).bearer_auth(&token).send().await?;

        if !role_resp.status().is_success() {
            let status = role_resp.status().as_u16();
            let body = role_resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let role_repr: serde_json::Value = role_resp.json().await?;

        // Assign the role to the user
        let token2 = self.get_admin_token().await?;
        let assign_url = format!(
            "{}/admin/realms/{}/users/{}/role-mappings/realm",
            self.config.host, realm, user_id
        );
        let assign_resp = self
            .http
            .post(&assign_url)
            .bearer_auth(&token2)
            .json(&serde_json::json!([role_repr]))
            .send()
            .await?;

        if !assign_resp.status().is_success() {
            let status = assign_resp.status().as_u16();
            let body = assign_resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        Ok(())
    }

    /// Get the SAML IdP configuration for a realm.
    /// Returns `None` if no SAML IdP is configured.
    pub async fn get_idp_config(
        &self,
        realm: &str,
    ) -> Result<Option<SamlIdpConfig>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/identity-provider/instances",
            self.config.host, realm
        );

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let instances: Vec<serde_json::Value> = resp.json().await?;

        // Find the SAML IdP
        let saml_idp = instances
            .into_iter()
            .find(|idp| idp.get("providerId").and_then(|v| v.as_str()) == Some("saml"));

        match saml_idp {
            None => Ok(None),
            Some(idp) => {
                let config_obj = idp
                    .get("config")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let entity_id = config_obj
                    .get("entityId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sso_url = config_obj
                    .get("singleSignOnServiceUrl")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let certificate = config_obj
                    .get("signingCertificate")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(Some(SamlIdpConfig {
                    entity_id,
                    sso_url,
                    certificate,
                }))
            }
        }
    }

    /// Create or update the SAML IdP configuration for a realm.
    pub async fn set_idp_config(
        &self,
        realm: &str,
        config: &SamlIdpConfig,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;

        let payload = serde_json::json!({
            "alias": "saml",
            "providerId": "saml",
            "enabled": true,
            "config": {
                "entityId": config.entity_id,
                "singleSignOnServiceUrl": config.sso_url,
                "signingCertificate": config.certificate,
                "validateSignature": "false",
                "nameIDPolicyFormat": "urn:oasis:names:tc:SAML:2.0:nameid-format:persistent"
            }
        });

        // Try to update existing; fall back to create on 404
        let update_url = format!(
            "{}/admin/realms/{}/identity-provider/instances/saml",
            self.config.host, realm
        );
        let update_resp = self
            .http
            .put(&update_url)
            .bearer_auth(&token)
            .json(&payload)
            .send()
            .await?;

        if update_resp.status().as_u16() == 404 {
            // IdP does not exist yet — create it
            let token2 = self.get_admin_token().await?;
            let create_url = format!(
                "{}/admin/realms/{}/identity-provider/instances",
                self.config.host, realm
            );
            let create_resp = self
                .http
                .post(&create_url)
                .bearer_auth(&token2)
                .json(&payload)
                .send()
                .await?;

            if !create_resp.status().is_success() {
                let status = create_resp.status().as_u16();
                let body = create_resp.text().await.unwrap_or_default();
                return Err(KeycloakAdminError::RealmError { status, body });
            }
        } else if !update_resp.status().is_success() {
            let status = update_resp.status().as_u16();
            let body = update_resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        Ok(())
    }
}
