// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # WebhookHmacGuard — Axum extractor for HMAC-SHA256 webhook verification (ADR-021)
//!
//! Extracts and verifies the `X-Aegis-Signature: sha256=<hex>` header on
//! incoming webhook requests. Uses constant-time comparison (`subtle` crate)
//! to prevent timing attacks.
//!
//! ## Usage
//!
//! ```rust,no_run
//! async fn my_handler(
//!     WebhookHmacGuard { source, body }: WebhookHmacGuard,
//! ) -> impl IntoResponse { ... }
//! ```
//!
//! The handler receives the verified raw body bytes. If verification fails,
//! the extractor returns `401 Unauthorized` before the handler is called.
//!
//! ## Secret Resolution
//!
//! Secrets are resolved via [`WebhookSecretProvider`]. The Phase 1 implementation
//! reads `AEGIS_WEBHOOK_SECRET_<SOURCE>` environment variables (upper-cased source
//! names, hyphens replaced with underscores). Phase 2 will swap to OpenBao KV
//! at path `aegis-system/kv/webhooks/{source}/hmac_secret` (ADR-034).

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{FromRef, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;

// ──────────────────────────────────────────────────────────────────────────────
// WebhookSecretProvider trait
// ──────────────────────────────────────────────────────────────────────────────

/// Provides HMAC secrets for webhook sources.
///
/// Phase 1: [`EnvWebhookSecretProvider`] reads environment variables.
/// Phase 2: Replace with OpenBao KV-backed implementation (ADR-034).
#[async_trait]
pub trait WebhookSecretProvider: Send + Sync {
    /// Return the raw secret bytes for the given source name.
    ///
    /// Returns `None` if the source is unknown or the secret is not configured.
    async fn get_secret(&self, source_name: &str) -> Option<Vec<u8>>;
}

/// Phase 1: reads `AEGIS_WEBHOOK_SECRET_{UPPER_SOURCE}` environment variables.
///
/// Source `my-webhook` → env var `AEGIS_WEBHOOK_SECRET_MY_WEBHOOK`.
pub struct EnvWebhookSecretProvider;

#[async_trait]
impl WebhookSecretProvider for EnvWebhookSecretProvider {
    async fn get_secret(&self, source_name: &str) -> Option<Vec<u8>> {
        let env_key = format!(
            "AEGIS_WEBHOOK_SECRET_{}",
            source_name.to_uppercase().replace(['-', '.'], "_")
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
// WebhookHmacGuard extractor
// ──────────────────────────────────────────────────────────────────────────────

/// Axum extractor that verifies the HMAC-SHA256 signature of an incoming webhook.
///
/// On success, the handler receives:
/// - `source`: the URL path segment (e.g. `"github"`, `"stripe"`)
/// - `body`: the verified raw request body bytes
pub struct WebhookHmacGuard {
    pub source: String,
    pub body: Bytes,
}

/// State fragment required by [`WebhookHmacGuard`].
///
/// Add `webhook_secret_provider: Arc<dyn WebhookSecretProvider>` to `AppState`
/// and implement `FromRef<AppState>` for `WebhookHmacState`.
pub struct WebhookHmacState {
    pub secret_provider: Arc<dyn WebhookSecretProvider>,
}

#[async_trait]
impl<S> axum::extract::FromRequest<S> for WebhookHmacGuard
where
    WebhookHmacState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = WebhookAuthError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let hmac_state = WebhookHmacState::from_ref(state);

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

        // ── Fetch secret for this source ──────────────────────────────────────
        let secret = hmac_state
            .secret_provider
            .get_secret(&source)
            .await
            .ok_or_else(|| {
                warn!(source = %source, "No HMAC secret configured for webhook source");
                WebhookAuthError::SecretNotFound {
                    source: source.clone(),
                }
            })?;

        // ── Read body bytes (must be done after header extraction) ────────────
        let body_bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
            .await
            .map_err(|e| WebhookAuthError::BodyReadError(e.to_string()))?;

        // ── Compute HMAC-SHA256 ───────────────────────────────────────────────
        let mut mac = HmacSha256::new_from_slice(&secret).expect("HMAC accepts any key length");
        mac.update(&body_bytes);
        let computed = mac.finalize().into_bytes();

        // ── Constant-time comparison ──────────────────────────────────────────
        let ct_match = computed.ct_eq(expected_bytes.as_slice());
        if ct_match.unwrap_u8() != 1 {
            warn!(
                source = %source,
                "Webhook HMAC signature verification failed"
            );
            return Err(WebhookAuthError::InvalidSignature);
        }

        debug!(source = %source, body_len = body_bytes.len(), "Webhook HMAC verified");

        Ok(WebhookHmacGuard {
            source,
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

    #[tokio::test]
    async fn env_provider_returns_none_for_missing_var() {
        let provider = EnvWebhookSecretProvider;
        let result = provider.get_secret("nonexistent-source-xyz").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn env_provider_normalises_source_name() {
        // Set a test env var
        std::env::set_var("AEGIS_WEBHOOK_SECRET_MY_SOURCE", "test-secret");
        let provider = EnvWebhookSecretProvider;
        let result = provider.get_secret("my-source").await;
        assert_eq!(result, Some(b"test-secret".to_vec()));
        std::env::remove_var("AEGIS_WEBHOOK_SECRET_MY_SOURCE");
    }
}
