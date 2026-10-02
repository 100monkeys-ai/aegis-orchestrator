// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool approvals (AEGIS ADR-126)
//!
//! An outbound tool call waits for its user's answer: approve once, always
//! allow, or deny. The gate in the tool dispatch path writes a
//! [`ToolApprovalRequest`] for every call of a gated tool, and a
//! [`ToolApprovalPolicy`] holds a user's "always allow" for one tool on one
//! binding (the call's `mailbox` argument, or none).
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

/// The argument whose value names the binding a gated call acts through
/// (ADR-126 D2: "the `mailbox` argument's binding id").
pub const BINDING_ARGUMENT: &str = "mailbox";

/// The number of body characters a mail tool's summary carries (ADR-126 D3).
pub const SUMMARY_BODY_CHARS: usize = 2_000;

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

/// The binding a call acts through: its `mailbox` argument, when a string.
pub fn binding_of(arguments: &Value) -> Option<String> {
    arguments
        .get(BINDING_ARGUMENT)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The text a user reads before answering.
///
/// For `mail.send` and `mail.reply`: the mailbox, recipients, subject and the
/// first [`SUMMARY_BODY_CHARS`] characters of the body (ADR-126 D3). For any
/// other tool: its name and its arguments, cut at the same length.
pub fn summarize(tool_name: &str, arguments: &Value) -> String {
    match tool_name {
        "mail.send" | "mail.reply" => {
            let field = |key: &str| -> String {
                match arguments.get(key) {
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
            };
            let body: String = field("body").chars().take(SUMMARY_BODY_CHARS).collect();
            format!(
                "{tool_name} from mailbox {}\nTo: {}\nSubject: {}\n\n{body}",
                field(BINDING_ARGUMENT),
                field("to"),
                field("subject"),
            )
        }
        _ => {
            let args: String = arguments
                .to_string()
                .chars()
                .take(SUMMARY_BODY_CHARS)
                .collect();
            format!("{tool_name} with arguments {args}")
        }
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

    #[test]
    fn a_mail_summary_names_the_mailbox_recipients_subject_and_cut_body() {
        let body = "z".repeat(SUMMARY_BODY_CHARS + 500);
        let summary = summarize(
            "mail.send",
            &json!({"mailbox": "b-1", "to": ["a@example.com", "b@example.com"], "subject": "Hi", "body": body}),
        );
        assert!(summary.contains("mailbox b-1"), "{summary}");
        assert!(
            summary.contains("To: a@example.com, b@example.com"),
            "{summary}"
        );
        assert!(summary.contains("Subject: Hi"), "{summary}");
        assert_eq!(summary.matches('z').count(), SUMMARY_BODY_CHARS);
    }

    #[test]
    fn the_binding_is_the_mailbox_argument() {
        assert_eq!(binding_of(&json!({"mailbox": "b-1"})), Some("b-1".into()));
        assert_eq!(binding_of(&json!({"to": "x"})), None);
    }
}
