// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Human approval request handlers.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::state::AppState;

#[derive(serde::Deserialize)]
pub(crate) struct ApprovalRequest {
    feedback: Option<String>,
    approved_by: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct RejectionRequest {
    reason: String,
    rejected_by: Option<String>,
}

/// GET /v1/human-approvals - List all pending approval requests
pub(crate) async fn list_pending_approvals_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:list")?;
    let pending = state.human_input_service.list_pending_requests().await;
    Ok(Json(serde_json::json!({
        "pending_requests": pending,
        "count": pending.len()
    })))
}

/// GET /v1/human-approvals/:id - Get a specific pending approval request
pub(crate) async fn get_pending_approval_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Path(id): Path<String>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:read")?;
    let request_id = match Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Ok(Json(serde_json::json!({"error": "Invalid request ID"}))),
    };

    match state
        .human_input_service
        .get_pending_request(request_id)
        .await
    {
        Some(request) => Ok(Json(serde_json::json!({ "request": request }))),
        None => Ok(Json(
            serde_json::json!({ "error": "Request not found or already completed" }),
        )),
    }
}

/// POST /v1/human-approvals/:id/approve - Approve a pending request
pub(crate) async fn approve_request_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Path(id): Path<String>,
    Json(payload): Json<ApprovalRequest>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:approve")?;
    let request_id = match Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Ok(Json(serde_json::json!({"error": "Invalid request ID"}))),
    };

    match state
        .human_input_service
        .submit_approval(request_id, payload.feedback, payload.approved_by)
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "approved",
            "request_id": id
        }))),
        Err(e) => Ok(Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

/// POST /v1/human-approvals/:id/reject - Reject a pending request
pub(crate) async fn reject_request_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Path(id): Path<String>,
    Json(payload): Json<RejectionRequest>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:reject")?;
    let request_id = match Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Ok(Json(serde_json::json!({"error": "Invalid request ID"}))),
    };

    match state
        .human_input_service
        .submit_rejection(request_id, payload.reason, payload.rejected_by)
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "rejected",
            "request_id": id
        }))),
        Err(e) => Ok(Json(serde_json::json!({ "error": e.to_string() }))),
    }
}
