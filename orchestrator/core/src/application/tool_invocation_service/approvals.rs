// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The tool invocation service's half of the approval gate (AEGIS ADR-126):
//! the run of a stored call on its user's approval, and the tool
//! `aegis.approval.status`.

use super::*;
use crate::application::tool_approval_service::{ApprovedCallRunner, ToolApprovalError};
use crate::domain::iam::{IdentityKind, TenantScope, UserIdentity, ZaruTier};
use crate::domain::tool_approval::{ToolApprovalId, ToolApprovalRequest};

#[async_trait::async_trait]
impl ApprovedCallRunner for ToolInvocationService {
    /// Run a stored call through the dispatch stages after the gate (the
    /// inner-loop judge, then dispatch), with the stored arguments, as the
    /// request's user, under the security context the call was evaluated
    /// under, looked up by name now. The agent's iteration has ended, so the
    /// run carries iteration 0 and no tool history, as a SEAL call does.
    async fn run_approved_call(&self, request: &ToolApprovalRequest) -> Result<Value, String> {
        let security_context = self
            .security_context_repo
            .find_by_name(&request.security_context_name)
            .await
            .map_err(|e| {
                format!(
                    "Failed to load security context '{}': {e}",
                    request.security_context_name
                )
            })?
            .ok_or_else(|| {
                format!(
                    "Security context '{}' no longer exists",
                    request.security_context_name
                )
            })?;
        let identity = UserIdentity {
            sub: request.user_sub.clone(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::from_security_context_name(&security_context.name)
                    .unwrap_or(ZaruTier::Free),
                tenant_id: request.tenant_id.clone(),
            },
        };
        let tenant_scope =
            TenantScope::new(request.tenant_id.clone(), identity.identity_kind.clone());
        let invocation_id = ToolInvocationId::new();
        let started_at = Instant::now();
        self.publish_invocation_requested(
            invocation_id,
            request.execution_id,
            request.agent_id,
            &request.tool_name,
            &request.arguments,
        );
        tracing::info!(
            approval_id = %request.id,
            tool_name = %request.tool_name,
            user_sub = %request.user_sub,
            "Running a stored tool call on its user's approval"
        );
        match self
            .dispatch_after_gate(
                &request.agent_id,
                request.execution_id,
                &tenant_scope,
                &security_context,
                request.tool_name.clone(),
                request.arguments.clone(),
                0,
                Vec::new(),
                Some(&identity),
                invocation_id,
                started_at,
            )
            .await
        {
            Ok(ToolInvocationResult::Direct(value)) => Ok(value),
            // A tool that runs inside the agent's container (cmd.run) needs
            // the live container, which ended with its iteration.
            Ok(ToolInvocationResult::DispatchRequired(_)) => Err(format!(
                "tool '{}' runs inside the agent's container, which has ended; \
                 an approved call of it cannot be run by the orchestrator",
                request.tool_name
            )),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl ToolInvocationService {
    /// `aegis.approval.status` (ADR-126 D4): the status and result of one
    /// gated call, for the call's own user only.
    pub(super) async fn invoke_aegis_approval_status_tool(
        &self,
        args: &Value,
        caller_identity: Option<&UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let approvals = self.tool_approval_service.as_ref().ok_or_else(|| {
            SealSessionError::ConfigurationError(
                "aegis.approval.status: the approval gate is not configured".to_string(),
            )
        })?;
        let raw = args
            .get("approval_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "required field 'approval_id' is missing or not a string".to_string(),
                )
            })?;
        let id = ToolApprovalId::from_string(raw).map_err(|e| {
            SealSessionError::InvalidArguments(format!("invalid approval_id '{raw}': {e}"))
        })?;
        let not_found = || SealSessionError::NotFound(format!("approval request {raw}"));
        let user_sub = caller_identity
            .map(|i| i.sub.as_str())
            .ok_or_else(not_found)?;
        let request = approvals
            .get_for_user(id, &tenant_scope.authenticated_tenant, user_sub)
            .await
            .map_err(|e| match e {
                ToolApprovalError::NotFound => not_found(),
                other => SealSessionError::InternalError(other.to_string()),
            })?;
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "approval_id": request.id.to_string(),
            "status": request.status.as_str(),
            "tool_name": request.tool_name,
            "summary": request.summary,
            "created_at": request.created_at,
            "decided_at": request.decided_at,
            "result": request.result,
            "error": request.error,
        })))
    }
}
