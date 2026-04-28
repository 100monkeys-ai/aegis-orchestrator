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

use crate::domain::iam::{IamError, IdentityKind, UserIdentity};
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
    resolve_effective_tenant_strict(identity, delegation).unwrap_or_else(|e| {
        // Strict resolution rejected the slug. The fallback path used to
        // silently return `consumer()` / `system()` here — that was unsafe
        // (a malformed `tenant_slug` could route a `TenantUser` into the
        // shared consumer tenant). Now we fail closed onto `system()`,
        // which has no application-level rights, and emit a structured
        // warning so the misconfiguration is observable. Callers that
        // need to reject the request entirely (e.g. middleware) should
        // call `resolve_effective_tenant_strict` directly.
        tracing::warn!(
            sub = identity.map(|i| i.sub.as_str()).unwrap_or(""),
            error = %e,
            "tenant slug rejected by strict resolution; failing closed onto TenantId::system()"
        );
        TenantId::system()
    })
}

/// Strict variant of [`resolve_effective_tenant`] — returns an error when the
/// authenticated identity carries a malformed tenant slug.
///
/// Prefer this over [`resolve_effective_tenant`] in any code path that can
/// surface an authentication error to the caller (HTTP middleware, gRPC
/// interceptors). The non-strict variant fails closed onto `system()` and
/// MUST only be used by internal call sites where rejection is impossible.
pub fn resolve_effective_tenant_strict(
    identity: Option<&UserIdentity>,
    delegation: Option<&str>,
) -> Result<TenantId, IamError> {
    match identity.map(|id| &id.identity_kind) {
        Some(IdentityKind::ServiceAccount { .. }) => match delegation.filter(|s| !s.is_empty()) {
            Some(slug) => {
                TenantId::from_realm_slug(slug).map_err(|e| IamError::InvalidTenantSlug {
                    slug: slug.to_string(),
                    reason: format!("delegation header is not a valid tenant slug: {e}"),
                })
            }
            None => Ok(TenantId::system()),
        },
        Some(_) => derive_tenant_id_strict(identity.expect("Some matched")),
        None => Ok(TenantId::default()),
    }
}

/// Derive the base tenant from an identity's claims, without any delegation
/// header. Strict — rejects malformed tenant slugs with [`IamError::InvalidTenantSlug`]
/// instead of silently degrading to a default tenant.
pub fn derive_tenant_id_strict(identity: &UserIdentity) -> Result<TenantId, IamError> {
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { tenant_id, .. } => Ok(tenant_id.clone()),
        IdentityKind::TenantUser { tenant_slug } => {
            TenantId::from_realm_slug(tenant_slug).map_err(|e| IamError::InvalidTenantSlug {
                slug: tenant_slug.clone(),
                reason: e.to_string(),
            })
        }
        IdentityKind::Operator { .. } => Ok(TenantId::system()),
        IdentityKind::ServiceAccount { .. } => Ok(TenantId::system()),
    }
}

/// Backwards-compatible internal helper kept for tests; new code should use
/// [`derive_tenant_id_strict`] which returns a `Result`.
#[cfg(test)]
pub(super) fn derive_tenant_id(identity: Option<&UserIdentity>) -> TenantId {
    match identity {
        Some(id) => derive_tenant_id_strict(id).unwrap_or_else(|e| {
            tracing::warn!(
                sub = %id.sub,
                error = %e,
                "tenant slug rejected by strict derivation; failing closed onto TenantId::system()"
            );
            TenantId::system()
        }),
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

    // ── ADR-097 footgun #6 regression tests ──────────────────────────────────
    //
    // A `TenantUser` carrying a malformed `tenant_slug` MUST NOT silently
    // collapse onto `TenantId::consumer()` — that would route their
    // requests into the global shared-consumer tenant and leak data
    // across users. The strict variant rejects the JWT outright; the
    // back-compat variant fails closed onto `TenantId::system()` (which
    // has no application rights) and emits a warning.

    #[test]
    fn derive_tenant_id_strict_rejects_malformed_tenant_user_slug() {
        let identity = tenant_user_identity("Bad Slug!"); // uppercase + space invalid
        let err = derive_tenant_id_strict(&identity).expect_err("must reject malformed slug");
        match err {
            IamError::InvalidTenantSlug { slug, .. } => assert_eq!(slug, "Bad Slug!"),
            other => panic!("expected InvalidTenantSlug, got {other:?}"),
        }
    }

    #[test]
    fn back_compat_derive_tenant_id_does_not_fall_back_to_consumer_on_bad_slug() {
        let identity = tenant_user_identity("Bad Slug!");
        let result = derive_tenant_id(Some(&identity));
        assert_ne!(
            result,
            TenantId::consumer(),
            "non-strict derive must not fall back onto consumer (ADR-097 footgun #6)"
        );
        assert_eq!(result, TenantId::system(), "must fail closed onto system");
    }

    #[test]
    fn strict_resolve_rejects_malformed_delegation_header() {
        let identity = service_account_identity();
        let err = resolve_effective_tenant_strict(Some(&identity), Some("Bad Slug!"))
            .expect_err("must reject malformed delegation slug");
        assert!(matches!(err, IamError::InvalidTenantSlug { .. }));
    }

    #[test]
    fn back_compat_resolve_does_not_fall_back_to_consumer_on_bad_delegation() {
        let identity = service_account_identity();
        let result = resolve_effective_tenant(Some(&identity), Some("Bad Slug!"));
        // Old behaviour: silently coerced to system(). New behaviour: still
        // system() (the slug fails validation), and we ASSERT it never
        // becomes consumer().
        assert_ne!(result, TenantId::consumer());
        assert_eq!(result, TenantId::system());
    }
}
