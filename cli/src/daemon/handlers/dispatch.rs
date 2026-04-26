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

use tracing::Instrument;

use crate::daemon::state::AppState;

pub(crate) async fn dispatch_gateway_handler(
    State(state): State<Arc<AppState>>,
    Json(agent_msg): Json<aegis_orchestrator_core::domain::dispatch::AgentMessage>,
) -> impl IntoResponse {
    use aegis_orchestrator_core::domain::dispatch::{AgentMessage, OrchestratorMessage};

    let started_at = std::time::Instant::now();

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

    // Open a per-request span so every downstream tracing call (inner loop,
    // provider registry, HTTP adapter) carries `execution_id` + `iteration`.
    // Use `Instrument` (not `.entered()`) so the resulting future stays `Send`
    // and can be served by axum.
    let span = tracing::info_span!(
        "dispatch_gateway",
        execution_id = exec_id_opt
            .map(|u| u.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        iteration = iteration_number,
    );

    async move {

    tracing::info!(
        model_alias = model_opt.as_deref().unwrap_or("(none)"),
        kind = match &agent_msg {
            AgentMessage::Generate { .. } => "generate",
            AgentMessage::DispatchResult { .. } => "dispatch_result",
        },
        "dispatch_gateway request received"
    );

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
                name: None,
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

    let result = state
        .inner_loop_service
        .handle_agent_message_with_identity(agent_msg, user_identity, None)
        .await;

    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    match &result {
        Ok(OrchestratorMessage::Final { .. }) => {
            tracing::info!(outcome = "final", elapsed_ms, "dispatch_gateway returning")
        }
        Ok(OrchestratorMessage::Dispatch { .. }) => tracing::info!(
            outcome = "dispatch",
            elapsed_ms,
            "dispatch_gateway returning"
        ),
        Err(_) => tracing::info!(outcome = "error", elapsed_ms, "dispatch_gateway returning"),
    }

    match result {
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

            // If the underlying error is a typed `LLMError`, surface its
            // class as both the HTTP status (so bootstrap.py / SDK callers
            // can react) and a structured `LlmCallFailed` execution event
            // (so parent agents reading the execution stream see the
            // upstream failure class without parsing strings).
            if let Some(llm_err) =
                e.downcast_ref::<aegis_orchestrator_core::domain::llm::LLMError>()
            {
                if agent_id.0 != Uuid::nil() {
                    if let (Some(exec_id), Some(model_alias)) = (exec_id_opt, model_opt.as_ref()) {
                        let error_class =
                            aegis_orchestrator_core::domain::events::LlmErrorClass::from(llm_err);
                        let event = aegis_orchestrator_core::domain::events::ExecutionEvent::LlmCallFailed {
                            execution_id: aegis_orchestrator_core::domain::execution::ExecutionId(
                                exec_id,
                            ),
                            agent_id,
                            iteration_number,
                            provider: "orchestrator".to_string(),
                            model: model_alias.clone(),
                            error_class,
                            message: llm_err.to_string(),
                            // The registry owns retry/fallback bookkeeping and
                            // does not surface counts back here. Encode "unknown"
                            // as 0 — downstream consumers should treat 0 as N/A.
                            attempts: 0,
                            elapsed_ms: 0,
                            fallback_attempted: false,
                            timestamp: chrono::Utc::now(),
                        };
                        state.event_bus.publish_execution_event(event);
                    }
                }

                let (status, tag) = llm_error_to_status(llm_err);
                return (
                    status,
                    Json(serde_json::json!({
                        "error": format!("{tag}: {llm_err}")
                    })),
                );
            }

            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        }
    }

    }
    .instrument(span)
    .await
}

/// Map an `LLMError` variant to a precise HTTP status + a short tag suitable
/// for inclusion in the JSON `error` body. Tags are stable identifiers that
/// downstream consumers (bootstrap.py stderr, SDK clients, log indexers) can
/// match against without parsing the upstream error message.
fn llm_error_to_status(
    e: &aegis_orchestrator_core::domain::llm::LLMError,
) -> (StatusCode, &'static str) {
    use aegis_orchestrator_core::domain::llm::LLMError;
    match e {
        LLMError::Authentication(_) => (StatusCode::BAD_GATEWAY, "upstream_authentication"),
        LLMError::RateLimit => (StatusCode::TOO_MANY_REQUESTS, "upstream_rate_limit"),
        LLMError::ModelNotFound(_) => (StatusCode::BAD_GATEWAY, "upstream_model_not_found"),
        LLMError::InvalidInput(_) => (StatusCode::BAD_GATEWAY, "upstream_invalid_input"),
        LLMError::ServiceUnavailable(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable")
        }
        LLMError::Network(_) => (StatusCode::BAD_GATEWAY, "upstream_network"),
        LLMError::Provider(_) => (StatusCode::BAD_GATEWAY, "upstream_provider"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::llm::LLMError;

    #[test]
    fn llm_error_to_status_authentication() {
        let (status, tag) = llm_error_to_status(&LLMError::Authentication("x".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(tag, "upstream_authentication");
    }

    #[test]
    fn llm_error_to_status_rate_limit() {
        let (status, tag) = llm_error_to_status(&LLMError::RateLimit);
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(tag, "upstream_rate_limit");
    }

    #[test]
    fn llm_error_to_status_model_not_found() {
        let (status, tag) = llm_error_to_status(&LLMError::ModelNotFound("x".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(tag, "upstream_model_not_found");
    }

    #[test]
    fn llm_error_to_status_invalid_input() {
        let (status, tag) = llm_error_to_status(&LLMError::InvalidInput("x".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(tag, "upstream_invalid_input");
    }

    #[test]
    fn llm_error_to_status_service_unavailable() {
        let (status, tag) = llm_error_to_status(&LLMError::ServiceUnavailable("x".into()));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(tag, "upstream_unavailable");
    }

    #[test]
    fn llm_error_to_status_network() {
        let (status, tag) = llm_error_to_status(&LLMError::Network("x".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(tag, "upstream_network");
    }

    #[test]
    fn llm_error_to_status_provider() {
        let (status, tag) = llm_error_to_status(&LLMError::Provider("x".into()));
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(tag, "upstream_provider");
    }
}
