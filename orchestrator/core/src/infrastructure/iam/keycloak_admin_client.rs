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

use crate::domain::tenancy::TenantTier;

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

    // ────────────────────────────────────────────────────────────────────────
    // Group management (ADR-111 §Keycloak Strategy — Pro/Business teams)
    // ────────────────────────────────────────────────────────────────────────

    /// Create a new Keycloak group in the given realm. Returns the group id
    /// extracted from the Location header.
    ///
    /// A 409 Conflict (group already exists) is treated as success: the
    /// existing group id is looked up and returned.
    pub async fn create_group(
        &self,
        realm: &str,
        group_name: &str,
    ) -> Result<String, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms/{}/groups", self.config.host, realm);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "name": group_name }))
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 409 {
            // Already exists — look it up.
            return self
                .find_group_by_name(realm, group_name)
                .await?
                .ok_or_else(|| KeycloakAdminError::RealmError {
                    status: 409,
                    body: format!(
                        "group {group_name} conflict but find_group_by_name returned None"
                    ),
                });
        }
        if !status.is_success() {
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status: code, body });
        }

        // Parse id out of the Location header: `.../groups/{id}`
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| KeycloakAdminError::RealmError {
                status: 0,
                body: "No Location header in create-group response".into(),
            })?;
        let id = location.rsplit('/').next().unwrap_or("").to_string();
        if id.is_empty() {
            return Err(KeycloakAdminError::RealmError {
                status: 0,
                body: format!("Unable to parse group id from Location: {location}"),
            });
        }
        Ok(id)
    }

    /// Delete a Keycloak group. A 404 (group not found) is treated as
    /// idempotent success.
    pub async fn delete_group(
        &self,
        realm: &str,
        group_id: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/groups/{}",
            self.config.host, realm, group_id
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

    /// Find a Keycloak group by exact name. Returns the group id if it exists.
    pub async fn find_group_by_name(
        &self,
        realm: &str,
        group_name: &str,
    ) -> Result<Option<String>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/groups?search={}&exact=true",
            self.config.host, realm, group_name
        );

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let groups: Vec<serde_json::Value> = resp.json().await?;
        let id = groups
            .into_iter()
            .find(|g| g.get("name").and_then(|v| v.as_str()) == Some(group_name))
            .and_then(|g| g.get("id").and_then(|v| v.as_str()).map(str::to_owned));
        Ok(id)
    }

    /// Attach a user to a group. Idempotent on Keycloak's side — repeated
    /// calls return 204.
    pub async fn add_user_to_group(
        &self,
        realm: &str,
        user_id: &str,
        group_id: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}/groups/{}",
            self.config.host, realm, user_id, group_id
        );

        let resp = self.http.put(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }
        Ok(())
    }

    /// Detach a user from a group. A 404 is treated as idempotent success.
    pub async fn remove_user_from_group(
        &self,
        realm: &str,
        user_id: &str,
        group_id: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}/groups/{}",
            self.config.host, realm, user_id, group_id
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

    /// List members of a Keycloak group.
    pub async fn list_group_members(
        &self,
        realm: &str,
        group_id: &str,
    ) -> Result<Vec<KeycloakUser>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/groups/{}/members",
            self.config.host, realm, group_id
        );

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let users: Vec<KeycloakUser> = resp.json().await?;
        Ok(users)
    }

    /// Look up a single Keycloak user by id.
    pub async fn get_user(
        &self,
        realm: &str,
        user_id: &str,
    ) -> Result<Option<KeycloakUser>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}",
            self.config.host, realm, user_id
        );

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }
        Ok(Some(resp.json().await?))
    }

    /// Look up a Keycloak user by email in a realm. Returns the first match.
    pub async fn find_user_by_email(
        &self,
        realm: &str,
        email: &str,
    ) -> Result<Option<KeycloakUser>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users?email={}&exact=true",
            self.config.host, realm, email
        );

        let resp = self.http.get(&url).bearer_auth(&token).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::RealmError { status, body });
        }

        let users: Vec<KeycloakUser> = resp.json().await?;
        Ok(users.into_iter().next())
    }

    // ────────────────────────────────────────────────────────────────────────
    // Team realm provisioning (ADR-111 §Keycloak Strategy — Enterprise teams)
    // ────────────────────────────────────────────────────────────────────────

    /// Create a dedicated realm for an Enterprise team and seed the composite
    /// roles `owner`, `admin`, `member`.
    ///
    /// Idempotent: reuses [`create_realm`](Self::create_realm) (which treats
    /// 409 as success) and [`create_realm_role`](Self::create_realm_role) for
    /// role seeding.
    ///
    /// The SAML IdP configuration is set separately via
    /// [`set_idp_config`](Self::set_idp_config) once the owner configures
    /// their corporate federation.
    pub async fn create_team_realm(&self, team_slug: &str) -> Result<(), KeycloakAdminError> {
        let realm = format!("team-{team_slug}");
        self.create_realm(&realm).await?;
        for role in ["owner", "admin", "member"] {
            self.create_realm_role(&realm, role).await?;
        }
        Ok(())
    }

    /// Create a realm role. A 409 (role already exists) is treated as success.
    pub async fn create_realm_role(
        &self,
        realm: &str,
        role: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms/{}/roles", self.config.host, realm);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({ "name": role }))
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

    // ────────────────────────────────────────────────────────────────────────
    // Team-aware invite (ADR-111 §Invitation Flow)
    // ────────────────────────────────────────────────────────────────────────

    /// Invite a user into a team.
    ///
    /// - **Enterprise** teams have a dedicated `team-{slug}` realm; this path
    ///   creates (or reuses) the user inside that realm.
    /// - **Pro / Business** teams are Keycloak groups inside `zaru-consumer`;
    ///   this path creates (or reuses) the user in `zaru-consumer` and
    ///   attaches them to the group named `team_slug` (the group is created
    ///   on demand).
    ///
    /// `invite_token` is stored as a user attribute so downstream flows can
    /// correlate the Keycloak user with the orchestrator `TeamInvitation`.
    /// Returns the Keycloak user id.
    pub async fn invite_team_user(
        &self,
        team_tier: TenantTier,
        team_slug: &str,
        email: &str,
        role: &str,
        invite_token: &str,
    ) -> Result<String, KeycloakAdminError> {
        let (realm, use_group) = match team_tier {
            TenantTier::Enterprise => (format!("team-{team_slug}"), false),
            _ => ("zaru-consumer".to_string(), true),
        };

        // Reuse an existing user if one already exists with this email;
        // otherwise create a fresh invited user.
        let user_id = match self.find_user_by_email(&realm, email).await? {
            Some(u) => {
                // Update the aegis_role and invite_token attributes in place.
                let token = self.get_admin_token().await?;
                let url = format!("{}/admin/realms/{}/users/{}", self.config.host, realm, u.id);
                let resp = self
                    .http
                    .put(&url)
                    .bearer_auth(&token)
                    .json(&serde_json::json!({
                        "attributes": {
                            "aegis_role": [role],
                            "team_invite_token": [invite_token],
                            "team_slug": [team_slug]
                        }
                    }))
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(KeycloakAdminError::AttributeError { status, body });
                }
                u.id
            }
            None => {
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
                        "requiredActions": ["VERIFY_EMAIL"],
                        "attributes": {
                            "aegis_role": [role],
                            "team_invite_token": [invite_token],
                            "team_slug": [team_slug]
                        }
                    }))
                    .send()
                    .await?;

                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    return Err(KeycloakAdminError::RealmError { status, body });
                }

                let location = resp
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| KeycloakAdminError::RealmError {
                        status: 0,
                        body: "No Location header in create-user response".into(),
                    })?;
                location.rsplit('/').next().unwrap_or("").to_string()
            }
        };

        if use_group {
            // Ensure the group exists, then attach the user.
            let group_id = match self.find_group_by_name(&realm, team_slug).await? {
                Some(id) => id,
                None => self.create_group(&realm, team_slug).await?,
            };
            self.add_user_to_group(&realm, &user_id, &group_id).await?;
        }

        Ok(user_id)
    }
}
