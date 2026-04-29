// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # WebhookHmacGuard — HMAC-SHA256 webhook verification (ADR-021)
//!
//! Verifies the `X-Aegis-Signature: sha256=<hex>` header on incoming webhook
//! requests. Uses constant-time comparison (`subtle` crate) to prevent timing
//! attacks.
//!
//! ## Usage
//!
//! Call [`WebhookHmacGuard::from_request`] with the raw [`axum::extract::Request`]
//! and a [`WebhookSecretProvider`] to obtain a verified guard:
//!
//! ```rust,no_run
//! use aegis_orchestrator_core::presentation::webhook_guard::{
//!     WebhookHmacGuard, WebhookSecretProvider,
//! };
//! use aegis_orchestrator_core::domain::tenant::TenantId;
//! use axum::extract::Request;
//! use std::sync::Arc;
//!
//! async fn my_handler(
//!     req: Request,
//!     provider: Arc<dyn WebhookSecretProvider>,
//!     candidate_tenants: Vec<TenantId>,
//! ) {
//!     match WebhookHmacGuard::from_request(req, provider.as_ref(), &candidate_tenants).await {
//!         Ok(guard) => { /* guard.source, guard.tenant_id, guard.body */ }
//!         Err(e) => { /* reject */ }
//!     }
//! }
//! ```
//!
//! ## Secret Resolution
//!
//! Secrets are resolved via [`WebhookSecretProvider`] keyed by
//! `(tenant_id, source_name)`. Audit 002 §4.2 closed the original
//! source-only-keyed lookup that allowed a single shared secret to
//! authenticate webhooks claiming any tenant. The provider is now
//! tenant-scoped and the caller MUST supply the candidate tenant before
//! HMAC verification — typically by enumerating
//! `WorkflowRegistry::find_routes_by_source` and trying each candidate
//! tenant's secret with constant-time comparison.
//!
//! Phase 1: [`EnvWebhookSecretProvider`] reads
//! `AEGIS_WEBHOOK_SECRET_<TENANT>_<SOURCE>` environment variables.
//! Phase 2 will swap to OpenBao KV at path
//! `aegis-system/kv/webhooks/{tenant}/{source}/hmac_secret` (ADR-034).

use crate::domain::tenant::TenantId;
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;

/// Maximum webhook body size accepted before HMAC verification (1 MiB).
///
/// Audit 002 §4.21 closed an unbounded `usize::MAX` body read that
/// allowed a pre-auth memory-DoS. Stripe and other major providers
/// document maximum payload sizes well below this cap; the few sources
/// that need higher caps must register their own per-source override.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 1_048_576;

// ──────────────────────────────────────────────────────────────────────────────
// WebhookSecretProvider trait
// ──────────────────────────────────────────────────────────────────────────────

/// Provides HMAC secrets for webhook sources, keyed by `(tenant_id, source_name)`.
///
/// Phase 1: [`EnvWebhookSecretProvider`] reads environment variables.
/// Phase 2: Replace with OpenBao KV-backed implementation (ADR-034).
#[async_trait]
pub trait WebhookSecretProvider: Send + Sync {
    /// Return the raw secret bytes for the given `(tenant, source)` pair.
    ///
    /// Returns `None` when no secret is configured for that pair.
    async fn get_secret(&self, tenant_id: &TenantId, source_name: &str) -> Option<Vec<u8>>;
}

/// Phase 1: reads `AEGIS_WEBHOOK_SECRET_{UPPER_TENANT}_{UPPER_SOURCE}`
/// environment variables.
///
/// `(tenant=u-abc123, source=my-webhook)` →
/// env var `AEGIS_WEBHOOK_SECRET_U_ABC123_MY_WEBHOOK`.
///
/// Tenant and source are both upper-cased and have `-` / `.` replaced
/// with `_`. Keying secrets by `(tenant, source)` is mandatory — see
/// Finding 4.2.
pub struct EnvWebhookSecretProvider;

fn env_token(s: &str) -> String {
    s.to_uppercase().replace(['-', '.'], "_")
}

#[async_trait]
impl WebhookSecretProvider for EnvWebhookSecretProvider {
    async fn get_secret(&self, tenant_id: &TenantId, source_name: &str) -> Option<Vec<u8>> {
        let env_key = format!(
            "AEGIS_WEBHOOK_SECRET_{}_{}",
            env_token(tenant_id.as_str()),
            env_token(source_name)
        );
        std::env::var(&env_key).ok().map(|s| s.into_bytes())
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum WebhookAuthError {
    /// The `X-Aegis-Signature` header is absent.
    MissingSignature,
    /// The header value is malformed (not `sha256=<hex>`).
    MalformedSignature,
    /// The hex-encoded signature is not valid hex.
    InvalidHex,
    /// No secret is configured for this source.
    SecretNotFound { source: String },
    /// The HMAC signatures do not match.
    InvalidSignature,
    /// Failed to read the request body.
    BodyReadError(String),
}

impl WebhookAuthError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            WebhookAuthError::MissingSignature => (StatusCode::UNAUTHORIZED, "missing_signature"),
            WebhookAuthError::MalformedSignature => {
                (StatusCode::BAD_REQUEST, "malformed_signature")
            }
            WebhookAuthError::InvalidHex => (StatusCode::BAD_REQUEST, "invalid_hex"),
            WebhookAuthError::SecretNotFound { .. } => {
                (StatusCode::UNAUTHORIZED, "secret_not_found")
            }
            WebhookAuthError::InvalidSignature => (StatusCode::UNAUTHORIZED, "invalid_signature"),
            WebhookAuthError::BodyReadError(_) => (StatusCode::BAD_REQUEST, "body_read_error"),
        }
    }
}

impl IntoResponse for WebhookAuthError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = serde_json::json!({
            "error": code,
            "message": self.to_string(),
        });
        (status, axum::Json(body)).into_response()
    }
}

impl std::fmt::Display for WebhookAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookAuthError::MissingSignature => write!(f, "X-Aegis-Signature header is required"),
            WebhookAuthError::MalformedSignature => {
                write!(f, "X-Aegis-Signature must be in the format 'sha256=<hex>'")
            }
            WebhookAuthError::InvalidHex => {
                write!(f, "X-Aegis-Signature contains invalid hex encoding")
            }
            WebhookAuthError::SecretNotFound { source } => {
                write!(f, "No HMAC secret configured for source '{source}'")
            }
            WebhookAuthError::InvalidSignature => {
                write!(f, "Webhook signature verification failed")
            }
            WebhookAuthError::BodyReadError(e) => write!(f, "Failed to read request body: {e}"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// WebhookHmacGuard
// ──────────────────────────────────────────────────────────────────────────────

/// Verifies the HMAC-SHA256 signature of an incoming webhook request.
///
/// On success, contains:
/// - `source`: the URL path segment (e.g. `"github"`, `"stripe"`)
/// - `body`: the verified raw request body bytes
///
/// Call [`WebhookHmacGuard::from_request`] to perform verification. This type
/// is intentionally not an axum extractor — callers hold the `AppState` and
/// pass the provider directly, which avoids the orphan-rule conflict that arises
/// when trying to implement `FromRef<Arc<AppState>>` for a type defined in a
/// foreign crate.
pub struct WebhookHmacGuard {
    pub source: String,
    /// Tenant whose `(tenant, source)` secret successfully verified the
    /// HMAC. Audit 002 §4.1 / §4.2: webhooks are unauthenticated, so the
    /// owning tenant can only be determined from the registration whose
    /// secret matched. Caller-supplied tenant headers are NEVER trusted.
    pub tenant_id: TenantId,
    pub body: Bytes,
}

impl WebhookHmacGuard {
    /// Verify the HMAC-SHA256 signature of `req` against every candidate
    /// `(tenant, source)` secret and return the matching tenant.
    ///
    /// `candidate_tenants` MUST be the set of tenants that have
    /// registered the source name in `WorkflowRegistry`. The body is
    /// read once with a finite cap (`MAX_WEBHOOK_BODY_BYTES`) BEFORE any
    /// HMAC work, closing audit 002 §4.21.
    ///
    /// Each candidate tenant's secret is fetched and the request HMAC
    /// is constant-time compared against it. We iterate the entire list
    /// even after a match to keep timing flat (audit 002 §4.2 / §4.13).
    ///
    /// Returns [`WebhookAuthError::InvalidSignature`] when no candidate
    /// matches, or [`WebhookAuthError::SecretNotFound`] when the
    /// candidate set is empty.
    pub async fn from_request(
        req: Request,
        provider: &dyn WebhookSecretProvider,
        candidate_tenants: &[TenantId],
    ) -> Result<Self, WebhookAuthError> {
        // ── Extract source from the URI path (last segment) ───────────────────
        // Path: /v1/webhooks/{source}  → last segment = source
        let source = req
            .uri()
            .path()
            .split('/')
            .rfind(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();

        // ── Extract X-Aegis-Signature header ──────────────────────────────────
        let sig_header = req
            .headers()
            .get("X-Aegis-Signature")
            .ok_or(WebhookAuthError::MissingSignature)?
            .to_str()
            .map_err(|_| WebhookAuthError::MalformedSignature)?
            .to_string();

        let hex_sig = sig_header
            .strip_prefix("sha256=")
            .ok_or(WebhookAuthError::MalformedSignature)?;

        let expected_bytes = hex::decode(hex_sig).map_err(|_| WebhookAuthError::InvalidHex)?;

        if candidate_tenants.is_empty() {
            warn!(
                source = %source,
                "No tenant has registered this webhook source; rejecting"
            );
            return Err(WebhookAuthError::SecretNotFound {
                source: source.clone(),
            });
        }

        // ── Read body bytes (bounded) ─────────────────────────────────────────
        let body_bytes = axum::body::to_bytes(req.into_body(), MAX_WEBHOOK_BODY_BYTES)
            .await
            .map_err(|e| WebhookAuthError::BodyReadError(e.to_string()))?;

        // ── Try every (tenant, source) secret with constant-time compare ─────
        let mut matched: Option<TenantId> = None;
        for tenant in candidate_tenants {
            let secret = match provider.get_secret(tenant, &source).await {
                Some(s) => s,
                None => continue,
            };

            let mut mac = HmacSha256::new_from_slice(&secret).expect("HMAC accepts any key length");
            mac.update(&body_bytes);
            let computed = mac.finalize().into_bytes();

            let ct_match = computed.ct_eq(expected_bytes.as_slice());
            if ct_match.unwrap_u8() == 1 && matched.is_none() {
                matched = Some(tenant.clone());
                // Do not break: keep iteration count flat for timing.
            }
        }

        let tenant_id = matched.ok_or_else(|| {
            warn!(
                source = %source,
                "Webhook HMAC signature verification failed for all candidate tenants"
            );
            WebhookAuthError::InvalidSignature
        })?;

        debug!(
            source = %source,
            tenant_id = %tenant_id,
            body_len = body_bytes.len(),
            "Webhook HMAC verified"
        );

        Ok(WebhookHmacGuard {
            source,
            tenant_id,
            body: body_bytes,
        })
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper: compute signature for outbound testing
// ──────────────────────────────────────────────────────────────────────────────

/// Compute the `sha256=<hex>` signature for a body + secret.
///
/// Useful in tests and for generating `X-Aegis-Signature` in client code.
pub fn compute_webhook_signature(body: &[u8], secret: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let result = mac.finalize().into_bytes();
    format!("sha256={}", hex::encode(result))
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_signature_is_deterministic() {
        let body = b"hello world";
        let secret = b"s3cr3t";
        let sig1 = compute_webhook_signature(body, secret);
        let sig2 = compute_webhook_signature(body, secret);
        assert_eq!(sig1, sig2);
        assert!(sig1.starts_with("sha256="));
    }

    #[test]
    fn compute_signature_different_secrets_differ() {
        let body = b"hello world";
        let sig1 = compute_webhook_signature(body, b"secret-a");
        let sig2 = compute_webhook_signature(body, b"secret-b");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn compute_signature_different_bodies_differ() {
        let secret = b"s3cr3t";
        let sig1 = compute_webhook_signature(b"body-1", secret);
        let sig2 = compute_webhook_signature(b"body-2", secret);
        assert_ne!(sig1, sig2);
    }

    fn ten(slug: &str) -> TenantId {
        TenantId::from_realm_slug(slug).unwrap()
    }

    #[tokio::test]
    async fn env_provider_returns_none_for_missing_var() {
        let provider = EnvWebhookSecretProvider;
        let result = provider
            .get_secret(&ten("u-nonexistent"), "nonexistent-source-xyz")
            .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn env_provider_normalises_tenant_and_source() {
        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_TENANT_X_MY_SOURCE", "test-secret");
        let provider = EnvWebhookSecretProvider;
        let result = provider.get_secret(&ten("u-tenant-x"), "my-source").await;
        assert_eq!(result, Some(b"test-secret".to_vec()));
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_TENANT_X_MY_SOURCE");
    }

    #[tokio::test]
    async fn env_provider_distinguishes_tenants_for_same_source() {
        // Audit 002 §4.2 regression: two tenants registering the same
        // source name MUST get distinct secrets. Pre-fix the lookup was
        // keyed only by source, so a single env var authenticated both.
        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_TENANT_A_SHARED_SRC", "secret-a");
        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_TENANT_B_SHARED_SRC", "secret-b");
        let provider = EnvWebhookSecretProvider;
        let a = provider
            .get_secret(&ten("u-tenant-a"), "shared-src")
            .await
            .unwrap();
        let b = provider
            .get_secret(&ten("u-tenant-b"), "shared-src")
            .await
            .unwrap();
        assert_eq!(a, b"secret-a".to_vec());
        assert_eq!(b, b"secret-b".to_vec());
        assert_ne!(a, b);
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_TENANT_A_SHARED_SRC");
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_TENANT_B_SHARED_SRC");
    }

    #[tokio::test]
    async fn from_request_rejects_missing_signature_header() {
        use axum::body::Body;
        use axum::http::Request;

        let provider = EnvWebhookSecretProvider;
        let req = Request::builder()
            .uri("/v1/webhooks/test-source")
            .body(Body::from(b"hello".as_slice()))
            .unwrap();

        let result = WebhookHmacGuard::from_request(req, &provider, &[ten("u-test")]).await;
        assert!(
            matches!(result, Err(WebhookAuthError::MissingSignature)),
            "expected MissingSignature, got {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn from_request_rejects_invalid_signature() {
        use axum::body::Body;
        use axum::http::Request;

        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_TEST_TEST_SOURCE", "correct-secret");

        let provider = EnvWebhookSecretProvider;
        let wrong_sig = compute_webhook_signature(b"hello", b"wrong-secret");

        let req = Request::builder()
            .uri("/v1/webhooks/test-source")
            .header("X-Aegis-Signature", wrong_sig)
            .body(Body::from(b"hello".as_slice()))
            .unwrap();

        let result = WebhookHmacGuard::from_request(req, &provider, &[ten("u-test")]).await;
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_TEST_TEST_SOURCE");

        assert!(
            matches!(result, Err(WebhookAuthError::InvalidSignature)),
            "expected InvalidSignature, got {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn from_request_accepts_valid_signature() {
        use axum::body::Body;
        use axum::http::Request;

        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_GOOD_GOOD_SOURCE", "my-secret");

        let provider = EnvWebhookSecretProvider;
        let body = b"payload";
        let sig = compute_webhook_signature(body, b"my-secret");

        let req = Request::builder()
            .uri("/v1/webhooks/good-source")
            .header("X-Aegis-Signature", sig)
            .body(Body::from(body.as_slice()))
            .unwrap();

        let result = WebhookHmacGuard::from_request(req, &provider, &[ten("u-good")]).await;
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_GOOD_GOOD_SOURCE");

        let guard = result.expect("expected Ok guard");
        assert_eq!(guard.source, "good-source");
        assert_eq!(guard.body.as_ref(), b"payload");
        assert_eq!(guard.tenant_id, ten("u-good"));
    }

    /// Audit 002 §4.2 regression: tenant-A's HMAC signature MUST NOT
    /// validate against tenant-B's `(tenant, source)` secret. Pre-fix
    /// the provider was keyed only by source, so a leaked tenant-A
    /// secret would also authenticate envelopes claiming tenant-B.
    #[tokio::test]
    async fn from_request_rejects_cross_tenant_signature() {
        use axum::body::Body;
        use axum::http::Request;

        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_TENANT_A_X_SRC", "secret-a");
        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_TENANT_B_X_SRC", "secret-b");

        let provider = EnvWebhookSecretProvider;
        let body = b"payload";
        // Signed with tenant-A's secret …
        let sig = compute_webhook_signature(body, b"secret-a");
        // … but the candidate set we feed to the guard is only tenant-B.
        let req = Request::builder()
            .uri("/v1/webhooks/x-src")
            .header("X-Aegis-Signature", sig)
            .body(Body::from(body.as_slice()))
            .unwrap();

        let result = WebhookHmacGuard::from_request(req, &provider, &[ten("u-tenant-b")]).await;
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_TENANT_A_X_SRC");
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_TENANT_B_X_SRC");

        assert!(
            matches!(result, Err(WebhookAuthError::InvalidSignature)),
            "tenant-A signature must NOT validate under tenant-B's secret, got {:?}",
            result.err()
        );
    }

    /// Audit 002 §4.2: the guard MUST reject when no tenant has
    /// registered the source — there is no fallback to a global secret.
    #[tokio::test]
    async fn from_request_rejects_when_no_candidate_tenants() {
        use axum::body::Body;
        use axum::http::Request;

        let provider = EnvWebhookSecretProvider;
        let body = b"payload";
        let sig = compute_webhook_signature(body, b"any");
        let req = Request::builder()
            .uri("/v1/webhooks/orphan")
            .header("X-Aegis-Signature", sig)
            .body(Body::from(body.as_slice()))
            .unwrap();

        let result = WebhookHmacGuard::from_request(req, &provider, &[]).await;
        assert!(
            matches!(result, Err(WebhookAuthError::SecretNotFound { .. })),
            "expected SecretNotFound for empty candidate set, got {:?}",
            result.err()
        );
    }

    /// Audit 002 §4.21 regression: a body exceeding
    /// `MAX_WEBHOOK_BODY_BYTES` MUST be rejected before HMAC work
    /// (no `usize::MAX` cap). Pre-fix the guard read the entire body
    /// into memory, enabling pre-auth memory DoS.
    #[tokio::test]
    async fn from_request_rejects_body_over_max_cap() {
        use axum::body::Body;
        use axum::http::Request;

        std::env::set_var("AEGIS_WEBHOOK_SECRET_U_LIMIT_LIMIT_SRC", "limit-secret");

        let provider = EnvWebhookSecretProvider;
        let oversized = vec![b'x'; MAX_WEBHOOK_BODY_BYTES + 1];
        // Sign correctly so we know it is the body cap (not the HMAC)
        // that rejects.
        let sig = compute_webhook_signature(&oversized, b"limit-secret");

        let req = Request::builder()
            .uri("/v1/webhooks/limit-src")
            .header("X-Aegis-Signature", sig)
            .body(Body::from(oversized))
            .unwrap();

        let result = WebhookHmacGuard::from_request(req, &provider, &[ten("u-limit")]).await;
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_U_LIMIT_LIMIT_SRC");

        assert!(
            matches!(result, Err(WebhookAuthError::BodyReadError(_))),
            "expected BodyReadError once the cap is exceeded, got {:?}",
            result.err()
        );
    }
}
