// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Credential Binding Domain (BC-11 Secrets & Identity, ADR-078)
//!
//! Domain model for user-owned third-party credential bindings. A
//! [`UserCredentialBinding`] is the aggregate root that associates a user's
//! external credential (e.g. an OpenAI API key, an OAuth2 token for GitHub)
//! with a secret stored in OpenBao and governs which agents and workflows are
//! permitted to use it via [`CredentialGrant`] entities.
//!
//! ## Type Map
//!
//! | Type | Role |
//! |------|------|
//! | [`CredentialBindingId`] | UUID newtype — aggregate root identity |
//! | [`CredentialGrantId`] | UUID newtype — grant entity identity |
//! | [`CredentialType`] | Credential protocol / mechanism enum |
//! | [`CredentialProvider`] | Third-party service identity |
//! | [`CredentialScope`] | Personal vs team ownership |
//! | [`CredentialStatus`] | Lifecycle state machine |
//! | [`GrantTarget`] | What is permitted to use this credential |
//! | [`CredentialMetadata`] | Supplemental user-visible metadata |
//! | [`CredentialGrant`] | Entity recording a single access grant |
//! | [`UserCredentialBinding`] | Aggregate root |
//! | [`CredentialBindingRepository`] | Repository trait (infrastructure impl in BC-11) |

use crate::domain::secrets::SecretPath;
use crate::domain::shared_kernel::{AgentId, TenantId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ============================================================================
// OAuth Pending State
// ============================================================================

/// Transient row tracking an in-flight OAuth2 PKCE authorisation flow.
///
/// Stored in the `oauth_pending_states` table until the callback arrives or
/// the row is expired by [`CredentialBindingRepository::delete_expired_oauth_states`].
#[derive(Debug, Clone)]
pub struct OAuthPendingState {
    /// The opaque CSRF / state token passed through the provider redirect.
    pub state: String,
    /// The binding that was created in `PendingOAuth` status.
    pub binding_id: CredentialBindingId,
    /// PKCE code verifier (held server-side; the challenge was sent to the provider).
    pub pkce_verifier: String,
    /// The redirect URI registered with the provider for this flow.
    pub redirect_uri: String,
    /// When the pending state was inserted (used to expire stale flows).
    pub created_at: DateTime<Utc>,
}

// ============================================================================
// Value Objects — Identities
// ============================================================================

/// Unique identifier for a [`UserCredentialBinding`] aggregate root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialBindingId(pub Uuid);

impl CredentialBindingId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CredentialBindingId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CredentialBindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a [`CredentialGrant`] entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialGrantId(pub Uuid);

impl CredentialGrantId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for CredentialGrantId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for CredentialGrantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Value Objects — Classification
// ============================================================================

/// The protocol or mechanism used by a bound credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    /// Sensitive value — masked after creation (covers API keys, static tokens, passwords).
    Secret,
    /// Non-sensitive configuration value — readable after creation (env vars, URLs, config).
    Variable,
    /// OAuth 2.0 token (access + optional refresh).
    OAuth2,
    /// Machine identity / service account credential (JSON blob).
    ServiceAccount,
    /// A mailbox connection (AEGIS ADR-125 D1): SMTP with IMAP (provider
    /// `imap`, the settings in [`CredentialMetadata::mailbox`] and the field
    /// `password` in OpenBao). A row stored before ADR-125's Update of
    /// 2026-10-04 may also carry an OAuth token in this type; it reads back
    /// as it was stored. A new OAuth binding is of type `oauth2`.
    Mailbox,
}

/// The external service or platform this credential authenticates with: an
/// opaque name (AEGIS ADR-125, Update of 2026-10-04, clause 1). An OAuth
/// binding's provider is the `provider` string of its `spec.oauth_providers`
/// entry; an API key's is the name its user gave. No provider name is known
/// to the code, except [`CredentialProvider::imap`], the settings form of a
/// standard protocol (a `mailbox` binding's SMTP-with-IMAP form).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialProvider {
    /// The provider's name: not a secret.
    name: String,
}

impl CredentialProvider {
    /// The provider of an SMTP-with-IMAP mailbox (ADR-125 D1, kept by its
    /// Update of 2026-10-04 as a protocol, not a product).
    pub const IMAP: &'static str = "imap";

    /// The provider of this name, as given.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// The provider of an SMTP-with-IMAP mailbox.
    pub fn imap() -> Self {
        Self::new(Self::IMAP)
    }

    /// The provider's name.
    pub fn as_str(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for CredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// How a mail session is secured (ADR-125 D1): TLS from the first byte
/// (`tls`, IMAP 993 and SMTP 465 by convention) or a plaintext greeting
/// upgraded by `STARTTLS` before any credential is sent (`starttls`, IMAP
/// 143 and SMTP 587). There is no unencrypted form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailSecurity {
    Tls,
    Starttls,
}

/// The non-secret settings of an SMTP-with-IMAP mailbox (ADR-125 D1's
/// table). The password is never here: it lives in OpenBao under the field
/// `password` at the binding's secret path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxSettings {
    /// The mailbox's address, the `From` of what it sends.
    pub address: String,
    /// The name shown beside the address, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: MailSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: MailSecurity,
    /// The login name both servers accept.
    pub username: String,
}

impl MailboxSettings {
    /// Refuse settings no session could use, before any connection is made.
    pub fn validate(&self) -> Result<(), String> {
        let address = self.address.trim();
        if address.is_empty() || !address.contains('@') {
            return Err("address must be an email address".to_string());
        }
        for (name, host) in [
            ("imap_host", &self.imap_host),
            ("smtp_host", &self.smtp_host),
        ] {
            if host.trim().is_empty() || host.chars().any(|c| c.is_whitespace() || c == '/') {
                return Err(format!("{name} must be a host name"));
            }
        }
        if self.imap_port == 0 || self.smtp_port == 0 {
            return Err("imap_port and smtp_port must be ports".to_string());
        }
        if self.username.is_empty() || self.username.contains(['\r', '\n', '\0']) {
            return Err("username must be one line".to_string());
        }
        Ok(())
    }
}

/// The non-secret settings of a calendar account reached over CalDAV (AEGIS
/// ADR-138 K4), stored in `credential_bindings.calendar_settings` (migration
/// 047). No secret is here: an OAuth binding's token lives in OpenBao at the
/// binding's secret path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarSettings {
    /// The CalDAV service root, an absolute URL ending in `/`; every other
    /// URL of the account is resolved against it and must keep its origin.
    pub server: String,
    /// The account's principal as a URI reference resolved against
    /// `server`: `<address>/user` for a Google calendar.
    pub principal: String,
    /// The account's email address.
    pub address: String,
}

/// Ownership scope of a credential binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope {
    /// Owned by a single user; only their agents/workflows may be granted access.
    Personal,
    /// Owned by a team; any member's agents/workflows may be granted access.
    Team { team_id: Uuid },
}

/// Lifecycle state of a [`UserCredentialBinding`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
    /// Credential is valid and all active grants are honoured.
    Active,
    /// Credential was explicitly revoked; no grants are honoured.
    Revoked,
    /// The credential's underlying token has passed its expiry date.
    Expired,
    /// OAuth2 flow initiated but not yet completed (no token stored).
    PendingOAuth,
    /// Row migrated from the legacy `user_provider_keys` table; requires
    /// re-validation before use.
    PendingMigration,
}

/// The agent, workflow, or wildcard target that has been granted access to a
/// credential binding via a [`CredentialGrant`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GrantTarget {
    /// A specific named agent is granted access.
    Agent { agent_id: AgentId },
    /// A specific workflow is granted access.
    Workflow { workflow_id: Uuid },
    /// All agents owned by the binding's owner are granted access.
    AllAgents,
}

// ============================================================================
// Value Object — Metadata
// ============================================================================

/// Supplemental, user-visible metadata attached to a [`UserCredentialBinding`].
///
/// None of these fields affect the security boundary; they exist for display
/// and discovery purposes only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialMetadata {
    /// Human-readable label chosen by the user (e.g. `"My OpenAI key"`).
    pub label: String,
    /// Arbitrary key/value tags for filtering and organisation.
    pub tags: Option<serde_json::Value>,
    /// Base URL of the target service when non-standard (e.g. a self-hosted
    /// GitLab instance).
    pub service_url: Option<String>,
    /// External account or user ID on the third-party platform, if known.
    pub external_account_id: Option<String>,
    /// OAuth2 scopes requested during the authorisation flow.
    pub oauth_scopes: Option<Vec<String>>,
    /// The non-secret settings of an `imap` mailbox (ADR-125 D1); `None`
    /// for every other binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<MailboxSettings>,
    /// What the binding's token reaches on the remote tool server its
    /// provider names, read from that server's grounding of the token, never
    /// typed by a person (AEGIS ADR-132 (7a) S2). `None` for every binding
    /// whose provider is not a remote server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<BindingReach>,
    /// The CalDAV settings of a calendar account (AEGIS ADR-138 K4): set by
    /// the OAuth callback on a binding granted the calendar scope; `None`
    /// for every other binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<CalendarSettings>,
}

/// What kind of reach a remote server's grounding reports for a token:
/// one instance or several (AEGIS ADR-132 (7a) S2, read from the instances
/// a `cortex.ground` reports), or the account the token belongs to (AEGIS
/// ADR-136 G14b, read from the `login` any other grounding tool reports).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachKind {
    /// The grounding reports several instances.
    Apex,
    /// The grounding reports exactly one instance.
    Instance,
    /// The grounding reports the account the token belongs to, by `login`.
    Account,
}

/// What a binding's token reaches, as the remote server's grounding of it
/// reported at `grounded_at` (AEGIS ADR-132 (7a) S2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingReach {
    pub kind: ReachKind,
    /// The one instance's slug; `None` for `apex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_slug: Option<String>,
    /// The one instance's id; `None` for `apex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Reserved for a workspace-scoped token, once the grounding says so;
    /// never written today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The account's login for `account`; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// When the server grounded the token.
    pub grounded_at: DateTime<Utc>,
}

// ============================================================================
// Entity — CredentialGrant
// ============================================================================

/// Records that a specific [`GrantTarget`] has been granted access to use the
/// parent [`UserCredentialBinding`].
///
/// Grants are stored inside the aggregate's `grants` collection and enforced
/// at credential-injection time by the application service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialGrant {
    pub id: CredentialGrantId,
    pub binding_id: CredentialBindingId,
    pub target: GrantTarget,
    pub granted_at: DateTime<Utc>,
    /// Identity of the actor who created this grant (user sub or service account id).
    pub granted_by: String,
}

// ============================================================================
// Aggregate Root — UserCredentialBinding
// ============================================================================

/// Aggregate root for a user-owned external credential binding (ADR-078).
///
/// Owns the [`SecretPath`] pointer into OpenBao where the raw credential value
/// is stored and governs which agents/workflows may access it via the
/// [`grants`](UserCredentialBinding::grants) collection.
///
/// ## Invariants
///
/// - `status == Revoked` → `grants` is empty (enforced by [`revoke`](Self::revoke))
/// - `status != Active` → all grant lookups return empty (enforced by
///   [`active_grants_for`](Self::active_grants_for))
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCredentialBinding {
    pub id: CredentialBindingId,
    /// Keycloak `sub` claim of the owning user.
    pub owner_user_id: String,
    pub tenant_id: TenantId,
    pub credential_type: CredentialType,
    pub provider: CredentialProvider,
    /// Structured pointer to the secret value in OpenBao (ADR-034).
    pub secret_path: SecretPath,
    pub scope: CredentialScope,
    pub status: CredentialStatus,
    pub metadata: CredentialMetadata,
    pub grants: Vec<CredentialGrant>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl UserCredentialBinding {
    /// Revoke this binding, clearing all existing grants.
    ///
    /// Sets `status = Revoked` and empties the `grants` collection. Once revoked
    /// the binding cannot be re-activated — a new binding must be created.
    pub fn revoke(&mut self) {
        self.status = CredentialStatus::Revoked;
        self.grants.clear();
        self.updated_at = Utc::now();
    }

    /// Grant `target` access to this credential.
    ///
    /// Creates a new [`CredentialGrant`], appends it to [`grants`](Self::grants),
    /// and returns the new grant's id. The grant is recorded regardless of the
    /// current `status`; call sites should check `status == Active` before
    /// creating grants if that invariant is required.
    pub fn add_grant(&mut self, target: GrantTarget, granted_by: String) -> CredentialGrantId {
        let grant_id = CredentialGrantId::new();
        self.grants.push(CredentialGrant {
            id: grant_id,
            binding_id: self.id,
            target,
            granted_at: Utc::now(),
            granted_by,
        });
        self.updated_at = Utc::now();
        grant_id
    }

    /// Revoke a specific grant by id.
    ///
    /// Returns `true` if the grant was found and removed, `false` if no grant
    /// with the given id existed.
    pub fn revoke_grant(&mut self, grant_id: &CredentialGrantId) -> bool {
        let before = self.grants.len();
        self.grants.retain(|g| &g.id != grant_id);
        let removed = self.grants.len() < before;
        if removed {
            self.updated_at = Utc::now();
        }
        removed
    }

    /// Return all grants matching `target` that are currently active.
    ///
    /// Returns an empty slice if `status != Active`.
    pub fn active_grants_for<'a>(&'a self, target: &GrantTarget) -> Vec<&'a CredentialGrant> {
        if self.status != CredentialStatus::Active {
            return Vec::new();
        }
        self.grants.iter().filter(|g| &g.target == target).collect()
    }
}

// ============================================================================
// Repository Trait
// ============================================================================

/// Persistence interface for the [`UserCredentialBinding`] aggregate root.
///
/// Implemented by the infrastructure layer (PostgreSQL). All queries are
/// tenant-scoped to enforce the multi-tenant data isolation boundary.
#[async_trait]
pub trait CredentialBindingRepository: Send + Sync {
    /// Upsert a [`UserCredentialBinding`] (insert or update by primary key).
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()>;

    /// Load a binding by its aggregate id, or `None` if not found.
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>>;

    /// Return all bindings owned by `owner_user_id` within `tenant_id`.
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>>;

    /// Return all grants on bindings owned by `owner_user_id` in `tenant_id`
    /// for a specific `provider` and `target`.
    async fn find_active_grants_for_target(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
        provider: &CredentialProvider,
        target: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>>;

    /// Permanently delete a binding and all its grants.
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()>;

    // -----------------------------------------------------------------------
    // OAuth pending state management
    // -----------------------------------------------------------------------

    /// Insert an OAuth2 PKCE pending state row.
    async fn save_oauth_state(
        &self,
        state: &str,
        binding_id: &CredentialBindingId,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<()>;

    /// Load a pending state by its opaque state token, or `None` if not found.
    async fn find_oauth_state(&self, state: &str) -> anyhow::Result<Option<OAuthPendingState>>;

    /// Delete a single pending state row (called after successful completion or rejection).
    async fn delete_oauth_state(&self, state: &str) -> anyhow::Result<()>;

    /// Delete all pending state rows older than `older_than` (TTL cleanup).
    ///
    /// Returns the number of rows deleted.
    async fn delete_expired_oauth_states(&self, older_than: DateTime<Utc>) -> anyhow::Result<u64>;
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared_kernel::TenantId;

    fn test_binding() -> UserCredentialBinding {
        UserCredentialBinding {
            id: CredentialBindingId::new(),
            owner_user_id: "user-sub-123".to_string(),
            tenant_id: TenantId::consumer(),
            credential_type: CredentialType::Secret,
            provider: CredentialProvider::new("openai"),
            secret_path: SecretPath::new("aegis-system", "kv", "openai/key"),
            scope: CredentialScope::Personal,
            status: CredentialStatus::Active,
            metadata: CredentialMetadata {
                label: "My OpenAI Key".to_string(),
                tags: None,
                service_url: None,
                external_account_id: None,
                oauth_scopes: None,
                mailbox: None,
                reach: None,
                calendar: None,
            },
            grants: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn revoke_clears_grants_and_sets_status() {
        let mut binding = test_binding();
        binding.add_grant(GrantTarget::AllAgents, "operator".to_string());
        assert_eq!(binding.grants.len(), 1);

        binding.revoke();

        assert_eq!(binding.status, CredentialStatus::Revoked);
        assert!(binding.grants.is_empty());
    }

    #[test]
    fn add_grant_returns_id_and_appends() {
        let mut binding = test_binding();
        let agent_id = AgentId::new();
        let grant_id =
            binding.add_grant(GrantTarget::Agent { agent_id }, "user-sub-123".to_string());
        assert_eq!(binding.grants.len(), 1);
        assert_eq!(binding.grants[0].id, grant_id);
    }

    #[test]
    fn revoke_grant_removes_by_id() {
        let mut binding = test_binding();
        let grant_id = binding.add_grant(GrantTarget::AllAgents, "user-sub-123".to_string());
        assert!(binding.revoke_grant(&grant_id));
        assert!(binding.grants.is_empty());
    }

    #[test]
    fn revoke_grant_returns_false_for_missing_id() {
        let mut binding = test_binding();
        let missing_id = CredentialGrantId::new();
        assert!(!binding.revoke_grant(&missing_id));
    }

    #[test]
    fn active_grants_for_returns_empty_when_revoked() {
        let mut binding = test_binding();
        binding.add_grant(GrantTarget::AllAgents, "user-sub-123".to_string());
        binding.status = CredentialStatus::Revoked;
        let grants = binding.active_grants_for(&GrantTarget::AllAgents);
        assert!(grants.is_empty());
    }

    #[test]
    fn active_grants_for_filters_by_target() {
        let mut binding = test_binding();
        let agent_id = AgentId::new();
        binding.add_grant(GrantTarget::Agent { agent_id }, "user-sub-123".to_string());
        binding.add_grant(GrantTarget::AllAgents, "user-sub-123".to_string());

        let grants = binding.active_grants_for(&GrantTarget::AllAgents);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].target, GrantTarget::AllAgents);
    }

    #[test]
    fn credential_binding_id_round_trips_serde() {
        let id = CredentialBindingId::new();
        let json = serde_json::to_string(&id).unwrap();
        let restored: CredentialBindingId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, restored);
    }

    #[test]
    fn credential_provider_is_its_name() {
        assert_eq!(CredentialProvider::new("openai").to_string(), "openai");
        assert_eq!(CredentialProvider::new("stripe").as_str(), "stripe");
        assert_eq!(CredentialProvider::imap().as_str(), "imap");
        assert_eq!(
            serde_json::to_string(&CredentialProvider::new("google")).unwrap(),
            "\"google\""
        );
        let read: CredentialProvider = serde_json::from_str("\"google_mail\"").unwrap();
        assert_eq!(read.as_str(), "google_mail");
    }

    /// Regression: `CredentialProvider` must implement `Hash` so it can be
    /// used as a `HashMap` key in `OAuthProviderRegistry` (CL5).
    #[test]
    fn credential_provider_is_hashable() {
        use std::collections::HashMap;
        let mut map: HashMap<CredentialProvider, &'static str> = HashMap::new();
        map.insert(CredentialProvider::new("github"), "github");
        map.insert(CredentialProvider::new("stripe"), "stripe");
        assert_eq!(map.get(&CredentialProvider::new("github")), Some(&"github"));
        assert_eq!(map.get(&CredentialProvider::new("stripe")), Some(&"stripe"));
    }
}
