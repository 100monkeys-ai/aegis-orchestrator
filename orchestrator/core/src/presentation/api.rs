// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # HTTP/SSE Presentation Layer — Axum (BC-2, BC-1, BC-12)
//!
//! Axum-based HTTP handlers for the orchestrator REST API and the
//! Server-Sent Events (SSE) endpoint consumed by the Zaru client
//! (`zaru-client`) for real-time execution streaming (ADR-026).
//!
//! All handlers delegate immediately to application-layer use cases.
//! No business logic lives here — see AGENTS.md §Anti-Patterns §Smart UI.
//!
//! ## Route Surface
//! | Method | Path | Handler |
//! |--------|------|---------|
//! | `POST` | `/v1/executions` | start execution |
//! | `GET` | `/v1/executions/{id}/stream` | SSE event stream |
//! | `GET` | `/v1/human-approvals` | list pending approvals |
//! | `GET` | `/v1/human-approvals/{id}` | get pending approval |
//! | `POST` | `/v1/human-approvals/{id}/approve` | approve request |
//! | `POST` | `/v1/human-approvals/{id}/reject` | reject request |
//! | `POST` | `/v1/smcp/attest` | SMCP attestation handshake (ADR-035) |
//! | `POST` | `/v1/smcp/invoke` | SMCP tool invocation (ADR-033) |
//! | `POST` | `/v1/dispatch-gateway` | Dispatch gateway — inner loop orchestration (ADR-038) |
//! | `POST` | `/v1/stimuli` | Stimulus ingestion — IAM/OIDC Bearer auth (ADR-021) |
//! | `POST` | `/v1/webhooks/{source}` | Webhook ingestion — HMAC-SHA256 (ADR-021) |
//! | `GET` | `/health` | liveness probe |

use crate::application::execution::ExecutionService;
use crate::application::inner_loop_service::InnerLoopService;
use crate::application::stimulus::StimulusService;
use crate::application::tool_invocation_service::ToolInvocationService;
use crate::domain::agent::AgentId;
use crate::domain::cluster::{
    ConfigLayerRepository, ConfigScope, ConfigType, NodeClusterRepository, NodeId,
};
use crate::domain::dispatch::AgentMessage;
use crate::domain::execution::ExecutionInput;
use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::mcp::PolicyViolation;
use crate::domain::repository::{TenantRepository, WorkflowExecutionRepository};
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::security_context::SecurityContext;
use crate::domain::smcp_session::SmcpSessionError;
use crate::domain::smcp_session_repository::SmcpSessionRepository;
use crate::domain::tenancy::{Tenant, TenantQuotas, TenantStatus, TenantTier};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::rate_limit::override_repository::{
    CreateOverrideRequest, RateLimitOverrideRepository,
};
use crate::presentation::metrics_middleware::metrics_middleware;
use crate::presentation::stimulus_handlers::{ingest_stimulus_handler, webhook_handler};
use crate::presentation::tenant_middleware::tenant_context_middleware;
use crate::presentation::webhook_guard::{
    EnvWebhookSecretProvider, WebhookHmacState, WebhookSecretProvider,
};
use axum::{
    extract::{FromRef, Path, Query, State},
    middleware,
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, post, put},
    Json, Router,
};
use futures::stream::Stream;
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::StreamExt;

pub struct AppState {
    pub execution_service: Arc<dyn ExecutionService>,
    pub human_input_service: Arc<crate::infrastructure::HumanInputService>,
    /// Inner loop gateway service (ADR-038). Optional until wired.
    pub inner_loop_service: Option<Arc<InnerLoopService>>,
    /// BC-8: Stimulus routing and webhook ingestion service (ADR-021). Optional until wired.
    pub stimulus_service: Option<Arc<dyn StimulusService>>,
    /// BC-8: HMAC secret provider for webhook requests. Defaults to EnvWebhookSecretProvider.
    pub webhook_secret_provider: Arc<dyn WebhookSecretProvider>,
    /// SMCP tool invocation service (ADR-033). Optional until wired.
    pub tool_invocation_service: Option<Arc<ToolInvocationService>>,
    /// SMCP attestation service (ADR-035). Optional until wired by the daemon.
    pub attestation_service:
        Option<Arc<dyn crate::infrastructure::smcp::attestation::AttestationService>>,
    /// Workflow execution repository for event-log snapshots (BC-3). Optional until wired.
    pub workflow_execution_repo: Option<Arc<dyn WorkflowExecutionRepository>>,
    /// Event bus for workflow SSE streaming (ADR-030). Optional until wired.
    pub event_bus: Option<Arc<EventBus>>,
    /// Tenant repository for admin tenant management (ADR-056). Optional until wired.
    pub tenant_repo: Option<Arc<dyn TenantRepository>>,
    /// SMCP session repository for token-based auth on SSE streams (ADR-035). Optional until wired.
    pub smcp_session_repo: Option<Arc<dyn SmcpSessionRepository>>,
    /// Rate-limit override repository for admin CRUD (ADR-072). Optional until wired.
    pub rate_limit_override_repo: Option<Arc<RateLimitOverrideRepository>>,
    /// Config layer repository for hierarchical config admin (ADR-060). Optional until wired.
    pub config_layer_repo: Option<Arc<dyn ConfigLayerRepository>>,
    /// Node cluster repository for node registry admin (ADR-059). Optional until wired.
    pub node_cluster_repo: Option<Arc<dyn NodeClusterRepository>>,
    /// Security context repository for SMCP policy admin (ADR-035). Optional until wired.
    pub security_context_repo: Option<Arc<dyn SecurityContextRepository>>,
    /// PostgreSQL pool for direct audit-log queries. Optional until wired.
    pub pg_pool: Option<Arc<sqlx::PgPool>>,
}

/// Enable webhook HMAC authentication via Axum extractor pulling state from [`AppState`].
impl FromRef<Arc<AppState>> for WebhookHmacState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        WebhookHmacState {
            secret_provider: state.webhook_secret_provider.clone(),
        }
    }
}

pub fn app(
    execution_service: Arc<dyn ExecutionService>,
    human_input_service: Arc<crate::infrastructure::HumanInputService>,
) -> Router {
    let state = Arc::new(AppState {
        execution_service,
        human_input_service,
        inner_loop_service: None,
        stimulus_service: None,
        webhook_secret_provider: Arc::new(EnvWebhookSecretProvider),
        tool_invocation_service: None,
        attestation_service: None,
        workflow_execution_repo: None,
        event_bus: None,
        tenant_repo: None,
        smcp_session_repo: None,
        rate_limit_override_repo: None,
        config_layer_repo: None,
        node_cluster_repo: None,
        security_context_repo: None,
        pg_pool: None,
    });

    Router::new()
        .route("/v1/executions", post(start_execution))
        .route("/v1/executions/:id/stream", get(stream_execution))
        .route("/v1/human-approvals", get(list_pending_approvals))
        .route("/v1/human-approvals/:id", get(get_pending_approval))
        .route("/v1/human-approvals/:id/approve", post(approve_request))
        .route("/v1/human-approvals/:id/reject", post(reject_request))
        // SMCP endpoints (ADR-035 §4.1)
        .route("/v1/smcp/attest", post(smcp_attestation))
        .route("/v1/smcp/invoke", post(smcp_tool_invoke))
        .route("/v1/smcp/tools", get(smcp_tools_list))
        // Dispatch gateway (ADR-038)
        .route("/v1/dispatch-gateway", post(dispatch_gateway))
        // BC-8 Stimulus-Response (ADR-021)
        .route("/v1/stimuli", post(ingest_stimulus_handler))
        .route("/v1/webhooks/:source", post(webhook_handler))
        .route(
            "/v1/workflows/executions/:id/logs",
            get(get_workflow_execution_logs),
        )
        .route(
            "/v1/workflows/executions/:id/logs/stream",
            get(stream_workflow_execution_logs),
        )
        // Admin tenant management (ADR-056)
        .route("/v1/admin/tenants", post(create_tenant))
        .route("/v1/admin/tenants", get(list_tenants))
        .route("/v1/admin/tenants/:slug/suspend", post(suspend_tenant))
        .route(
            "/v1/admin/tenants/:slug",
            get(get_tenant).delete(soft_delete_tenant),
        )
        // Admin rate-limit override management (ADR-072)
        .route(
            "/v1/admin/rate-limits/overrides",
            get(list_rate_limit_overrides).post(upsert_rate_limit_override),
        )
        .route(
            "/v1/admin/rate-limits/overrides/:id",
            delete(delete_rate_limit_override),
        )
        .route("/v1/admin/rate-limits/usage", get(get_rate_limit_usage))
        // Admin tenant quotas (ADR-073)
        .route("/v1/admin/tenants/:slug/quotas", put(update_tenant_quotas))
        // Admin config layer management (ADR-060)
        .route(
            "/v1/admin/config/layers",
            get(list_config_layers).put(upsert_config_layer),
        )
        .route(
            "/v1/admin/config/layers/:layer_id",
            delete(delete_config_layer),
        )
        .route("/v1/admin/config/effective", get(get_effective_config))
        // Admin node registry (ADR-059)
        .route("/v1/admin/nodes", get(list_nodes))
        .route(
            "/v1/admin/nodes/:node_id",
            get(get_node).delete(deregister_node_handler),
        )
        .route("/v1/admin/nodes/:node_id/drain", post(drain_node))
        .route(
            "/v1/admin/nodes/:node_id/push-config",
            post(push_node_config),
        )
        // Admin security context management (ADR-035)
        .route("/v1/admin/security-contexts", get(list_security_contexts))
        .route(
            "/v1/admin/security-contexts/:name",
            get(get_security_context)
                .put(upsert_security_context)
                .delete(delete_security_context),
        )
        // Admin audit log (ADR-073)
        .route("/v1/admin/audit-log", get(query_audit_log))
        // Health endpoints (ADR-062)
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .layer(middleware::from_fn(tenant_context_middleware))
        .layer(middleware::from_fn(metrics_middleware))
        .with_state(state)
}

/// Build an Axum router with full SMCP service wiring.
///
/// This is the preferred constructor when the inner loop gateway is available.
pub fn app_with_inner_loop(
    execution_service: Arc<dyn ExecutionService>,
    human_input_service: Arc<crate::infrastructure::HumanInputService>,
    inner_loop_service: Arc<InnerLoopService>,
) -> Router {
    let state = Arc::new(AppState {
        execution_service,
        human_input_service,
        inner_loop_service: Some(inner_loop_service),
        stimulus_service: None,
        webhook_secret_provider: Arc::new(EnvWebhookSecretProvider),
        tool_invocation_service: None,
        attestation_service: None,
        workflow_execution_repo: None,
        event_bus: None,
        tenant_repo: None,
        smcp_session_repo: None,
        rate_limit_override_repo: None,
        config_layer_repo: None,
        node_cluster_repo: None,
        security_context_repo: None,
        pg_pool: None,
    });

    Router::new()
        .route("/v1/executions", post(start_execution))
        .route("/v1/executions/:id/stream", get(stream_execution))
        .route("/v1/human-approvals", get(list_pending_approvals))
        .route("/v1/human-approvals/:id", get(get_pending_approval))
        .route("/v1/human-approvals/:id/approve", post(approve_request))
        .route("/v1/human-approvals/:id/reject", post(reject_request))
        .route("/v1/smcp/attest", post(smcp_attestation))
        .route("/v1/smcp/invoke", post(smcp_tool_invoke))
        .route("/v1/smcp/tools", get(smcp_tools_list))
        .route("/v1/dispatch-gateway", post(dispatch_gateway))
        // BC-8 Stimulus-Response (ADR-021)
        .route("/v1/stimuli", post(ingest_stimulus_handler))
        .route("/v1/webhooks/:source", post(webhook_handler))
        .route(
            "/v1/workflows/executions/:id/logs",
            get(get_workflow_execution_logs),
        )
        .route(
            "/v1/workflows/executions/:id/logs/stream",
            get(stream_workflow_execution_logs),
        )
        // Admin tenant management (ADR-056)
        .route("/v1/admin/tenants", post(create_tenant))
        .route("/v1/admin/tenants", get(list_tenants))
        .route("/v1/admin/tenants/:slug/suspend", post(suspend_tenant))
        .route(
            "/v1/admin/tenants/:slug",
            get(get_tenant).delete(soft_delete_tenant),
        )
        // Admin rate-limit override management (ADR-072)
        .route(
            "/v1/admin/rate-limits/overrides",
            get(list_rate_limit_overrides).post(upsert_rate_limit_override),
        )
        .route(
            "/v1/admin/rate-limits/overrides/:id",
            delete(delete_rate_limit_override),
        )
        .route("/v1/admin/rate-limits/usage", get(get_rate_limit_usage))
        // Admin tenant quotas (ADR-073)
        .route("/v1/admin/tenants/:slug/quotas", put(update_tenant_quotas))
        // Admin config layer management (ADR-060)
        .route(
            "/v1/admin/config/layers",
            get(list_config_layers).put(upsert_config_layer),
        )
        .route(
            "/v1/admin/config/layers/:layer_id",
            delete(delete_config_layer),
        )
        .route("/v1/admin/config/effective", get(get_effective_config))
        // Admin node registry (ADR-059)
        .route("/v1/admin/nodes", get(list_nodes))
        .route(
            "/v1/admin/nodes/:node_id",
            get(get_node).delete(deregister_node_handler),
        )
        .route("/v1/admin/nodes/:node_id/drain", post(drain_node))
        .route(
            "/v1/admin/nodes/:node_id/push-config",
            post(push_node_config),
        )
        // Admin security context management (ADR-035)
        .route("/v1/admin/security-contexts", get(list_security_contexts))
        .route(
            "/v1/admin/security-contexts/:name",
            get(get_security_context)
                .put(upsert_security_context)
                .delete(delete_security_context),
        )
        // Admin audit log (ADR-073)
        .route("/v1/admin/audit-log", get(query_audit_log))
        // Health endpoints (ADR-062)
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .layer(middleware::from_fn(tenant_context_middleware))
        .layer(middleware::from_fn(metrics_middleware))
        .with_state(state)
}

/// Liveness probe — always returns 200 if the process is running.
async fn health_live() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

/// Readiness probe — returns 200 when the service is ready to accept traffic.
async fn health_ready() -> impl IntoResponse {
    Json(json!({"status": "ready"}))
}

#[derive(serde::Deserialize)]
pub struct StartExecutionRequest {
    pub agent_id: String,
    pub input: String,
    #[serde(default)]
    pub context_overrides: Option<serde_json::Value>,
}

async fn start_execution(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<StartExecutionRequest>,
) -> impl IntoResponse {
    let agent_id = match uuid::Uuid::parse_str(&payload.agent_id) {
        Ok(id) => AgentId(id),
        Err(_) => return Json(json!({"error": "Invalid agent ID"})),
    };

    // Let ExecutionService render the agent's prompt_template
    // instead of bypassing it by setting intent directly. This ensures agents
    // behave consistently regardless of API type (gRPC, REST, CLI).
    let input = ExecutionInput {
        intent: None, // Let ExecutionService render agent's prompt_template
        payload: serde_json::json!({
            "input": payload.input,  // User-provided input from REST API
            "context_overrides": payload.context_overrides,
        }),
    };

    match state
        .execution_service
        // ADR-083: operator context for internal REST API (no identity extraction)
        .start_execution(agent_id, input, "aegis-system-operator".to_string())
        .await
    {
        Ok(id) => Json(json!({ "execution_id": id.0.to_string() })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

/// Query parameters for the SSE stream endpoint.
/// The `token` parameter supports browser EventSource clients that cannot set headers.
#[derive(serde::Deserialize, Default)]
struct StreamExecutionQuery {
    token: Option<String>,
}

async fn stream_execution(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<StreamExecutionQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    // --- SMCP token authentication ---
    // Extract token from Authorization header or query parameter (for EventSource clients).
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(|t| t.to_string())
        .or(query.token);

    let token = match token {
        Some(t) => t,
        None => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Missing SMCP security token"})),
            )
                .into_response();
        }
    };

    // Validate the token against active SMCP sessions.
    let session_repo = match &state.smcp_session_repo {
        Some(repo) => repo,
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "SMCP session repository not configured"})),
            )
                .into_response();
        }
    };

    match session_repo.find_active_by_security_token(&token).await {
        Ok(Some(_session)) => {
            // Token is valid — session is active.
            // TODO: optionally validate execution belongs to session.agent_id()
        }
        Ok(None) => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"error": "Invalid or expired SMCP security token"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to validate SMCP session token for SSE stream");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Session validation failed"})),
            )
                .into_response();
        }
    }

    // --- Execution ID parsing ---
    let execution_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => crate::domain::execution::ExecutionId(uid),
        Err(_) => {
            let stream: Pin<
                Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>,
            > = Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1)
                    .map(Ok::<_, axum::Error>),
            );
            return Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response();
        }
    };

    let stream: Pin<
        Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>,
    > = match state.execution_service.stream_execution(execution_id).await {
        Ok(s) => Box::pin(s.map(|event_res| {
            match event_res {
                Ok(event) => Ok(axum::response::sse::Event::default()
                    .data(serde_json::to_string(&event).unwrap_or_default())),
                Err(_) => Ok(axum::response::sse::Event::default().data("error")),
            }
        })),
        Err(_) => {
            // Return empty stream on error
            Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1)
                    .map(Ok::<_, axum::Error>),
            )
        }
    };

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

// ============================================================================
// Workflow Execution Log Endpoints
// ============================================================================

#[derive(serde::Deserialize, Default)]
struct WorkflowLogsQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

/// `GET /v1/workflows/executions/:id/logs` — return the persisted event log
/// for a workflow execution as a JSON snapshot.
///
/// Returns 400 (bad UUID), 503 (repo unavailable), or 200 with the event list.
async fn get_workflow_execution_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<WorkflowLogsQuery>,
) -> impl IntoResponse {
    let execution_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => crate::domain::execution::ExecutionId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Invalid execution ID"})),
            )
                .into_response();
        }
    };

    let repo = match &state.workflow_execution_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "Workflow execution repository not configured",
                    "execution_id": id,
                })),
            )
                .into_response();
        }
    };

    let limit = params.limit.unwrap_or(100);
    let offset = params.offset.unwrap_or(0);

    match repo
        .find_events_by_execution(execution_id, limit, offset)
        .await
    {
        Ok(events) => {
            let count = events.len();
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({
                    "execution_id": id,
                    "events": events,
                    "count": count,
                    "limit": limit,
                    "offset": offset,
                })),
            )
                .into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/workflows/executions/:id/logs/stream` — SSE stream of workflow
/// execution events for the given execution.
///
/// Subscribes to the `EventBus` for the given execution and streams
/// `WorkflowEvent`s until a terminal event (Completed / Failed / Cancelled)
/// is received or the client disconnects.
async fn stream_workflow_execution_logs(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let execution_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => crate::domain::execution::ExecutionId(uid),
        Err(_) => {
            let stream: Pin<
                Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>,
            > = Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1)
                    .map(Ok::<_, axum::Error>),
            );
            return Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default());
        }
    };

    let event_bus = match &state.event_bus {
        Some(eb) => eb.clone(),
        None => {
            let stream: Pin<
                Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>,
            > = Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1)
                    .map(Ok::<_, axum::Error>),
            );
            return Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default());
        }
    };

    let (tx, rx) = tokio::sync::mpsc::channel(32);

    tokio::spawn(async move {
        let mut receiver = event_bus.subscribe_workflow_execution(execution_id);
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let is_terminal = matches!(
                        event,
                        crate::domain::events::WorkflowEvent::WorkflowExecutionCompleted { .. }
                            | crate::domain::events::WorkflowEvent::WorkflowExecutionFailed { .. }
                            | crate::domain::events::WorkflowEvent::WorkflowExecutionCancelled { .. }
                    );
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    if tx
                        .send(axum::response::sse::Event::default().data(data))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if is_terminal {
                        break;
                    }
                }
                Err(crate::infrastructure::event_bus::EventBusError::Closed) => break,
                Err(_) => continue,
            }
        }
    });

    let stream: Pin<
        Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>,
    > = Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, axum::Error>));

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// ============================================================================
// Human Approval Endpoints
// ============================================================================

/// List all pending approval requests
async fn list_pending_approvals(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let pending = state.human_input_service.list_pending_requests().await;
    Json(json!({ "pending_requests": pending }))
}

/// Get a specific pending approval request
async fn get_pending_approval(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let request_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Json(json!({"error": "Invalid request ID"})),
    };

    match state
        .human_input_service
        .get_pending_request(request_id)
        .await
    {
        Some(request) => Json(json!({ "request": request })),
        None => Json(json!({ "error": "Request not found or already completed" })),
    }
}

#[derive(serde::Deserialize)]
pub struct ApprovalRequest {
    pub feedback: Option<String>,
    pub approved_by: Option<String>,
}

/// Approve a pending request
async fn approve_request(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<ApprovalRequest>,
) -> impl IntoResponse {
    let request_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Json(json!({"error": "Invalid request ID"})),
    };

    match state
        .human_input_service
        .submit_approval(request_id, payload.feedback, payload.approved_by)
        .await
    {
        Ok(()) => Json(json!({ "status": "approved" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

#[derive(serde::Deserialize)]
pub struct RejectionRequest {
    pub reason: String,
    pub rejected_by: Option<String>,
}

/// Reject a pending request
async fn reject_request(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<RejectionRequest>,
) -> impl IntoResponse {
    let request_id = match uuid::Uuid::parse_str(&id) {
        Ok(uid) => uid,
        Err(_) => return Json(json!({"error": "Invalid request ID"})),
    };

    match state
        .human_input_service
        .submit_rejection(request_id, payload.reason, payload.rejected_by)
        .await
    {
        Ok(()) => Json(json!({ "status": "rejected" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ============================================================================
// SMCP Endpoints (ADR-035 §4.1)
// ============================================================================

/// Agent attestation request payload (ADR-035 §4.1).
///
/// Sent by the agent's `bootstrap.py` at the start of each execution to
/// establish an SMCP session and receive a SecurityToken.
#[derive(serde::Deserialize)]
pub struct SmcpAttestationRequest {
    /// Container ID or execution identifier.
    pub container_id: Option<String>,
    /// Base64-encoded ephemeral Ed25519 public key generated by the agent.
    pub agent_public_key: String,
    /// Agent ID (UUID).
    pub agent_id: Option<String>,
    /// Execution ID (UUID).
    pub execution_id: Option<String>,
    /// Optional explicit security context for Zaru-facing sessions.
    pub security_context: Option<String>,
    /// Optional upstream principal subject for audit correlation.
    pub principal_subject: Option<String>,
    /// Optional upstream user identifier for consumer-facing sessions.
    pub user_id: Option<String>,
    /// Optional workload identifier for audit correlation.
    pub workload_id: Option<String>,
    /// Optional Zaru tier string used to derive the security context.
    pub zaru_tier: Option<String>,
    /// Tenant slug identifying the requesting principal's tenant (ADR-056).
    ///
    /// When omitted, defaults to the consumer tenant (`zaru-consumer`).
    pub tenant_id: Option<String>,
}

/// Handle SMCP attestation handshake (ADR-035 §4.1).
///
/// The agent sends its ephemeral public key; the orchestrator verifies the
/// container identity, looks up the SecurityContext, creates an SmcpSession,
/// and returns a signed SecurityToken (JWT).
async fn smcp_attestation(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SmcpAttestationRequest>,
) -> impl IntoResponse {
    let attestation_service = match &state.attestation_service {
        Some(service) => service.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "SMCP attestation service not configured"
                })),
            )
                .into_response();
        }
    };

    let tenant_id = payload
        .tenant_id
        .as_deref()
        .and_then(|s| TenantId::from_realm_slug(s).ok())
        .unwrap_or_else(TenantId::consumer);

    let request = crate::infrastructure::smcp::attestation::AttestationRequest {
        agent_id: payload.agent_id,
        execution_id: payload.execution_id,
        container_id: payload.container_id,
        public_key_pem: payload.agent_public_key,
        security_context: payload.security_context,
        principal_subject: payload.principal_subject,
        user_id: payload.user_id,
        workload_id: payload.workload_id,
        zaru_tier: payload.zaru_tier,
        tenant_id,
    };

    match attestation_service.attest(request).await {
        Ok(response) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "security_token": response.security_token,
            })),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}

/// SMCP tool invocation request payload (ADR-033 §3).
#[derive(serde::Deserialize)]
pub struct SmcpToolInvokeRequest {
    /// Signed SMCP security token (JWT issued at attestation).
    pub security_token: String,
    /// Ed25519 signature over the serialized `payload`.
    pub signature: String,
    /// Inner MCP payload (tool name + arguments).
    pub payload: serde_json::Value,
    /// SMCP protocol version (e.g. "smcp/1.0"); included in canonical signed message.
    #[serde(default)]
    pub protocol: Option<String>,
    /// ISO-8601 timestamp; included in canonical signed message.
    #[serde(default)]
    pub timestamp: Option<String>,
}

/// Handle SMCP tool invocation (ADR-033 §3).
///
/// The agent wraps each MCP tool call in an SMCP envelope signed with its
/// ephemeral key. The orchestrator verifies the signature, evaluates the
/// SecurityContext policy, and routes to the appropriate tool server.
async fn smcp_tool_invoke(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SmcpToolInvokeRequest>,
) -> impl IntoResponse {
    tracing::info!("SMCP tool invocation request received");

    let tool_svc = match &state.tool_invocation_service {
        Some(svc) => svc.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "SMCP tool invocation service not wired in this configuration"
                })),
            )
                .into_response();
        }
    };

    let inner_mcp = serde_json::to_vec(&req.payload).unwrap_or_default();
    let envelope = crate::infrastructure::smcp::envelope::SmcpEnvelope {
        protocol: req.protocol,
        security_token: req.security_token,
        signature: req.signature,
        inner_mcp,
        timestamp: req.timestamp,
    };

    match tool_svc.invoke_tool(&envelope).await {
        Ok(res) => (axum::http::StatusCode::OK, Json(res)).into_response(),
        Err(e) => {
            // Map domain errors to HTTP status + MCP JSON-RPC error codes (ADR-055).
            // Callers receive a standard `{"jsonrpc":"2.0","error":{"code":N,"message":"..."}}` body
            // so MCP clients can handle them uniformly regardless of transport.
            let (http_status, rpc_code, retry_after) = match &e {
                SmcpSessionError::SessionExpired
                | SmcpSessionError::SignatureVerificationFailed(_) => {
                    (axum::http::StatusCode::UNAUTHORIZED, -32000_i32, None)
                }
                SmcpSessionError::PolicyViolation(PolicyViolation::RateLimitExceeded {
                    retry_after_seconds,
                    ..
                }) => (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    -32005_i32,
                    Some(*retry_after_seconds),
                ),
                SmcpSessionError::SessionInactive(
                    crate::domain::smcp_session::SessionStatus::Expired,
                ) => (axum::http::StatusCode::UNAUTHORIZED, -32000_i32, None),
                SmcpSessionError::PolicyViolation(_) | SmcpSessionError::SessionInactive(_) => {
                    (axum::http::StatusCode::FORBIDDEN, -32000_i32, None)
                }
                SmcpSessionError::MalformedPayload(_)
                | SmcpSessionError::ReplayProtectionFailed(_) => {
                    (axum::http::StatusCode::BAD_REQUEST, -32600_i32, None)
                }
                SmcpSessionError::InvalidArguments(_) => (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    -32602_i32,
                    None,
                ),
                SmcpSessionError::JudgeTimeout(_) | SmcpSessionError::InternalError(_) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    -32603_i32,
                    None,
                ),
            };
            let body = Json(json!({
                "jsonrpc": "2.0",
                "error": {
                    "code": rpc_code,
                    "message": e.to_string(),
                    "data": null
                }
            }));
            let mut response = (http_status, body).into_response();
            if let Some(secs) = retry_after {
                if let Ok(val) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                    response
                        .headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, val);
                }
            }
            response
        }
    }
}

#[derive(serde::Deserialize, Default)]
struct SmcpToolsQuery {
    security_context: Option<String>,
}

async fn smcp_tools_list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SmcpToolsQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let tool_svc = match &state.tool_invocation_service {
        Some(svc) => svc.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "SMCP tool discovery service not wired in this configuration"
                })),
            )
                .into_response();
        }
    };

    let security_context = headers
        .get("X-Zaru-Security-Context")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or(query.security_context);

    let tools_result = if let Some(ref security_context) = security_context {
        tool_svc
            .get_available_tools_for_context(security_context)
            .await
    } else {
        tool_svc.get_available_tools().await
    };

    match tools_result {
        Ok(tools) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "protocol": "smcp/v1",
                "attestation_endpoint": "/v1/smcp/attest",
                "invoke_endpoint": "/v1/smcp/invoke",
                "security_context": security_context,
                "tools": tools,
            })),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}

// ============================================================================
// Inner Loop Gateway (ADR-038)
// ============================================================================

/// Handle dispatch gateway request (ADR-038).
///
/// This is the single entry point for agent code. `bootstrap.py` calls
/// `POST /v1/dispatch-gateway` and the orchestrator handles the entire
/// LLM → tool call → LLM cycle internally via the Dispatch Protocol.
async fn dispatch_gateway(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AgentMessage>,
) -> impl IntoResponse {
    let inner_loop_service = match &state.inner_loop_service {
        Some(svc) => svc.clone(),
        None => {
            return Json(json!({
                "error": "Inner loop service not configured"
            }));
        }
    };

    match inner_loop_service.handle_agent_message(payload).await {
        Ok(response) => {
            Json(serde_json::to_value(response).unwrap_or(json!({"error": "serialization failed"})))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ============================================================================
// Admin Tenant Management Endpoints (ADR-056)
// ============================================================================

/// Guard that checks the request identity is an admin operator.
/// Returns the `UserIdentity` on success, or an error response on failure.
#[allow(clippy::result_large_err)]
fn require_admin(extensions: &axum::http::Extensions) -> Result<UserIdentity, Response> {
    let identity = extensions.get::<UserIdentity>().cloned().ok_or_else(|| {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response()
    })?;

    match &identity.identity_kind {
        IdentityKind::Operator { aegis_role } if aegis_role.is_admin() => Ok(identity),
        _ => Err((
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({"error": "Admin role required"})),
        )
            .into_response()),
    }
}

#[derive(serde::Deserialize)]
pub struct CreateTenantRequest {
    pub slug: String,
    pub display_name: String,
    #[serde(default = "default_tier")]
    pub tier: String,
}

fn default_tier() -> String {
    "enterprise".to_string()
}

/// `POST /v1/admin/tenants` — create a new tenant
async fn create_tenant(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: CreateTenantRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let slug = match TenantId::from_realm_slug(&payload.slug) {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid tenant slug: {e}")})),
            )
                .into_response();
        }
    };

    let tier = match payload.tier.as_str() {
        "consumer" => TenantTier::Consumer,
        "enterprise" => TenantTier::Enterprise,
        "system" => TenantTier::System,
        other => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid tier: {other}")})),
            )
                .into_response();
        }
    };

    let keycloak_realm = format!("tenant-{}", payload.slug);
    let openbao_namespace = format!("tenant/{}", payload.slug);

    let tenant = Tenant::new(
        slug,
        payload.display_name,
        tier,
        keycloak_realm,
        openbao_namespace,
    );

    match repo.insert(&tenant).await {
        Ok(()) => (
            axum::http::StatusCode::CREATED,
            Json(serde_json::to_value(&tenant).unwrap_or(json!({"status": "created"}))),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/admin/tenants` — list all active tenants
async fn list_tenants(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.find_all_active().await {
        Ok(tenants) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "tenants": tenants,
                "count": tenants.len(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/admin/tenants/:slug/suspend` — suspend a tenant
async fn suspend_tenant(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    let tenant_id = match TenantId::from_realm_slug(&slug) {
        Ok(id) => id,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid tenant slug: {e}")})),
            )
                .into_response();
        }
    };

    match repo
        .update_status(&tenant_id, &TenantStatus::Suspended)
        .await
    {
        Ok(()) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "suspended", "slug": slug})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/admin/tenants/:slug` — soft-delete a tenant
async fn soft_delete_tenant(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    let tenant_id = match TenantId::from_realm_slug(&slug) {
        Ok(id) => id,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid tenant slug: {e}")})),
            )
                .into_response();
        }
    };

    match repo.update_status(&tenant_id, &TenantStatus::Deleted).await {
        Ok(()) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "deleted", "slug": slug})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============================================================================
// Admin Rate-Limit Override Endpoints (ADR-072)
// ============================================================================

/// Query parameters for `GET /v1/admin/rate-limits/overrides`.
#[derive(Debug, serde::Deserialize)]
pub struct ListOverridesParams {
    pub tenant_id: Option<String>,
    pub user_id: Option<String>,
}

/// Query parameters for `GET /v1/admin/rate-limits/usage`.
#[derive(Debug, serde::Deserialize)]
pub struct UsageParams {
    pub scope_type: String,
    pub scope_id: String,
}

/// `GET /v1/admin/rate-limits/overrides` — list rate-limit overrides
async fn list_rate_limit_overrides(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListOverridesParams>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    match repo
        .list(params.tenant_id.as_deref(), params.user_id.as_deref())
        .await
    {
        Ok(overrides) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "overrides": overrides,
                "count": overrides.len(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/admin/rate-limits/overrides` — create or update a rate-limit override
async fn upsert_rate_limit_override(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: CreateOverrideRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    // Validate: exactly one of tenant_id or user_id must be set (matches DB constraint)
    if payload.tenant_id.is_some() == payload.user_id.is_some() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "Exactly one of tenant_id or user_id must be provided"})),
        )
            .into_response();
    }

    match repo.upsert(&payload).await {
        Ok(row) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&row).unwrap_or(json!({"status": "upserted"}))),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/admin/rate-limits/overrides/:id` — delete a rate-limit override
async fn delete_rate_limit_override(
    State(state): State<Arc<AppState>>,
    Path(id): Path<uuid::Uuid>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.delete(id).await {
        Ok(true) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "deleted", "id": id.to_string()})),
        )
            .into_response(),
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": "Override not found"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/admin/rate-limits/usage` — get current rate-limit counter usage
async fn get_rate_limit_usage(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UsageParams>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.get_usage(&params.scope_type, &params.scope_id).await {
        Ok(rows) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "usage": rows,
                "count": rows.len(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============================================================================
// Admin Tenant Detail + Quotas Endpoints (ADR-073)
// ============================================================================

/// `GET /v1/admin/tenants/:slug` — get tenant detail
async fn get_tenant(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    let tenant_id = match TenantId::from_realm_slug(&slug) {
        Ok(id) => id,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid tenant slug: {e}")})),
            )
                .into_response();
        }
    };

    match repo.find_by_slug(&tenant_id).await {
        Ok(Some(tenant)) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&tenant).unwrap_or(json!({"error": "serialization failed"}))),
        )
            .into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Tenant '{}' not found", slug)})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct UpdateQuotasRequest {
    max_concurrent_executions: Option<u32>,
    max_agents: Option<u32>,
    max_storage_gb: Option<f64>,
}

/// `PUT /v1/admin/tenants/:slug/quotas` — update tenant quotas
async fn update_tenant_quotas(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: UpdateQuotasRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let tenant_id = match TenantId::from_realm_slug(&slug) {
        Ok(id) => id,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid tenant slug: {e}")})),
            )
                .into_response();
        }
    };

    // Fetch current tenant to get existing quotas for partial update
    let current = match repo.find_by_slug(&tenant_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(json!({"error": format!("Tenant '{}' not found", slug)})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let quotas = TenantQuotas {
        max_concurrent_executions: payload
            .max_concurrent_executions
            .unwrap_or(current.quotas.max_concurrent_executions),
        max_agents: payload.max_agents.unwrap_or(current.quotas.max_agents),
        max_storage_gb: payload
            .max_storage_gb
            .unwrap_or(current.quotas.max_storage_gb),
    };

    match repo.update_quotas(&tenant_id, &quotas).await {
        Ok(()) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "slug": slug,
                "quotas": {
                    "max_concurrent_executions": quotas.max_concurrent_executions,
                    "max_agents": quotas.max_agents,
                    "max_storage_gb": quotas.max_storage_gb,
                }
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============================================================================
// Admin Config Layer Endpoints (ADR-060, ADR-073)
// ============================================================================

#[derive(serde::Deserialize, Default)]
struct ListConfigLayersParams {
    scope: Option<String>,
    target_id: Option<String>,
    config_type: Option<String>,
}

#[allow(clippy::result_large_err)]
fn parse_config_type(s: &str) -> Result<ConfigType, Response> {
    match s {
        "aegis_config" | "aegis-config" => Ok(ConfigType::AegisConfig),
        "runtime_registry" | "runtime-registry" => Ok(ConfigType::RuntimeRegistry),
        other => Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid config_type: {other}")})),
        )
            .into_response()),
    }
}

#[allow(clippy::result_large_err)]
fn parse_config_scope(s: &str) -> Result<ConfigScope, Response> {
    match s {
        "global" => Ok(ConfigScope::Global),
        "tenant" => Ok(ConfigScope::Tenant),
        "node" => Ok(ConfigScope::Node),
        other => Err((
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid scope: {other}")})),
        )
            .into_response()),
    }
}

/// `GET /v1/admin/config/layers` — list config layers
async fn list_config_layers(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListConfigLayersParams>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.config_layer_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Config layer repository not configured"})),
            )
                .into_response();
        }
    };

    let config_type = match params.config_type.as_deref() {
        Some(ct) => match parse_config_type(ct) {
            Ok(t) => t,
            Err(resp) => return resp,
        },
        None => ConfigType::AegisConfig,
    };

    // If scope + target_id provided, get a specific layer
    if let (Some(scope_str), Some(target_id)) =
        (params.scope.as_deref(), params.target_id.as_deref())
    {
        let scope = match parse_config_scope(scope_str) {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match repo.get_layer(&scope, target_id, &config_type).await {
            Ok(Some(layer)) => {
                return (
                    axum::http::StatusCode::OK,
                    Json(serde_json::to_value(&layer).unwrap_or(json!({}))),
                )
                    .into_response();
            }
            Ok(None) => {
                return (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(json!({"error": "Config layer not found"})),
                )
                    .into_response();
            }
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    }

    match repo.list_layers(&config_type).await {
        Ok(layers) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "layers": serde_json::to_value(&layers).unwrap_or(json!([])),
                "count": layers.len(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct UpsertConfigLayerRequest {
    scope: String,
    scope_key: String,
    config_type: String,
    payload: serde_json::Value,
}

/// `PUT /v1/admin/config/layers` — upsert a config layer
async fn upsert_config_layer(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.config_layer_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Config layer repository not configured"})),
            )
                .into_response();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 256).await {
        Ok(b) => b,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: UpsertConfigLayerRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let scope = match parse_config_scope(&payload.scope) {
        Ok(s) => s,
        Err(resp) => return resp,
    };

    let config_type = match parse_config_type(&payload.config_type) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    match repo
        .upsert_layer(&scope, &payload.scope_key, &config_type, payload.payload)
        .await
    {
        Ok(snapshot) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&snapshot).unwrap_or(json!({"status": "upserted"}))),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/admin/config/layers/:layer_id` — delete a config layer
///
/// The layer_id is expected as `{scope}:{scope_key}:{config_type}` (URL-encoded).
async fn delete_config_layer(
    State(state): State<Arc<AppState>>,
    Path(layer_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.config_layer_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Config layer repository not configured"})),
            )
                .into_response();
        }
    };

    // Parse composite layer_id: "scope:scope_key:config_type"
    let parts: Vec<&str> = layer_id.splitn(3, ':').collect();
    if parts.len() != 3 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error": "layer_id must be formatted as 'scope:scope_key:config_type'"})),
        )
            .into_response();
    }

    let scope = match parse_config_scope(parts[0]) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let scope_key = parts[1];
    let config_type = match parse_config_type(parts[2]) {
        Ok(t) => t,
        Err(resp) => return resp,
    };

    match repo.delete_layer(&scope, scope_key, &config_type).await {
        Ok(true) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "deleted", "layer_id": layer_id})),
        )
            .into_response(),
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": "Config layer not found"})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct EffectiveConfigParams {
    node_id: String,
    config_type: Option<String>,
    tenant_id: Option<String>,
}

/// `GET /v1/admin/config/effective` — get merged effective config for a node
async fn get_effective_config(
    State(state): State<Arc<AppState>>,
    Query(params): Query<EffectiveConfigParams>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.config_layer_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Config layer repository not configured"})),
            )
                .into_response();
        }
    };

    let node_id = match uuid::Uuid::parse_str(&params.node_id) {
        Ok(uid) => NodeId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid node_id UUID"})),
            )
                .into_response();
        }
    };

    let config_type = match params.config_type.as_deref() {
        Some(ct) => match parse_config_type(ct) {
            Ok(t) => t,
            Err(resp) => return resp,
        },
        None => ConfigType::AegisConfig,
    };

    match repo
        .get_merged_config(&node_id, params.tenant_id.as_deref(), &config_type)
        .await
    {
        Ok(merged) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&merged).unwrap_or(json!({}))),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============================================================================
// Admin Node Registry Endpoints (ADR-059, ADR-073)
// ============================================================================

/// `GET /v1/admin/nodes` — list all registered nodes
async fn list_nodes(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.node_cluster_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Node cluster repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.list_all_peers().await {
        Ok(peers) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "nodes": peers,
                "count": peers.len(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/admin/nodes/:node_id` — get a single registered node
async fn get_node(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.node_cluster_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Node cluster repository not configured"})),
            )
                .into_response();
        }
    };

    let nid = match uuid::Uuid::parse_str(&node_id) {
        Ok(uid) => NodeId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid node_id UUID"})),
            )
                .into_response();
        }
    };

    match repo.find_registered_node(&nid).await {
        Ok(Some(node)) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&node).unwrap_or(json!({}))),
        )
            .into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Node '{}' not found", node_id)})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/admin/nodes/:node_id/drain` — start draining a node
async fn drain_node(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.node_cluster_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Node cluster repository not configured"})),
            )
                .into_response();
        }
    };

    let nid = match uuid::Uuid::parse_str(&node_id) {
        Ok(uid) => NodeId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid node_id UUID"})),
            )
                .into_response();
        }
    };

    match repo.start_drain(&nid).await {
        Ok(()) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "draining", "node_id": node_id})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PushNodeConfigRequest {
    config_payload: serde_json::Value,
}

/// `POST /v1/admin/nodes/:node_id/push-config` — push config to a node
async fn push_node_config(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let config_repo = match &state.config_layer_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Config layer repository not configured"})),
            )
                .into_response();
        }
    };

    let node_repo = match &state.node_cluster_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Node cluster repository not configured"})),
            )
                .into_response();
        }
    };

    let nid = match uuid::Uuid::parse_str(&node_id) {
        Ok(uid) => NodeId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid node_id UUID"})),
            )
                .into_response();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 256).await {
        Ok(b) => b,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: PushNodeConfigRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    // Upsert the node-scoped config layer
    let snapshot = match config_repo
        .upsert_layer(
            &ConfigScope::Node,
            &nid.to_string(),
            &ConfigType::AegisConfig,
            payload.config_payload,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    // Record the new config version on the node
    if let Err(e) = node_repo
        .record_config_version(&nid, &snapshot.version)
        .await
    {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Config saved but version recording failed: {e}")})),
        )
            .into_response();
    }

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "status": "pushed",
            "node_id": node_id,
            "config_version": snapshot.version,
        })),
    )
        .into_response()
}

/// `DELETE /v1/admin/nodes/:node_id` — deregister a node
async fn deregister_node_handler(
    State(state): State<Arc<AppState>>,
    Path(node_id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.node_cluster_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Node cluster repository not configured"})),
            )
                .into_response();
        }
    };

    let nid = match uuid::Uuid::parse_str(&node_id) {
        Ok(uid) => NodeId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid node_id UUID"})),
            )
                .into_response();
        }
    };

    match repo.deregister(&nid, "admin deregistration").await {
        Ok(()) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "deregistered", "node_id": node_id})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============================================================================
// Admin SecurityContext Endpoints (ADR-035, ADR-073)
// ============================================================================

/// `GET /v1/admin/security-contexts` — list all security contexts
async fn list_security_contexts(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.security_context_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Security context repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.list_all().await {
        Ok(contexts) => (
            axum::http::StatusCode::OK,
            Json(json!({
                "security_contexts": contexts,
                "count": contexts.len(),
            })),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/admin/security-contexts/:name` — get a security context by name
async fn get_security_context(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.security_context_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Security context repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.find_by_name(&name).await {
        Ok(Some(ctx)) => (
            axum::http::StatusCode::OK,
            Json(serde_json::to_value(&ctx).unwrap_or(json!({}))),
        )
            .into_response(),
        Ok(None) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Security context '{}' not found", name)})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `PUT /v1/admin/security-contexts/:name` — upsert a security context
async fn upsert_security_context(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.security_context_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Security context repository not configured"})),
            )
                .into_response();
        }
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let mut context: SecurityContext = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    // Ensure name in path matches body
    context.name = name.clone();

    match repo.save(context).await {
        Ok(()) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "saved", "name": name})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/admin/security-contexts/:name` — delete a security context
async fn delete_security_context(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let repo = match &state.security_context_repo {
        Some(r) => r.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Security context repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.delete(&name).await {
        Ok(true) => (
            axum::http::StatusCode::OK,
            Json(json!({"status": "deleted", "name": name})),
        )
            .into_response(),
        Ok(false) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": format!("Security context '{}' not found", name)})),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ============================================================================
// Admin Audit Log Endpoints (ADR-073)
// ============================================================================

#[derive(serde::Deserialize, Default)]
struct AuditLogParams {
    actor: Option<String>,
    action: Option<String>,
    target: Option<String>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /v1/admin/audit-log` — query admin audit log
async fn query_audit_log(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AuditLogParams>,
    request: axum::extract::Request,
) -> Response {
    if let Err(resp) = require_admin(request.extensions()) {
        return resp;
    }

    let pool = match &state.pg_pool {
        Some(p) => p.clone(),
        None => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Database pool not configured for audit queries"})),
            )
                .into_response();
        }
    };

    let limit = params.limit.unwrap_or(50).min(500);
    let offset = params.offset.unwrap_or(0);

    // Build dynamic query with optional filters
    let mut query = String::from(
        "SELECT id, actor_id, action, target_resource, before_state, after_state, created_at \
         FROM admin_audit_log WHERE 1=1",
    );
    let mut bind_idx = 1u32;
    let mut binds: Vec<String> = Vec::new();

    if let Some(ref actor) = params.actor {
        query.push_str(&format!(" AND actor_id = ${bind_idx}"));
        bind_idx += 1;
        binds.push(actor.clone());
    }
    if let Some(ref action) = params.action {
        query.push_str(&format!(" AND action = ${bind_idx}"));
        bind_idx += 1;
        binds.push(action.clone());
    }
    if let Some(ref target) = params.target {
        query.push_str(&format!(" AND target_resource = ${bind_idx}"));
        bind_idx += 1;
        binds.push(target.clone());
    }
    if let Some(ref from) = params.from {
        query.push_str(&format!(" AND created_at >= ${bind_idx}::timestamptz"));
        bind_idx += 1;
        binds.push(from.clone());
    }
    if let Some(ref to) = params.to {
        query.push_str(&format!(" AND created_at <= ${bind_idx}::timestamptz"));
        bind_idx += 1;
        binds.push(to.clone());
    }

    query.push_str(&format!(
        " ORDER BY created_at DESC LIMIT ${bind_idx} OFFSET ${}",
        bind_idx + 1
    ));

    let mut sql_query = sqlx::query(&query);
    for b in &binds {
        sql_query = sql_query.bind(b);
    }
    sql_query = sql_query.bind(limit).bind(offset);

    match sql_query.fetch_all(pool.as_ref()).await {
        Ok(rows) => {
            let entries: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    use sqlx::Row;
                    json!({
                        "id": r.get::<uuid::Uuid, _>("id").to_string(),
                        "actor_id": r.get::<String, _>("actor_id"),
                        "action": r.get::<String, _>("action"),
                        "target_resource": r.get::<String, _>("target_resource"),
                        "before_state": r.get::<Option<serde_json::Value>, _>("before_state"),
                        "after_state": r.get::<Option<serde_json::Value>, _>("after_state"),
                        "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
                    })
                })
                .collect();

            (
                axum::http::StatusCode::OK,
                Json(json!({
                    "entries": entries,
                    "count": entries.len(),
                    "limit": limit,
                    "offset": offset,
                })),
            )
                .into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::json;
    use std::pin::Pin;
    use tokio_stream::Stream;

    use crate::presentation::webhook_guard::EnvWebhookSecretProvider;

    struct NoopExecutionService;

    #[async_trait]
    impl ExecutionService for NoopExecutionService {
        async fn start_execution(
            &self,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _security_context_name: String,
        ) -> Result<crate::domain::execution::ExecutionId> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn start_execution_with_id(
            &self,
            _execution_id: crate::domain::execution::ExecutionId,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _security_context_name: String,
        ) -> Result<crate::domain::execution::ExecutionId> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn start_child_execution(
            &self,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _parent_execution_id: crate::domain::execution::ExecutionId,
        ) -> Result<crate::domain::execution::ExecutionId> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn get_execution(
            &self,
            _id: crate::domain::execution::ExecutionId,
        ) -> Result<crate::domain::execution::Execution> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn get_iterations(
            &self,
            _exec_id: crate::domain::execution::ExecutionId,
        ) -> Result<Vec<crate::domain::execution::Iteration>> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn cancel_execution(&self, _id: crate::domain::execution::ExecutionId) -> Result<()> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn stream_execution(
            &self,
            _id: crate::domain::execution::ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<crate::domain::events::ExecutionEvent>> + Send>>>
        {
            Ok(Box::pin(tokio_stream::empty()))
        }

        async fn stream_agent_events(
            &self,
            _id: AgentId,
        ) -> Result<
            Pin<
                Box<
                    dyn Stream<Item = Result<crate::infrastructure::event_bus::DomainEvent>> + Send,
                >,
            >,
        > {
            Ok(Box::pin(tokio_stream::empty()))
        }

        async fn list_executions(
            &self,
            _agent_id: Option<AgentId>,
            _limit: usize,
        ) -> Result<Vec<crate::domain::execution::Execution>> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn delete_execution(&self, _id: crate::domain::execution::ExecutionId) -> Result<()> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn record_llm_interaction(
            &self,
            _execution_id: crate::domain::execution::ExecutionId,
            _iteration: u8,
            _interaction: crate::domain::execution::LlmInteraction,
        ) -> Result<()> {
            Err(anyhow!("not used in presentation api tests"))
        }

        async fn store_iteration_trajectory(
            &self,
            _execution_id: crate::domain::execution::ExecutionId,
            _iteration: u8,
            _trajectory: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            Err(anyhow!("not used in presentation api tests"))
        }
    }

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState {
            execution_service: Arc::new(NoopExecutionService),
            human_input_service: Arc::new(crate::infrastructure::HumanInputService::new()),
            inner_loop_service: None,
            stimulus_service: None,
            webhook_secret_provider: Arc::new(EnvWebhookSecretProvider),
            tool_invocation_service: None,
            attestation_service: None,
            workflow_execution_repo: None,
            event_bus: None,
            tenant_repo: None,
            smcp_session_repo: None,
            rate_limit_override_repo: None,
            config_layer_repo: None,
            node_cluster_repo: None,
            security_context_repo: None,
            pg_pool: None,
        })
    }

    #[tokio::test]
    async fn smcp_tool_invoke_returns_service_unavailable_when_tool_service_is_missing() {
        let response = smcp_tool_invoke(
            State(test_state()),
            Json(SmcpToolInvokeRequest {
                security_token: "token".to_string(),
                signature: "sig".to_string(),
                payload: json!({
                    "jsonrpc": "2.0",
                    "method": "fs.read",
                    "params": { "path": "/tmp/test.txt" }
                }),
                protocol: None,
                timestamp: None,
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "error": "SMCP tool invocation service not wired in this configuration"
            })
        );
    }

    #[tokio::test]
    async fn smcp_tools_list_returns_service_unavailable_when_tool_service_is_missing() {
        let response = smcp_tools_list(
            State(test_state()),
            Query(SmcpToolsQuery::default()),
            axum::http::HeaderMap::new(),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "error": "SMCP tool discovery service not wired in this configuration"
            })
        );
    }
}
