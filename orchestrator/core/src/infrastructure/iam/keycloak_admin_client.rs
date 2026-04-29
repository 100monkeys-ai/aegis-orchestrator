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
///
/// # Security
///
/// Audit 002 §4.37.10 — `admin_password` is wrapped in
/// [`SensitiveString`](crate::domain::secrets::SensitiveString) so the
/// derived `Debug` impl emits `[REDACTED]` rather than the raw value. The
/// previous `String` field combined with `#[derive(Debug)]` meant any
/// `tracing::error!(?config, ...)` or `panic!("{config:?}")` site could
/// dump the admin credential into logs. Mirrors the §4.30 fix already
/// applied to other config structs in BC-11.
#[derive(Debug, Clone)]
pub struct KeycloakAdminConfig {
    pub host: String,
    pub admin_username: String,
    pub admin_password: crate::domain::secrets::SensitiveString,
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

/// Build the full user representation body for a Keycloak PUT /users/{id} call,
/// merging `attribute = [value]` into the user's existing attributes.
///
/// `createdTimestamp` is intentionally excluded — Keycloak rejects it on PUT.
fn build_set_attribute_body(
    user: &KeycloakUser,
    attribute: &str,
    value: &str,
) -> serde_json::Value {
    build_set_multivalue_body(user, attribute, &[value.to_string()])
}

/// Multi-valued variant of [`build_set_attribute_body`]. Keycloak stores user
/// attributes as `Map<String, List<String>>`, so a multi-valued attribute is
/// expressed by passing the full list. An empty `values` slice clears the
/// attribute (Keycloak preserves the key with an empty list, which the
/// upstream MCP middleware treats as "no memberships").
fn build_set_multivalue_body(
    user: &KeycloakUser,
    attribute: &str,
    values: &[String],
) -> serde_json::Value {
    let mut attrs: std::collections::HashMap<String, Vec<String>> =
        user.attributes.clone().unwrap_or_default();
    attrs.insert(attribute.to_string(), values.to_vec());

    serde_json::json!({
        "id": user.id,
        "email": user.email,
        "firstName": user.first_name,
        "lastName": user.last_name,
        "enabled": true,
        "attributes": attrs
    })
}

impl KeycloakAdminClient {
    pub fn new(config: KeycloakAdminConfig) -> Self {
        // Audit 002 §4.37.9 — bound the wait on a frozen Keycloak host. A
        // naked `Client::new()` inherits no implicit total/connect timeout,
        // which means a non-responsive admin endpoint stalls every
        // `set_user_attribute` / realm operation indefinitely.
        let http = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("keycloak admin http client must build with valid defaults");
        Self {
            http,
            config,
            cached_token: RwLock::new(None),
        }
    }

    /// Obtain an admin access token, using cache if valid.
    ///
    /// # Authentication mechanism
    ///
    /// This call uses the OAuth 2.0 Resource Owner Password Credentials
    /// (ROPC) grant against Keycloak's `master` realm with the built-in
    /// `admin-cli` client. The orchestrator presents
    /// `admin_username` / `admin_password` and receives a short-lived admin
    /// access token used for subsequent Keycloak Admin REST API calls.
    ///
    /// # Deprecation status (audit 002 finding 4.37.19)
    ///
    /// ROPC is being phased out by the IETF — the OAuth 2.0 Security Best
    /// Current Practice draft (`draft-ietf-oauth-security-topics`) and
    /// OAuth 2.1 both deprecate the password grant because it requires the
    /// client to handle and transmit user credentials, defeats MFA, and
    /// has no PKCE-equivalent protection. Keycloak still supports it for
    /// administrative bootstrap clients but recommends migrating to
    /// service-account credentials (the `client_credentials` grant against
    /// a dedicated confidential client with `realm-management` roles).
    ///
    /// # Planned migration
    ///
    /// Replace the ROPC call below with `client_credentials` once a
    /// dedicated `aegis-admin` confidential client is provisioned in the
    /// `master` realm with the minimum required `realm-management` role
    /// composites (`manage-users`, `view-users`, `manage-realm` as needed).
    /// The orchestrator config will then carry `admin_client_id` /
    /// `admin_client_secret` instead of `admin_username` / `admin_password`,
    /// and Vault/OpenBao can rotate the secret without touching the human
    /// admin account. Tracked as a follow-up to audit 002 finding 4.37.19;
    /// no immediate code change because the alternative path is not yet
    /// provisioned.
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
                ("username", self.config.admin_username.as_str()),
                // Audit 002 §4.37.10 — only expose the wrapped value at
                // the point of injection into the form body.
                ("password", self.config.admin_password.expose()),
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
    ///
    /// Keycloak's PUT `/admin/realms/{realm}/users/{id}` is a **full replace**:
    /// sending only `{"attributes": {...}}` causes Keycloak to null out `email`
    /// and reject with 400 `error-user-attribute-required`.  We therefore build
    /// a complete user representation, merging the new attribute over the
    /// existing ones.
    pub async fn set_user_attribute(
        &self,
        realm: &str,
        user: &KeycloakUser,
        attribute: &str,
        value: &str,
    ) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}",
            self.config.host, realm, user.id
        );

        let body = build_set_attribute_body(user, attribute, value);

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::AttributeError { status, body });
        }

        Ok(())
    }

    /// Set the multi-valued `team_memberships` user attribute.
    ///
    /// The upstream MCP middleware reads this list of `t-{uuid}` tenant slugs
    /// off the JWT to decide whether the caller may transact against a team
    /// tenant; missing values cause a fail-closed 403. Mirrors
    /// [`set_user_attribute`](Self::set_user_attribute) but writes a
    /// multi-valued list instead of a single value, which Keycloak's user
    /// attributes natively support.
    ///
    /// Passing an empty `tenants` slice clears the attribute (the user is no
    /// longer a member of any team).
    pub async fn set_user_team_memberships(
        &self,
        realm: &str,
        user_id: &str,
        tenants: &[String],
    ) -> Result<(), KeycloakAdminError> {
        let user = match self.get_user(realm, user_id).await? {
            Some(u) => u,
            None => {
                // User not found in this realm — nothing to stamp. Caller
                // (TeamService / backfill) decides whether this is a drift
                // condition; we return Ok to keep behaviour consistent with
                // the tier-sync soft-heal path.
                return Ok(());
            }
        };

        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}",
            self.config.host, realm, user.id
        );
        let body = build_set_multivalue_body(&user, "team_memberships", tenants);

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::AttributeError { status, body });
        }
        Ok(())
    }

    /// List all users in a realm (up to 1000).
    pub async fn list_realm_users(
        &self,
        realm: &str,
    ) -> Result<Vec<KeycloakUser>, KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms/{}/users?max=1000", self.config.host, realm);

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
        // POLICY (ADR-097 footgun #9): Non-Enterprise team tiers (Pro,
        // Business) reuse the shared `zaru-consumer` realm and rely on
        // Keycloak group membership for tenant scoping; only Enterprise
        // gets a dedicated `team-{slug}` realm.
        //
        // SECURITY IMPLICATIONS:
        //   - Pro/Business team users authenticate against `zaru-consumer`,
        //     so their JWTs carry the consumer realm's signing key. Tenant
        //     isolation for these users is enforced at the application
        //     layer via the `tenant_id` claim (ADR-097 per-user tenant
        //     derivation) and the group attribute, NOT by realm boundary.
        //   - A misconfigured group claim or a missing `tenant_id` claim
        //     downgrades a team user into the global consumer tenant.
        //     The IAM service is responsible for rejecting tokens whose
        //     `tenant_id` claim is missing/malformed (ADR-097 footgun #1).
        //   - Enterprise tenants get cryptographic isolation via realm
        //     boundary; Pro/Business do not.
        //
        // This is operational policy, not a bug. If this trade-off becomes
        // unacceptable, switch all tiers to dedicated realms (more
        // Keycloak overhead, full isolation).
        let (realm, use_group) = match team_tier {
            TenantTier::Enterprise => (format!("team-{team_slug}"), false),
            _ => ("zaru-consumer".to_string(), true),
        };

        tracing::info!(
            team_tier = ?team_tier,
            team_slug = %team_slug,
            realm = %realm,
            shared_realm = %use_group,
            "inviting team user; non-Enterprise tiers share the zaru-consumer realm and rely on application-layer tenant isolation"
        );

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit 002 §4.37.10 regression — `KeycloakAdminConfig`'s `Debug`
    /// output must NOT contain the admin password. Before the fix,
    /// `admin_password: String` combined with `#[derive(Debug)]` meant
    /// any `tracing::error!(?config, ...)` site dumped the credential
    /// to logs. Wrapping the field in `SensitiveString` redirects
    /// `Debug` to `[REDACTED]` regardless of the value.
    #[test]
    fn keycloak_admin_config_debug_redacts_password() {
        let cfg = KeycloakAdminConfig {
            host: "https://auth.example.com".to_string(),
            admin_username: "admin".to_string(),
            admin_password: crate::domain::secrets::SensitiveString::new(
                "super-secret-do-not-leak-12345",
            ),
        };
        let dumped = format!("{cfg:?}");
        assert!(
            !dumped.contains("super-secret-do-not-leak-12345"),
            "Debug output must NOT contain the raw password; got: {dumped}"
        );
        assert!(
            dumped.contains("REDACTED"),
            "Debug output must mark the password as redacted; got: {dumped}"
        );
        // Other fields should still be visible for diagnostics.
        assert!(dumped.contains("auth.example.com"));
        assert!(dumped.contains("admin"));
    }

    // ── build_set_attribute_body ───────────────────────────────────────────

    /// The PUT body must include `id`, `email`, `firstName`, `lastName`,
    /// `enabled: true`, and the merged attributes map.  `createdTimestamp`
    /// must be absent (Keycloak rejects it on PUT).
    #[test]
    fn set_user_attribute_includes_existing_fields() {
        let mut existing_attrs = std::collections::HashMap::new();
        existing_attrs.insert("tenant_id".to_string(), vec!["u-abc".to_string()]);

        let user = KeycloakUser {
            id: "user-123".to_string(),
            email: Some("alice@example.com".to_string()),
            first_name: Some("Alice".to_string()),
            last_name: Some("Smith".to_string()),
            created_timestamp: 1_700_000_000,
            attributes: Some(existing_attrs),
        };

        let body = build_set_attribute_body(&user, "zaru_tier", "pro");

        // Required identity fields are present.
        assert_eq!(body["id"], "user-123");
        assert_eq!(body["email"], "alice@example.com");
        assert_eq!(body["firstName"], "Alice");
        assert_eq!(body["lastName"], "Smith");
        assert_eq!(body["enabled"], true);

        // createdTimestamp must NOT be present — Keycloak rejects it on PUT.
        assert!(
            body.get("createdTimestamp").is_none(),
            "createdTimestamp must be omitted from PUT body"
        );

        // New attribute is set.
        assert_eq!(body["attributes"]["zaru_tier"][0], "pro");

        // Existing attribute is preserved.
        assert_eq!(body["attributes"]["tenant_id"][0], "u-abc");
    }

    /// When the user has no existing attributes the body is still well-formed
    /// and the new attribute is included.
    #[test]
    fn set_user_attribute_handles_no_existing_attributes() {
        let user = KeycloakUser {
            id: "user-456".to_string(),
            email: None,
            first_name: None,
            last_name: None,
            created_timestamp: 0,
            attributes: None,
        };

        let body = build_set_attribute_body(&user, "zaru_tier", "business");

        assert_eq!(body["id"], "user-456");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["attributes"]["zaru_tier"][0], "business");
    }

    /// The multi-valued attribute body preserves all supplied values verbatim
    /// and does not collapse the list to a single element.
    #[test]
    fn set_multivalue_body_preserves_full_list() {
        let user = KeycloakUser {
            id: "user-multi".to_string(),
            email: Some("multi@example.com".to_string()),
            first_name: None,
            last_name: None,
            created_timestamp: 0,
            attributes: None,
        };

        let tenants = vec![
            "t-11111111-2222-3333-4444-555555555555".to_string(),
            "t-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
        ];
        let body = build_set_multivalue_body(&user, "team_memberships", &tenants);

        let arr = body["attributes"]["team_memberships"]
            .as_array()
            .expect("team_memberships must serialize as a JSON array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], tenants[0]);
        assert_eq!(arr[1], tenants[1]);
    }

    /// An empty tenants slice clears the attribute by writing an empty list,
    /// rather than dropping the key entirely. This matches Keycloak's
    /// `Map<String, List<String>>` storage model and the MCP middleware's
    /// "no memberships" semantics.
    #[test]
    fn set_multivalue_body_empty_list_clears_attribute() {
        let mut existing = std::collections::HashMap::new();
        existing.insert(
            "team_memberships".to_string(),
            vec!["t-old".to_string(), "t-older".to_string()],
        );

        let user = KeycloakUser {
            id: "user-clear".to_string(),
            email: Some("clear@example.com".to_string()),
            first_name: None,
            last_name: None,
            created_timestamp: 0,
            attributes: Some(existing),
        };

        let body = build_set_multivalue_body(&user, "team_memberships", &[]);
        let arr = body["attributes"]["team_memberships"]
            .as_array()
            .expect("attribute key must remain present with an empty list");
        assert!(arr.is_empty());
    }

    /// A new value for an existing attribute key overwrites the old one.
    #[test]
    fn set_user_attribute_overwrites_existing_key() {
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("zaru_tier".to_string(), vec!["free".to_string()]);

        let user = KeycloakUser {
            id: "user-789".to_string(),
            email: Some("bob@example.com".to_string()),
            first_name: None,
            last_name: None,
            created_timestamp: 0,
            attributes: Some(attrs),
        };

        let body = build_set_attribute_body(&user, "zaru_tier", "business");

        assert_eq!(body["attributes"]["zaru_tier"][0], "business");
    }
}
