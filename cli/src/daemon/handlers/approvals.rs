// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Human approval request handlers.
//!
//! ADR-097 §approvals: non-operator callers MUST be tenant-scoped — they
//! may only see/approve/reject requests in their own tenant. Operators
//! see and act on all tenants' requests.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use uuid::Uuid;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::infrastructure::HumanInputService;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::{is_operator, refuse_read_only_operator, tenant_id_from_identity};

/// State of the human-approval sub-router.
#[derive(Clone)]
pub(crate) struct ApprovalsState {
    pub(crate) human_input_service: Arc<HumanInputService>,
}

/// The `/v1/human-approvals*` routes. Merged into the daemon router by
/// `router::create_router`, beneath the same authentication layers as every
/// other route.
pub(crate) fn approvals_router(state: ApprovalsState) -> Router {
    Router::new()
        .route("/v1/human-approvals", get(list_pending_approvals_handler))
        .route(
            "/v1/human-approvals/{id}",
            get(get_pending_approval_handler),
        )
        .route(
            "/v1/human-approvals/{id}/approve",
            post(approve_request_handler),
        )
        .route(
            "/v1/human-approvals/{id}/reject",
            post(reject_request_handler),
        )
        .with_state(state)
}

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
    State(state): State<ApprovalsState>,
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
    State(state): State<ApprovalsState>,
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
    State(state): State<ApprovalsState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<String>,
    Json(payload): Json<ApprovalRequest>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:approve")?;
    refuse_read_only_operator(identity.as_ref().map(|e| &e.0))?;
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
    State(state): State<ApprovalsState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<String>,
    Json(payload): Json<RejectionRequest>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("approval:reject")?;
    refuse_read_only_operator(identity.as_ref().map(|e| &e.0))?;
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

#[cfg(test)]
mod tests {
    //! Approve and reject are writes (ADR-073 §3e): an `aegis:readonly`
    //! operator may not perform them, and a consumer or tenant user may act
    //! only on requests in their own tenant (ADR-097 §approvals). Driven
    //! through the daemon's real authentication stack against the real
    //! in-memory `HumanInputService`; a refused call must leave the request
    //! pending.

    use super::{approvals_router, ApprovalsState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, serve, tenant_user,
    };
    use aegis_orchestrator_core::domain::execution::ExecutionId;
    use aegis_orchestrator_core::domain::iam::{AegisRole, UserIdentity};
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::HumanInputService;
    use std::sync::Arc;
    use uuid::Uuid;

    const SCOPES: &str = "approval:list approval:read approval:approve approval:reject";

    /// Open a pending request in `tenant` and return its id once the
    /// service lists it. The requester waits in a background task, as an
    /// execution does.
    async fn pending_request(svc: &Arc<HumanInputService>, tenant: TenantId) -> Uuid {
        let requester = svc.clone();
        tokio::spawn(async move {
            let _ = requester
                .request_input(tenant, ExecutionId::new(), "approve?".into(), 600)
                .await;
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(req) = svc.list_pending_requests().await.first() {
                return req.id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the pending request never appeared in the service"
            );
            tokio::task::yield_now().await;
        }
    }

    async fn is_pending(svc: &HumanInputService, id: Uuid) -> bool {
        svc.get_pending_request(id).await.is_some()
    }

    fn owner() -> UserIdentity {
        consumer("owner-sub")
    }

    fn owner_tenant() -> TenantId {
        TenantId::for_consumer_user("owner-sub").expect("owner tenant")
    }

    async fn act(
        svc: &Arc<HumanInputService>,
        caller: &UserIdentity,
        verb: &str,
        id: Uuid,
    ) -> (u16, serde_json::Value) {
        let base = serve(
            approvals_router(ApprovalsState {
                human_input_service: svc.clone(),
            }),
            Some(identity_provider(&[("caller", caller.clone(), SCOPES)])),
            None,
        )
        .await;
        let body = if verb == "approve" {
            serde_json::json!({ "feedback": "ok" })
        } else {
            serde_json::json!({ "reason": "no" })
        };
        send(
            &base,
            &reqwest::Method::POST,
            &format!("/v1/human-approvals/{id}/{verb}"),
            &Some(body),
            Some("caller"),
        )
        .await
    }

    #[tokio::test]
    async fn approvals_refuse_read_only_operators_and_callers_outside_the_tenant() {
        let callers = [
            ("aegis:readonly operator", operator(AegisRole::Readonly)),
            ("consumer in another tenant", consumer("other-sub")),
            ("tenant user of acme", tenant_user("acme-sub", "acme")),
        ];
        let mut failures = Vec::new();
        for (label, caller) in &callers {
            for verb in ["approve", "reject"] {
                let svc = Arc::new(HumanInputService::new());
                let id = pending_request(&svc, owner_tenant()).await;
                let (status, body) = act(&svc, caller, verb, id).await;
                if !is_pending(&svc, id).await {
                    failures.push(format!(
                        "{verb} by {label} completed another tenant's request (answered {status} {body})"
                    ));
                }
                if label.contains("readonly") && status != 403 {
                    failures.push(format!(
                        "{verb} by {label} answered {status} {body}; expected 403"
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "a caller without write authority acted on a human-approval request:\n{}",
            failures.join("\n")
        );
    }

    #[tokio::test]
    async fn approvals_serve_the_owning_tenant_and_writing_operators() {
        let callers = [
            ("owning consumer", owner()),
            ("aegis:admin operator", operator(AegisRole::Admin)),
            ("aegis:operator operator", operator(AegisRole::Operator)),
        ];
        let mut failures = Vec::new();
        for (label, caller) in &callers {
            for (verb, done) in [("approve", "approved"), ("reject", "rejected")] {
                let svc = Arc::new(HumanInputService::new());
                let id = pending_request(&svc, owner_tenant()).await;
                let (status, body) = act(&svc, caller, verb, id).await;
                if status != 200 || body["status"] != done || is_pending(&svc, id).await {
                    failures.push(format!(
                        "{verb} by {label} answered {status} {body}; expected 200 {done} and the request completed"
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "a caller with write authority was refused on a human-approval request:\n{}",
            failures.join("\n")
        );
    }
}
