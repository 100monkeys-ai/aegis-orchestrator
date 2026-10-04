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
//! - `/v1/seal/attest` — SEAL attestation handshake
//! - `/v1/seal/invoke` — SEAL tool invocation (uses SecurityToken)
//! - `/v1/seal/tools` — SEAL tool discovery metadata
//! - `/v1/webhooks/*` — webhook ingestion (HMAC auth)
//!
//! Note: ALL `/v1/executions/*` endpoints require JWT auth — each handler
//! enforces a fine-grained scope via `scope_guard.require(...)`. These paths
//! are NOT exempt.
//!
//! Note: `/v1/temporal-events` is NOT exempt — it validates its own Bearer JWT
//! from the `aegis-temporal-worker` service account directly in the handler.

use crate::domain::iam::IdentityProvider;
use axum::{
    extract::Request,
    http::{header, StatusCode},
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

impl<S> axum::extract::FromRequestParts<S> for ScopeGuard
where
    S: Send + Sync,
{
    type Rejection = (axum::http::StatusCode, axum::Json<serde_json::Value>);

    fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let guard = parts
            .extensions
            .get::<ScopeGuard>()
            .cloned()
            .unwrap_or_default();
        std::future::ready(Ok(guard))
    }
}

/// The validated JWT's full claims, inserted beside the identity wherever
/// the middleware attaches one. The operator escalation's routes read the
/// token's `azp` and `consumer_sub` from it (AEGIS ADR-129 D10, D13).
#[derive(Clone, Debug, Default)]
pub struct TokenClaims(pub serde_json::Value);

impl TokenClaims {
    /// A string claim, if present.
    pub fn str_claim(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.as_str())
    }
}

/// Paths exempt from IAM/OIDC JWT auth.
/// These endpoints use other auth mechanisms (SEAL attestation, HMAC, or are unauthenticated).
const EXEMPT_PATH_PREFIXES: &[&str] = &[
    "/health",
    "/v1/api-keys/validate",
    "/v1/billing/prices",
    "/v1/dispatch-gateway",
    // Authenticated by its own handler (`cli/src/daemon/handlers/llm.rs`):
    // a JWT, whose identity this middleware still attaches
    // (`JWT_IDENTITY_ON_EXEMPT_PATH_PREFIXES`), or an `aegis_*` API key by
    // the lookup `/v1/seal/attest` uses; anything else is 401 (Zaru ADR-0049
    // D4, AEGIS ADR-124's Update of 2026-10-01).
    "/v1/llm/aliases",
    // Authenticated by their own handlers
    // (`cli/src/daemon/handlers/operator_escalations.rs`, AEGIS ADR-129):
    // the operator web interface's routes take a JWT, whose identity and
    // claims this middleware still attaches, and answer 403 to anything
    // else, an `aegis_*` key included (D13); the redemption routes take an
    // `aegis_*` key and refuse everything else `escalation_requires_api_key`
    // (D11).
    "/v1/admin/operator-escalation",
    "/v1/operator-escalations",
    "/v1/seal/attest",
    "/v1/seal/invoke",
    "/v1/seal/tools",
    "/v1/webhooks",
];

/// Exempt paths on which the middleware still attaches the identity (and
/// scopes) of a valid JWT, but refuses nothing: a request with no token, an
/// `aegis_*` key or an invalid token passes with no identity, and the
/// path's handler decides.
const JWT_IDENTITY_ON_EXEMPT_PATH_PREFIXES: &[&str] =
    &["/v1/llm/aliases", "/v1/admin/operator-escalation"];

/// Whether an exempt path still receives a valid JWT's identity.
fn attaches_jwt_identity_when_exempt(path: &str) -> bool {
    JWT_IDENTITY_ON_EXEMPT_PATH_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// Check whether a request path is exempt from IAM/OIDC auth.
fn is_exempt(path: &str) -> bool {
    EXEMPT_PATH_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// On a path of `JWT_IDENTITY_ON_EXEMPT_PATH_PREFIXES`: attach the identity
/// and scopes of a valid Bearer JWT, as the authenticated branch below does.
/// A missing header, a non-Bearer header, an `aegis_*` API key (never a JWT)
/// or an invalid token attaches nothing and refuses nothing.
async fn attach_jwt_identity_if_valid(
    iam_service: &dyn IdentityProvider,
    request: &mut Request,
    route: &str,
) {
    let Some(token) = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string)
    else {
        return;
    };
    if token.starts_with("aegis_") {
        return;
    }
    match iam_service.validate_token(&token).await {
        Ok(validated) => {
            request.extensions_mut().insert(validated.identity);
            let scope_str = validated
                .raw_claims
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let scopes: Vec<String> = scope_str.split_whitespace().map(String::from).collect();
            request.extensions_mut().insert(ScopeGuard(scopes));
            request
                .extensions_mut()
                .insert(TokenClaims(validated.raw_claims));
        }
        Err(e) => {
            warn!(route = %route, error = %e, "HTTP JWT validation failed");
        }
    }
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
    // Logged in place of the path, which can carry a secret (the invitation
    // accept route carries the invitation token).
    let route = crate::presentation::matched_route(&request);

    // Skip auth for exempt paths
    if is_exempt(&path) {
        if attaches_jwt_identity_when_exempt(&path) {
            attach_jwt_identity_if_valid(iam_service.as_ref(), &mut request, &route).await;
        }
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
            warn!(route = %route, "HTTP request missing Authorization header");
            return (StatusCode::UNAUTHORIZED, "Missing Authorization header").into_response();
        }
    };

    // Strip "Bearer " prefix
    let token = match auth_header.strip_prefix("Bearer ") {
        Some(t) => t,
        None => {
            warn!(
                route = %route,
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
            request
                .extensions_mut()
                .insert(TokenClaims(validated.raw_claims));
            next.run(request).await
        }
        Err(e) => {
            warn!(route = %route, error = %e, "HTTP JWT validation failed");
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
        assert!(is_exempt("/v1/api-keys/validate"));
        assert!(is_exempt("/v1/dispatch-gateway/some-id"));
        assert!(is_exempt("/v1/seal/attest"));
        assert!(is_exempt("/v1/seal/invoke"));
        assert!(is_exempt("/v1/seal/tools"));
        assert!(is_exempt("/v1/webhooks/github"));
    }

    #[test]
    fn non_exempt_paths_require_auth() {
        assert!(!is_exempt("/v1/stimuli"));
        assert!(!is_exempt("/v1/agents"));
        assert!(!is_exempt("/v1/swarms"));
        // temporal-events authenticates via JWT in the handler itself
        assert!(!is_exempt("/v1/temporal-events"));
        // api-keys CRUD requires JWT; only /validate is exempt
        assert!(!is_exempt("/v1/api-keys"));
    }

    /// Zaru ADR-0049 D4: the alias route authenticates itself (a JWT or an
    /// `aegis_*` key), and is the only `/v1/llm` path; the JWT-only routes
    /// an API key must not reach stay non-exempt.
    #[test]
    fn the_alias_route_alone_is_exempt_and_attaches_a_jwt_identity() {
        assert!(is_exempt("/v1/llm/aliases/zaru-chat"));
        assert!(attaches_jwt_identity_when_exempt(
            "/v1/llm/aliases/zaru-chat"
        ));
        for path in [
            "/v1/credentials",
            "/v1/credentials/some-id",
            "/v1/agents",
            "/v1/agents/some-id",
        ] {
            assert!(!is_exempt(path), "{path} must stay behind the JWT layer");
        }
        for path in ["/v1/seal/attest", "/v1/webhooks/github", "/health"] {
            assert!(
                !attaches_jwt_identity_when_exempt(path),
                "{path} keeps the exempt behaviour it had"
            );
        }
    }

    /// AEGIS ADR-129: the operator web interface's escalation routes are
    /// exempt and still receive a JWT's identity (their handler answers 403
    /// to an API key, D13); the redemption routes are exempt and receive no
    /// JWT identity (their handler takes an API key only, D11).
    #[test]
    fn operator_escalation_routes_authenticate_themselves() {
        for path in [
            "/v1/admin/operator-escalation-codes",
            "/v1/admin/operator-escalations",
            "/v1/admin/operator-escalations/some-id",
            "/v1/admin/operator-escalations/end-for-operator",
        ] {
            assert!(is_exempt(path), "{path}");
            assert!(attaches_jwt_identity_when_exempt(path), "{path}");
        }
        for path in [
            "/v1/operator-escalations",
            "/v1/operator-escalations/current",
        ] {
            assert!(is_exempt(path), "{path}");
            assert!(!attaches_jwt_identity_when_exempt(path), "{path}");
        }
        assert!(!is_exempt("/v1/admin/rate-limits/overrides"));
    }

    /// Regression: the broad `/v1/executions` prefix was previously exempt,
    /// causing all execution handlers' `scope_guard.require(...)` calls to
    /// always 403 because the middleware was skipped and ScopeGuard was empty.
    #[test]
    fn execution_paths_are_not_exempt() {
        assert!(!is_exempt("/v1/executions"));
        assert!(!is_exempt("/v1/executions/some-id"));
        assert!(!is_exempt("/v1/executions/some-id/cancel"));
        assert!(!is_exempt("/v1/executions/some-id/events"));
    }

    #[test]
    fn scope_guard_require_present() {
        let guard = ScopeGuard(vec!["agent:read".to_string()]);
        assert!(guard.require("agent:read").is_ok());
    }

    #[test]
    fn scope_guard_require_missing() {
        let guard = ScopeGuard(vec![]);
        let result = guard.require("agent:read");
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn scope_guard_require_wrong_scope() {
        let guard = ScopeGuard(vec!["agent:list".to_string()]);
        let result = guard.require("agent:execute");
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    }
}

#[cfg(test)]
mod request_path_logging_tests {
    use super::*;
    use crate::domain::iam::{
        AegisRole, IamError, IdentityRealm, ValidatedIdentityToken, ZaruTier,
    };
    use crate::presentation::test_log_capture::capture_logs;
    use tower::util::ServiceExt;

    /// Refuses every token, so each request takes a refusal branch.
    struct RefusingProvider;

    #[async_trait::async_trait]
    impl IdentityProvider for RefusingProvider {
        async fn validate_token(&self, _: &str) -> Result<ValidatedIdentityToken, IamError> {
            Err(IamError::MissingClaim {
                claim: "sub".to_string(),
            })
        }
        fn resolve_tier(&self, _: &ValidatedIdentityToken) -> Result<ZaruTier, IamError> {
            Ok(ZaruTier::Free)
        }
        fn resolve_role(&self, _: &ValidatedIdentityToken) -> Result<AegisRole, IamError> {
            Err(IamError::MissingClaim {
                claim: "aegis_role".to_string(),
            })
        }
        fn known_realms(&self) -> Vec<IdentityRealm> {
            vec![]
        }
    }

    /// A refused request to a route whose path carries a secret (the
    /// invitation accept route carries the invitation token) is logged with
    /// the matched route template, never the path or its query string. All
    /// three refusal branches: no Authorization header, a header that is not
    /// Bearer, and a token the provider refuses.
    #[test]
    fn a_refused_request_logs_the_route_template_not_the_path() {
        for authorization in [None, Some("Basic abc"), Some("Bearer abc")] {
            let app = axum::Router::new()
                .route(
                    "/v1/colony/invitations/{token}/accept",
                    axum::routing::post(|| async { "accepted" }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    Arc::new(RefusingProvider) as Arc<dyn IdentityProvider>,
                    iam_auth_middleware,
                ));
            let mut request = axum::http::Request::builder().method("POST").uri(
                "/v1/colony/invitations/Mk7-path-invitation-token-marker/accept?q=Mk7-path-query-marker",
            );
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            let request = request.body(axum::body::Body::empty()).unwrap();

            let (status, logs) =
                capture_logs(async move { app.oneshot(request).await.unwrap().status() });

            assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
            for marker in ["Mk7-path-invitation-token-marker", "Mk7-path-query-marker"] {
                assert!(
                    !logs.contains(marker),
                    "{authorization:?}: the request path reached the log:\n{logs}"
                );
            }
            assert!(
                logs.contains("/v1/colony/invitations/{token}/accept"),
                "{authorization:?}: the log does not name the route:\n{logs}"
            );
        }
    }
}
