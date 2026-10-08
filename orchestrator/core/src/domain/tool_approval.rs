// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool approvals (AEGIS ADR-126)
//!
//! An outbound tool call waits for its user's answer: approve once, always
//! allow, or deny. The gate in the tool dispatch path writes a
//! [`ToolApprovalRequest`] for every call of a gated tool, and a
//! [`ToolApprovalPolicy`] holds a user's "always allow" for one tool on one
//! binding (the argument the tool's [`ApprovalContract`] names, or none).
//!
//! Both are stored durably by a [`ToolApprovalRepository`]: a pending request
//! outlives the agent that made the call and the process that stored it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;

/// How long a pending request waits for its user before the sweep expires it
/// (ADR-126 D3).
pub const PENDING_APPROVAL_TTL_HOURS: i64 = 72;

/// The number of characters of each argument a summary carries (ADR-126
/// D3, as its Update of 2026-10-04 clause 1 reads it).
pub const SUMMARY_ARGUMENT_CHARS: usize = 2_000;

/// Identifier of one approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolApprovalId(pub Uuid);

impl ToolApprovalId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for ToolApprovalId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ToolApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier of one "always allow" policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolApprovalPolicyId(pub Uuid);

impl ToolApprovalPolicyId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl Default for ToolApprovalPolicyId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ToolApprovalPolicyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where a request stands (ADR-126 D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalStatus {
    /// Waiting for its user's answer.
    Pending,
    /// Approved for this one call, which the orchestrator then ran.
    ApprovedOnce,
    /// Approved, with an "always allow" policy written; the call was run.
    ApprovedAlways,
    /// Refused by its user; nothing ran.
    Denied,
    /// No answer within [`PENDING_APPROVAL_TTL_HOURS`]; nothing ran.
    Expired,
    /// Matched an "always allow" policy; the call proceeded at once.
    AutoAllowed,
}

impl ToolApprovalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::ApprovedOnce => "approved_once",
            Self::ApprovedAlways => "approved_always",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::AutoAllowed => "auto_allowed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved_once" => Some(Self::ApprovedOnce),
            "approved_always" => Some(Self::ApprovedAlways),
            "denied" => Some(Self::Denied),
            "expired" => Some(Self::Expired),
            "auto_allowed" => Some(Self::AutoAllowed),
            _ => None,
        }
    }
}

impl std::fmt::Display for ToolApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A user's answer to a pending request (ADR-126 D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalDecision {
    Once,
    Always,
    Deny,
}

impl ToolApprovalDecision {
    /// The status a pending request takes on this answer.
    pub fn status(&self) -> ToolApprovalStatus {
        match self {
            Self::Once => ToolApprovalStatus::ApprovedOnce,
            Self::Always => ToolApprovalStatus::ApprovedAlways,
            Self::Deny => ToolApprovalStatus::Denied,
        }
    }
}

/// One gated tool call and what became of it (`tool_approval_requests`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolApprovalRequest {
    pub id: ToolApprovalId,
    pub tenant_id: TenantId,
    /// The execution's initiating user, the only one who may answer.
    pub user_sub: String,
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    pub tool_name: String,
    /// The exact arguments the agent passed; a run on approval uses these.
    pub arguments: Value,
    pub summary: String,
    pub binding_id: Option<String>,
    /// The security context the call was evaluated under; a run on approval
    /// dispatches under the context of this name.
    pub security_context_name: String,
    /// The Zaru conversation the gated call was made in; absent for a call
    /// no conversation started (ADR-126, Update of 2026-10-07 (2), clause 1).
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// The policy an `auto_allowed` call matched.
    pub policy_id: Option<ToolApprovalPolicyId>,
    pub status: ToolApprovalStatus,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl ToolApprovalRequest {
    /// Whether this request is still pending at `now` but past its wait.
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        self.status == ToolApprovalStatus::Pending
            && now - self.created_at >= chrono::Duration::hours(PENDING_APPROVAL_TTL_HOURS)
    }

    /// Whether `user_sub` in `tenant_id` is this request's user.
    pub fn belongs_to(&self, tenant_id: &TenantId, user_sub: &str) -> bool {
        &self.tenant_id == tenant_id && self.user_sub == user_sub
    }
}

/// A user's "always allow" for one tool on one binding
/// (`tool_approval_policies`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolApprovalPolicy {
    pub id: ToolApprovalPolicyId,
    pub tenant_id: TenantId,
    pub user_sub: String,
    pub tool_name: String,
    pub binding_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// What a tool declares to the approval gate (AEGIS ADR-126, Update of
/// 2026-10-04, clause 1), from its input contract
/// ([`ToolInputContract`](crate::domain::mcp::ToolInputContract)) or from its
/// capability entry in the node configuration. The gate knows no tool name
/// and no argument name of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalContract {
    /// The argument naming the credential binding the call acts through; a
    /// policy is keyed on its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_argument: Option<String>,
    /// The arguments a user reads before answering, in this order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_summary: Option<Vec<String>>,
}

impl ApprovalContract {
    /// Whether the tool declares neither key.
    pub fn is_empty(&self) -> bool {
        self.binding_argument.is_none() && self.approval_summary.is_none()
    }

    /// The binding a call acts through: the value of the declared binding
    /// argument, when a string; none when the tool declares no binding.
    pub fn binding_of(&self, arguments: &Value) -> Option<String> {
        let name = self.binding_argument.as_deref()?;
        arguments
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The text a user reads before answering.
    ///
    /// With `approval_summary` declared: the tool's name, then one line
    /// `<argument>: <value>` for each listed argument, in its order, each
    /// value cut at [`SUMMARY_ARGUMENT_CHARS`] characters. Otherwise the
    /// fallback: the tool's name and its arguments, cut at the same length.
    pub fn summarize(&self, tool_name: &str, arguments: &Value) -> String {
        match &self.approval_summary {
            Some(names) => {
                let mut summary = tool_name.to_string();
                for name in names {
                    let value: String = argument_text(arguments.get(name))
                        .chars()
                        .take(SUMMARY_ARGUMENT_CHARS)
                        .collect();
                    summary.push('\n');
                    summary.push_str(name);
                    summary.push_str(": ");
                    summary.push_str(&value);
                }
                summary
            }
            None => {
                let args: String = arguments
                    .to_string()
                    .chars()
                    .take(SUMMARY_ARGUMENT_CHARS)
                    .collect();
                format!("{tool_name} with arguments {args}")
            }
        }
    }
}

/// An argument's value as a user reads it: a string as itself, a list of
/// strings joined by `, `, anything else as JSON, absent as nothing.
fn argument_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            })
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Durable store of approval requests and policies (ADR-126 D3).
#[async_trait]
pub trait ToolApprovalRepository: Send + Sync {
    async fn insert_request(&self, request: &ToolApprovalRequest) -> Result<(), RepositoryError>;

    async fn find_request(
        &self,
        id: ToolApprovalId,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError>;

    /// One user's requests, newest first, optionally of one status.
    async fn list_requests_for_user(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError>;

    /// Every user's requests, newest first (an operator's read).
    async fn list_requests(
        &self,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError>;

    /// Move a request from `pending` to `status`, atomically: `None` when it
    /// was not pending (already decided or expired), so two answers never
    /// both win.
    async fn decide_pending(
        &self,
        id: ToolApprovalId,
        status: ToolApprovalStatus,
        decided_by: Option<&str>,
        decided_at: DateTime<Utc>,
    ) -> Result<Option<ToolApprovalRequest>, RepositoryError>;

    /// Record what the call returned, or why it failed.
    async fn record_outcome(
        &self,
        id: ToolApprovalId,
        result: Option<&Value>,
        error: Option<&str>,
    ) -> Result<(), RepositoryError>;

    /// Expire every request still pending that was created before `cutoff`;
    /// returns the requests expired.
    async fn expire_pending_before(
        &self,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<ToolApprovalRequest>, RepositoryError>;

    async fn insert_policy(&self, policy: &ToolApprovalPolicy) -> Result<(), RepositoryError>;

    /// The user's unrevoked policy for this tool on this binding.
    async fn find_active_policy(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        tool_name: &str,
        binding_id: Option<&str>,
    ) -> Result<Option<ToolApprovalPolicy>, RepositoryError>;

    /// The user's unrevoked policies, newest first.
    async fn list_active_policies(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<ToolApprovalPolicy>, RepositoryError>;

    /// Revoke the user's policy; `false` when the user has no such unrevoked
    /// policy.
    async fn revoke_policy(
        &self,
        id: ToolApprovalPolicyId,
        tenant_id: &TenantId,
        user_sub: &str,
        revoked_at: DateTime<Utc>,
    ) -> Result<bool, RepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_round_trips_through_its_text() {
        for status in [
            ToolApprovalStatus::Pending,
            ToolApprovalStatus::ApprovedOnce,
            ToolApprovalStatus::ApprovedAlways,
            ToolApprovalStatus::Denied,
            ToolApprovalStatus::Expired,
            ToolApprovalStatus::AutoAllowed,
        ] {
            assert_eq!(ToolApprovalStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ToolApprovalStatus::parse("approved"), None);
    }

    /// Test (a)'s domain half: the binding is the argument the contract
    /// declares, whatever its name, and no argument without a declaration.
    #[test]
    fn the_binding_is_the_argument_the_contract_declares() {
        let contract = ApprovalContract {
            binding_argument: Some("account".into()),
            approval_summary: None,
        };
        assert_eq!(
            contract.binding_of(&json!({"account": "b-1", "mailbox": "m-1"})),
            Some("b-1".into())
        );
        assert_eq!(contract.binding_of(&json!({"mailbox": "m-1"})), None);
        assert_eq!(
            ApprovalContract::default().binding_of(&json!({"mailbox": "m-1"})),
            None,
            "no tool name and no argument name is known to the gate"
        );
    }

    /// Test (b): a summary lists exactly the declared arguments, in order,
    /// each cut at 2,000 characters.
    #[test]
    fn a_declared_summary_lists_exactly_those_arguments_each_cut() {
        let contract = ApprovalContract {
            binding_argument: Some("account".into()),
            approval_summary: Some(vec!["recipients".into(), "text".into(), "title".into()]),
        };
        let long = "z".repeat(SUMMARY_ARGUMENT_CHARS + 500);
        let summary = contract.summarize(
            "chat.post",
            &json!({
                "account": "b-1",
                "recipients": ["a@example.com", "b@example.com"],
                "text": long,
                "title": "y".repeat(SUMMARY_ARGUMENT_CHARS + 1),
                "secret_extra": "not listed"
            }),
        );
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(lines[0], "chat.post");
        assert_eq!(lines[1], "recipients: a@example.com, b@example.com");
        assert_eq!(
            lines[2],
            format!("text: {}", "z".repeat(SUMMARY_ARGUMENT_CHARS))
        );
        assert_eq!(
            lines[3],
            format!("title: {}", "y".repeat(SUMMARY_ARGUMENT_CHARS))
        );
        assert_eq!(lines.len(), 4, "{summary}");
        assert!(!summary.contains("not listed"), "{summary}");
        assert!(!summary.contains("b-1"), "{summary}");
    }

    /// Test (c): a tool declaring neither key keeps today's fallback text,
    /// whatever its name (a former mail tool included).
    #[test]
    fn a_tool_declaring_neither_key_keeps_the_fallback() {
        let args = json!({"mailbox": "b-1", "to": "x@example.com", "body": "w".repeat(3_000)});
        for tool in ["mail.send", "anything.else"] {
            let summary = ApprovalContract::default().summarize(tool, &args);
            let expected: String = args
                .to_string()
                .chars()
                .take(SUMMARY_ARGUMENT_CHARS)
                .collect();
            assert_eq!(summary, format!("{tool} with arguments {expected}"));
        }
    }
}
