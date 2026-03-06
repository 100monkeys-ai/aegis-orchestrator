// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # IAM/OIDC HTTP Auth Middleware (ADR-041)
//!
//! Axum middleware layer that validates IAM/OIDC Bearer JWTs on incoming HTTP
//! requests. When authentication succeeds, the resolved [`UserIdentity`] is
//! inserted into the request's extensions for use by downstream handlers.
//!
//! ## Exempt Paths
//!
//! The following paths are exempt from JWT validation (they use alternative
//! auth mechanisms):
//! - `/health` — health check
//! - `/v1/dispatch-gateway/*` — Dispatch Protocol (container ↔ orchestrator)
//! - `/v1/smcp/attest` — SMCP attestation handshake
//! - `/v1/smcp/invoke` — SMCP tool invocation (uses SecurityToken)
//! - `/v1/webhooks/*` — webhook ingestion (HMAC auth)
//! - `/v1/temporal-events` — Temporal callbacks (HMAC auth)

use crate::domain::iam::IdentityProvider;
use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tracing::warn;

/// Paths exempt from IAM/OIDC JWT auth.
/// These endpoints use other auth mechanisms (SMCP attestation, HMAC, or are unauthenticated).
const EXEMPT_PATH_PREFIXES: &[&str] = &[
    "/health",
    "/v1/dispatch-gateway",
    "/v1/smcp/attest",
    "/v1/smcp/invoke",
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
        assert!(is_exempt("/v1/smcp/attest"));
        assert!(is_exempt("/v1/smcp/invoke"));
        assert!(is_exempt("/v1/webhooks/github"));
        assert!(is_exempt("/v1/temporal-events"));
    }

    #[test]
    fn non_exempt_paths_require_auth() {
        assert!(!is_exempt("/v1/stimuli"));
        assert!(!is_exempt("/v1/agents"));
        assert!(!is_exempt("/v1/executions"));
        assert!(!is_exempt("/api/v1/status"));
    }
}
