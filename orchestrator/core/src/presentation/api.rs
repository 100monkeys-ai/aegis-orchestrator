// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use axum::{
    routing::{get, post},
    Router, Json, extract::{State, Path},
    response::{Sse, IntoResponse},
};
use std::sync::Arc;
use tokio_stream::StreamExt;
use crate::application::execution::ExecutionService;
use crate::domain::execution::ExecutionInput;
use crate::domain::agent::AgentId;
use serde_json::json;
use futures::stream::Stream;
use std::pin::Pin;

pub struct AppState {
    pub execution_service: Arc<dyn ExecutionService>,
    pub human_input_service: Arc<crate::infrastructure::HumanInputService>,
}

pub fn app(
    execution_service: Arc<dyn ExecutionService>,
    human_input_service: Arc<crate::infrastructure::HumanInputService>,
) -> Router {
    let state = Arc::new(AppState { 
        execution_service,
        human_input_service,
    });
    
    Router::new()
        .route("/executions", post(start_execution))
        .route("/executions/:id/stream", get(stream_execution))
        .route("/human-approvals", get(list_pending_approvals))
        .route("/human-approvals/:id", get(get_pending_approval))
        .route("/human-approvals/:id/approve", post(approve_request))
        .route("/human-approvals/:id/reject", post(reject_request))
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

    let input = ExecutionInput {
        intent: Some(payload.input),
        payload: serde_json::Value::Null,
    };

    match state.execution_service.start_execution(agent_id, input).await {
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
            let stream: Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>> = Box::pin(tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1).map(Ok::<_, axum::Error>));
            return Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default());
        }
    };

    let stream: Pin<Box<dyn Stream<Item = Result<axum::response::sse::Event, axum::Error>> + Send>> = match state.execution_service.stream_execution(execution_id).await {
        Ok(s) => {
            Box::pin(s.map(|event_res| {
                match event_res {
                    Ok(event) => {
                        Ok(axum::response::sse::Event::default()
                            .data(serde_json::to_string(&event).unwrap_or_default()))
                    },
                    Err(_) => Ok(axum::response::sse::Event::default().data("error")),
                }
            }))
        }
        Err(_) => {
             // Return empty stream on error
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(tokio::sync::mpsc::channel(1).1).map(Ok::<_, axum::Error>))
        }
    };
    
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// ============================================================================
// Human Approval Endpoints
// ============================================================================

/// List all pending approval requests
async fn list_pending_approvals(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
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

    match state.human_input_service.get_pending_request(request_id).await {
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

    match state.human_input_service
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

    match state.human_input_service
        .submit_rejection(request_id, payload.reason, payload.rejected_by)
        .await
    {
        Ok(()) => Json(json!({ "status": "rejected" })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}
