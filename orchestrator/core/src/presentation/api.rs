// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # HTTP/SSE Presentation Layer — Axum (BC-2, BC-1, BC-12)
//!
//! Axum-based HTTP handlers for the orchestrator REST API and the
//! Server-Sent Events (SSE) endpoint consumed by the Control Plane UI
//! (`aegis-control-plane`) for real-time execution streaming (ADR-026).
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
use crate::domain::dispatch::AgentMessage;
use crate::domain::execution::ExecutionInput;
use crate::domain::repository::WorkflowExecutionRepository;
use crate::domain::smcp_session::SmcpSessionError;
use crate::infrastructure::event_bus::EventBus;
use crate::presentation::metrics_middleware::metrics_middleware;
use crate::presentation::stimulus_handlers::{ingest_stimulus_handler, webhook_handler};
use crate::presentation::webhook_guard::{
    EnvWebhookSecretProvider, WebhookHmacState, WebhookSecretProvider,
};
use axum::{
    extract::{FromRef, Path, Query, State},
    middleware,
    response::{IntoResponse, Sse},
    routing::{get, post},
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
        .layer(middleware::from_fn(metrics_middleware))
        .with_state(state)
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
        .start_execution(agent_id, input)
        .await
    {
        Ok(id) => Json(json!({ "execution_id": id.0.to_string() })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

async fn stream_execution(
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

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
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
        protocol: None,
        security_token: req.security_token,
        signature: req.signature,
        inner_mcp,
        timestamp: None,
    };

    match tool_svc.invoke_tool(&envelope).await {
        Ok(res) => (axum::http::StatusCode::OK, Json(res)).into_response(),
        Err(e) => {
            // Map domain errors to HTTP status + MCP JSON-RPC error codes (ADR-055).
            // Callers receive a standard `{"jsonrpc":"2.0","error":{"code":N,"message":"..."}}` body
            // so MCP clients can handle them uniformly regardless of transport.
            let (http_status, rpc_code) = match &e {
                SmcpSessionError::SessionExpired
                | SmcpSessionError::SignatureVerificationFailed(_) => {
                    (axum::http::StatusCode::UNAUTHORIZED, -32000_i32)
                }
                SmcpSessionError::PolicyViolation(_) | SmcpSessionError::SessionInactive(_) => {
                    (axum::http::StatusCode::FORBIDDEN, -32000_i32)
                }
                SmcpSessionError::MalformedPayload(_)
                | SmcpSessionError::ReplayProtectionFailed(_) => {
                    (axum::http::StatusCode::BAD_REQUEST, -32600_i32)
                }
                SmcpSessionError::InvalidArguments(_) => {
                    (axum::http::StatusCode::UNPROCESSABLE_ENTITY, -32602_i32)
                }
                SmcpSessionError::JudgeTimeout(_) | SmcpSessionError::InternalError(_) => {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, -32603_i32)
                }
            };
            (
                http_status,
                Json(json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": rpc_code,
                        "message": e.to_string(),
                        "data": null
                    }
                })),
            )
                .into_response()
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
