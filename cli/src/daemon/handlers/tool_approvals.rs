// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool approval routes (AEGIS ADR-126 D4)
//!
//! How a user answers a gated tool call, and manages "always allow":
//!
//! | Route | Caller | Scope |
//! |-------|--------|-------|
//! | `GET /v1/tool-approvals?status=` | the user (own requests); an operator (every request) | `tool_approval:read` |
//! | `POST /v1/tool-approvals/{id}/decision` | the request's own user | `tool_approval:decide` |
//! | `GET /v1/tool-approval-policies` | the user (own policies) | `tool_approval:read` |
//! | `DELETE /v1/tool-approval-policies/{id}` | the policy's own user | `tool_approval:decide` |
//!
//! A user is a consumer or tenant user, matched by tenant and `sub`; another
//! user's request or policy is answered 404, exactly as one that does not
//! exist (as `/v1/credentials/{id}` answers another user's binding). An
//! operator reads the requests and never answers for a user (403). A
//! service account is refused (403). A second decision answers 409, as does
//! a decision on an expired request.
//!
//! On "once" and "always" the orchestrator runs the stored call through
//! the tool invocation service (the dispatch stages after the gate, as the
//! request's user) and the response carries its result.

use std::sync::Arc;

use aegis_orchestrator_core::application::tool_approval_service::{
    ApprovedCallRunner, ToolApprovalError, ToolApprovalService,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalDecision, ToolApprovalId, ToolApprovalPolicy, ToolApprovalPolicyId,
    ToolApprovalRequest, ToolApprovalStatus,
};
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::daemon::handlers::tenant_id_from_identity;

type Refusal = (StatusCode, Json<Value>);

/// State of the tool approval sub-router.
#[derive(Clone)]
pub(crate) struct ToolApprovalsState {
    /// `None` when the gate is not configured: every route answers 503.
    pub(crate) service: Option<Arc<ToolApprovalService>>,
    /// Runs an approved call: the tool invocation service.
    pub(crate) runner: Arc<dyn ApprovedCallRunner>,
}

/// The `/v1/tool-approvals*` and `/v1/tool-approval-policies*` routes,
/// merged into the daemon router by `router::create_router` beneath the
/// same authentication layers as every other route.
pub(crate) fn tool_approvals_router(state: ToolApprovalsState) -> Router {
    Router::new()
        .route("/v1/tool-approvals", get(list_tool_approvals_handler))
        .route(
            "/v1/tool-approvals/{id}/decision",
            post(decide_tool_approval_handler),
        )
        .route(
            "/v1/tool-approval-policies",
            get(list_tool_approval_policies_handler),
        )
        .route(
            "/v1/tool-approval-policies/{id}",
            delete(revoke_tool_approval_policy_handler),
        )
        .with_state(state)
}

/// Who is asking.
enum Caller {
    User { tenant_id: TenantId, sub: String },
    Operator,
}

fn refusal(status: StatusCode, error: &str) -> Refusal {
    (status, Json(json!({ "error": error })))
}

fn caller(identity: Option<&UserIdentity>, tenant: Option<&TenantId>) -> Result<Caller, Refusal> {
    let identity =
        identity.ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. } => Ok(Caller::User {
            tenant_id: tenant
                .cloned()
                .unwrap_or_else(|| tenant_id_from_identity(Some(identity))),
            sub: identity.sub.clone(),
        }),
        IdentityKind::Operator { .. } => Ok(Caller::Operator),
        IdentityKind::ServiceAccount { .. } => Err(refusal(
            StatusCode::FORBIDDEN,
            "Consumer or tenant user identity required",
        )),
    }
}

/// The caller as a user; an operator is refused, since only the user
/// answers for themself.
fn user(caller: Caller) -> Result<(TenantId, String), Refusal> {
    match caller {
        Caller::User { tenant_id, sub } => Ok((tenant_id, sub)),
        Caller::Operator => Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the request's own user may answer or manage tool approvals",
        )),
    }
}

fn service(state: &ToolApprovalsState) -> Result<&Arc<ToolApprovalService>, Refusal> {
    state.service.as_ref().ok_or_else(|| {
        refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "Tool approvals are not configured on this node",
        )
    })
}

fn from_service_error(e: ToolApprovalError) -> Refusal {
    match e {
        ToolApprovalError::NotFound => refusal(StatusCode::NOT_FOUND, "Not found"),
        ToolApprovalError::AlreadyDecided(status) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "already_decided", "status": status.as_str() })),
        ),
        ToolApprovalError::Expired => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "expired", "status": ToolApprovalStatus::Expired.as_str() })),
        ),
        ToolApprovalError::RequiresUser(_) | ToolApprovalError::Repository(_) => {
            tracing::error!(error = %e, "Tool approval store failed");
            refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Tool approval store failed",
            )
        }
    }
}

fn request_view(request: &ToolApprovalRequest) -> Value {
    json!({
        "id": request.id.to_string(),
        "tenant_id": request.tenant_id.as_str(),
        "user_sub": request.user_sub,
        "execution_id": request.execution_id.to_string(),
        "agent_id": request.agent_id.to_string(),
        "tool_name": request.tool_name,
        "arguments": request.arguments,
        "summary": request.summary,
        "binding_id": request.binding_id,
        "status": request.status.as_str(),
        "created_at": request.created_at,
        "decided_at": request.decided_at,
        "decided_by": request.decided_by,
        "result": request.result,
        "error": request.error,
    })
}

fn policy_view(policy: &ToolApprovalPolicy) -> Value {
    json!({
        "id": policy.id.to_string(),
        "tool_name": policy.tool_name,
        "binding_id": policy.binding_id,
        "created_at": policy.created_at,
        "created_by": policy.created_by,
    })
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct StatusQuery {
    status: Option<String>,
}

/// `GET /v1/tool-approvals?status=pending`: the caller's requests, newest
/// first; an operator's read lists every user's.
pub(crate) async fn list_tool_approvals_handler(
    State(state): State<ToolApprovalsState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("tool_approval:read")?;
    let caller = caller(identity.as_deref(), tenant.as_deref())?;
    let service = service(&state)?;
    let status = match query.status.as_deref() {
        None | Some("") => None,
        Some(text) => Some(ToolApprovalStatus::parse(text).ok_or_else(|| {
            refusal(StatusCode::BAD_REQUEST, &format!("Unknown status '{text}'"))
        })?),
    };
    let requests = match caller {
        Caller::User { tenant_id, sub } => service.list_for_user(&tenant_id, &sub, status).await,
        Caller::Operator => service.list_all(status).await,
    }
    .map_err(from_service_error)?;
    Ok(Json(json!({
        "count": requests.len(),
        "requests": requests.iter().map(request_view).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct DecisionBody {
    decision: ToolApprovalDecision,
}

/// `POST /v1/tool-approvals/{id}/decision` with `{"decision": "once" |
/// "always" | "deny"}`: the request's own user answers.
pub(crate) async fn decide_tool_approval_handler(
    State(state): State<ToolApprovalsState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
    Json(body): Json<DecisionBody>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("tool_approval:decide")?;
    let (tenant_id, sub) = user(caller(identity.as_deref(), tenant.as_deref())?)?;
    let service = service(&state)?;
    let id = ToolApprovalId::from_string(&id)
        .map_err(|_| refusal(StatusCode::NOT_FOUND, "Not found"))?;
    let decided = service
        .decide(id, &tenant_id, &sub, body.decision, state.runner.as_ref())
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "request": request_view(&decided) })))
}

/// `GET /v1/tool-approval-policies`: the caller's "always allow" policies.
pub(crate) async fn list_tool_approval_policies_handler(
    State(state): State<ToolApprovalsState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("tool_approval:read")?;
    let (tenant_id, sub) = user(caller(identity.as_deref(), tenant.as_deref())?)?;
    let policies = service(&state)?
        .list_policies(&tenant_id, &sub)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({
        "count": policies.len(),
        "policies": policies.iter().map(policy_view).collect::<Vec<_>>(),
    })))
}

/// `DELETE /v1/tool-approval-policies/{id}`: revoke an "always allow"; the
/// next matching call waits for its user again.
pub(crate) async fn revoke_tool_approval_policy_handler(
    State(state): State<ToolApprovalsState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<StatusCode, Refusal> {
    scope_guard.require("tool_approval:decide")?;
    let (tenant_id, sub) = user(caller(identity.as_deref(), tenant.as_deref())?)?;
    let service = service(&state)?;
    let id = ToolApprovalPolicyId::from_string(&id)
        .map_err(|_| refusal(StatusCode::NOT_FOUND, "Not found"))?;
    service
        .revoke_policy(id, &tenant_id, &sub)
        .await
        .map_err(from_service_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    //! Driven through the daemon's real authentication stack against the
    //! real service over the in-memory store, with a runner that records the
    //! calls it is asked to run.

    use super::{tool_approvals_router, ToolApprovalsState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, serve, service_account,
    };
    use aegis_orchestrator_core::application::tool_approval_service::{
        ApprovedCallRunner, GateOutcome, GatedCall, ToolApprovalService,
    };
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::execution::ExecutionId;
    use aegis_orchestrator_core::domain::iam::{AegisRole, UserIdentity};
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::domain::tool_approval::{ToolApprovalId, ToolApprovalRequest};
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
    use reqwest::Method;
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    const SCOPES: &str = "tool_approval:read tool_approval:decide";

    #[derive(Default)]
    struct RecordingRunner {
        ran: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait::async_trait]
    impl ApprovedCallRunner for RecordingRunner {
        async fn run_approved_call(&self, request: &ToolApprovalRequest) -> Result<Value, String> {
            self.ran
                .lock()
                .unwrap()
                .push((request.user_sub.clone(), request.arguments.clone()));
            Ok(json!({"sent": true}))
        }
    }

    struct Fixture {
        service: Arc<ToolApprovalService>,
        runner: Arc<RecordingRunner>,
        base: String,
    }

    async fn fixture(callers: &[(&str, UserIdentity, &str)]) -> Fixture {
        let service = Arc::new(ToolApprovalService::new(
            Arc::new(InMemoryToolApprovalRepository::new()),
            Arc::new(EventBus::new(64)),
        ));
        let runner = Arc::new(RecordingRunner::default());
        let base = serve(
            tool_approvals_router(ToolApprovalsState {
                service: Some(service.clone()),
                runner: runner.clone(),
            }),
            Some(identity_provider(callers)),
            None,
        )
        .await;
        Fixture {
            service,
            runner,
            base,
        }
    }

    fn owner() -> UserIdentity {
        consumer("owner-sub")
    }

    async fn pending(service: &ToolApprovalService, args: Value) -> ToolApprovalId {
        let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
        let outcome = service
            .gate(GatedCall {
                tenant_id: &tenant,
                user_sub: Some("owner-sub"),
                execution_id: ExecutionId::new(),
                agent_id: AgentId::new(),
                tool_name: "mail.send",
                arguments: &args,
                security_context_name: "zaru-pro",
            })
            .await
            .expect("gate");
        match outcome {
            GateOutcome::Pending { result } => {
                ToolApprovalId::from_string(result["approval_id"].as_str().unwrap()).unwrap()
            }
            other => panic!("expected pending, got {other:?}"),
        }
    }

    async fn decide(f: &Fixture, token: &str, id: ToolApprovalId, decision: &str) -> (u16, Value) {
        send(
            &f.base,
            &Method::POST,
            &format!("/v1/tool-approvals/{id}/decision"),
            &Some(json!({ "decision": decision })),
            Some(token),
        )
        .await
    }

    #[tokio::test]
    async fn the_user_lists_and_answers_once_and_a_second_decision_is_409() {
        let f = fixture(&[("owner", owner(), SCOPES)]).await;
        let args =
            json!({"mailbox": "b-1", "to": "x@example.com", "subject": "Hi", "body": "Hello"});
        let id = pending(&f.service, args.clone()).await;

        let (status, body) = send(
            &f.base,
            &Method::GET,
            "/v1/tool-approvals?status=pending",
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["count"], 1, "{body}");
        assert_eq!(body["requests"][0]["id"], id.to_string());
        assert!(body["requests"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("Subject: Hi"));

        let (status, body) = decide(&f, "owner", id, "once").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["request"]["status"], "approved_once", "{body}");
        assert_eq!(body["request"]["result"], json!({"sent": true}), "{body}");
        assert_eq!(
            *f.runner.ran.lock().unwrap(),
            vec![("owner-sub".to_string(), args)],
            "the stored call ran once, with the stored arguments, as its user"
        );

        let (status, body) = decide(&f, "owner", id, "deny").await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(f.runner.ran.lock().unwrap().len(), 1, "nothing ran again");
    }

    #[tokio::test]
    async fn another_user_gets_404_an_operator_reads_and_cannot_decide() {
        let f = fixture(&[
            ("owner", owner(), SCOPES),
            ("other", consumer("other-sub"), SCOPES),
            ("op", operator(AegisRole::Admin), SCOPES),
            ("svc", service_account(), SCOPES),
        ])
        .await;
        let id = pending(&f.service, json!({"mailbox": "b-1"})).await;

        let (status, body) = decide(&f, "other", id, "once").await;
        assert_eq!(status, 404, "another user's request: {body}");
        let (_, body) = send(
            &f.base,
            &Method::GET,
            "/v1/tool-approvals",
            &None,
            Some("other"),
        )
        .await;
        assert_eq!(body["count"], 0, "another user lists none: {body}");

        let (status, body) = send(
            &f.base,
            &Method::GET,
            "/v1/tool-approvals",
            &None,
            Some("op"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["count"], 1, "an operator reads every request: {body}");
        let (status, body) = decide(&f, "op", id, "once").await;
        assert_eq!(status, 403, "an operator never answers for a user: {body}");

        let (status, _) = decide(&f, "svc", id, "once").await;
        assert_eq!(status, 403);

        assert!(f.runner.ran.lock().unwrap().is_empty(), "nothing ran");
        let (status, body) = decide(&f, "owner", id, "deny").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["request"]["status"], "denied");
        assert!(f.runner.ran.lock().unwrap().is_empty(), "deny runs nothing");
    }

    #[tokio::test]
    async fn the_scopes_are_required() {
        let f = fixture(&[
            ("reader", owner(), "tool_approval:read"),
            ("none", owner(), "execution:read"),
        ])
        .await;
        let id = pending(&f.service, json!({})).await;
        let (status, _) = send(
            &f.base,
            &Method::GET,
            "/v1/tool-approvals",
            &None,
            Some("none"),
        )
        .await;
        assert_eq!(status, 403);
        let (status, _) = decide(&f, "reader", id, "once").await;
        assert_eq!(status, 403);
        assert!(f.runner.ran.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn always_writes_a_policy_the_user_lists_and_revokes() {
        let f = fixture(&[
            ("owner", owner(), SCOPES),
            ("other", consumer("other-sub"), SCOPES),
        ])
        .await;
        let id = pending(&f.service, json!({"mailbox": "b-1"})).await;
        let (status, body) = decide(&f, "owner", id, "always").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["request"]["status"], "approved_always");

        let (status, body) = send(
            &f.base,
            &Method::GET,
            "/v1/tool-approval-policies",
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["count"], 1, "{body}");
        assert_eq!(body["policies"][0]["tool_name"], "mail.send");
        assert_eq!(body["policies"][0]["binding_id"], "b-1");
        let policy_id = body["policies"][0]["id"].as_str().unwrap().to_string();

        let (status, _) = send(
            &f.base,
            &Method::DELETE,
            &format!("/v1/tool-approval-policies/{policy_id}"),
            &None,
            Some("other"),
        )
        .await;
        assert_eq!(status, 404, "another user cannot revoke it");
        let (status, _) = send(
            &f.base,
            &Method::DELETE,
            &format!("/v1/tool-approval-policies/{policy_id}"),
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 204);
        let (_, body) = send(
            &f.base,
            &Method::GET,
            "/v1/tool-approval-policies",
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(body["count"], 0, "{body}");
    }
}
