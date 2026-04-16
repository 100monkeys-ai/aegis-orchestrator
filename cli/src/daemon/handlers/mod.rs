// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP handler modules for the daemon server.

use aegis_orchestrator_core::domain::{
    iam::{resolve_effective_tenant, IdentityKind, UserIdentity},
    tenant::TenantId,
};

pub(crate) mod admin;
pub(crate) mod agents;
pub(crate) mod api_keys;
pub(crate) mod approvals;
pub(crate) mod billing;
pub(crate) mod cluster;
pub(crate) mod colony;
pub(crate) mod cortex;
pub(crate) mod credentials;
pub(crate) mod dispatch;
pub(crate) mod executions;
pub(crate) mod health;
pub(crate) mod observability;
pub(crate) mod seal;
pub(crate) mod stimulus;
pub(crate) mod swarms;
pub(crate) mod tenant_provisioning;
pub(crate) mod volumes;
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
///
/// Delegates to the canonical domain-layer gate [`resolve_effective_tenant`]
/// (ADR-100).
pub(crate) fn tenant_id_from_request(
    identity: Option<&UserIdentity>,
    delegation_tenant_id: Option<&str>,
) -> TenantId {
    resolve_effective_tenant(identity, delegation_tenant_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::iam::{AegisRole, ZaruTier};

    fn service_account_identity() -> UserIdentity {
        UserIdentity {
            sub: "svc-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-temporal-worker".into(),
            },
        }
    }

    fn consumer_user_identity() -> UserIdentity {
        UserIdentity {
            sub: "user-1".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        }
    }

    fn tenant_user_identity(slug: &str) -> UserIdentity {
        UserIdentity {
            sub: "tu-1".into(),
            realm_slug: format!("tenant-{slug}"),
            email: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: slug.into(),
            },
        }
    }

    // Gap 100-4 regression tests: service account delegation via X-Tenant-Id

    #[test]
    fn service_account_with_delegation_slug_returns_that_tenant() {
        let identity = service_account_identity();
        let result = tenant_id_from_request(Some(&identity), Some("u-abc"));
        assert_eq!(result.as_str(), "u-abc");
    }

    #[test]
    fn service_account_with_empty_delegation_returns_system() {
        let identity = service_account_identity();
        let result = tenant_id_from_request(Some(&identity), Some(""));
        assert_eq!(result, TenantId::system());
    }

    #[test]
    fn service_account_with_no_delegation_returns_system() {
        let identity = service_account_identity();
        let result = tenant_id_from_request(Some(&identity), None);
        assert_eq!(result, TenantId::system());
    }

    #[test]
    fn consumer_user_ignores_delegation_header() {
        let identity = consumer_user_identity();
        // Header must be silently ignored for non-service-account identities.
        let result = tenant_id_from_request(Some(&identity), Some("u-abc"));
        assert_eq!(result, TenantId::consumer());
    }

    #[test]
    fn tenant_user_ignores_delegation_header() {
        let identity = tenant_user_identity("acme");
        let result = tenant_id_from_request(Some(&identity), Some("u-abc"));
        assert_eq!(result.as_str(), "acme");
    }

    #[test]
    fn none_identity_with_delegation_returns_default() {
        // Unauthenticated caller: header is meaningless, return default tenant.
        let result = tenant_id_from_request(None, Some("u-abc"));
        assert_eq!(result, TenantId::default());
    }
}

pub(crate) fn bounded_limit(limit: Option<usize>, default: usize, maximum: usize) -> usize {
    limit.unwrap_or(default).min(maximum).max(1)
}
