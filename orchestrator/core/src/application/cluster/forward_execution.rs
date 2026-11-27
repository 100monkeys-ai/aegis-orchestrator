// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The receiving side of a forwarded start (AEGIS ADR-152 D5.3, D5.5).
//!
//! The forwarding node vouches for the caller; the cluster server has
//! already checked that node's envelope, its live lease and the forward's
//! age. Here the execution is claimed under the vouched identity, for the
//! tenant the forwarding node authorised, and nothing is read from the
//! input that the caller's identity does not allow. A repeat of a forward
//! already claimed answers the same.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::application::execution::ExecutionService;
use crate::domain::agent::AgentId;
use crate::domain::execution::{ExecutionId, ExecutionInput};
use crate::domain::iam::UserIdentity;
use crate::domain::tenant::TenantId;

pub struct ForwardExecutionRequest {
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    pub input: String,
    /// The tenant the forwarding node authorised the caller for.
    pub tenant_id: TenantId,
    pub security_context_name: String,
    /// The vouched caller; `None` for a system start.
    pub caller: Option<UserIdentity>,
}

/// What a forward did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// This node claimed the execution and runs it.
    Claimed,
    /// This node had claimed it already (a repeated forward).
    AlreadyClaimed,
}

/// The forwarded input must name the tenant the forwarding node authorised
/// the caller for; the receiving node never takes a tenant from input alone.
pub fn require_authorised_tenant(input: &ExecutionInput, authorised: &TenantId) -> Result<()> {
    let named = input.input.get("tenant_id").and_then(|v| v.as_str());
    if named == Some(authorised.as_str()) {
        Ok(())
    } else {
        Err(anyhow!(
            "the input names tenant {named:?}, not the tenant the forwarding node authorised"
        ))
    }
}

pub struct ForwardExecutionUseCase {
    execution_service: Arc<dyn ExecutionService>,
}

impl ForwardExecutionUseCase {
    pub fn new(execution_service: Arc<dyn ExecutionService>) -> Self {
        Self { execution_service }
    }

    pub async fn execute(&self, req: ForwardExecutionRequest) -> Result<ForwardOutcome> {
        let input: ExecutionInput = serde_json::from_str(&req.input)
            .map_err(|e| anyhow!("invalid execution input: {e}"))?;
        require_authorised_tenant(&input, &req.tenant_id)?;
        if self
            .execution_service
            .get_execution_for_tenant(&req.tenant_id, req.execution_id)
            .await
            .is_ok()
        {
            return Ok(ForwardOutcome::AlreadyClaimed);
        }
        self.execution_service
            .start_execution_with_id(
                req.execution_id,
                req.agent_id,
                input,
                req.security_context_name,
                req.caller.as_ref(),
            )
            .await?;
        Ok(ForwardOutcome::Claimed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tenant: Option<&str>) -> ExecutionInput {
        let mut v = serde_json::json!({ "intent": "x" });
        if let Some(t) = tenant {
            v["tenant_id"] = serde_json::json!(t);
        }
        serde_json::from_value(serde_json::json!({ "intent": null, "input": v })).unwrap()
    }

    #[test]
    fn a_forward_runs_only_for_the_tenant_the_forwarding_node_authorised() {
        let authorised = TenantId::from_string("acme").unwrap();
        assert!(require_authorised_tenant(&input(Some("acme")), &authorised).is_ok());
        assert!(require_authorised_tenant(&input(Some("zaru-consumer")), &authorised).is_err());
        assert!(
            require_authorised_tenant(&input(None), &authorised).is_err(),
            "no fallback tenant"
        );
    }
}
