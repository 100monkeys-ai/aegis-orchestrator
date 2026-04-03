// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # IAM/OIDC HTTP Auth Middleware (ADR-041)
//!
//! Axum middleware layer that validates IAM/OIDC Bearer JWTs on incoming HTTP
//! requests. When authentication succeeds, the resolved [`crate::domain::iam::UserIdentity`] is
//! inserted into the request's extensions for use by downstream handlers.
//!
//! ## Exempt Paths
//!
//! The following paths are exempt from JWT validation (they use alternative
//! auth mechanisms):
//! - `/health` — health check
//! - `/v1/dispatch-gateway/*` — Dispatch Protocol (container ↔ orchestrator)
//! - `/v1/executions/*` — execution SSE stream (uses SEAL SecurityToken)
//! - `/v1/seal/attest` — SEAL attestation handshake
//! - `/v1/seal/invoke` — SEAL tool invocation (uses SecurityToken)
//! - `/v1/seal/tools` — SEAL tool discovery metadata
//! - `/v1/webhooks/*` — webhook ingestion (HMAC auth)
//! - `/v1/temporal-events` — Temporal callbacks (HMAC auth)

use crate::domain::iam::IdentityProvider;
use axum::{
    extract::Request,
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tracing::warn;

/// Scopes extracted from the validated JWT. Populated by `iam_auth_middleware`.
/// Use `scope_guard.require("agent:execute")?` in handlers for fine-grained enforcement.
#[derive(Clone, Debug, Default)]
pub struct ScopeGuard(pub Vec<String>);

impl ScopeGuard {
    pub fn require(
        &self,
        scope: &str,
    ) -> Result<(), (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
        if self.0.iter().any(|s| s == scope) {
            Ok(())
        } else {
            Err((
                axum::http::StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": "insufficient_scope",
                    "required": scope
                })),
            ))
        }
    }
}

#[async_trait::async_trait]
impl<S> axum::extract::FromRequestParts<S> for ScopeGuard
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, axum::Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<ScopeGuard>()
            .cloned()
            .unwrap_or_default())
    }
}

/// Paths exempt from IAM/OIDC JWT auth.
/// These endpoints use other auth mechanisms (SEAL attestation, HMAC, or are unauthenticated).
const EXEMPT_PATH_PREFIXES: &[&str] = &[
    "/health",
    "/v1/dispatch-gateway",
    "/v1/executions",
    "/v1/seal/attest",
    "/v1/seal/invoke",
    "/v1/seal/tools",
    "/v1/webhooks",
    "/v1/temporal-events",
];

/// Check whether a request path is exempt from IAM/OIDC auth.
fn is_exempt(path: &str) -> bool {
    EXEMPT_PATH_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Axum middleware function for IAM/OIDC JWT authentication.
///
/// Usage:
/// ```rust,ignore
/// use axum::middleware;
///
/// let iam_service: Arc<dyn IdentityProvider> = /* ... */;
/// let app = Router::new()
///     .route("/v1/stimuli", post(handle_stimulus))
///     .layer(middleware::from_fn_with_state(iam_service, iam_auth_middleware));
/// ```
pub async fn iam_auth_middleware(
    axum::extract::State(iam_service): axum::extract::State<Arc<dyn IdentityProvider>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Skip auth for exempt paths
    if is_exempt(&path) {
        return next.run(request).await;
    }

    // Extract Authorization header
    let auth_header = match request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(h) => h.to_string(),
        None => {
            warn!(path, "HTTP request missing Authorization header");
            return (StatusCode::UNAUTHORIZED, "Missing Authorization header").into_response();
        }
    };

    // Strip "Bearer " prefix
    let token = match auth_header.strip_prefix("Bearer ") {
        Some(t) => t,
        None => {
            warn!(
                path,
                "Invalid Authorization header format (expected Bearer)"
            );
            return (
                StatusCode::UNAUTHORIZED,
                "Invalid Authorization header format",
            )
                .into_response();
        }
    };

    // Validate JWT
    match iam_service.validate_token(token).await {
        Ok(validated) => {
            // Insert UserIdentity into request extensions for downstream handlers
            request.extensions_mut().insert(validated.identity);
            // Extract resource:action scopes from the JWT "scope" claim
            let scope_str = validated
                .raw_claims
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let scopes: Vec<String> = scope_str.split_whitespace().map(String::from).collect();
            request.extensions_mut().insert(ScopeGuard(scopes));
            next.run(request).await
        }
        Err(e) => {
            warn!(path, error = %e, "HTTP JWT validation failed");
            // Return a static message — do not echo JWT error detail to the caller
            // to prevent leaking token contents or internal error paths.
            (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exempt_paths_recognized() {
        assert!(is_exempt("/health"));
        assert!(is_exempt("/v1/dispatch-gateway/some-id"));
        assert!(is_exempt("/v1/executions"));
        assert!(is_exempt("/v1/executions/some-id/stream"));
        assert!(is_exempt("/v1/seal/attest"));
        assert!(is_exempt("/v1/seal/invoke"));
        assert!(is_exempt("/v1/seal/tools"));
        assert!(is_exempt("/v1/webhooks/github"));
        assert!(is_exempt("/v1/temporal-events"));
    }

    #[test]
    fn non_exempt_paths_require_auth() {
        assert!(!is_exempt("/v1/stimuli"));
        assert!(!is_exempt("/v1/agents"));
        assert!(!is_exempt("/api/v1/status"));
    }
}
