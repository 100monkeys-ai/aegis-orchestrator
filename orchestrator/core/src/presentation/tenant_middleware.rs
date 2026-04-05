// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! TenantContext middleware (ADR-056)
//!
//! Extracts `TenantId` from validated `UserIdentity` and inserts it into
//! request extensions. Runs after `iam_auth_middleware`.

use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::tenant::TenantId;
use axum::{extract::Request, middleware::Next, response::Response};

/// Derive TenantId from a UserIdentity
pub fn derive_tenant_id(identity: &UserIdentity) -> TenantId {
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { tenant_id, .. } => tenant_id.clone(),
        IdentityKind::TenantUser { tenant_slug } => {
            TenantId::from_realm_slug(tenant_slug.clone()).unwrap_or_else(|_| TenantId::consumer())
        }
        IdentityKind::Operator { .. } => TenantId::system(),
        IdentityKind::ServiceAccount { .. } => TenantId::system(),
    }
}

/// Paths exempt from tenant extraction (match the IAM exempt paths)
fn is_tenant_exempt(path: &str) -> bool {
    path == "/health"
        || path.starts_with("/v1/dispatch-gateway")
        || path.starts_with("/v1/seal/")
        || path.starts_with("/v1/webhooks/")
        || path == "/v1/temporal-events"
}

/// TenantContext middleware
///
/// Extracts TenantId from UserIdentity (inserted by iam_auth_middleware).
/// Supports admin cross-tenant access via X-Aegis-Tenant header.
pub async fn tenant_context_middleware(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();

    // Skip tenant extraction for exempt paths
    if is_tenant_exempt(&path) {
        return next.run(request).await;
    }

    // Extract UserIdentity from extensions (set by iam_auth_middleware)
    let identity = request.extensions().get::<UserIdentity>().cloned();

    let tenant_id = match &identity {
        Some(id) => {
            let base_tenant = derive_tenant_id(id);

            // Check for admin cross-tenant header
            if let Some(override_slug) = request.headers().get("x-aegis-tenant") {
                if let IdentityKind::Operator { ref aegis_role } = id.identity_kind {
                    if aegis_role.is_admin() {
                        if let Ok(slug_str) = override_slug.to_str() {
                            match TenantId::from_realm_slug(slug_str) {
                                Ok(target_tenant) => {
                                    tracing::info!(
                                        admin_sub = %id.sub,
                                        source_tenant = %base_tenant,
                                        target_tenant = %target_tenant,
                                        "Admin cross-tenant access"
                                    );
                                    target_tenant
                                }
                                Err(_) => base_tenant,
                            }
                        } else {
                            base_tenant
                        }
                    } else {
                        base_tenant
                    }
                } else {
                    base_tenant
                }
            } else {
                base_tenant
            }
        }
        None => {
            // No identity — this happens for unauthenticated paths that somehow
            // got past iam_auth_middleware. Use default tenant.
            TenantId::default()
        }
    };

    let mut request = request;
    request.extensions_mut().insert(tenant_id);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::iam::{AegisRole, ZaruTier};

    #[test]
    fn derive_consumer_tenant() {
        let id = UserIdentity {
            sub: "user-1".to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        };
        assert_eq!(derive_tenant_id(&id), TenantId::consumer());
    }

    #[test]
    fn derive_operator_tenant() {
        let id = UserIdentity {
            sub: "admin-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Admin,
            },
        };
        assert_eq!(derive_tenant_id(&id), TenantId::system());
    }

    #[test]
    fn derive_tenant_user() {
        let id = UserIdentity {
            sub: "tu-1".to_string(),
            realm_slug: "tenant-acme".to_string(),
            email: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "tenant-acme".to_string(),
            },
        };
        let tid = derive_tenant_id(&id);
        assert_eq!(tid.as_str(), "tenant-acme");
    }

    #[test]
    fn derive_service_account_tenant() {
        let id = UserIdentity {
            sub: "sa-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "sdk-python".to_string(),
            },
        };
        assert_eq!(derive_tenant_id(&id), TenantId::system());
    }

    #[test]
    fn exempt_paths() {
        assert!(is_tenant_exempt("/health"));
        assert!(is_tenant_exempt("/v1/dispatch-gateway"));
        assert!(is_tenant_exempt("/v1/dispatch-gateway/abc"));
        assert!(is_tenant_exempt("/v1/seal/attest"));
        assert!(is_tenant_exempt("/v1/seal/invoke"));
        assert!(is_tenant_exempt("/v1/webhooks/github"));
        assert!(is_tenant_exempt("/v1/temporal-events"));
        assert!(!is_tenant_exempt("/v1/executions"));
        assert!(!is_tenant_exempt("/v1/agents"));
    }
}
