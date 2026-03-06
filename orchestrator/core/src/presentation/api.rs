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
//! | `POST` | `/v1/stimuli` | Stimulus ingestion — Keycloak Bearer auth (ADR-021) |
//! | `POST` | `/v1/webhooks/{source}` | Webhook ingestion — HMAC-SHA256 (ADR-021) |
//! | `GET` | `/health` | liveness probe |

use crate::application::execution::ExecutionService;
use crate::application::inner_loop_service::InnerLoopService;
use crate::application::stimulus::StimulusService;
use crate::application::tool_invocation_service::ToolInvocationService;
use crate::domain::agent::AgentId;
use crate::domain::dispatch::AgentMessage;
use crate::domain::execution::ExecutionInput;
use crate::presentation::stimulus_handlers::{ingest_stimulus_handler, webhook_handler};
use crate::presentation::webhook_guard::{
    EnvWebhookSecretProvider, WebhookHmacState, WebhookSecretProvider,
};
use axum::{
    extract::{FromRef, Path, State},
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
        // Dispatch gateway (ADR-038)
        .route("/v1/dispatch-gateway", post(dispatch_gateway))
        // BC-8 Stimulus-Response (ADR-021)
        .route("/v1/stimuli", post(ingest_stimulus_handler))
        .route("/v1/webhooks/:source", post(webhook_handler))
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
        .route("/v1/dispatch-gateway", post(dispatch_gateway))
        // BC-8 Stimulus-Response (ADR-021)
        .route("/v1/stimuli", post(ingest_stimulus_handler))
        .route("/v1/webhooks/:source", post(webhook_handler))
        .with_state(state)
}

#[derive(serde::Deserialize)]
pub struct StartExecutionRequest {
    pub agent_id: String,
    pub input: String,
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
            "input": payload.input  // User-provided input from REST API
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
    pub container_id: String,
    /// Base64-encoded ephemeral Ed25519 public key generated by the agent.
    pub agent_public_key: String,
    /// Agent ID (UUID).
    pub agent_id: String,
    /// Execution ID (UUID).
    pub execution_id: String,
}

/// Handle SMCP attestation handshake (ADR-035 §4.1).
///
/// The agent sends its ephemeral public key; the orchestrator verifies the
/// container identity, looks up the SecurityContext, creates an SmcpSession,
/// and returns a signed SecurityToken (JWT).
async fn smcp_attestation(
    State(_state): State<Arc<AppState>>,
    Json(payload): Json<SmcpAttestationRequest>,
) -> impl IntoResponse {
    // Phase 1: Return a placeholder attestation response.
    // Phase 2 will wire to AttestationServiceImpl for full crypto flow.
    tracing::info!(
        agent_id = %payload.agent_id,
        execution_id = %payload.execution_id,
        container_id = %payload.container_id,
        "SMCP attestation request received"
    );

    Json(json!({
        "security_token": format!("placeholder-token-{}", payload.execution_id),
        "session_id": uuid::Uuid::new_v4().to_string(),
        "expires_at": chrono::Utc::now() + chrono::Duration::hours(1),
    }))
}

/// SMCP tool invocation request payload (ADR-033 §3).
#[derive(serde::Deserialize)]
pub struct SmcpToolInvokeRequest {
    /// Agent ID (UUID).
    pub agent_id: String,
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
    tracing::info!(agent_id = %req.agent_id, "SMCP tool invocation request received");

    let tool_svc = match &state.tool_invocation_service {
        Some(svc) => svc.clone(),
        None => {
            return (
                axum::http::StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "SMCP tool invocation service not wired in this configuration"
                })),
            )
                .into_response();
        }
    };

    let agent_id = match uuid::Uuid::parse_str(&req.agent_id) {
        Ok(uid) => AgentId(uid),
        Err(_) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid agent_id" })),
            )
                .into_response();
        }
    };

    let inner_mcp = serde_json::to_vec(&req.payload).unwrap_or_default();
    let envelope = crate::infrastructure::smcp::envelope::SmcpEnvelope {
        security_token: req.security_token,
        signature: req.signature,
        inner_mcp,
    };

    match tool_svc.invoke_tool(&agent_id, &envelope).await {
        Ok(res) => (axum::http::StatusCode::OK, Json(res)).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
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
