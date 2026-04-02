// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP handler modules for the daemon server.

use aegis_orchestrator_core::domain::{
    iam::{IdentityKind, UserIdentity},
    tenant::TenantId,
};

pub(crate) mod admin;
pub(crate) mod agents;
pub(crate) mod approvals;
pub(crate) mod cluster;
pub(crate) mod cortex;
pub(crate) mod dispatch;
pub(crate) mod executions;
pub(crate) mod health;
pub(crate) mod observability;
pub(crate) mod seal;
pub(crate) mod swarms;
pub(crate) mod workflow_executions;
pub(crate) mod workflows;

/// Default maximum number of executions that can be returned by a single
/// `list_executions` request when `max_execution_list_limit` is not
/// explicitly configured. This value should remain consistent with the
/// default used in configuration rendering.
pub const DEFAULT_MAX_EXECUTION_LIST_LIMIT: usize = 1000;

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub(crate) struct LimitQuery {
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CortexQueryParams {
    pub(crate) q: Option<String>,
    pub(crate) limit: Option<usize>,
}

pub(crate) fn tenant_id_from_identity(identity: Option<&UserIdentity>) -> TenantId {
    match identity.map(|identity| &identity.identity_kind) {
        Some(IdentityKind::ConsumerUser { .. }) => TenantId::consumer(),
        Some(IdentityKind::TenantUser { tenant_slug }) => {
            TenantId::from_realm_slug(tenant_slug).unwrap_or_else(|_| TenantId::consumer())
        }
        Some(IdentityKind::Operator { .. }) => TenantId::system(),
        Some(IdentityKind::ServiceAccount { .. }) => TenantId::system(),
        None => TenantId::default(),
    }
}

pub(crate) fn bounded_limit(limit: Option<usize>, default: usize, maximum: usize) -> usize {
    limit.unwrap_or(default).min(maximum).max(1)
}
