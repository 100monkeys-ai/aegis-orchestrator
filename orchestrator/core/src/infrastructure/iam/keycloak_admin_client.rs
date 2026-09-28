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
    /// Whether the user may sign in, as Keycloak holds it. A write of an
    /// attribute sends it back unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub attributes: Option<std::collections::HashMap<String, Vec<String>>>,
}

/// What [`KeycloakAdminClient::write_user_attributes`] did.
#[derive(Debug)]
pub enum UserWrite {
    /// The realm has no such user; nothing was written.
    Missing,
    /// The user already held every value; nothing was written.
    Unchanged,
    /// The user was written. `before` is the user as it was read, inside
    /// the lock, just before the write.
    Written { before: KeycloakUser },
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

/// How long a write to a Keycloak user waits for another write to the same
/// user to finish before it gives up.
const USER_WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// (realm, user id) -> the lock writes to that user take.
type UserWriteLocks =
    std::collections::HashMap<(String, String), std::sync::Weak<tokio::sync::Mutex<()>>>;

/// HTTP client for Keycloak Admin REST API operations.
///
/// Writes to one user are made one at a time: see
/// [`KeycloakAdminClient::write_user_attributes`]. The lock is held in this
/// process, which is enough while one orchestrator process writes a realm's
/// users.
pub struct KeycloakAdminClient {
    http: Client,
    config: KeycloakAdminConfig,
    cached_token: RwLock<Option<CachedToken>>,
    /// (realm, user id) -> the lock writes to that user take. Entries whose
    /// lock nobody holds or waits for are dropped as new ones are added.
    user_writes: std::sync::Mutex<UserWriteLocks>,
    user_write_wait: std::time::Duration,
}

struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum KeycloakAdminError {
    /// The admin token endpoint refused the grant. Carries only the RFC 6749
    /// §5.2 `error` and `error_description` of the response body; anything
    /// else in the body is dropped.
    #[error(
        "failed to obtain admin token: HTTP {status}{}",
        oauth_error_detail(.error.as_deref(), .error_description.as_deref())
    )]
    TokenError {
        status: u16,
        error: Option<String>,
        error_description: Option<String>,
    },
    #[error("failed to set user attribute: {status} {body}")]
    AttributeError { status: u16, body: String },
    #[error("realm operation failed: {status} {body}")]
    RealmError { status: u16, body: String },
    /// Another write to the same user did not finish in time. Nothing was
    /// written.
    #[error(
        "another change to Keycloak user {user_id} in realm {realm} did not finish within \
         {seconds} seconds, so this change was not made; try again"
    )]
    UserWriteBusy {
        realm: String,
        user_id: String,
        seconds: u64,
    },
    /// A SAML identity provider configuration was refused before anything
    /// was sent to Keycloak. The message says what to fix.
    #[error("{0}")]
    InvalidIdpConfig(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

/// The fields of an RFC 6749 §5.2 error response. Any other field in the
/// body is ignored.
#[derive(Deserialize, Default)]
struct OAuthErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `": <error> — <error_description>"`, or as much of it as is present.
fn oauth_error_detail(error: Option<&str>, description: Option<&str>) -> String {
    match (error, description) {
        (Some(e), Some(d)) => format!(": {e} — {d}"),
        (Some(e), None) => format!(": {e}"),
        (None, Some(d)) => format!(": {d}"),
        (None, None) => String::new(),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

/// The realm a new tenant or team realm is created as.
///
/// Keycloak's defaults leave brute-force protection and email verification
/// off. A realm the orchestrator creates has the protections of the
/// `zaru-consumer` realm, as the deployment's Keycloak bootstrap sets them:
/// verified email, one account per email address, sign-in with the email
/// address as the username, password reset, and the consumer realm's token
/// and session lifetimes. Where the consumer realm sets nothing, or where an
/// Enterprise realm needs its own value, the value here is the safe one:
///
/// - self-registration is off: people reach a tenant or team realm by
///   invitation or through the team's identity provider;
/// - brute-force detection is on, with Keycloak's own lockout settings
///   (a temporary lockout after 30 failures, growing by a minute to at most
///   fifteen, counted over twelve hours; never permanent);
/// - passwords need 12 characters and may not be the username or email;
/// - HTTPS is required for every address outside the private networks
///   (`external`, as in the consumer realm, which leaves the setting at
///   Keycloak's default).
///
/// Email delivery (SMTP) is not set: it holds a credential the orchestrator
/// does not have, and it is configured with the realm's other deployment
/// settings.
fn protected_realm_representation(realm_name: &str) -> serde_json::Value {
    serde_json::json!({
        "realm": realm_name,
        "enabled": true,
        "verifyEmail": true,
        "duplicateEmailsAllowed": false,
        "loginWithEmailAllowed": true,
        "registrationEmailAsUsername": true,
        "resetPasswordAllowed": true,
        "registrationAllowed": false,
        "editUsernameAllowed": false,
        "sslRequired": "external",
        "bruteForceProtected": true,
        "permanentLockout": false,
        "failureFactor": 30,
        "waitIncrementSeconds": 60,
        "maxFailureWaitSeconds": 900,
        "maxDeltaTimeSeconds": 43200,
        "minimumQuickLoginWaitSeconds": 60,
        "quickLoginCheckMilliSeconds": 1000,
        "passwordPolicy": "length(12) and maxLength(128) and notUsername and notEmail",
        "accessTokenLifespan": 1800,
        "ssoSessionIdleTimeout": 259200,
        "ssoSessionMaxLifespan": 1209600
    })
}

/// The user representation a `PUT /users/{id}` sends: the user as it was
/// read, with `attributes` in place of its attributes.
///
/// `createdTimestamp` is left out: Keycloak rejects it on PUT. `enabled` is
/// sent as it was read, so a write of an attribute never enables a user an
/// administrator disabled.
fn build_user_body(
    user: &KeycloakUser,
    attributes: std::collections::HashMap<String, Vec<String>>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": user.id,
        "email": user.email,
        "firstName": user.first_name,
        "lastName": user.last_name,
        "attributes": attributes
    });
    if let Some(enabled) = user.enabled {
        body["enabled"] = serde_json::json!(enabled);
    }
    body
}

#[cfg(test)]
fn build_set_attribute_body(
    user: &KeycloakUser,
    attribute: &str,
    value: &str,
) -> serde_json::Value {
    build_set_multivalue_body(user, attribute, &[value.to_string()])
}

#[cfg(test)]
fn build_set_multivalue_body(
    user: &KeycloakUser,
    attribute: &str,
    values: &[String],
) -> serde_json::Value {
    let mut attrs = user.attributes.clone().unwrap_or_default();
    attrs.insert(attribute.to_string(), values.to_vec());
    build_user_body(user, attrs)
}

/// Refuse a SAML signing certificate that is missing or is not an X.509
/// certificate. Takes what Keycloak's `signingCertificate` takes: one or
/// more certificates separated by commas, each as PEM or as bare base64.
/// Only the shape is checked (base64 of a DER certificate: a sequence of the
/// signed part, the algorithm and the signature); Keycloak checks each
/// response's signature with it.
fn check_signing_certificate(text: &str) -> Result<(), KeycloakAdminError> {
    use base64::Engine;
    if text.trim().is_empty() {
        return Err(KeycloakAdminError::InvalidIdpConfig(
            "the identity provider's signing certificate is missing; add the certificate the provider signs its SAML responses with"
                .to_string(),
        ));
    }
    for one in text.split(',') {
        let body: String = one
            .lines()
            .filter(|l| !l.trim_start().starts_with("-----"))
            .flat_map(|l| l.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        let is_certificate = base64::engine::general_purpose::STANDARD
            .decode(body.as_bytes())
            .ok()
            .is_some_and(|der| der_is_certificate(&der));
        if !is_certificate {
            return Err(KeycloakAdminError::InvalidIdpConfig(
                "the identity provider's signing certificate is not a certificate; paste the X.509 certificate the provider signs its SAML responses with, as PEM or base64"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

/// One DER element at the start of `der`: (tag, contents, rest).
fn der_element(der: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = der.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |acc, b| (acc << 8) | *b as usize);
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// An X.509 certificate in DER: a SEQUENCE, filling the whole input, of a
/// SEQUENCE (the signed part), a SEQUENCE (the algorithm) and a BIT STRING
/// (the signature).
fn der_is_certificate(der: &[u8]) -> bool {
    let Some((0x30, cert, [])) = der_element(der) else {
        return false;
    };
    let Some((0x30, _, rest)) = der_element(cert) else {
        return false;
    };
    let Some((0x30, _, rest)) = der_element(rest) else {
        return false;
    };
    matches!(der_element(rest), Some((0x03, _, [])))
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
            user_writes: std::sync::Mutex::new(std::collections::HashMap::new()),
            user_write_wait: USER_WRITE_WAIT,
        }
    }

    /// Wait at most `wait` for another write to the same user (10 seconds
    /// unless set).
    pub fn with_user_write_wait(mut self, wait: std::time::Duration) -> Self {
        self.user_write_wait = wait;
        self
    }

    /// Take the lock for writes to `user_id` in `realm`, waiting at most the
    /// client's user write wait.
    async fn lock_user(
        &self,
        realm: &str,
        user_id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, KeycloakAdminError> {
        let lock = {
            let mut locks = self
                .user_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            locks.retain(|_, held| held.strong_count() > 0);
            let key = (realm.to_string(), user_id.to_string());
            match locks.get(&key).and_then(std::sync::Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(key, std::sync::Arc::downgrade(&lock));
                    lock
                }
            }
        };
        tokio::time::timeout(self.user_write_wait, lock.lock_owned())
            .await
            .map_err(|_| KeycloakAdminError::UserWriteBusy {
                realm: realm.to_string(),
                user_id: user_id.to_string(),
                seconds: self.user_write_wait.as_secs(),
            })
    }

    /// Set `attributes` on a user, each to the full list of values given,
    /// leaving everything else on the user as it is.
    ///
    /// Keycloak's `PUT /admin/realms/{realm}/users/{id}` replaces the user's
    /// representation and has no version check, so a write is a read, a
    /// merge and a write of the whole user. Two of them at once would each
    /// read the user before the other wrote, and the second would drop what
    /// the first wrote. So every write to a user goes through here, and
    /// holds a lock keyed on the realm and the user's id from the read to
    /// the end of the write: writes to one user are made one at a time. A
    /// write that cannot take the lock within the client's wait fails with
    /// [`KeycloakAdminError::UserWriteBusy`] and writes nothing.
    ///
    /// Nothing is written when the user already holds every value.
    pub async fn write_user_attributes(
        &self,
        realm: &str,
        user_id: &str,
        attributes: &[(&str, Vec<String>)],
    ) -> Result<UserWrite, KeycloakAdminError> {
        let _one_at_a_time = self.lock_user(realm, user_id).await?;
        let Some(user) = self.get_user(realm, user_id).await? else {
            return Ok(UserWrite::Missing);
        };
        let mut merged = user.attributes.clone().unwrap_or_default();
        let mut changed = false;
        for (name, values) in attributes {
            if merged.get(*name) != Some(values) {
                merged.insert((*name).to_string(), values.clone());
                changed = true;
            }
        }
        if !changed {
            return Ok(UserWrite::Unchanged);
        }

        let token = self.get_admin_token().await?;
        let url = format!(
            "{}/admin/realms/{}/users/{}",
            self.config.host, realm, user.id
        );
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&token)
            .json(&build_user_body(&user, merged))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(KeycloakAdminError::AttributeError { status, body });
        }
        Ok(UserWrite::Written { before: user })
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
            // Keep the two fields RFC 6749 §5.2 defines for a refused grant;
            // drop the rest of the body.
            let body: OAuthErrorBody = resp.json().await.unwrap_or_default();
            return Err(KeycloakAdminError::TokenError {
                status,
                error: body.error,
                error_description: body.error_description,
            });
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

    /// Create a new Keycloak realm for an enterprise tenant (ADR-056) or an
    /// Enterprise team, with its protections on (see
    /// `protected_realm_representation`).
    ///
    /// Idempotent: a 409 Conflict response (realm already exists) is treated
    /// as success, and the existing realm is not changed.
    pub async fn create_realm(&self, realm_name: &str) -> Result<(), KeycloakAdminError> {
        let token = self.get_admin_token().await?;
        let url = format!("{}/admin/realms", self.config.host);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&token)
            .json(&protected_realm_representation(realm_name))
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

    /// Set a single-valued attribute on a user in the given realm, through
    /// [`write_user_attributes`](Self::write_user_attributes). A user the
    /// realm does not have is left alone.
    pub async fn set_user_attribute(
        &self,
        realm: &str,
        user_id: &str,
        attribute: &str,
        value: &str,
    ) -> Result<(), KeycloakAdminError> {
        self.write_user_attributes(realm, user_id, &[(attribute, vec![value.to_string()])])
            .await
            .map(|_| ())
    }

    /// Set the multi-valued `team_memberships` user attribute, through
    /// [`write_user_attributes`](Self::write_user_attributes).
    ///
    /// The upstream MCP middleware reads this list of `t-{uuid}` tenant slugs
    /// off the JWT to decide whether the caller may transact against a team
    /// tenant; missing values cause a fail-closed 403.
    ///
    /// Passing an empty `tenants` slice clears the attribute (the user is no
    /// longer a member of any team). A user the realm does not have is left
    /// alone: the caller (TeamService / backfill) decides whether that is
    /// drift, as the tier sync does.
    pub async fn set_user_team_memberships(
        &self,
        realm: &str,
        user_id: &str,
        tenants: &[String],
    ) -> Result<(), KeycloakAdminError> {
        self.write_user_attributes(realm, user_id, &[("team_memberships", tenants.to_vec())])
            .await
            .map(|_| ())
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
    ///
    /// Keycloak checks the signature of every SAML response against the
    /// provider's signing certificate, always: without that check anyone
    /// who can reach the realm's broker endpoint could sign in as any user.
    /// So the configuration must carry a signing certificate, and one that
    /// has none, or holds something that is not a certificate, is refused
    /// before anything is sent. The email a provider sends is not trusted
    /// as verified. There is no setting that turns either check off.
    pub async fn set_idp_config(
        &self,
        realm: &str,
        config: &SamlIdpConfig,
    ) -> Result<(), KeycloakAdminError> {
        check_signing_certificate(&config.certificate)?;
        let token = self.get_admin_token().await?;

        let payload = serde_json::json!({
            "alias": "saml",
            "providerId": "saml",
            "enabled": true,
            "trustEmail": false,
            "config": {
                "entityId": config.entity_id,
                "singleSignOnServiceUrl": config.sso_url,
                "signingCertificate": config.certificate,
                "validateSignature": "true",
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
        let url = format!("{}/admin/realms/{}/groups", self.config.host, realm);

        let resp = self
            .http
            .get(&url)
            .query(&[("search", group_name), ("exact", "true")])
            .bearer_auth(&token)
            .send()
            .await?;

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
        let url = format!("{}/admin/realms/{}/users", self.config.host, realm);

        // Encoded as a query parameter: a `+` or `&` in the address is part
        // of the address, not query syntax.
        let resp = self
            .http
            .get(&url)
            .query(&[("email", email), ("exact", "true")])
            .bearer_auth(&token)
            .send()
            .await?;

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
    /// An invitation writes no user attribute. An existing user is left as
    /// it is: Keycloak's `PUT /users/{id}` replaces the whole user, so a
    /// write here would drop the attributes other parts of the platform
    /// keep on it (`tenant_id`, `zaru_tier`, `team_memberships`). Nothing
    /// reads an attribute about the invitation, and the invitation token
    /// is never sent: accepting an invitation goes to the orchestrator,
    /// which checks the token against the digest it stores.
    ///
    /// Returns the Keycloak user id.
    pub async fn invite_team_user(
        &self,
        team_tier: TenantTier,
        team_slug: &str,
        email: &str,
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
            Some(u) => u.id,
            None => self.create_invited_user(&realm, email).await?,
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

    /// Create the user an invitation is for, with its email as username,
    /// the verify-email action and no attributes. Returns the user id.
    ///
    /// Another invitation for the same email can create the user between
    /// this invitation's search and its create; Keycloak then refuses the
    /// create with 409, and the user that was created is looked up and
    /// used.
    async fn create_invited_user(
        &self,
        realm: &str,
        email: &str,
    ) -> Result<String, KeycloakAdminError> {
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
            }))
            .send()
            .await?;

        if resp.status().as_u16() == 409 {
            return match self.find_user_by_email(realm, email).await? {
                Some(u) => Ok(u.id),
                None => Err(KeycloakAdminError::RealmError {
                    status: 409,
                    body: "the user create was refused as a conflict and no user has this email"
                        .into(),
                }),
            };
        }
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
        Ok(location.rsplit('/').next().unwrap_or("").to_string())
    }
}

/// The same client against a real Keycloak (CI starts one).
#[cfg(test)]
#[path = "keycloak_live_tests.rs"]
mod live_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The admin token endpoint refuses the grant with an RFC 6749 error body
    /// that also carries an unrelated field holding a marker. The error the
    /// client returns keeps `error` and `error_description` and drops the
    /// rest of the body.
    #[tokio::test]
    async fn token_error_keeps_only_the_oauth_error_fields() {
        async fn refuse() -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "error": "invalid_grant",
                    "error_description": "Invalid user credentials",
                    "echo": "Mk7-keycloak-error-body-marker",
                })),
            )
        }
        let app = axum::Router::new().route(
            "/realms/master/protocol/openid-connect/token",
            axum::routing::post(refuse),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let kc = KeycloakAdminClient::new(KeycloakAdminConfig {
            host: format!("http://{addr}"),
            admin_username: "admin".to_string(),
            admin_password: crate::domain::secrets::SensitiveString::new("admin-password"),
        });

        let err = kc
            .create_realm("r")
            .await
            .expect_err("the refused grant is an error");
        let printed = format!("{err} {err:?}");
        assert!(
            !printed.contains("Mk7-keycloak-error-body-marker"),
            "the token error carried more of the body than its OAuth fields: {printed}"
        );
        for kept in ["401", "invalid_grant", "Invalid user credentials"] {
            assert!(
                printed.contains(kept),
                "the token error lost {kept:?}: {printed}"
            );
        }
    }

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

    // ── invite_team_user against a Keycloak double ─────────────────────────

    /// A loopback stand-in for the Keycloak admin API that keeps its users.
    /// Like Keycloak, `PUT /users/{id}` replaces the user's representation:
    /// what the body leaves out is gone. Every call is recorded as
    /// (method, path, body).
    #[derive(Default)]
    pub(crate) struct KeycloakDouble {
        /// How long `GET /users/{id}` takes to answer, after it has read the
        /// user: two writers that read at once both read the user as it was.
        pub(crate) get_user_delay: Option<std::time::Duration>,
        /// realm -> user id -> user representation
        pub(crate) users: std::collections::HashMap<
            String,
            std::collections::BTreeMap<String, serde_json::Value>,
        >,
        /// realm -> group name -> group id
        groups: std::collections::HashMap<String, std::collections::BTreeMap<String, String>>,
        /// (realm, user id, group id)
        group_members: Vec<(String, String, String)>,
        calls: Vec<(String, String, String)>,
        /// How many user searches still answer "nobody", as a search made
        /// just before another request created the user would.
        stale_searches: u32,
        /// realm -> identity provider alias -> representation
        idps: std::collections::HashMap<
            String,
            std::collections::BTreeMap<String, serde_json::Value>,
        >,
        /// realm name -> realm representation, as created
        realms: std::collections::BTreeMap<String, serde_json::Value>,
        /// (realm, role name)
        realm_roles: Vec<(String, String)>,
    }

    pub(crate) type SharedDouble = std::sync::Arc<std::sync::Mutex<KeycloakDouble>>;

    async fn keycloak_double(
        state: axum::extract::State<SharedDouble>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let delay = {
            let d = state.0.lock().unwrap();
            let parts: Vec<&str> = uri.path().trim_start_matches('/').split('/').collect();
            let user_read = method == axum::http::Method::GET
                && parts.len() == 5
                && parts[0] == "admin"
                && parts[3] == "users";
            if user_read {
                d.get_user_delay
            } else {
                None
            }
        };
        // The answer is what the stand-in holds when the request arrives;
        // a delayed answer arrives later.
        let answer = keycloak_double_now(state, method, uri, body);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        answer
    }

    fn keycloak_double_now(
        axum::extract::State(double): axum::extract::State<SharedDouble>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let path = uri.path().to_string();
        let query = uri.query().unwrap_or("").to_string();
        let body_text = String::from_utf8_lossy(&body).to_string();
        let mut d = double.lock().unwrap();
        d.calls
            .push((method.to_string(), path.clone(), body_text.clone()));

        if path == "/realms/master/protocol/openid-connect/token" {
            return axum::Json(
                serde_json::json!({"access_token": "admin-token", "expires_in": 300}),
            )
            .into_response();
        }
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if method == axum::http::Method::POST && parts == ["admin", "realms"] {
            // Like Keycloak: a realm that exists already is refused with 409.
            let rep: serde_json::Value = serde_json::from_str(&body_text).unwrap();
            let name = rep["realm"].as_str().unwrap_or_default().to_string();
            if d.realms.contains_key(&name) {
                return axum::http::StatusCode::CONFLICT.into_response();
            }
            d.realms.insert(name, rep);
            return axum::http::StatusCode::CREATED.into_response();
        }
        // admin / realms / {realm} / ...
        if parts.len() < 4 || parts[0] != "admin" || parts[1] != "realms" {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
        let realm = parts[2].to_string();
        let rest = &parts[3..];
        // Decoded as a web server decodes a query string: `+` is a space
        // and `%XX` is a byte.
        let param = |name: &str| {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        match (method.as_str(), rest) {
            ("GET", ["users"]) => {
                if d.stale_searches > 0 {
                    d.stale_searches -= 1;
                    return axum::Json(Vec::<serde_json::Value>::new()).into_response();
                }
                let email = param("email").unwrap_or_default();
                let found: Vec<serde_json::Value> = d
                    .users
                    .get(&realm)
                    .map(|m| {
                        m.values()
                            .filter(|u| u["email"].as_str() == Some(email.as_str()))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                axum::Json(found).into_response()
            }
            ("POST", ["users"]) => {
                let mut rep: serde_json::Value = serde_json::from_str(&body_text).unwrap();
                let email = rep["email"].as_str().unwrap_or_default().to_string();
                let realm_users = d.users.entry(realm.clone()).or_default();
                if realm_users
                    .values()
                    .any(|u| u["email"].as_str() == Some(email.as_str()))
                {
                    return (
                        axum::http::StatusCode::CONFLICT,
                        axum::Json(
                            serde_json::json!({"errorMessage": "User exists with same email"}),
                        ),
                    )
                        .into_response();
                }
                let id = format!("user-{}", realm_users.len() + 1);
                rep["id"] = serde_json::json!(id);
                rep["createdTimestamp"] = serde_json::json!(0);
                realm_users.insert(id.clone(), rep);
                (
                    axum::http::StatusCode::CREATED,
                    [(axum::http::header::LOCATION, format!("{path}/{id}"))],
                    "",
                )
                    .into_response()
            }
            ("GET", ["users", id]) => match d.users.get(&realm).and_then(|m| m.get(*id)) {
                Some(u) => axum::Json(u.clone()).into_response(),
                None => axum::http::StatusCode::NOT_FOUND.into_response(),
            },
            ("PUT", ["users", id]) => {
                let Some(stored) = d.users.get_mut(&realm).and_then(|m| m.get_mut(*id)) else {
                    return axum::http::StatusCode::NOT_FOUND.into_response();
                };
                // Full replace: keep only the id and what the body carries.
                let mut rep: serde_json::Value = serde_json::from_str(&body_text).unwrap();
                rep["id"] = stored["id"].clone();
                rep["createdTimestamp"] = stored["createdTimestamp"].clone();
                *stored = rep;
                axum::http::StatusCode::NO_CONTENT.into_response()
            }
            ("GET", ["groups"]) => {
                let name = param("search").unwrap_or_default();
                let found: Vec<serde_json::Value> = d
                    .groups
                    .get(&realm)
                    .and_then(|g| g.get(&name))
                    .map(|gid| vec![serde_json::json!({"id": gid, "name": name})])
                    .unwrap_or_default();
                axum::Json(found).into_response()
            }
            ("POST", ["groups"]) => {
                let rep: serde_json::Value = serde_json::from_str(&body_text).unwrap();
                let name = rep["name"].as_str().unwrap_or_default().to_string();
                let groups = d.groups.entry(realm.clone()).or_default();
                let gid = format!("group-{}", groups.len() + 1);
                groups.insert(name, gid.clone());
                (
                    axum::http::StatusCode::CREATED,
                    [(axum::http::header::LOCATION, format!("{path}/{gid}"))],
                    "",
                )
                    .into_response()
            }
            ("PUT", ["users", id, "groups", gid]) => {
                let member = (realm.clone(), id.to_string(), gid.to_string());
                if !d.group_members.contains(&member) {
                    d.group_members.push(member);
                }
                axum::http::StatusCode::NO_CONTENT.into_response()
            }
            ("POST", ["roles"]) => {
                let rep: serde_json::Value = serde_json::from_str(&body_text).unwrap();
                let role = (
                    realm.clone(),
                    rep["name"].as_str().unwrap_or_default().to_string(),
                );
                if d.realm_roles.contains(&role) {
                    return axum::http::StatusCode::CONFLICT.into_response();
                }
                d.realm_roles.push(role);
                axum::http::StatusCode::CREATED.into_response()
            }
            ("GET", ["identity-provider", "instances"]) => {
                let all: Vec<serde_json::Value> = d
                    .idps
                    .get(&realm)
                    .map(|m| m.values().cloned().collect())
                    .unwrap_or_default();
                axum::Json(all).into_response()
            }
            ("POST", ["identity-provider", "instances"]) => {
                let rep: serde_json::Value = serde_json::from_str(&body_text).unwrap();
                let alias = rep["alias"].as_str().unwrap_or_default().to_string();
                d.idps.entry(realm.clone()).or_default().insert(alias, rep);
                axum::http::StatusCode::CREATED.into_response()
            }
            ("PUT", ["identity-provider", "instances", alias]) => {
                // Like Keycloak: the representation is replaced whole.
                let Some(stored) = d.idps.get_mut(&realm).and_then(|m| m.get_mut(*alias)) else {
                    return axum::http::StatusCode::NOT_FOUND.into_response();
                };
                *stored = serde_json::from_str(&body_text).unwrap();
                axum::http::StatusCode::NO_CONTENT.into_response()
            }
            _ => axum::http::StatusCode::NOT_FOUND.into_response(),
        }
    }

    pub(crate) async fn serve_keycloak_double(double: SharedDouble) -> KeycloakAdminClient {
        let app = axum::Router::new()
            .fallback(keycloak_double)
            .with_state(double);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        KeycloakAdminClient::new(KeycloakAdminConfig {
            host: format!("http://{addr}"),
            admin_username: "admin".to_string(),
            admin_password: crate::domain::secrets::SensitiveString::new("admin-password"),
        })
    }

    /// The Keycloak realm a team of this tier invites into.
    fn invite_realm(tier: &TenantTier) -> &'static str {
        match tier {
            TenantTier::Enterprise => "team-acme",
            _ => "zaru-consumer",
        }
    }

    /// A user who already exists, with the attributes the platform keeps on
    /// every consumer: the tenant, the tier and the team memberships.
    pub(crate) fn existing_user() -> serde_json::Value {
        serde_json::json!({
            "id": "user-9",
            "username": "invitee@example.com",
            "email": "invitee@example.com",
            "emailVerified": true,
            "firstName": "Ada",
            "lastName": "Lovelace",
            "enabled": true,
            "createdTimestamp": 0,
            "attributes": {
                "tenant_id": ["u-0123456789abcdef0123456789abcdef"],
                "zaru_tier": ["business"],
                "team_memberships": ["t-11111111-2222-3333-4444-555555555555"]
            }
        })
    }

    /// Inviting a person who already has a Keycloak user leaves every
    /// attribute that user had exactly as it was, whatever the team's tier.
    #[tokio::test]
    async fn inviting_an_existing_user_keeps_the_users_attributes() {
        for tier in [TenantTier::Business, TenantTier::Enterprise] {
            let realm = invite_realm(&tier);
            let double = SharedDouble::default();
            let before = existing_user();
            double
                .lock()
                .unwrap()
                .users
                .entry(realm.to_string())
                .or_default()
                .insert("user-9".to_string(), before.clone());
            let kc = serve_keycloak_double(double.clone()).await;

            let user_id = kc
                .invite_team_user(tier.clone(), "acme", "invitee@example.com")
                .await
                .expect("the invitation reaches Keycloak");
            assert_eq!(user_id, "user-9", "the existing user was not reused");

            let d = double.lock().unwrap();
            let after = &d.users[realm]["user-9"];
            for key in ["tenant_id", "zaru_tier", "team_memberships"] {
                assert_eq!(
                    after["attributes"][key], before["attributes"][key],
                    "inviting an existing {tier:?} user changed its {key} attribute: before {}, after {}",
                    before["attributes"], after["attributes"]
                );
            }
            for field in ["email", "firstName", "lastName"] {
                assert_eq!(
                    after[field], before[field],
                    "inviting an existing {tier:?} user changed its {field}"
                );
            }
        }
    }

    /// An invitation writes no user attribute: nothing reads one. An
    /// existing user is not rewritten at all, and a new user is created
    /// without attributes.
    #[tokio::test]
    async fn an_invitation_writes_no_user_attribute() {
        for tier in [TenantTier::Business, TenantTier::Enterprise] {
            let realm = invite_realm(&tier);
            for existing in [true, false] {
                let double = SharedDouble::default();
                if existing {
                    double
                        .lock()
                        .unwrap()
                        .users
                        .entry(realm.to_string())
                        .or_default()
                        .insert("user-9".to_string(), existing_user());
                }
                let kc = serve_keycloak_double(double.clone()).await;
                let user_id = kc
                    .invite_team_user(tier.clone(), "acme", "invitee@example.com")
                    .await
                    .expect("the invitation reaches Keycloak");

                let d = double.lock().unwrap();
                let user_path = format!("/admin/realms/{realm}/users/{user_id}");
                let rewrites: Vec<_> = d
                    .calls
                    .iter()
                    .filter(|(m, p, _)| m == "PUT" && *p == user_path)
                    .collect();
                assert!(
                    rewrites.is_empty(),
                    "inviting a {tier:?} user rewrote the user: {rewrites:?}"
                );
                if !existing {
                    let created = &d.users[realm][&user_id];
                    assert!(
                        created.get("attributes").is_none(),
                        "inviting a new {tier:?} user wrote attributes: {created}"
                    );
                    assert_eq!(created["email"], "invitee@example.com");
                }
                if matches!(tier, TenantTier::Business) {
                    assert!(
                        d.group_members
                            .iter()
                            .any(|(r, u, _)| r == realm && *u == user_id),
                        "the {tier:?} invitee was not put in the team's group: {:?}",
                        d.calls
                    );
                }
            }
        }
    }

    /// Two invitations for a person with no Keycloak user can both search,
    /// find nobody, and both create one. Keycloak refuses the second create
    /// with 409. The second invitation then uses the user the first one
    /// created, leaves its attributes alone, and still puts it in its team.
    #[tokio::test]
    async fn an_invitation_that_loses_the_create_race_uses_the_user_that_won() {
        let double = SharedDouble::default();
        {
            let mut d = double.lock().unwrap();
            // The first invitation created this user after the second
            // invitation's search found nobody.
            d.users
                .entry("zaru-consumer".to_string())
                .or_default()
                .insert("user-9".to_string(), existing_user());
            d.stale_searches = 1;
        }
        let kc = serve_keycloak_double(double.clone()).await;

        let user_id = kc
            .invite_team_user(TenantTier::Business, "acme", "invitee@example.com")
            .await
            .expect("an invitation whose create is refused with 409 still invites the user");

        let d = double.lock().unwrap();
        assert_eq!(
            user_id, "user-9",
            "the invitation did not use the existing user"
        );
        assert_eq!(d.users["zaru-consumer"].len(), 1);
        assert_eq!(
            d.users["zaru-consumer"]["user-9"]["attributes"],
            existing_user()["attributes"],
            "the invitation changed the existing user's attributes"
        );
        assert!(
            d.group_members
                .iter()
                .any(|(r, u, _)| r == "zaru-consumer" && u == "user-9"),
            "the invitee was not put in the team's group: {:?}",
            d.calls
        );
    }

    // ── SAML identity provider ─────────────────────────────────────────────

    /// A self-signed certificate made for these tests; it signs nothing.
    pub(crate) const TEST_IDP_CERTIFICATE_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIDJzCCAg+gAwIBAgIUANWjdiTFGbUnyYlLX4fymSLj8fowDQYJKoZIhvcNAQEL\nBQAwIzEhMB8GA1UEAwwYc2FtbC1pZHAuZXhhbXBsZS5pbnZhbGlkMB4XDTI2MDky\nODE3NDM0OVoXDTM2MDkyNTE3NDM0OVowIzEhMB8GA1UEAwwYc2FtbC1pZHAuZXhh\nbXBsZS5pbnZhbGlkMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAxAPv\nOG12dD2iDGxl0e0VZk1c41pJUFS0pdl9l2LmrK6qiM+fIPO0JfT/rlRFLNgEKKoN\n/0QTnS9bTJLCn+XSQmWWdjvjSi+uDwcl7I/MiMAGqJf1aXgEB3P1YMehdjf3OmLL\nwvXvAjk2raB4NIazQo9LdjhkDf1riiAnmJG37fjh5g2ZZ4NeMA2jKNWilCVIKQIL\nn901al87VFzfP7R+eNP8GGy5/DC2/xHo8NSFT28n0xW3D0M4tNKR3ychl5gKQ3FE\n2BWJekb8Qql9JlJrdQopE13F/Rx9CqGjt+Y5Mec/W45OnokvEKKs+vISWG1iozus\nhWlYuoGUog8h/o9GuQIDAQABo1MwUTAdBgNVHQ4EFgQUFH/7aTCr3O5HcDHu4Hwc\nDcugVlgwHwYDVR0jBBgwFoAUFH/7aTCr3O5HcDHu4HwcDcugVlgwDwYDVR0TAQH/\nBAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAC0MUQ9kAjCxyLFZSbZ7O0RHcdUgV\nbMzVT34EXFk3gRPF1qZHXzmKU1y/2QUCLwP3G0UWidfhLeGHZHq+Tqg0Ixu1Zywz\nJqo/MbGL1+JvLu0KZkht6+Bn4Zwj7gWQIWAldp4NPrKNBxzy18HiImzu3G/ri+lz\naezC27mUZEM8koc5p7A16HaOoRANLs4AL8sf5oCK/MWFYMR6WO9LEbNB70jB+wwW\nKhwNZtF5OdATAEesuwCYeaNt4GWig6bGaKqOfnibzFCh5AX8Z7IyPshZaeMgjq7F\n5RLtBusi4q8KxZldB33DR7F8vX6qxyk6X8mNfUWvkpdD4CvCJLqbUEsa6Q==\n-----END CERTIFICATE-----";

    pub(crate) fn saml_config(certificate: &str) -> SamlIdpConfig {
        SamlIdpConfig {
            entity_id: "https://idp.example.invalid/metadata".to_string(),
            sso_url: "https://idp.example.invalid/sso".to_string(),
            certificate: certificate.to_string(),
        }
    }

    /// Every setting in a stored SAML provider that would weaken a check,
    /// as (setting, value found). Empty when the provider validates
    /// signatures and trusts no email it is sent.
    pub(crate) fn weakened_checks(idp: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
        let mut found = Vec::new();
        let config = &idp["config"];
        if config["validateSignature"] != serde_json::json!("true") {
            found.push((
                "validateSignature".to_string(),
                config["validateSignature"].clone(),
            ));
        }
        if idp["trustEmail"] == serde_json::json!(true) {
            found.push(("trustEmail".to_string(), idp["trustEmail"].clone()));
        }
        found
    }

    /// A team's SAML provider is written so that Keycloak checks the
    /// signature of every response against the provider's certificate,
    /// whether the provider is created or an existing one is replaced, and
    /// with the certificate as PEM or as bare base64.
    #[tokio::test]
    async fn saml_provider_is_written_to_validate_signatures() {
        let bare: String = TEST_IDP_CERTIFICATE_PEM
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        for certificate in [TEST_IDP_CERTIFICATE_PEM.to_string(), bare] {
            let double = SharedDouble::default();
            let kc = serve_keycloak_double(double.clone()).await;
            for write in ["create", "replace"] {
                kc.set_idp_config("team-acme", &saml_config(&certificate))
                    .await
                    .unwrap_or_else(|e| panic!("the SAML provider {write} was refused: {e}"));
                let d = double.lock().unwrap();
                let idp = &d.idps["team-acme"]["saml"];
                assert!(
                    weakened_checks(idp).is_empty(),
                    "the SAML provider ({write}) was written with a check turned off: {:?}",
                    weakened_checks(idp)
                );
                assert_eq!(
                    idp["config"]["signingCertificate"],
                    serde_json::json!(certificate)
                );
            }
        }
    }

    /// A SAML provider with no signing certificate, or with something that
    /// is not a certificate, is refused with a plain error, and nothing is
    /// sent to Keycloak.
    #[tokio::test]
    async fn saml_provider_without_a_signing_certificate_is_refused() {
        for certificate in ["", "   ", "not a certificate", "aGVsbG8gd29ybGQ="] {
            let double = SharedDouble::default();
            let kc = serve_keycloak_double(double.clone()).await;
            let result = kc
                .set_idp_config("team-acme", &saml_config(certificate))
                .await;
            let d = double.lock().unwrap();
            assert!(
                matches!(result, Err(KeycloakAdminError::InvalidIdpConfig(_))),
                "a SAML provider with the certificate {certificate:?} was accepted: {result:?}; Keycloak holds {:?}",
                d.idps.get("team-acme")
            );
            assert!(
                d.idps.is_empty()
                    && !d
                        .calls
                        .iter()
                        .any(|(_, p, _)| p.contains("identity-provider")),
                "a refused SAML provider reached Keycloak: {:?}",
                d.calls
            );
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("signing certificate"),
                "the refusal does not say what to fix: {message}"
            );
        }
    }

    /// An invitation to an address with a plus sign finds the user who
    /// already has it: the address reaches Keycloak's search as the
    /// address, not with the plus read as a space.
    #[tokio::test]
    async fn an_existing_user_whose_address_has_a_plus_sign_is_found() {
        for email in ["ada+team@example.com", "ada&b=c@example.com"] {
            let double = SharedDouble::default();
            let mut user = existing_user();
            user["email"] = serde_json::json!(email);
            user["username"] = serde_json::json!(email);
            double
                .lock()
                .unwrap()
                .users
                .entry("zaru-consumer".to_string())
                .or_default()
                .insert("user-9".to_string(), user);
            let kc = serve_keycloak_double(double.clone()).await;

            let found = kc
                .find_user_by_email("zaru-consumer", email)
                .await
                .expect("the search reaches Keycloak");
            assert_eq!(
                found.map(|u| u.id),
                Some("user-9".to_string()),
                "the user with the address {email:?} was not found"
            );
            let user_id = kc
                .invite_team_user(TenantTier::Business, "acme", email)
                .await
                .unwrap_or_else(|e| panic!("inviting {email:?} failed: {e}"));
            assert_eq!(
                user_id, "user-9",
                "inviting {email:?} did not use the existing user"
            );
        }
    }

    // ── writes to one user ─────────────────────────────────────────────────

    /// Two writes to one user at once, each of a different attribute, both
    /// land. Keycloak's `PUT /users/{id}` replaces the user whole and has no
    /// version check, so without writes being made one at a time the second
    /// PUT carries the user as it was before the first and drops what the
    /// first wrote.
    #[tokio::test]
    async fn two_writes_to_one_user_at_once_keep_both() {
        let double = SharedDouble::default();
        {
            let mut d = double.lock().unwrap();
            d.get_user_delay = Some(std::time::Duration::from_millis(300));
            d.users
                .entry("zaru-consumer".to_string())
                .or_default()
                .insert("user-9".to_string(), existing_user());
        }
        let kc = std::sync::Arc::new(serve_keycloak_double(double.clone()).await);
        let tier = {
            let kc = kc.clone();
            tokio::spawn(async move {
                kc.set_user_attribute("zaru-consumer", "user-9", "zaru_tier", "enterprise")
                    .await
            })
        };
        let memberships = {
            let kc = kc.clone();
            tokio::spawn(async move {
                kc.set_user_team_memberships(
                    "zaru-consumer",
                    "user-9",
                    &["t-99999999-8888-7777-6666-555555555555".to_string()],
                )
                .await
            })
        };
        tier.await.unwrap().expect("the tier write succeeds");
        memberships
            .await
            .unwrap()
            .expect("the memberships write succeeds");

        let user = double.lock().unwrap().users["zaru-consumer"]["user-9"].clone();
        let attributes = &user["attributes"];
        let lost: Vec<&str> = [
            ("zaru_tier", serde_json::json!(["enterprise"])),
            (
                "team_memberships",
                serde_json::json!(["t-99999999-8888-7777-6666-555555555555"]),
            ),
            (
                "tenant_id",
                serde_json::json!(["u-0123456789abcdef0123456789abcdef"]),
            ),
        ]
        .iter()
        .filter(|(name, value)| attributes.get(*name) != Some(value))
        .map(|(name, _)| *name)
        .collect();
        assert!(
            lost.is_empty(),
            "two writes to one user at once lost {lost:?}: the user holds {attributes}"
        );
    }

    /// A write that waits longer than the client allows for another write
    /// to the same user fails with a sentence that says so, and writes
    /// nothing. A write to another user does not wait.
    #[tokio::test]
    async fn a_write_that_waits_too_long_for_another_fails_plainly() {
        let double = SharedDouble::default();
        {
            let mut d = double.lock().unwrap();
            d.get_user_delay = Some(std::time::Duration::from_millis(600));
            let users = d.users.entry("zaru-consumer".to_string()).or_default();
            users.insert("user-9".to_string(), existing_user());
            let mut other = existing_user();
            other["id"] = serde_json::json!("user-8");
            users.insert("user-8".to_string(), other);
        }
        let kc = std::sync::Arc::new(
            serve_keycloak_double(double.clone())
                .await
                .with_user_write_wait(std::time::Duration::from_millis(100)),
        );
        let first = {
            let kc = kc.clone();
            tokio::spawn(async move {
                kc.set_user_attribute("zaru-consumer", "user-9", "zaru_tier", "pro")
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = kc
            .set_user_attribute("zaru-consumer", "user-9", "zaru_tier", "enterprise")
            .await;
        let message = match second {
            Err(e @ KeycloakAdminError::UserWriteBusy { .. }) => e.to_string(),
            other => panic!("a write that waited too long did not fail plainly: {other:?}"),
        };
        assert!(
            message.contains("user-9") && message.contains("try again"),
            "the refusal does not say what happened: {message}"
        );
        kc.set_user_attribute("zaru-consumer", "user-8", "zaru_tier", "business")
            .await
            .expect("a write to another user does not wait");
        first.await.unwrap().expect("the first write succeeds");
        let d = double.lock().unwrap();
        assert_eq!(
            d.users["zaru-consumer"]["user-9"]["attributes"]["zaru_tier"],
            serde_json::json!(["pro"]),
            "the refused write changed the user"
        );
    }

    /// A write of an attribute changes that attribute and nothing else: a
    /// user an administrator disabled stays disabled.
    #[tokio::test]
    async fn an_attribute_write_leaves_a_disabled_user_disabled() {
        let double = SharedDouble::default();
        let mut user = existing_user();
        user["enabled"] = serde_json::json!(false);
        double
            .lock()
            .unwrap()
            .users
            .entry("zaru-consumer".to_string())
            .or_default()
            .insert("user-9".to_string(), user);
        let kc = serve_keycloak_double(double.clone()).await;
        kc.set_user_attribute("zaru-consumer", "user-9", "zaru_tier", "pro")
            .await
            .expect("the write succeeds");
        kc.set_user_team_memberships("zaru-consumer", "user-9", &[])
            .await
            .expect("the write succeeds");
        let user = double.lock().unwrap().users["zaru-consumer"]["user-9"].clone();
        assert_eq!(
            user["enabled"],
            serde_json::json!(false),
            "an attribute write enabled a disabled user: {user}"
        );
    }

    // ── realms the orchestrator creates ────────────────────────────────────

    /// What a realm the orchestrator creates must hold: the protections of
    /// the `zaru-consumer` realm, and a safe value where the consumer realm
    /// sets none or where an Enterprise realm needs its own.
    fn expected_realm_protections() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("enabled", serde_json::json!(true)),
            ("verifyEmail", serde_json::json!(true)),
            ("duplicateEmailsAllowed", serde_json::json!(false)),
            ("loginWithEmailAllowed", serde_json::json!(true)),
            ("registrationEmailAsUsername", serde_json::json!(true)),
            ("resetPasswordAllowed", serde_json::json!(true)),
            ("registrationAllowed", serde_json::json!(false)),
            ("editUsernameAllowed", serde_json::json!(false)),
            ("sslRequired", serde_json::json!("external")),
            ("bruteForceProtected", serde_json::json!(true)),
            ("permanentLockout", serde_json::json!(false)),
            ("failureFactor", serde_json::json!(30)),
            ("waitIncrementSeconds", serde_json::json!(60)),
            ("maxFailureWaitSeconds", serde_json::json!(900)),
            ("maxDeltaTimeSeconds", serde_json::json!(43200)),
            ("minimumQuickLoginWaitSeconds", serde_json::json!(60)),
            ("quickLoginCheckMilliSeconds", serde_json::json!(1000)),
            (
                "passwordPolicy",
                serde_json::json!("length(12) and maxLength(128) and notUsername and notEmail"),
            ),
            ("accessTokenLifespan", serde_json::json!(1800)),
            ("ssoSessionIdleTimeout", serde_json::json!(259200)),
            ("ssoSessionMaxLifespan", serde_json::json!(1209600)),
        ]
    }

    /// Every realm the orchestrator creates, an Enterprise tenant's or an
    /// Enterprise team's, is created with its protections on, not with
    /// Keycloak's defaults (brute-force protection and email verification
    /// off).
    #[tokio::test]
    async fn created_realms_have_the_consumer_realms_protections() {
        let double = SharedDouble::default();
        let kc = serve_keycloak_double(double.clone()).await;
        kc.create_team_realm("acme")
            .await
            .expect("the team realm is created");
        kc.create_realm("tenant-acme")
            .await
            .expect("the tenant realm is created");

        let realms = double.lock().unwrap().realms.clone();
        for name in ["team-acme", "tenant-acme"] {
            let rep = realms
                .get(name)
                .unwrap_or_else(|| panic!("the realm {name} was not created"));
            let wrong: Vec<String> = expected_realm_protections()
                .into_iter()
                .filter(|(key, value)| rep.get(*key) != Some(value))
                .map(|(key, value)| format!("{key} (wanted {value}, sent {})", rep[key]))
                .collect();
            assert!(
                wrong.is_empty(),
                "the realm {name} was created without these protections: {wrong:?}"
            );
        }
    }

    /// A realm that exists already is left as it is: creating it again
    /// sends nothing that changes it.
    #[tokio::test]
    async fn creating_a_realm_that_exists_changes_nothing() {
        let double = SharedDouble::default();
        double.lock().unwrap().realms.insert(
            "team-acme".to_string(),
            serde_json::json!({"realm": "team-acme"}),
        );
        let kc = serve_keycloak_double(double.clone()).await;
        kc.create_team_realm("acme")
            .await
            .expect("an existing realm is not an error");
        let d = double.lock().unwrap();
        assert_eq!(
            d.realms["team-acme"],
            serde_json::json!({"realm": "team-acme"})
        );
        assert!(
            !d.calls
                .iter()
                .any(|(method, path, _)| method == "PUT" && path == "/admin/realms/team-acme"),
            "an existing realm was rewritten: {:?}",
            d.calls
        );
    }

    // ── build_set_attribute_body ───────────────────────────────────────────

    /// The PUT body must include `id`, `email`, `firstName`, `lastName`,
    /// `enabled` as it was read, and the merged attributes map.
    /// `createdTimestamp` must be absent (Keycloak rejects it on PUT).
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
            enabled: Some(true),
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
            enabled: None,
            attributes: None,
        };

        let body = build_set_attribute_body(&user, "zaru_tier", "business");

        assert_eq!(body["id"], "user-456");
        assert!(
            body.get("enabled").is_none(),
            "a user read with no `enabled` is written with none"
        );
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
            enabled: None,
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
            enabled: None,
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
            enabled: None,
            attributes: Some(attrs),
        };

        let body = build_set_attribute_body(&user, "zaru_tier", "business");

        assert_eq!(body["attributes"]["zaru_tier"][0], "business");
    }
}
