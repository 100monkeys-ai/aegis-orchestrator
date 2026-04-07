// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # IAM/OIDC gRPC Auth Interceptor (ADR-041)
//!
//! Tonic interceptor that validates IAM/OIDC Bearer JWTs on incoming gRPC calls.
//! Exempted methods (e.g. inner loop channels) are configured via
//! `spec.grpc_auth.exempt_methods` in `aegis-config.yaml`.
//!
//! When authentication succeeds, the resolved [`UserIdentity`] is inserted into
//! the request's extensions so downstream handlers can access it.
//!
//! ## Tenant Injection (gap 056-10)
//!
//! After JWT validation, [`validate_grpc_request`] derives a [`TenantId`] from
//! the validated identity and inserts it into the request extensions.  Operator
//! callers may supply an `x-aegis-tenant` metadata key to override the derived
//! tenant (same semantics as the HTTP `X-Aegis-Tenant` header).

use crate::domain::iam::{IdentityKind, IdentityProvider, UserIdentity};
use crate::domain::node_config::GrpcAuthConfig;
use crate::domain::tenant::TenantId;
use crate::presentation::keycloak_auth::ScopeGuard;
use crate::presentation::tenant_middleware::derive_tenant_id;
use std::collections::HashSet;
use std::sync::Arc;
use tonic::{Request, Status};
use tracing::warn;

/// gRPC interceptor that validates IAM/OIDC Bearer JWTs.
///
/// Installed on the tonic server via `InterceptedService` when
/// `spec.grpc_auth.enabled` is `true` in the node config.
#[derive(Clone)]
pub struct GrpcIamAuthInterceptor {
    iam_service: Arc<dyn IdentityProvider>,
    exempt_methods: HashSet<String>,
}

impl GrpcIamAuthInterceptor {
    /// Create a new interceptor from config.
    pub fn new(iam_service: Arc<dyn IdentityProvider>, config: &GrpcAuthConfig) -> Self {
        Self {
            iam_service,
            exempt_methods: config.exempt_methods.iter().cloned().collect(),
        }
    }

    /// Check if a gRPC method path is exempt from authentication.
    fn is_exempt(&self, method: &str) -> bool {
        self.exempt_methods.contains(method)
    }

    /// Extract and validate the Bearer token from gRPC metadata.
    /// This is the synchronous portion — it extracts the token string.
    /// The actual async validation must be handled at the service layer.
    pub fn extract_bearer_token<T>(&self, request: &Request<T>) -> Option<String> {
        request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.to_string())
    }
}

/// Validate a gRPC request asynchronously using the interceptor's IAM service.
///
/// Since `tonic::Interceptor` is synchronous, this function is intended to be called
/// from within the async gRPC handler methods. The handler calls this before processing
/// the request and rejects with `Status::Unauthenticated` if validation fails.
///
/// ## Return value
///
/// Returns `Ok(None)` for exempt methods (no auth required). For authenticated calls,
/// returns `Ok(Some((identity, tenant_id, scope_guard)))` where `tenant_id` is derived
/// from the identity or overridden via the `x-aegis-tenant` metadata key (admin only),
/// and `scope_guard` carries the resource:action scopes extracted from the JWT "scope" claim.
pub async fn validate_grpc_request<T>(
    interceptor: &GrpcIamAuthInterceptor,
    request: &Request<T>,
    method: &str,
) -> Result<Option<(UserIdentity, TenantId, ScopeGuard)>, Status> {
    // Check exemption
    if interceptor.is_exempt(method) {
        return Ok(None);
    }

    // Extract bearer token
    let token = interceptor.extract_bearer_token(request).ok_or_else(|| {
        warn!(method, "gRPC request missing Authorization header");
        Status::unauthenticated("Missing Authorization header")
    })?;

    // Validate JWT
    let validated = interceptor
        .iam_service
        .validate_token(&token)
        .await
        .map_err(|e| {
            warn!(method, error = %e, "gRPC JWT validation failed");
            Status::unauthenticated(format!("Token validation failed: {e}"))
        })?;

    let identity = validated.identity;

    // Derive tenant from identity, then check for admin override via x-aegis-tenant metadata.
    let base_tenant = derive_tenant_id(&identity);
    let tenant_id = if let IdentityKind::Operator { ref aegis_role } = identity.identity_kind {
        if aegis_role.is_admin() {
            if let Some(override_val) = request.metadata().get("x-aegis-tenant") {
                if let Ok(slug_str) = override_val.to_str() {
                    TenantId::from_realm_slug(slug_str).unwrap_or(base_tenant)
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
    };

    // Extract resource:action scopes from the JWT "scope" claim.
    let scope_str = validated
        .raw_claims
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let scopes: Vec<String> = scope_str.split_whitespace().map(String::from).collect();
    let scope_guard = ScopeGuard(scopes);

    Ok(Some((identity, tenant_id, scope_guard)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> GrpcAuthConfig {
        GrpcAuthConfig {
            enabled: true,
            exempt_methods: vec!["/aegis.v1.InnerLoop/Generate".to_string()],
        }
    }

    #[test]
    fn exempt_methods_checked_correctly() {
        // We can't create a real iam_service in unit tests without async,
        // but we can test the exemption logic.
        let config = test_config();
        let exempt: HashSet<String> = config.exempt_methods.iter().cloned().collect();

        assert!(exempt.contains("/aegis.v1.InnerLoop/Generate"));
        assert!(!exempt.contains("/aegis.v1.AegisRuntime/ExecuteAgent"));
    }
}
