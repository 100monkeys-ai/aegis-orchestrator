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
    extract::{Path, State},
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
use aegis_orchestrator_core::domain::stimulus::{Stimulus, StimulusSource};
use aegis_orchestrator_core::presentation::webhook_guard::WebhookHmacGuard;

// ──────────────────────────────────────────────────────────────────────────────
// Request body types
// ──────────────────────────────────────────────────────────────────────────────

/// Request body for `POST /v1/stimuli`.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct IngestStimulusBody {
    /// Source name (e.g. `"github"`, `"stripe"`, `"custom-webhook"`).
    pub(crate) source: String,
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

    match stimulus_service.ingest(stimulus).await {
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
/// Authentication is performed by [`WebhookHmacGuard`]: it reads the
/// `X-Aegis-Signature: sha256=<hex>` header and verifies it against the
/// secret configured for this source.
///
/// Returns `202 Accepted` with `{ stimulus_id, workflow_execution_id }`.
pub(crate) async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
    guard: WebhookHmacGuard,
) -> impl IntoResponse {
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

    match stimulus_service.ingest(stimulus).await {
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
