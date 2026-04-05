// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP handler modules for the daemon server.

use aegis_orchestrator_core::domain::{
    iam::{IdentityKind, UserIdentity},
    tenant::TenantId,
};

pub(crate) mod admin;
pub(crate) mod agents;
pub(crate) mod api_keys;
pub(crate) mod approvals;
pub(crate) mod cluster;
pub(crate) mod cortex;
pub(crate) mod dispatch;
pub(crate) mod executions;
pub(crate) mod health;
pub(crate) mod observability;
pub(crate) mod seal;
pub(crate) mod swarms;
pub(crate) mod tenant_provisioning;
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

pub(crate) const TENANT_DELEGATION_HEADER: &str = "x-tenant-id";

pub(crate) fn tenant_id_from_identity(identity: Option<&UserIdentity>) -> TenantId {
    match identity.map(|identity| &identity.identity_kind) {
        Some(IdentityKind::ConsumerUser { tenant_id, .. }) => tenant_id.clone(),
        Some(IdentityKind::TenantUser { tenant_slug }) => {
            TenantId::from_realm_slug(tenant_slug).unwrap_or_else(|_| TenantId::consumer())
        }
        Some(IdentityKind::Operator { .. }) => TenantId::system(),
        Some(IdentityKind::ServiceAccount { .. }) => TenantId::system(),
        None => TenantId::default(),
    }
}

/// Resolve the effective tenant for a request, honoring `X-Tenant-Id` header
/// delegation when the caller is a service account.
///
/// Service accounts authenticate as `aegis-system` by default. When they
/// supply `X-Tenant-Id`, that value becomes the effective tenant so that the
/// worker can operate on behalf of a user-owned tenant (e.g. when the
/// Temporal worker looks up agents deployed in a user tenant).
///
/// All other identity kinds are unaffected — their tenant is always derived
/// solely from the JWT claims via [`tenant_id_from_identity`].
pub(crate) fn tenant_id_from_request(
    identity: Option<&UserIdentity>,
    delegation_tenant_id: Option<&str>,
) -> TenantId {
    match identity.map(|id| &id.identity_kind) {
        Some(IdentityKind::ServiceAccount { .. }) => {
            if let Some(t) = delegation_tenant_id.filter(|s| !s.is_empty()) {
                TenantId::from_realm_slug(t).unwrap_or_else(|_| TenantId::system())
            } else {
                TenantId::system()
            }
        }
        _ => tenant_id_from_identity(identity),
    }
}

pub(crate) fn bounded_limit(limit: Option<usize>, default: usize, maximum: usize) -> usize {
    limit.unwrap_or(default).min(maximum).max(1)
}
