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
use tracing::warn;

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

        // Parse Stripe-Signature: t=TIMESTAMP,v1=SIGNATURE
        let mut timestamp = "";
        let mut sig_v1 = "";
        for part in stripe_sig.split(',') {
            if let Some(t) = part.strip_prefix("t=") {
                timestamp = t;
            } else if let Some(v) = part.strip_prefix("v1=") {
                sig_v1 = v;
            }
        }

        if timestamp.is_empty() || sig_v1.is_empty() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Malformed Stripe-Signature header" })),
            )
                .into_response();
        }

        // Compute expected signature: HMAC-SHA256(webhook_secret, "TIMESTAMP.BODY")
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let mut mac = match HmacSha256::new_from_slice(webhook_secret.as_bytes()) {
            Ok(m) => m,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Invalid webhook secret" })),
                )
                    .into_response();
            }
        };
        mac.update(timestamp.as_bytes());
        mac.update(b".");
        mac.update(&body_bytes);
        let expected = hex::encode(mac.finalize().into_bytes());

        if expected != sig_v1 {
            warn!("Stripe webhook signature mismatch");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid Stripe signature" })),
            )
                .into_response();
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
