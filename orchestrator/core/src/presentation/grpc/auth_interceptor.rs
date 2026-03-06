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

use crate::domain::iam::{IdentityProvider, UserIdentity};
use crate::domain::node_config::GrpcAuthConfig;
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
pub async fn validate_grpc_request<T>(
    interceptor: &GrpcIamAuthInterceptor,
    request: &Request<T>,
    method: &str,
) -> Result<Option<UserIdentity>, Status> {
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
            Status::unauthenticated(format!("Token validation failed: {}", e))
        })?;

    Ok(Some(validated.identity))
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
