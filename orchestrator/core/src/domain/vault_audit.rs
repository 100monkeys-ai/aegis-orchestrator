// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The vault's audit row (AEGIS ADR-150 V7, V10)
//!
//! Every act on the vault through the orchestrator's API appends one row to
//! `admin_audit_log` (migration 003): who acted (`actor_id`, the caller's
//! `sub`), what was done ([`VaultAction`]), to what (`target_resource`), and
//! an `after_state` object naming the caller's kind, the binding's tenant,
//! provider and type, the purpose of a read and its outcome. `before_state`
//! is always null.
//!
//! **No row carries a value**, a hash, a prefix, a length or any other
//! function of one: the row types below hold no field a value could be put
//! in, and a value is never formatted into a target or an outcome.
//!
//! A value is never answered without its row: the value reads write their
//! row before they answer and refuse with `vault_unavailable` when the row
//! cannot be written (V7). Every other act writes its row after the act;
//! [`append_after_act`] logs a failed write at error naming the action and
//! the target, and never undoes the act.

use async_trait::async_trait;
use serde::Serialize;

/// What was done to the vault: the ten actions of AEGIS ADR-150 V7 and the
/// three of V10, as `admin_audit_log.action` spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VaultAction {
    /// A key binding's value answered to its owner (V3).
    ValueRead,
    /// A value read refused; the outcome names the refusal's code.
    ValueRefused,
    /// A binding written: an API key, a mailbox or a calendar account.
    BindingStored,
    /// A binding's secret rotated.
    BindingRotated,
    /// A binding revoked, which deletes its value and its row.
    BindingRevoked,
    /// A grant added to a binding.
    GrantAdded,
    /// A grant revoked.
    GrantRevoked,
    /// A platform secret path written by an operator.
    SecretWritten,
    /// A platform secret path deleted by an operator.
    SecretDeleted,
    /// A binding's value used by a tool invocation inside the orchestrator
    /// (V5); written by a later change, named here so the set is whole.
    ToolValueUsed,
    /// A declared client's secret written (V10).
    ClientSecretWritten,
    /// A declared client's secret read, or the read refused (V10).
    ClientSecretRead,
    /// A declared client's secret deleted (V10).
    ClientSecretDeleted,
}

impl VaultAction {
    /// Every action, in the order of V7 and then V10.
    pub const ALL: [VaultAction; 13] = [
        VaultAction::ValueRead,
        VaultAction::ValueRefused,
        VaultAction::BindingStored,
        VaultAction::BindingRotated,
        VaultAction::BindingRevoked,
        VaultAction::GrantAdded,
        VaultAction::GrantRevoked,
        VaultAction::SecretWritten,
        VaultAction::SecretDeleted,
        VaultAction::ToolValueUsed,
        VaultAction::ClientSecretWritten,
        VaultAction::ClientSecretRead,
        VaultAction::ClientSecretDeleted,
    ];

    /// The action as `admin_audit_log.action` stores it.
    pub fn as_str(&self) -> &'static str {
        match self {
            VaultAction::ValueRead => "vault.value_read",
            VaultAction::ValueRefused => "vault.value_refused",
            VaultAction::BindingStored => "vault.binding_stored",
            VaultAction::BindingRotated => "vault.binding_rotated",
            VaultAction::BindingRevoked => "vault.binding_revoked",
            VaultAction::GrantAdded => "vault.grant_added",
            VaultAction::GrantRevoked => "vault.grant_revoked",
            VaultAction::SecretWritten => "vault.secret_written",
            VaultAction::SecretDeleted => "vault.secret_deleted",
            VaultAction::ToolValueUsed => "vault.tool_value_used",
            VaultAction::ClientSecretWritten => "vault.client_secret_written",
            VaultAction::ClientSecretRead => "vault.client_secret_read",
            VaultAction::ClientSecretDeleted => "vault.client_secret_deleted",
        }
    }
}

impl std::fmt::Display for VaultAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The kind of caller a row names in `after_state.caller` (V7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultCaller {
    /// A person's own Keycloak session (consumer or tenant user).
    Session,
    /// An operator's Keycloak session.
    Operator,
    /// A run, for a tool invocation's use of a value (V5).
    Execution,
    /// Any other identity refused a value read (a service account).
    Other,
}

/// The `after_state` of a row about a credential binding or a platform
/// secret path (V7). Fields a refusal cannot know (the provider of a binding
/// the caller may not reach) are null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BindingAuditState {
    pub caller: VaultCaller,
    pub tenant_id: Option<String>,
    pub provider: Option<String>,
    /// The binding's type, serialised as `credential_type`.
    #[serde(rename = "credential_type")]
    pub binding_type: Option<String>,
    /// The purpose the caller named; value reads only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// `answered`, or the refusal's code.
    pub outcome: String,
    /// The run's id; tool use only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

/// The `after_state` of a row about a declared client's secret (V10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientSecretAuditState {
    pub client_id: String,
    pub kind: String,
    /// The person the client keeps the secret for; null when unknown.
    pub owner_sub: Option<String>,
    /// The purpose the client named; reads only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    /// `answered`, or the refusal's code.
    pub outcome: String,
}

/// The outcome of an act that was carried out.
pub const OUTCOME_ANSWERED: &str = "answered";

/// One row of `admin_audit_log` about the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultAuditRow {
    /// The caller's `sub`.
    pub actor_id: String,
    pub action: VaultAction,
    /// `credential:<binding id>`, `secret:<engine>/<path>` or
    /// `client-secret:<client_id>/<kind>/<id>`.
    pub target_resource: String,
    pub after_state: serde_json::Value,
}

impl VaultAuditRow {
    /// A row about the credential binding `binding_id`.
    pub fn binding(
        actor_id: impl Into<String>,
        action: VaultAction,
        binding_id: impl std::fmt::Display,
        state: &BindingAuditState,
    ) -> Self {
        Self {
            actor_id: actor_id.into(),
            action,
            target_resource: format!("credential:{binding_id}"),
            after_state: serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
        }
    }

    /// A row about the platform secret path `engine/path`.
    pub fn secret_path(
        actor_id: impl Into<String>,
        action: VaultAction,
        engine: &str,
        path: &str,
        state: &BindingAuditState,
    ) -> Self {
        Self {
            actor_id: actor_id.into(),
            action,
            target_resource: format!("secret:{engine}/{path}"),
            after_state: serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
        }
    }

    /// A row about the client secret `<client_id>/<kind>/<id>`.
    pub fn client_secret(
        actor_id: impl Into<String>,
        action: VaultAction,
        id: impl std::fmt::Display,
        state: &ClientSecretAuditState,
    ) -> Self {
        Self {
            actor_id: actor_id.into(),
            action,
            target_resource: format!("client-secret:{}/{}/{id}", state.client_id, state.kind),
            after_state: serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
        }
    }
}

/// Where vault audit rows are appended: `admin_audit_log` in production
/// (`PostgresVaultAuditSink`), in process for tests and for a daemon run
/// without a database (`InMemoryVaultAuditSink`).
#[async_trait]
pub trait VaultAuditSink: Send + Sync {
    /// Append `row`. An error means the row was not written.
    async fn append(&self, row: &VaultAuditRow) -> anyhow::Result<()>;
}

/// Append the row of an act already carried out: a failed write is logged
/// at error naming the action and the target, and the act stands (V7).
pub async fn append_after_act(sink: &dyn VaultAuditSink, row: VaultAuditRow) {
    if let Err(e) = sink.append(&row).await {
        tracing::error!(
            action = %row.action,
            target = %row.target_resource,
            error = %e,
            "the vault audit row was not written"
        );
    }
}
