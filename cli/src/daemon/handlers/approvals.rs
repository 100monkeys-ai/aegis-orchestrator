// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Human approval request handlers.
//!
//! ADR-097 §approvals: non-operator callers MUST be tenant-scoped — they
//! may only see/approve/reject requests in their own tenant. Operators
//! see and act on all tenants' requests.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::{is_operator, tenant_id_from_identity};
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

/// GET /v1/human-approvals - List pending approval requests.
///
/// Operators see every tenant's pending requests; non-operators see only
/// their own tenant's. Before this gate the handler leaked cross-tenant
/// requests to anyone holding `approval:list`.
pub(crate) async fn list_pending_approvals_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:list")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let pending = if is_operator(identity_ref) {
        state.human_input_service.list_pending_requests().await
    } else {
        let tenant_id = tenant_id_from_identity(identity_ref);
        state
            .human_input_service
            .list_pending_requests_for_tenant(&tenant_id)
            .await
    };
    Ok(Json(serde_json::json!({
        "count": pending.len(),
        "pending_requests": pending,
    })))
}

/// GET /v1/human-approvals/:id - Get a specific pending approval request.
pub(crate) async fn get_pending_approval_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
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

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_filter = if is_operator(identity_ref) {
        None
    } else {
        Some(tenant_id_from_identity(identity_ref))
    };

    match state
        .human_input_service
        .get_pending_request_for_tenant(tenant_filter.as_ref(), request_id)
        .await
    {
        Some(request) => Ok(Json(serde_json::json!({ "request": request }))),
        None => Ok(Json(
            serde_json::json!({ "error": "Request not found or already completed" }),
        )),
    }
}

/// POST /v1/human-approvals/:id/approve - Approve a pending request.
pub(crate) async fn approve_request_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
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

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_filter = if is_operator(identity_ref) {
        None
    } else {
        Some(tenant_id_from_identity(identity_ref))
    };

    match state
        .human_input_service
        .submit_approval_for_tenant(
            tenant_filter.as_ref(),
            request_id,
            payload.feedback,
            payload.approved_by,
        )
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "approved",
            "request_id": id
        }))),
        Err(e) => Ok(Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

/// POST /v1/human-approvals/:id/reject - Reject a pending request.
pub(crate) async fn reject_request_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
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

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_filter = if is_operator(identity_ref) {
        None
    } else {
        Some(tenant_id_from_identity(identity_ref))
    };

    match state
        .human_input_service
        .submit_rejection_for_tenant(
            tenant_filter.as_ref(),
            request_id,
            payload.reason,
            payload.rejected_by,
        )
        .await
    {
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "rejected",
            "request_id": id
        }))),
        Err(e) => Ok(Json(serde_json::json!({ "error": e.to_string() }))),
    }
}
