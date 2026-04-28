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
pub(crate) mod canvas;
pub(crate) mod cluster;
pub(crate) mod colony;
pub(crate) mod consumer;
pub(crate) mod cortex;
pub(crate) mod credentials;
pub(crate) mod dispatch;
pub(crate) mod executions;
pub(crate) mod git_repo;
pub(crate) mod health;
pub(crate) mod observability;
pub(crate) mod script;
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
    match identity {
        Some(UserIdentity {
            identity_kind: IdentityKind::ConsumerUser { tenant_id, .. },
            sub,
            ..
        }) => {
            // Self-heal: if the JWT's tenant_id claim was absent, the IAM
            // service falls back to the shared "zaru-consumer" slug. For any
            // per-user concern (billing, tier sync, provisioning), derive the
            // real per-user tenant from the sub. ADR-097.
            if tenant_id.is_consumer() {
                TenantId::for_consumer_user(sub).unwrap_or_else(|_| tenant_id.clone())
            } else {
                tenant_id.clone()
            }
        }
        Some(identity) => match &identity.identity_kind {
            IdentityKind::TenantUser { tenant_slug } => {
                // ADR-097 footgun #11 (Phase 3 finding): a malformed
                // `tenant_slug` MUST NOT silently collapse onto
                // `TenantId::consumer()` — that would route a tenant
                // user's HTTP requests into the shared consumer tenant.
                // Fail closed onto `TenantId::system()` (no application
                // rights) and emit a structured warning. The
                // tenant_context middleware rejects malformed slugs
                // upstream with 401, but this helper is reached by code
                // paths that bypass the middleware (e.g. tests, exempt
                // paths) so it must also be safe.
                match TenantId::from_realm_slug(tenant_slug) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(
                            sub = %identity.sub,
                            tenant_slug = %tenant_slug,
                            error = %e,
                            "tenant_slug rejected; failing closed onto TenantId::system() (ADR-097)"
                        );
                        TenantId::system()
                    }
                }
            }
            IdentityKind::Operator { .. } => TenantId::system(),
            IdentityKind::ServiceAccount { .. } => TenantId::system(),
            IdentityKind::ConsumerUser { .. } => unreachable!("handled above"),
        },
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

/// Read the `TenantId` resolved by the `tenant_context_middleware` (ADR-056 /
/// ADR-111) out of the request's extensions.
///
/// The middleware always inserts a `TenantId` for non-exempt paths; if the
/// extension is somehow missing (tests bypassing the middleware, misconfigured
/// router), fall back to the caller's JWT-derived tenant so handlers still
/// behave deterministically.
pub(crate) fn resolved_tenant(
    request: &axum::extract::Request,
    identity: Option<&UserIdentity>,
) -> TenantId {
    request
        .extensions()
        .get::<TenantId>()
        .cloned()
        .unwrap_or_else(|| tenant_id_from_identity(identity))
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
            name: None,
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
            name: None,
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
            name: None,
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
