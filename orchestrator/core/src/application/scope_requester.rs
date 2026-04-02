// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Shared requester identity for scope-change authorization checks (ADR-076).
//!
//! Extracted from `workflow_scope` so both `AgentScopeService` and
//! `WorkflowScopeService` can share the same authorization logic.

use crate::domain::tenant::TenantId;

/// Requester identity for authorization checks.
pub struct ScopeChangeRequester {
    pub user_id: String,
    pub roles: Vec<String>,
    pub tenant_id: TenantId,
}

impl ScopeChangeRequester {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    pub fn is_operator_or_admin(&self) -> bool {
        self.has_role("aegis:operator") || self.has_role("aegis:admin")
    }

    pub fn is_tenant_admin(&self) -> bool {
        self.has_role("tenant:admin") || self.is_operator_or_admin()
    }
}
