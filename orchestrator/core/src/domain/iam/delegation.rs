// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Service Account Tenant Delegation Gate (ADR-100)
//!
//! Provides [`resolve_effective_tenant`], the canonical gate function that
//! resolves the effective [`TenantId`] for any request, honoring the
//! `X-Tenant-Id` / `x-tenant-id` delegation header when — and only when —
//! the caller is a [`IdentityKind::ServiceAccount`].
//!
//! This is the single authoritative implementation shared by the HTTP daemon
//! handlers and the gRPC presentation layer. No other code should reimplement
//! this logic.

use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::tenant::TenantId;

/// Resolve the effective [`TenantId`] for a request.
///
/// # Delegation rules (ADR-100)
///
/// | Caller kind | `delegation` value | Effective tenant |
/// |---|---|---|
/// | `ServiceAccount` | `Some(non-empty slug)` | `TenantId::from_realm_slug(slug)` (falls back to `system()` on parse error) |
/// | `ServiceAccount` | `None` or `Some("")` | `TenantId::system()` |
/// | Any other identity kind | any | `derive_tenant_id(identity)` (header is ignored) |
/// | `None` (unauthenticated) | any | `TenantId::default()` |
///
/// This function is the single source of truth for the delegation gate and
/// MUST be used by all entry points (HTTP and gRPC) rather than reimplementing
/// the same match logic inline.
pub fn resolve_effective_tenant(
    identity: Option<&UserIdentity>,
    delegation: Option<&str>,
) -> TenantId {
    match identity.map(|id| &id.identity_kind) {
        Some(IdentityKind::ServiceAccount { .. }) => {
            if let Some(t) = delegation.filter(|s| !s.is_empty()) {
                TenantId::from_realm_slug(t).unwrap_or_else(|_| TenantId::system())
            } else {
                TenantId::system()
            }
        }
        _ => derive_tenant_id(identity),
    }
}

/// Derive the base tenant from an identity's claims, without any delegation
/// header. This is the non-delegating path used for all non-service-account
/// callers.
pub(super) fn derive_tenant_id(identity: Option<&UserIdentity>) -> TenantId {
    match identity.map(|id| &id.identity_kind) {
        Some(IdentityKind::ConsumerUser { tenant_id, .. }) => tenant_id.clone(),
        Some(IdentityKind::TenantUser { tenant_slug }) => {
            TenantId::from_realm_slug(tenant_slug).unwrap_or_else(|_| TenantId::consumer())
        }
        Some(IdentityKind::Operator { .. }) => TenantId::system(),
        Some(IdentityKind::ServiceAccount { .. }) => TenantId::system(),
        None => TenantId::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::iam::{AegisRole, ZaruTier};

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

    #[test]
    fn service_account_with_delegation_slug_returns_that_tenant() {
        let identity = service_account_identity();
        let result = resolve_effective_tenant(Some(&identity), Some("u-abc"));
        assert_eq!(result.as_str(), "u-abc");
    }

    #[test]
    fn service_account_with_empty_delegation_returns_system() {
        let identity = service_account_identity();
        let result = resolve_effective_tenant(Some(&identity), Some(""));
        assert_eq!(result, TenantId::system());
    }

    #[test]
    fn service_account_with_no_delegation_returns_system() {
        let identity = service_account_identity();
        let result = resolve_effective_tenant(Some(&identity), None);
        assert_eq!(result, TenantId::system());
    }

    #[test]
    fn consumer_user_ignores_delegation_header() {
        let identity = consumer_user_identity();
        let result = resolve_effective_tenant(Some(&identity), Some("u-abc"));
        // Header is ignored; derive from claims → TenantId::consumer()
        assert_eq!(result, TenantId::consumer());
    }

    #[test]
    fn tenant_user_ignores_delegation_header() {
        let identity = tenant_user_identity("acme");
        let result = resolve_effective_tenant(Some(&identity), Some("u-abc"));
        // Header is ignored; derive from claims → TenantId::from_realm_slug("acme")
        assert_eq!(result.as_str(), "acme");
    }

    #[test]
    fn operator_ignores_delegation_header() {
        let identity = UserIdentity {
            sub: "op-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Admin,
            },
        };
        let result = resolve_effective_tenant(Some(&identity), Some("u-abc"));
        // Header is ignored; derive from claims → TenantId::system()
        assert_eq!(result, TenantId::system());
    }

    #[test]
    fn none_identity_ignores_delegation_returns_default() {
        let result = resolve_effective_tenant(None, Some("u-abc"));
        assert_eq!(result, TenantId::default());
    }
}
