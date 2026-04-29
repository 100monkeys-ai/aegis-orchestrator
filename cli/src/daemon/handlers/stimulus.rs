// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Stimulus HTTP Handlers (BC-8 — ADR-021)
//!
//! Two endpoints:
//!
//! | Method | Path | Auth | Handler |
//! |--------|------|------|---------|
//! | `POST` | `/v1/stimuli` | IAM/OIDC Bearer JWT | [`ingest_stimulus_handler`] |
//! | `POST` | `/v1/webhooks/{source}` | HMAC-SHA256 (`X-Aegis-Signature`) | [`webhook_handler`] |
//!
//! Both endpoints:
//! - Return `202 Accepted` with `{ stimulus_id, workflow_execution_id }` and `Location` header
//! - Return `409 Conflict` for idempotent duplicates
//! - Return `422 Unprocessable Entity` for low-confidence classification
//! - Return `503 Service Unavailable` if stimulus service is not wired in app state

use axum::{
    extract::{Extension, Request, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::Engine as _;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tracing::{info, warn};

use crate::daemon::state::AppState;
use aegis_orchestrator_core::application::stimulus::StimulusError;
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::stimulus::{Stimulus, StimulusSource};
use aegis_orchestrator_core::presentation::webhook_guard::WebhookHmacGuard;

// ──────────────────────────────────────────────────────────────────────────────
// Request body types
// ──────────────────────────────────────────────────────────────────────────────

/// Request body for `POST /v1/stimuli`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct IngestStimulusBody {
    /// Raw stimulus content (JSON, plain text, or base64-encoded binary).
    pub(crate) content: String,
    /// Optional idempotency key to prevent duplicate processing.
    pub(crate) idempotency_key: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Maximum allowed clock skew between the Stripe webhook timestamp and our clock.
/// Matches Stripe's recommended 5-minute tolerance (see Finding 4.12).
pub(crate) const STRIPE_SIGNATURE_TOLERANCE_SECS: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StripeSigError {
    /// Header missing required `t=` or `v1=` components, or t was not a valid integer.
    Malformed,
    /// HMAC could not be initialised with the provided secret.
    InvalidSecret,
    /// `|now - t| > STRIPE_SIGNATURE_TOLERANCE_SECS` — replay window exceeded.
    OutsideTolerance,
    /// Constant-time signature comparison failed.
    Mismatch,
}

/// Verify a Stripe webhook signature header against `body` using `secret`.
///
/// Implements Stripe's `Stripe-Signature` scheme:
///   `t=<unix-ts>,v1=<hex-hmac>`  — HMAC-SHA256 over `"<t>.<body>"`.
///
/// Hardening applied here (Findings 4.12 / 4.32):
/// 1. Signature compare uses [`subtle::ConstantTimeEq`] (no timing oracle).
/// 2. Timestamp must be within [`STRIPE_SIGNATURE_TOLERANCE_SECS`] of `now_unix`
///    (replay protection).
pub(crate) fn verify_stripe_signature(
    header: &str,
    body: &[u8],
    secret: &str,
    now_unix: i64,
) -> Result<(), StripeSigError> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use subtle::ConstantTimeEq;
    type HmacSha256 = Hmac<Sha256>;

    let mut timestamp = "";
    let mut sig_v1 = "";
    for part in header.split(',') {
        if let Some(t) = part.strip_prefix("t=") {
            timestamp = t;
        } else if let Some(v) = part.strip_prefix("v1=") {
            sig_v1 = v;
        }
    }

    if timestamp.is_empty() || sig_v1.is_empty() {
        return Err(StripeSigError::Malformed);
    }

    let ts: i64 = timestamp.parse().map_err(|_| StripeSigError::Malformed)?;

    if (now_unix - ts).abs() > STRIPE_SIGNATURE_TOLERANCE_SECS {
        return Err(StripeSigError::OutsideTolerance);
    }

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| StripeSigError::InvalidSecret)?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());

    // Constant-time compare on the hex bytes. Length mismatch yields ct_eq == 0.
    if expected.as_bytes().ct_eq(sig_v1.as_bytes()).into() {
        Ok(())
    } else {
        Err(StripeSigError::Mismatch)
    }
}

fn axum_headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.as_str().to_string(), val.to_string()))
        })
        .collect()
}

fn stimulus_error_response(e: StimulusError) -> (StatusCode, axum::Json<serde_json::Value>) {
    let status = match e.http_status() {
        409 => StatusCode::CONFLICT,
        422 => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!({
        "error": e.error_code(),
        "message": e.to_string(),
    });
    (status, Json(body))
}

fn workflow_execution_logs_location(workflow_execution_id: &str) -> String {
    format!("/v1/workflows/executions/{workflow_execution_id}/logs")
}

// ──────────────────────────────────────────────────────────────────────────────
// Handlers
// ──────────────────────────────────────────────────────────────────────────────

/// `POST /v1/stimuli` — Authenticated (IAM/OIDC Bearer JWT)
///
/// Accepts a stimulus from any authenticated caller. Auth is enforced by the
/// upstream middleware layer.
///
/// Returns `202 Accepted` with `{ stimulus_id, workflow_execution_id }`.
pub(crate) async fn ingest_stimulus_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Json(body): Json<IngestStimulusBody>,
) -> impl IntoResponse {
    let stimulus_service = match &state.stimulus_service {
        Some(svc) => svc.clone(),
        None => {
            warn!("StimulusService not configured; rejecting POST /v1/stimuli");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "stimulus_service_unavailable", "message": "Stimulus service is not configured" })),
            ).into_response();
        }
    };

    let stimulus = Stimulus::new(StimulusSource::HttpApi, body.content)
        .with_headers(axum_headers_to_map(&headers));
    let stimulus = if let Some(key) = body.idempotency_key {
        stimulus.with_idempotency_key(key)
    } else {
        stimulus
    };

    let caller_identity = identity.map(|ext| ext.0);
    match stimulus_service.ingest(stimulus, caller_identity).await {
        Ok(resp) => {
            let location = workflow_execution_logs_location(&resp.workflow_execution_id);
            let mut response_headers = HeaderMap::new();
            if let Ok(loc_val) = location.parse() {
                response_headers.insert(axum::http::header::LOCATION, loc_val);
            }
            (
                StatusCode::ACCEPTED,
                response_headers,
                Json(json!({
                    "stimulus_id": resp.stimulus_id.to_string(),
                    "workflow_execution_id": resp.workflow_execution_id,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let (status, body) = stimulus_error_response(e);
            (status, body).into_response()
        }
    }
}

/// `POST /v1/webhooks/{source}` — HMAC-SHA256 verified
///
/// Accepts a raw webhook payload from an external system. The `source` path
/// parameter identifies the webhook source (e.g. `"github"`, `"stripe"`).
///
/// Authentication is performed by [`WebhookHmacGuard::from_request`]: it reads
/// the `X-Aegis-Signature: sha256=<hex>` header and verifies it against the
/// secret configured for this source.
///
/// Returns `202 Accepted` with `{ stimulus_id, workflow_execution_id }`.
pub(crate) async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> impl IntoResponse {
    // Extract source from the URI path: /v1/webhooks/{source}
    let source = req
        .uri()
        .path()
        .strip_prefix("/v1/webhooks/")
        .unwrap_or("")
        .to_string();

    let headers = req.headers().clone();

    // ── Stripe webhook: use Stripe signature verification ────────────────────
    // Stripe sends `Stripe-Signature` header, not `X-Aegis-Signature`.
    // Verify using HMAC-SHA256 with the Stripe webhook secret from BillingConfig.
    if source == "stripe" {
        let stripe_sig = match headers
            .get("stripe-signature")
            .and_then(|v| v.to_str().ok())
        {
            Some(s) => s.to_string(),
            None => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "Missing Stripe-Signature header" })),
                )
                    .into_response();
            }
        };

        let body_bytes = match axum::body::to_bytes(req.into_body(), 1_048_576).await {
            Ok(b) => b,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "Failed to read request body" })),
                )
                    .into_response();
            }
        };

        // Verify Stripe signature: extract timestamp and v1 signature from header
        let webhook_secret = state.billing_config.as_ref().and_then(|c| {
            use aegis_orchestrator_core::domain::node_config::resolve_env_value;
            c.stripe_webhook_secret
                .as_ref()
                .and_then(|s| resolve_env_value(s).ok())
        });

        let webhook_secret = match webhook_secret {
            Some(s) if !s.is_empty() => s,
            _ => {
                warn!("Stripe webhook secret not configured");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "Stripe webhook secret not configured" })),
                )
                    .into_response();
            }
        };

        // Verify Stripe signature with constant-time compare and replay tolerance.
        let now_unix = chrono::Utc::now().timestamp();
        match verify_stripe_signature(&stripe_sig, &body_bytes, &webhook_secret, now_unix) {
            Ok(()) => {}
            Err(StripeSigError::Malformed) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "Malformed Stripe-Signature header" })),
                )
                    .into_response();
            }
            Err(StripeSigError::InvalidSecret) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Invalid webhook secret" })),
                )
                    .into_response();
            }
            Err(StripeSigError::OutsideTolerance) => {
                warn!("Stripe webhook timestamp outside tolerance window — possible replay");
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "Stripe signature timestamp outside tolerance" })),
                )
                    .into_response();
            }
            Err(StripeSigError::Mismatch) => {
                warn!("Stripe webhook signature mismatch");
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "Invalid Stripe signature" })),
                )
                    .into_response();
            }
        }

        // Signature verified — process Stripe event
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&body_str) {
            let event_type = event
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown");
            let data = event
                .get("data")
                .and_then(|d| d.get("object"))
                .cloned()
                .unwrap_or_default();
            info!(event_type, "Processing Stripe webhook event");

            crate::daemon::handlers::billing::process_stripe_event(&state, event_type, &data).await;
        }

        return (StatusCode::OK, Json(json!({ "received": true }))).into_response();
    }

    // ── HMAC verification (non-Stripe webhooks) ─────────────────────────────
    let guard =
        match WebhookHmacGuard::from_request(req, state.webhook_secret_provider.as_ref()).await {
            Ok(g) => g,
            Err(e) => return e.into_response(),
        };

    let stimulus_service = match &state.stimulus_service {
        Some(svc) => svc.clone(),
        None => {
            warn!(source = %source, "StimulusService not configured; rejecting webhook");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "stimulus_service_unavailable", "message": "Stimulus service is not configured" })),
            ).into_response();
        }
    };

    // Convert verified body bytes to String (treat as UTF-8 if possible, else base64)
    let content = String::from_utf8(guard.body.to_vec())
        .unwrap_or_else(|_| base64::engine::general_purpose::STANDARD.encode(&guard.body));

    let stimulus = Stimulus::new(
        StimulusSource::Webhook {
            source_name: source.clone(),
        },
        content,
    )
    .with_headers(axum_headers_to_map(&headers));

    match stimulus_service.ingest(stimulus, None).await {
        Ok(resp) => {
            let location = workflow_execution_logs_location(&resp.workflow_execution_id);
            let mut response_headers = HeaderMap::new();
            if let Ok(loc_val) = location.parse() {
                response_headers.insert(axum::http::header::LOCATION, loc_val);
            }
            (
                StatusCode::ACCEPTED,
                response_headers,
                Json(json!({
                    "stimulus_id": resp.stimulus_id.to_string(),
                    "workflow_execution_id": resp.workflow_execution_id,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let (status, body) = stimulus_error_response(e);
            (status, body).into_response()
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Regression tests — Findings 4.12 / 4.32 (security-audits/002 §4.12, §4.32)
// ──────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod stripe_signature_tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    type HmacSha256 = Hmac<Sha256>;

    const SECRET: &str = "whsec_test_signing_key";

    fn sign(ts: i64, body: &[u8], secret: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(ts.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        let v1 = hex::encode(mac.finalize().into_bytes());
        format!("t={ts},v1={v1}")
    }

    /// A correctly-signed envelope inside the tolerance window must verify.
    #[test]
    fn valid_signature_inside_window_is_accepted() {
        let now = 1_700_000_000;
        let body = br#"{"type":"checkout.session.completed"}"#;
        let header = sign(now, body, SECRET);
        assert_eq!(verify_stripe_signature(&header, body, SECRET, now), Ok(()));
    }

    /// 4.12 regression: an envelope older than `STRIPE_SIGNATURE_TOLERANCE_SECS`
    /// must be rejected even though the HMAC is still arithmetically valid.
    /// Before the fix this returned Ok and enabled webhook replay.
    #[test]
    fn replay_outside_tolerance_window_is_rejected() {
        let signed_at = 1_700_000_000;
        let body = br#"{"type":"customer.subscription.updated"}"#;
        let header = sign(signed_at, body, SECRET);
        let now = signed_at + STRIPE_SIGNATURE_TOLERANCE_SECS + 1;
        assert_eq!(
            verify_stripe_signature(&header, body, SECRET, now),
            Err(StripeSigError::OutsideTolerance)
        );
        let now_future = signed_at - STRIPE_SIGNATURE_TOLERANCE_SECS - 1;
        assert_eq!(
            verify_stripe_signature(&header, body, SECRET, now_future),
            Err(StripeSigError::OutsideTolerance)
        );
    }

    /// 4.12 regression: a captured (header, body) pair re-sent inside the
    /// tolerance window verifies successfully (idempotency is the downstream
    /// handler's responsibility); but **outside** the window it is rejected.
    #[test]
    fn replay_inside_window_verifies_outside_window_does_not() {
        let signed_at = 1_700_000_000;
        let body = br#"{"type":"checkout.session.completed","id":"evt_1"}"#;
        let header = sign(signed_at, body, SECRET);

        assert_eq!(
            verify_stripe_signature(&header, body, SECRET, signed_at + 30),
            Ok(())
        );

        assert_eq!(
            verify_stripe_signature(&header, body, SECRET, signed_at + 600),
            Err(StripeSigError::OutsideTolerance)
        );
    }

    /// 4.12 regression: tampering with the body invalidates the signature even
    /// when the original `(t, v1)` header is replayed inside the window.
    #[test]
    fn tampered_body_with_valid_old_signature_is_rejected() {
        let signed_at = 1_700_000_000;
        let original = br#"{"amount":1000}"#;
        let header = sign(signed_at, original, SECRET);
        let tampered = br#"{"amount":9999}"#;
        assert_eq!(
            verify_stripe_signature(&header, tampered, SECRET, signed_at),
            Err(StripeSigError::Mismatch)
        );
    }

    /// 4.32 regression: a signature differing by a single trailing character
    /// must be rejected. Constant-time compare handles this; this test pins
    /// behaviour against a tampered v1.
    #[test]
    fn tampered_signature_is_rejected_constant_time() {
        let signed_at = 1_700_000_000;
        let body = b"payload";
        let valid = sign(signed_at, body, SECRET);
        let mut bytes: Vec<u8> = valid.into_bytes();
        let last = bytes.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert_eq!(
            verify_stripe_signature(&tampered, body, SECRET, signed_at),
            Err(StripeSigError::Mismatch)
        );
    }

    /// 4.32 regression: signatures of the wrong length must be rejected
    /// (and must not panic in the constant-time compare).
    #[test]
    fn wrong_length_signature_is_rejected() {
        let signed_at = 1_700_000_000;
        let body = b"payload";
        let header_short = format!("t={signed_at},v1=deadbeef");
        let header_long = format!("t={signed_at},v1={}", "a".repeat(128));
        assert_eq!(
            verify_stripe_signature(&header_short, body, SECRET, signed_at),
            Err(StripeSigError::Mismatch)
        );
        assert_eq!(
            verify_stripe_signature(&header_long, body, SECRET, signed_at),
            Err(StripeSigError::Mismatch)
        );
    }

    /// Malformed headers (no `t=`, no `v1=`, non-numeric `t=`) are rejected
    /// without crashing.
    #[test]
    fn malformed_header_is_rejected() {
        let body = b"payload";
        assert_eq!(
            verify_stripe_signature("v1=deadbeef", body, SECRET, 0),
            Err(StripeSigError::Malformed)
        );
        assert_eq!(
            verify_stripe_signature("t=1700000000", body, SECRET, 1_700_000_000),
            Err(StripeSigError::Malformed)
        );
        assert_eq!(
            verify_stripe_signature("t=not-a-number,v1=deadbeef", body, SECRET, 0),
            Err(StripeSigError::Malformed)
        );
    }

    /// 4.32 CI gate: the Stripe verification path MUST NOT use raw `==` / `!=`
    /// on signature byte sequences. Grep our own source as a guard against
    /// future regressions.
    #[test]
    fn no_raw_eq_compare_in_stripe_verify_path() {
        let src = include_str!("stimulus.rs");
        let fn_start = src
            .find("pub(crate) fn verify_stripe_signature")
            .expect("verify_stripe_signature must exist");
        let fn_body = &src[fn_start..];
        let fn_end = fn_body
            .find("\nfn axum_headers_to_map")
            .or_else(|| fn_body.find("\n}\n\nfn "))
            .unwrap_or(fn_body.len());
        let fn_body = &fn_body[..fn_end];
        assert!(
            fn_body.contains("ct_eq("),
            "verify_stripe_signature must use ConstantTimeEq::ct_eq for signature compare"
        );
        assert!(
            !fn_body.contains("expected != sig_v1") && !fn_body.contains("expected == sig_v1"),
            "verify_stripe_signature must NOT use raw == / != on signature bytes"
        );
    }
}
