// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Dispatch gateway and temporal events handlers.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::infrastructure::TemporalEventPayload;

use crate::daemon::state::AppState;

pub(crate) async fn dispatch_gateway_handler(
    State(state): State<Arc<AppState>>,
    Json(agent_msg): Json<aegis_orchestrator_core::domain::dispatch::AgentMessage>,
) -> impl IntoResponse {
    use aegis_orchestrator_core::domain::dispatch::{AgentMessage, OrchestratorMessage};

    let (exec_id_opt, iteration_number, prompt_opt, model_opt) = match &agent_msg {
        AgentMessage::Generate {
            execution_id,
            iteration_number,
            prompt,
            model_alias,
            ..
        } => (
            Uuid::parse_str(execution_id).ok(),
            *iteration_number,
            Some(prompt.clone()),
            Some(model_alias.clone()),
        ),
        AgentMessage::DispatchResult { execution_id, .. } => {
            (Uuid::parse_str(execution_id).ok(), 0, None, None)
        }
    };

    // Resolve agent_id and initiating user identity from the execution record.
    let (agent_id, user_identity) = if let Some(exec_id) = exec_id_opt {
        let execution_id = aegis_orchestrator_core::domain::execution::ExecutionId(exec_id);
        if let Ok(exec) = state
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
        {
            let identity = exec.initiating_user_sub.as_ref().map(|sub| UserIdentity {
                sub: sub.clone(),
                realm_slug: "zaru-consumer".to_string(),
                email: None,
                identity_kind: IdentityKind::ConsumerUser {
                    zaru_tier: ZaruTier::Free,
                    tenant_id: exec.tenant_id.clone(),
                },
            });
            (exec.agent_id, identity)
        } else {
            tracing::warn!("Could not find execution {} for LLM event", exec_id);
            (
                aegis_orchestrator_core::domain::agent::AgentId(Uuid::nil()),
                None,
            )
        }
    } else {
        (
            aegis_orchestrator_core::domain::agent::AgentId(Uuid::nil()),
            None,
        )
    };

    match state
        .inner_loop_service
        .handle_agent_message_with_identity(agent_msg, user_identity, None)
        .await
    {
        Ok(OrchestratorMessage::Final {
            content,
            tool_calls_executed,
            trajectory,
            ..
        }) => {
            // Publish LlmInteraction event for observability
            if agent_id.0 != Uuid::nil() {
                if let (Some(exec_id), Some(prompt), Some(model_alias)) =
                    (exec_id_opt, prompt_opt, model_opt)
                {
                    let event =
                        aegis_orchestrator_core::domain::events::ExecutionEvent::LlmInteraction {
                            execution_id: aegis_orchestrator_core::domain::execution::ExecutionId(
                                exec_id,
                            ),
                            agent_id,
                            iteration_number,
                            provider: "orchestrator".to_string(),
                            model: model_alias.clone(),
                            input_tokens: None,
                            output_tokens: None,
                            prompt: prompt.clone(),
                            response: content.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                    state.event_bus.publish_execution_event(event);

                    let interaction = aegis_orchestrator_core::domain::execution::LlmInteraction {
                        provider: "orchestrator".to_string(),
                        model: model_alias.clone(),
                        prompt: prompt.clone(),
                        response: content.clone(),
                        timestamp: chrono::Utc::now(),
                    };
                    let _ = state
                        .execution_service
                        .record_llm_interaction(
                            aegis_orchestrator_core::domain::execution::ExecutionId(exec_id),
                            iteration_number,
                            interaction,
                        )
                        .await;

                    // Store the live trajectory from the inner loop directly — it already
                    // has correct statuses (succeeded/failed/dispatched) because
                    // InnerLoopService builds it in real time as tool calls complete.
                    if !trajectory.is_empty() {
                        let _ = state
                            .execution_service
                            .store_iteration_trajectory(
                                aegis_orchestrator_core::domain::execution::ExecutionId(exec_id),
                                iteration_number,
                                trajectory,
                            )
                            .await;
                    }
                }
            }

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "content": content,
                    "tool_calls_executed": tool_calls_executed,
                })),
            )
        }
        Ok(OrchestratorMessage::Dispatch {
            dispatch_id,
            action,
        }) => {
            // Respond with the dispatch action so bootstrap.py can execute it
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(OrchestratorMessage::Dispatch {
                        dispatch_id,
                        action,
                    })
                    .unwrap_or_else(
                        |_| serde_json::json!({"error": "dispatch serialization failed"}),
                    ),
                ),
            )
        }
        Err(e) => {
            tracing::error!("Inner loop generation failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}

/// POST /v1/temporal-events - Receive events from Temporal worker
pub(crate) async fn temporal_events_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<TemporalEventPayload>,
) -> impl IntoResponse {
    // Authenticate via Bearer JWT from the aegis-temporal-worker service account.
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match auth_header.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t,
        None => {
            tracing::warn!("temporal-events: missing or malformed Authorization header");
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Unauthorized"})),
            )
                .into_response();
        }
    };

    let iam = match state.iam_service.as_ref() {
        Some(svc) => svc,
        None => {
            tracing::error!(
                "temporal-events: IAM service not configured; cannot authenticate request"
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Unauthorized"})),
            )
                .into_response();
        }
    };

    match iam.validate_token(token).await {
        Ok(validated) => match &validated.identity.identity_kind {
            IdentityKind::ServiceAccount { client_id } if client_id == "aegis-temporal-worker" => {}
            other => {
                tracing::warn!(
                    identity_kind = ?other,
                    "temporal-events: token is valid but not from aegis-temporal-worker"
                );
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "Unauthorized"})),
                )
                    .into_response();
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "temporal-events: JWT validation failed");
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Unauthorized"})),
            )
                .into_response();
        }
    }

    match state.temporal_event_listener.handle_event(payload).await {
        Ok(execution_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "execution_id": execution_id,
                "status": "received"
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to process event: {}", e)
            })),
        )
            .into_response(),
    }
}
