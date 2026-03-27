// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Stimulus HTTP Handlers (BC-8 — ADR-021)
//!
//! Two endpoints:
//!
//! | Method | Path | Auth | Handler |
//! |--------|------|------|---------|
//! | `POST` | `/v1/stimuli` | IAM/OIDC Bearer JWT | [`ingest_stimulus_handler`] |
//! | `POST` | `/v1/webhooks/{source}` | HMAC-SHA256 (`X-Aegis-Signature`) | [`webhook_handler`] |
//!
//! Both endpoints:
//! - Return `202 Accepted` with `{ stimulus_id, workflow_execution_id }` and `Location` header
//! - Return `409 Conflict` for idempotent duplicates
//! - Return `422 Unprocessable Entity` for low-confidence classification
//! - Return `503 Service Unavailable` if stimulus service is not wired in app state

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use base64::Engine as _;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tracing::warn;

use crate::application::stimulus::StimulusError;
use crate::domain::stimulus::{Stimulus, StimulusSource};
use crate::presentation::api::AppState;
use crate::presentation::webhook_guard::WebhookHmacGuard;

// ──────────────────────────────────────────────────────────────────────────────
// Request body types
// ──────────────────────────────────────────────────────────────────────────────

/// Request body for `POST /v1/stimuli`.
#[derive(Debug, serde::Deserialize)]
pub struct IngestStimulusBody {
    /// Source name (e.g. `"github"`, `"stripe"`, `"custom-webhook"`).
    pub source: String,
    /// Raw stimulus content (JSON, plain text, or base64-encoded binary).
    pub content: String,
    /// Optional idempotency key to prevent duplicate processing.
    pub idempotency_key: Option<String>,
}

// ──────────────────────────────────────────────────────────────────────────────
// Helpers
// ──────────────────────────────────────────────────────────────────────────────

fn axum_headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.as_str().to_string(), val.to_string()))
        })
        .collect()
}

fn stimulus_error_response(e: StimulusError) -> (StatusCode, axum::Json<serde_json::Value>) {
    let status = match e.http_status() {
        409 => StatusCode::CONFLICT,
        422 => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = json!({
        "error": e.error_code(),
        "message": e.to_string(),
    });
    (status, Json(body))
}

fn workflow_execution_logs_location(workflow_execution_id: &str) -> String {
    format!("/v1/workflows/executions/{workflow_execution_id}/logs")
}

// ──────────────────────────────────────────────────────────────────────────────
// Handlers
// ──────────────────────────────────────────────────────────────────────────────

/// `POST /v1/stimuli` — Authenticated (IAM/OIDC Bearer JWT)
///
/// Accepts a stimulus from any authenticated caller. Auth is enforced by the
/// upstream middleware layer.
///
/// Returns `202 Accepted` with `{ stimulus_id, workflow_execution_id }`.
pub async fn ingest_stimulus_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<IngestStimulusBody>,
) -> impl IntoResponse {
    let stimulus_service = match &state.stimulus_service {
        Some(svc) => svc.clone(),
        None => {
            warn!("StimulusService not configured; rejecting POST /v1/stimuli");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "stimulus_service_unavailable", "message": "Stimulus service is not configured" })),
            ).into_response();
        }
    };

    let stimulus = Stimulus::new(StimulusSource::HttpApi, body.content)
        .with_headers(axum_headers_to_map(&headers));
    let stimulus = if let Some(key) = body.idempotency_key {
        stimulus.with_idempotency_key(key)
    } else {
        stimulus
    };

    match stimulus_service.ingest(stimulus).await {
        Ok(resp) => {
            let location = workflow_execution_logs_location(&resp.workflow_execution_id);
            let mut response_headers = HeaderMap::new();
            if let Ok(loc_val) = location.parse() {
                response_headers.insert(axum::http::header::LOCATION, loc_val);
            }
            (
                StatusCode::ACCEPTED,
                response_headers,
                Json(json!({
                    "stimulus_id": resp.stimulus_id.to_string(),
                    "workflow_execution_id": resp.workflow_execution_id,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let (status, body) = stimulus_error_response(e);
            (status, body).into_response()
        }
    }
}

/// `POST /v1/webhooks/{source}` — HMAC-SHA256 verified
///
/// Accepts a raw webhook payload from an external system. The `source` path
/// parameter identifies the webhook source (e.g. `"github"`, `"stripe"`).
///
/// Authentication is performed by [`WebhookHmacGuard`]: it reads the
/// `X-Aegis-Signature: sha256=<hex>` header and verifies it against the
/// secret configured for this source.
///
/// Returns `202 Accepted` with `{ stimulus_id, workflow_execution_id }`.
pub async fn webhook_handler(
    State(state): State<Arc<AppState>>,
    Path(source): Path<String>,
    headers: HeaderMap,
    guard: WebhookHmacGuard,
) -> impl IntoResponse {
    let stimulus_service = match &state.stimulus_service {
        Some(svc) => svc.clone(),
        None => {
            warn!(source = %source, "StimulusService not configured; rejecting webhook");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "stimulus_service_unavailable", "message": "Stimulus service is not configured" })),
            ).into_response();
        }
    };

    // Convert verified body bytes to String (treat as UTF-8 if possible, else base64)
    let content = String::from_utf8(guard.body.to_vec())
        .unwrap_or_else(|_| base64::engine::general_purpose::STANDARD.encode(&guard.body));

    let stimulus = Stimulus::new(
        StimulusSource::Webhook {
            source_name: source.clone(),
        },
        content,
    )
    .with_headers(axum_headers_to_map(&headers));

    match stimulus_service.ingest(stimulus).await {
        Ok(resp) => {
            let location = workflow_execution_logs_location(&resp.workflow_execution_id);
            let mut response_headers = HeaderMap::new();
            if let Ok(loc_val) = location.parse() {
                response_headers.insert(axum::http::header::LOCATION, loc_val);
            }
            (
                StatusCode::ACCEPTED,
                response_headers,
                Json(json!({
                    "stimulus_id": resp.stimulus_id.to_string(),
                    "workflow_execution_id": resp.workflow_execution_id,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let (status, body) = stimulus_error_response(e);
            (status, body).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::execution::ExecutionService;
    use crate::application::stimulus::{StimulusIngestResponse, StimulusService};
    use crate::domain::agent::AgentId;
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{
        Execution, ExecutionId, Iteration, LlmInteraction, TrajectoryStep,
    };
    use crate::infrastructure::HumanInputService;
    use crate::presentation::webhook_guard::{EnvWebhookSecretProvider, WebhookSecretProvider};
    use anyhow::Result;
    use async_trait::async_trait;
    use axum::body::{to_bytes, Bytes};
    use axum::http::header::LOCATION;
    use axum::http::HeaderName;
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    struct PanicExecutionService;

    #[async_trait]
    impl ExecutionService for PanicExecutionService {
        async fn start_execution(
            &self,
            _agent_id: AgentId,
            _input: crate::domain::execution::ExecutionInput,
        ) -> Result<ExecutionId> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn start_child_execution(
            &self,
            _agent_id: AgentId,
            _input: crate::domain::execution::ExecutionInput,
            _parent_execution_id: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn get_execution(&self, _id: ExecutionId) -> Result<Execution> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn get_iterations(&self, _exec_id: ExecutionId) -> Result<Vec<Iteration>> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn cancel_execution(&self, _id: ExecutionId) -> Result<()> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn stream_execution(
            &self,
            _id: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
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
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn list_executions(
            &self,
            _agent_id: Option<AgentId>,
            _limit: usize,
        ) -> Result<Vec<Execution>> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn delete_execution(&self, _id: ExecutionId) -> Result<()> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn record_llm_interaction(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _interaction: LlmInteraction,
        ) -> Result<()> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }

        async fn store_iteration_trajectory(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _trajectory: Vec<TrajectoryStep>,
        ) -> Result<()> {
            anyhow::bail!("execution service should not be used by stimulus handler tests")
        }
    }

    #[derive(Clone)]
    enum StubOutcome {
        Success {
            stimulus_id: crate::domain::stimulus::StimulusId,
            workflow_execution_id: String,
        },
        Duplicate {
            original_id: crate::domain::stimulus::StimulusId,
        },
        LowConfidence,
    }

    struct RecordingStimulusService {
        outcome: StubOutcome,
        captured: Mutex<Vec<Stimulus>>,
    }

    impl RecordingStimulusService {
        fn new(outcome: StubOutcome) -> Self {
            Self {
                outcome,
                captured: Mutex::new(Vec::new()),
            }
        }

        fn captured(&self) -> Vec<Stimulus> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl StimulusService for RecordingStimulusService {
        async fn ingest(
            &self,
            stimulus: Stimulus,
        ) -> Result<StimulusIngestResponse, StimulusError> {
            self.captured.lock().unwrap().push(stimulus);
            match &self.outcome {
                StubOutcome::Success {
                    stimulus_id,
                    workflow_execution_id,
                } => Ok(StimulusIngestResponse {
                    stimulus_id: *stimulus_id,
                    workflow_execution_id: workflow_execution_id.clone(),
                }),
                StubOutcome::Duplicate { original_id } => Err(StimulusError::IdempotentDuplicate {
                    original_id: *original_id,
                }),
                StubOutcome::LowConfidence => Err(StimulusError::LowConfidence {
                    confidence: 0.42,
                    threshold: 0.7,
                }),
            }
        }

        async fn register_route(
            &self,
            _source_name: &str,
            _workflow_id: crate::domain::workflow::WorkflowId,
        ) -> Result<()> {
            Ok(())
        }

        async fn remove_route(&self, _source_name: &str) -> bool {
            false
        }
    }

    fn app_state(stimulus_service: Option<Arc<dyn StimulusService>>) -> Arc<AppState> {
        Arc::new(AppState {
            execution_service: Arc::new(PanicExecutionService),
            human_input_service: Arc::new(HumanInputService::new()),
            inner_loop_service: None,
            stimulus_service,
            webhook_secret_provider: Arc::new(EnvWebhookSecretProvider)
                as Arc<dyn WebhookSecretProvider>,
            tool_invocation_service: None,
            attestation_service: None,
            workflow_execution_repo: None,
            event_bus: None,
            tenant_repo: None,
            smcp_session_repo: None,
        })
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn header_map(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[tokio::test]
    async fn ingest_handler_returns_202_location_and_forwards_headers() {
        let stub = Arc::new(RecordingStimulusService::new(StubOutcome::Success {
            stimulus_id: crate::domain::stimulus::StimulusId(Uuid::new_v4()),
            workflow_execution_id: "wf-exec-123".to_string(),
        }));
        let response = ingest_stimulus_handler(
            State(app_state(Some(stub.clone()))),
            header_map(&[("x-aegis-tenant", "tenant-7"), ("x-request-id", "req-42")]),
            Json(IngestStimulusBody {
                source: "github".to_string(),
                content: "{\"event\":\"push\"}".to_string(),
                idempotency_key: Some("idem-1".to_string()),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers().get(LOCATION).unwrap(),
            "/v1/workflows/executions/wf-exec-123/logs"
        );

        let body = response_json(response).await;
        assert_eq!(body["workflow_execution_id"], "wf-exec-123");

        let captured = stub.captured();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].source, StimulusSource::HttpApi);
        assert_eq!(captured[0].content, "{\"event\":\"push\"}");
        assert_eq!(captured[0].idempotency_key.as_deref(), Some("idem-1"));
        assert_eq!(
            captured[0]
                .headers
                .get("x-aegis-tenant")
                .map(String::as_str),
            Some("tenant-7")
        );
        assert_eq!(
            captured[0].headers.get("x-request-id").map(String::as_str),
            Some("req-42")
        );
    }

    #[tokio::test]
    async fn ingest_handler_maps_conflict_errors_to_409() {
        let original_id = crate::domain::stimulus::StimulusId(Uuid::new_v4());
        let response = ingest_stimulus_handler(
            State(app_state(Some(Arc::new(RecordingStimulusService::new(
                StubOutcome::Duplicate { original_id },
            ))))),
            HeaderMap::new(),
            Json(IngestStimulusBody {
                source: "github".to_string(),
                content: "payload".to_string(),
                idempotency_key: Some("idem-1".to_string()),
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = response_json(response).await;
        assert_eq!(body["error"], "idempotent_duplicate");
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains(&original_id.to_string()));
    }

    #[tokio::test]
    async fn ingest_handler_maps_low_confidence_errors_to_422() {
        let response = ingest_stimulus_handler(
            State(app_state(Some(Arc::new(RecordingStimulusService::new(
                StubOutcome::LowConfidence,
            ))))),
            HeaderMap::new(),
            Json(IngestStimulusBody {
                source: "github".to_string(),
                content: "payload".to_string(),
                idempotency_key: None,
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response_json(response).await;
        assert_eq!(body["error"], "classification_failed");
    }

    #[tokio::test]
    async fn ingest_handler_returns_503_when_stimulus_service_missing() {
        let response = ingest_stimulus_handler(
            State(app_state(None)),
            HeaderMap::new(),
            Json(IngestStimulusBody {
                source: "github".to_string(),
                content: "payload".to_string(),
                idempotency_key: None,
            }),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response_json(response).await;
        assert_eq!(body["error"], "stimulus_service_unavailable");
    }

    #[tokio::test]
    async fn webhook_handler_base64_encodes_binary_body_and_preserves_headers() {
        let stub = Arc::new(RecordingStimulusService::new(StubOutcome::Success {
            stimulus_id: crate::domain::stimulus::StimulusId(Uuid::new_v4()),
            workflow_execution_id: "wf-exec-999".to_string(),
        }));
        let raw_body = vec![0, 159, 146, 150];

        let response = webhook_handler(
            State(app_state(Some(stub.clone()))),
            Path("github".to_string()),
            header_map(&[("x-github-event", "push")]),
            WebhookHmacGuard {
                source: "github".to_string(),
                body: Bytes::from(raw_body.clone()),
            },
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers().get(LOCATION).unwrap(),
            "/v1/workflows/executions/wf-exec-999/logs"
        );

        let captured = stub.captured();
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].source,
            StimulusSource::Webhook {
                source_name: "github".to_string()
            }
        );
        assert_eq!(
            captured[0].content,
            base64::engine::general_purpose::STANDARD.encode(raw_body)
        );
        assert_eq!(
            captured[0]
                .headers
                .get("x-github-event")
                .map(String::as_str),
            Some("push")
        );
    }
}
