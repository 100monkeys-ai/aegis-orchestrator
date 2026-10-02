// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool approval service (AEGIS ADR-126)
//!
//! The application half of the approval gate:
//!
//! - [`ToolApprovalService::gate`] is called by the tool dispatch path for a
//!   gated call, after the security context allowed it and before the
//!   inner-loop judge (D2). It refuses a call with no initiating user, lets a
//!   call matching the user's "always allow" proceed (an `auto_allowed` row),
//!   and otherwise stores a `pending` row and tells the caller to return
//!   `approval_pending`.
//! - [`ToolApprovalService::decide`] takes the user's answer (D4). "once" and
//!   "always" run the stored call through an [`ApprovedCallRunner`] (the
//!   dispatch stages after the gate, as the request's user, with the stored
//!   arguments) and record its result; "always" also writes a policy;
//!   "deny" runs nothing.
//! - [`ToolApprovalService::expire_stale`] expires requests pending for
//!   72 hours; the daemon runs it every ten minutes
//!   ([`ToolApprovalService::spawn_expiry_sweep`]).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::domain::agent::AgentId;
use crate::domain::events::MCPToolEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::repository::RepositoryError;
use crate::domain::tenant::TenantId;
use crate::domain::tool_approval::{
    binding_of, summarize, ToolApprovalDecision, ToolApprovalId, ToolApprovalPolicy,
    ToolApprovalPolicyId, ToolApprovalRepository, ToolApprovalRequest, ToolApprovalStatus,
    PENDING_APPROVAL_TTL_HOURS,
};
use crate::infrastructure::event_bus::EventBus;

/// How often the daemon expires stale pending requests (ADR-126 D3).
pub const EXPIRY_SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// The error code a gated call with no initiating user is refused with
/// (ADR-126 D1).
pub const APPROVAL_REQUIRES_USER: &str = "approval_requires_user";

#[derive(Debug, thiserror::Error)]
pub enum ToolApprovalError {
    /// The call's execution has no initiating user to ask.
    #[error("{APPROVAL_REQUIRES_USER}: tool '{0}' requires its user's approval, and this call has no initiating user")]
    RequiresUser(String),
    /// No such request or policy for this caller (another user's is answered
    /// the same way).
    #[error("not found")]
    NotFound,
    /// The request was already answered.
    #[error("approval request already decided: {0}")]
    AlreadyDecided(ToolApprovalStatus),
    /// The request waited 72 hours with no answer; it is never run.
    #[error("approval request expired")]
    Expired,
    #[error("approval store: {0}")]
    Repository(#[from] RepositoryError),
}

/// A call the gate is asked about.
#[derive(Debug, Clone)]
pub struct GatedCall<'a> {
    pub tenant_id: &'a TenantId,
    /// The execution's initiating user, if any.
    pub user_sub: Option<&'a str>,
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    pub tool_name: &'a str,
    pub arguments: &'a Value,
    pub security_context_name: &'a str,
}

/// What the dispatch path does with a gated call.
#[derive(Debug, Clone, PartialEq)]
pub enum GateOutcome {
    /// An "always allow" policy matched: run the call; its `auto_allowed`
    /// row is `approval_id`, whose outcome the caller records.
    Proceed { approval_id: ToolApprovalId },
    /// Stored as pending: return `result` (`approval_pending`) at once.
    Pending { result: Value },
}

/// Runs a stored call through the dispatch stages after the gate, with the
/// stored arguments, as the request's user. Implemented by the tool
/// invocation service.
#[async_trait]
pub trait ApprovedCallRunner: Send + Sync {
    async fn run_approved_call(&self, request: &ToolApprovalRequest) -> Result<Value, String>;
}

pub struct ToolApprovalService {
    repo: Arc<dyn ToolApprovalRepository>,
    event_bus: Arc<EventBus>,
}

impl ToolApprovalService {
    pub fn new(repo: Arc<dyn ToolApprovalRepository>, event_bus: Arc<EventBus>) -> Self {
        Self { repo, event_bus }
    }

    /// The gate (ADR-126 D2), for a call of a gated tool.
    pub async fn gate(&self, call: GatedCall<'_>) -> Result<GateOutcome, ToolApprovalError> {
        let user_sub = call
            .user_sub
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolApprovalError::RequiresUser(call.tool_name.to_string()))?;
        let binding_id = binding_of(call.arguments);
        let now = Utc::now();
        let policy = self
            .repo
            .find_active_policy(
                call.tenant_id,
                user_sub,
                call.tool_name,
                binding_id.as_deref(),
            )
            .await?;
        let mut request = ToolApprovalRequest {
            id: ToolApprovalId::new(),
            tenant_id: call.tenant_id.clone(),
            user_sub: user_sub.to_string(),
            execution_id: call.execution_id,
            agent_id: call.agent_id,
            tool_name: call.tool_name.to_string(),
            arguments: call.arguments.clone(),
            summary: summarize(call.tool_name, call.arguments),
            binding_id,
            security_context_name: call.security_context_name.to_string(),
            policy_id: None,
            status: ToolApprovalStatus::Pending,
            created_at: now,
            decided_at: None,
            decided_by: None,
            result: None,
            error: None,
        };
        if let Some(policy) = policy {
            request.status = ToolApprovalStatus::AutoAllowed;
            request.policy_id = Some(policy.id);
            request.decided_at = Some(now);
            self.repo.insert_request(&request).await?;
            tracing::info!(
                approval_id = %request.id,
                policy_id = %policy.id,
                tool_name = %request.tool_name,
                "Gated tool call allowed by the user's always-allow policy"
            );
            return Ok(GateOutcome::Proceed {
                approval_id: request.id,
            });
        }
        self.repo.insert_request(&request).await?;
        tracing::info!(
            approval_id = %request.id,
            tool_name = %request.tool_name,
            execution_id = %request.execution_id,
            "Gated tool call stored pending its user's approval"
        );
        self.event_bus
            .publish_mcp_event(MCPToolEvent::ApprovalRequested {
                approval_id: request.id,
                execution_id: request.execution_id,
                agent_id: request.agent_id,
                tenant_id: request.tenant_id.clone(),
                user_sub: request.user_sub.clone(),
                tool_name: request.tool_name.clone(),
                summary: request.summary.clone(),
                requested_at: now,
            });
        Ok(GateOutcome::Pending {
            result: serde_json::json!({
                "status": "approval_pending",
                "approval_id": request.id.to_string(),
                "summary": request.summary,
            }),
        })
    }

    /// Record what an allowed call returned.
    pub async fn record_outcome(
        &self,
        id: ToolApprovalId,
        outcome: &Result<Value, String>,
    ) -> Result<(), ToolApprovalError> {
        match outcome {
            Ok(value) => self.repo.record_outcome(id, Some(value), None).await?,
            Err(error) => self.repo.record_outcome(id, None, Some(error)).await?,
        }
        Ok(())
    }

    /// One request, if it is `user_sub`'s in `tenant_id`.
    pub async fn get_for_user(
        &self,
        id: ToolApprovalId,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<ToolApprovalRequest, ToolApprovalError> {
        match self.repo.find_request(id).await? {
            Some(request) if request.belongs_to(tenant_id, user_sub) => Ok(request),
            _ => Err(ToolApprovalError::NotFound),
        }
    }

    pub async fn list_for_user(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, ToolApprovalError> {
        Ok(self
            .repo
            .list_requests_for_user(tenant_id, user_sub, status)
            .await?)
    }

    /// Every user's requests: an operator's read (ADR-126 D4).
    pub async fn list_all(
        &self,
        status: Option<ToolApprovalStatus>,
    ) -> Result<Vec<ToolApprovalRequest>, ToolApprovalError> {
        Ok(self.repo.list_requests(status).await?)
    }

    /// The user's answer (ADR-126 D4). Only the request's own user reaches
    /// it; anyone else is answered [`ToolApprovalError::NotFound`].
    pub async fn decide(
        &self,
        id: ToolApprovalId,
        tenant_id: &TenantId,
        user_sub: &str,
        decision: ToolApprovalDecision,
        runner: &dyn ApprovedCallRunner,
    ) -> Result<ToolApprovalRequest, ToolApprovalError> {
        let request = self.get_for_user(id, tenant_id, user_sub).await?;
        let now = Utc::now();
        match request.status {
            ToolApprovalStatus::Pending if request.is_stale(now) => {
                if let Some(expired) = self
                    .repo
                    .decide_pending(id, ToolApprovalStatus::Expired, None, now)
                    .await?
                {
                    self.publish_decided(&expired);
                }
                return Err(ToolApprovalError::Expired);
            }
            ToolApprovalStatus::Pending => {}
            ToolApprovalStatus::Expired => return Err(ToolApprovalError::Expired),
            decided => return Err(ToolApprovalError::AlreadyDecided(decided)),
        }

        // The pending row is claimed before anything runs, so two answers
        // never both run the call.
        let Some(mut decided) = self
            .repo
            .decide_pending(id, decision.status(), Some(user_sub), now)
            .await?
        else {
            let current = self.get_for_user(id, tenant_id, user_sub).await?;
            return Err(match current.status {
                ToolApprovalStatus::Expired => ToolApprovalError::Expired,
                status => ToolApprovalError::AlreadyDecided(status),
            });
        };
        self.publish_decided(&decided);

        if decision == ToolApprovalDecision::Always {
            let policy = ToolApprovalPolicy {
                id: ToolApprovalPolicyId::new(),
                tenant_id: decided.tenant_id.clone(),
                user_sub: decided.user_sub.clone(),
                tool_name: decided.tool_name.clone(),
                binding_id: decided.binding_id.clone(),
                created_at: now,
                created_by: user_sub.to_string(),
                revoked_at: None,
            };
            self.repo.insert_policy(&policy).await?;
        }

        if decision != ToolApprovalDecision::Deny {
            let outcome = runner.run_approved_call(&decided).await;
            self.record_outcome(decided.id, &outcome).await?;
            match outcome {
                Ok(value) => decided.result = Some(value),
                Err(error) => decided.error = Some(error),
            }
        }
        Ok(decided)
    }

    pub async fn list_policies(
        &self,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<Vec<ToolApprovalPolicy>, ToolApprovalError> {
        Ok(self.repo.list_active_policies(tenant_id, user_sub).await?)
    }

    pub async fn revoke_policy(
        &self,
        id: ToolApprovalPolicyId,
        tenant_id: &TenantId,
        user_sub: &str,
    ) -> Result<(), ToolApprovalError> {
        if self
            .repo
            .revoke_policy(id, tenant_id, user_sub, Utc::now())
            .await?
        {
            Ok(())
        } else {
            Err(ToolApprovalError::NotFound)
        }
    }

    /// Expire every request pending since before `now` minus 72 hours;
    /// returns how many were expired.
    pub async fn expire_stale(&self, now: DateTime<Utc>) -> Result<usize, ToolApprovalError> {
        let cutoff = now - chrono::Duration::hours(PENDING_APPROVAL_TTL_HOURS);
        let expired = self.repo.expire_pending_before(cutoff, now).await?;
        for request in &expired {
            self.publish_decided(request);
        }
        Ok(expired.len())
    }

    /// Run [`Self::expire_stale`] every `interval` for the life of the
    /// process (the daemon passes [`EXPIRY_SWEEP_INTERVAL`]).
    pub fn spawn_expiry_sweep(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                match self.expire_stale(Utc::now()).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(expired = n, "Expired stale tool approval requests"),
                    Err(e) => {
                        tracing::warn!(error = %e, "Tool approval expiry sweep failed")
                    }
                }
            }
        })
    }

    fn publish_decided(&self, request: &ToolApprovalRequest) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::ApprovalDecided {
                approval_id: request.id,
                execution_id: request.execution_id,
                agent_id: request.agent_id,
                tool_name: request.tool_name.clone(),
                status: request.status,
                decided_by: request.decided_by.clone(),
                decided_at: request.decided_at.unwrap_or_else(Utc::now),
            });
    }
}
