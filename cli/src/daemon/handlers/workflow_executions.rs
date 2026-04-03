// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Workflow execution handlers, log streaming, signal, cancel, remove, and helpers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use sqlx::Row;
use uuid::Uuid;

use aegis_orchestrator_core::domain::events::WorkflowEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::node_config::{resolve_env_value, NodeConfigManifest};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::temporal_proto::temporal::api::common::v1::WorkflowExecution as TemporalWorkflowExecution;
use aegis_orchestrator_core::infrastructure::temporal_proto::temporal::api::workflowservice::v1::{
    DeleteWorkflowExecutionRequest, RequestCancelWorkflowExecutionRequest,
};
use aegis_orchestrator_core::infrastructure::TemporalEventPayload;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;
use crate::daemon::temporal_helpers::{connect_temporal_workflow_client, temporal_namespace};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkflowLogEventView {
    execution_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow_name: Option<String>,
    event_type: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iteration_number: Option<u8>,
    timestamp: String,
    details: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporal_workflow_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporal_run_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowExecutionTemporalLinkage {
    pub(crate) temporal_workflow_id: String,
    pub(crate) temporal_run_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub(crate) struct WorkflowLogQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

pub(crate) async fn workflow_name_map_for_ids(
    workflow_repo: Arc<dyn aegis_orchestrator_core::domain::repository::WorkflowRepository>,
    tenant_id: &TenantId,
    workflow_ids: &[aegis_orchestrator_core::domain::workflow::WorkflowId],
) -> HashMap<Uuid, String> {
    let mut map = HashMap::new();
    for workflow_id in workflow_ids {
        if map.contains_key(&workflow_id.0) {
            continue;
        }
        if let Ok(Some(workflow)) = workflow_repo
            .find_by_id_for_tenant(tenant_id, *workflow_id)
            .await
        {
            map.insert(workflow_id.0, workflow.metadata.name);
        }
    }
    map
}

pub(crate) fn workflow_event_message(event: &WorkflowEvent) -> String {
    match event {
        WorkflowEvent::WorkflowExecutionStarted { .. } => "Workflow execution started".to_string(),
        WorkflowEvent::WorkflowStateEntered { state_name, .. } => {
            format!("Entered workflow state {state_name}")
        }
        WorkflowEvent::WorkflowStateExited { state_name, .. } => {
            format!("Exited workflow state {state_name}")
        }
        WorkflowEvent::WorkflowIterationStarted {
            iteration_number, ..
        } => format!("Workflow iteration {iteration_number} started"),
        WorkflowEvent::WorkflowIterationCompleted {
            iteration_number, ..
        } => format!("Workflow iteration {iteration_number} completed"),
        WorkflowEvent::WorkflowIterationFailed {
            iteration_number,
            error,
            ..
        } => format!("Workflow iteration {iteration_number} failed: {error}"),
        WorkflowEvent::WorkflowExecutionCompleted { .. } => {
            "Workflow execution completed".to_string()
        }
        WorkflowEvent::WorkflowExecutionFailed { reason, .. } => {
            format!("Workflow execution failed: {reason}")
        }
        WorkflowEvent::WorkflowExecutionCancelled { .. } => {
            "Workflow execution cancelled".to_string()
        }
        WorkflowEvent::WorkflowRegistered { name, .. } => {
            format!("Workflow {name} registered")
        }
        WorkflowEvent::WorkflowScopeChanged {
            workflow_name,
            new_scope,
            ..
        } => {
            format!("Workflow {workflow_name} scope changed to {new_scope}")
        }
        WorkflowEvent::SubworkflowTriggered {
            child_workflow_id,
            mode,
            parent_state_name,
            ..
        } => format!(
            "Subworkflow {child_workflow_id} triggered from state {parent_state_name} (mode: {mode})"
        ),
        WorkflowEvent::SubworkflowCompleted {
            child_execution_id,
            result_key,
            ..
        } => format!(
            "Subworkflow execution {child_execution_id} completed (result_key: {result_key})"
        ),
        WorkflowEvent::SubworkflowFailed {
            child_execution_id,
            reason,
            ..
        } => format!("Subworkflow execution {child_execution_id} failed: {reason}"),
        WorkflowEvent::IntentExecutionPipelineStarted { intent, .. } => {
            format!("Intent execution pipeline started: {intent}")
        }
        WorkflowEvent::IntentExecutionPipelineCompleted { final_result, .. } => {
            format!("Intent execution pipeline completed: {final_result}")
        }
        WorkflowEvent::IntentExecutionPipelineFailed { reason, .. } => {
            format!("Intent execution pipeline failed: {reason}")
        }
    }
}

pub(crate) fn workflow_event_type_name(event: &WorkflowEvent) -> &'static str {
    match event {
        WorkflowEvent::WorkflowRegistered { .. } => "WorkflowRegistered",
        WorkflowEvent::WorkflowScopeChanged { .. } => "WorkflowScopeChanged",
        WorkflowEvent::WorkflowExecutionStarted { .. } => "WorkflowExecutionStarted",
        WorkflowEvent::WorkflowStateEntered { .. } => "WorkflowStateEntered",
        WorkflowEvent::WorkflowStateExited { .. } => "WorkflowStateExited",
        WorkflowEvent::WorkflowIterationStarted { .. } => "WorkflowIterationStarted",
        WorkflowEvent::WorkflowIterationCompleted { .. } => "WorkflowIterationCompleted",
        WorkflowEvent::WorkflowIterationFailed { .. } => "WorkflowIterationFailed",
        WorkflowEvent::WorkflowExecutionCompleted { .. } => "WorkflowExecutionCompleted",
        WorkflowEvent::WorkflowExecutionFailed { .. } => "WorkflowExecutionFailed",
        WorkflowEvent::WorkflowExecutionCancelled { .. } => "WorkflowExecutionCancelled",
        WorkflowEvent::SubworkflowTriggered { .. } => "SubworkflowTriggered",
        WorkflowEvent::SubworkflowCompleted { .. } => "SubworkflowCompleted",
        WorkflowEvent::SubworkflowFailed { .. } => "SubworkflowFailed",
        WorkflowEvent::IntentExecutionPipelineStarted { .. } => "IntentExecutionPipelineStarted",
        WorkflowEvent::IntentExecutionPipelineCompleted { .. } => {
            "IntentExecutionPipelineCompleted"
        }
        WorkflowEvent::IntentExecutionPipelineFailed { .. } => "IntentExecutionPipelineFailed",
    }
}

pub(crate) fn workflow_event_view_from_domain(
    event: &WorkflowEvent,
    workflow_name: Option<String>,
    workflow_id: Option<Uuid>,
    temporal_linkage: Option<&WorkflowExecutionTemporalLinkage>,
) -> WorkflowLogEventView {
    let (execution_id, state_name, iteration_number, timestamp, details) = match event {
        WorkflowEvent::WorkflowRegistered { registered_at, .. } => (
            Uuid::nil(),
            None,
            None,
            registered_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowScopeChanged { changed_at, .. } => (
            Uuid::nil(),
            None,
            None,
            changed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowExecutionStarted {
            execution_id,
            started_at,
            ..
        } => (
            execution_id.0,
            None,
            None,
            started_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowStateEntered {
            execution_id,
            state_name,
            entered_at,
        } => (
            execution_id.0,
            Some(state_name.clone()),
            None,
            entered_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowStateExited {
            execution_id,
            state_name,
            exited_at,
            ..
        } => (
            execution_id.0,
            Some(state_name.clone()),
            None,
            exited_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowIterationStarted {
            execution_id,
            iteration_number,
            started_at,
        } => (
            execution_id.0,
            None,
            Some(*iteration_number),
            started_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowIterationCompleted {
            execution_id,
            iteration_number,
            completed_at,
            ..
        } => (
            execution_id.0,
            None,
            Some(*iteration_number),
            completed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowIterationFailed {
            execution_id,
            iteration_number,
            failed_at,
            ..
        } => (
            execution_id.0,
            None,
            Some(*iteration_number),
            failed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowExecutionCompleted {
            execution_id,
            completed_at,
            ..
        } => (
            execution_id.0,
            None,
            None,
            completed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowExecutionFailed {
            execution_id,
            failed_at,
            ..
        } => (
            execution_id.0,
            None,
            None,
            failed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::WorkflowExecutionCancelled {
            execution_id,
            cancelled_at,
        } => (
            execution_id.0,
            None,
            None,
            cancelled_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::SubworkflowTriggered {
            parent_execution_id,
            triggered_at,
            parent_state_name,
            ..
        } => (
            parent_execution_id.0,
            Some(parent_state_name.clone()),
            None,
            triggered_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::SubworkflowCompleted {
            parent_execution_id,
            completed_at,
            ..
        } => (
            parent_execution_id.0,
            None,
            None,
            completed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::SubworkflowFailed {
            parent_execution_id,
            failed_at,
            ..
        } => (
            parent_execution_id.0,
            None,
            None,
            failed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::IntentExecutionPipelineStarted {
            pipeline_execution_id,
            started_at,
            ..
        } => (
            pipeline_execution_id.0,
            None,
            None,
            started_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::IntentExecutionPipelineCompleted {
            pipeline_execution_id,
            completed_at,
            ..
        } => (
            pipeline_execution_id.0,
            None,
            None,
            completed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        WorkflowEvent::IntentExecutionPipelineFailed {
            pipeline_execution_id,
            failed_at,
            ..
        } => (
            pipeline_execution_id.0,
            None,
            None,
            failed_at.to_rfc3339(),
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
    };

    WorkflowLogEventView {
        execution_id,
        workflow_id,
        workflow_name,
        event_type: workflow_event_type_name(event).to_string(),
        message: workflow_event_message(event),
        state_name,
        iteration_number,
        timestamp,
        details,
        temporal_workflow_id: temporal_linkage.map(|linkage| linkage.temporal_workflow_id.clone()),
        temporal_run_id: temporal_linkage.map(|linkage| linkage.temporal_run_id.clone()),
    }
}

pub(crate) fn workflow_log_event_from_payload(
    execution_id: ExecutionId,
    workflow_id: Uuid,
    workflow_name: Option<String>,
    payload: TemporalEventPayload,
    temporal_linkage: Option<&WorkflowExecutionTemporalLinkage>,
) -> WorkflowLogEventView {
    let event_type = payload.event_type.clone();
    let message = match event_type.as_str() {
        "WorkflowExecutionStarted" => "Workflow execution started".to_string(),
        "WorkflowStateEntered" => payload
            .state_name
            .as_ref()
            .map(|name| format!("Entered workflow state {name}"))
            .unwrap_or_else(|| "Entered workflow state".to_string()),
        "WorkflowStateExited" => payload
            .state_name
            .as_ref()
            .map(|name| format!("Exited workflow state {name}"))
            .unwrap_or_else(|| "Exited workflow state".to_string()),
        "WorkflowIterationStarted" => payload
            .iteration_number
            .map(|iteration| format!("Workflow iteration {iteration} started"))
            .unwrap_or_else(|| "Workflow iteration started".to_string()),
        "WorkflowIterationCompleted" => payload
            .iteration_number
            .map(|iteration| format!("Workflow iteration {iteration} completed"))
            .unwrap_or_else(|| "Workflow iteration completed".to_string()),
        "WorkflowIterationFailed" => match (payload.iteration_number, payload.error.as_deref()) {
            (Some(iteration), Some(error)) => {
                format!("Workflow iteration {iteration} failed: {error}")
            }
            (Some(iteration), None) => format!("Workflow iteration {iteration} failed"),
            _ => "Workflow iteration failed".to_string(),
        },
        "WorkflowExecutionCompleted" => "Workflow execution completed".to_string(),
        "WorkflowExecutionFailed" => payload
            .error
            .as_ref()
            .map(|error| format!("Workflow execution failed: {error}"))
            .unwrap_or_else(|| "Workflow execution failed".to_string()),
        "WorkflowExecutionCancelled" => "Workflow execution cancelled".to_string(),
        _ => event_type.clone(),
    };

    WorkflowLogEventView {
        execution_id: execution_id.0,
        workflow_id: Some(workflow_id),
        workflow_name,
        event_type,
        message,
        state_name: payload.state_name.clone(),
        iteration_number: payload.iteration_number,
        timestamp: payload.timestamp.clone(),
        details: serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null),
        temporal_workflow_id: temporal_linkage.map(|linkage| linkage.temporal_workflow_id.clone()),
        temporal_run_id: temporal_linkage.map(|linkage| linkage.temporal_run_id.clone()),
    }
}

pub(crate) async fn workflow_execution_temporal_linkage(
    config: &NodeConfigManifest,
    execution_id: Uuid,
) -> anyhow::Result<Option<WorkflowExecutionTemporalLinkage>> {
    use anyhow::Context;
    let Some(database) = config.spec.database.as_ref() else {
        return Ok(None);
    };
    let database_url = resolve_env_value(&database.url)
        .with_context(|| "Failed to resolve database URL for workflow execution linkage")?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .context("Failed to connect to PostgreSQL for workflow execution linkage")?;

    let row = sqlx::query(
        r#"
        SELECT temporal_workflow_id, temporal_run_id
        FROM workflow_executions
        WHERE id = $1
        "#,
    )
    .bind(execution_id)
    .fetch_optional(&pool)
    .await
    .context("Failed to query workflow execution linkage")?;

    Ok(row.map(|row| WorkflowExecutionTemporalLinkage {
        temporal_workflow_id: row.get("temporal_workflow_id"),
        temporal_run_id: row.get("temporal_run_id"),
    }))
}

/// GET /v1/workflows/executions - List workflow executions (paginated, newest first)
pub(crate) async fn list_workflow_executions_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20);
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let workflow_id = params
        .get("workflow_id")
        .and_then(|value| Uuid::parse_str(value).ok())
        .map(aegis_orchestrator_core::domain::workflow::WorkflowId);

    let repo_result = if let Some(workflow_id) = workflow_id {
        state
            .workflow_execution_repo
            .find_by_workflow_for_tenant(&tenant_id, workflow_id, limit, offset)
            .await
    } else {
        state
            .workflow_execution_repo
            .list_paginated_for_tenant(&tenant_id, limit, offset)
            .await
    };

    match repo_result {
        Ok(executions) => {
            let workflow_ids: Vec<_> = executions
                .iter()
                .map(|execution| execution.workflow_id)
                .collect();
            let workflow_name_map =
                workflow_name_map_for_ids(state.workflow_repo.clone(), &tenant_id, &workflow_ids)
                    .await;
            let list: Vec<serde_json::Value> = executions
                .iter()
                .map(|e| {
                    let workflow_name = workflow_name_map.get(&e.workflow_id.0).cloned();
                    serde_json::json!({
                        "execution_id": e.id.0,
                        "workflow_id": e.workflow_id.0,
                        "workflow_name": workflow_name,
                        "status": format!("{:?}", e.status).to_lowercase(),
                        "current_state": e.current_state.as_str(),
                        "started_at": e.started_at,
                        "last_transition_at": e.last_transition_at,
                    })
                })
                .collect();
            (StatusCode::OK, Json(list)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// GET /v1/workflows/executions/:execution_id - Get execution details
pub(crate) async fn get_workflow_execution_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let execution = match state
        .workflow_execution_repo
        .find_by_id_for_tenant(&tenant_id, ExecutionId(execution_id))
        .await
    {
        Ok(Some(execution)) => execution,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "workflow execution not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let workflow_name = match state
        .workflow_repo
        .find_by_id_for_tenant(&tenant_id, execution.workflow_id)
        .await
    {
        Ok(Some(workflow)) => Some(workflow.metadata.name),
        _ => None,
    };

    let temporal_linkage =
        match workflow_execution_temporal_linkage(&state.config, execution_id).await {
            Ok(linkage) => linkage,
            Err(error) => {
                tracing::warn!(
                    "Failed to load workflow execution linkage for {}: {}",
                    execution_id,
                    error
                );
                None
            }
        };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "execution_id": execution.id.0,
            "workflow_id": execution.workflow_id.0,
            "workflow_name": workflow_name,
            "status": format!("{:?}", execution.status).to_lowercase(),
            "current_state": execution.current_state.as_str(),
            "started_at": execution.started_at,
            "last_transition_at": execution.last_transition_at,
            "blackboard": execution.blackboard.to_json(),
            "state_outputs": execution.state_outputs,
            "temporal_workflow_id": temporal_linkage.as_ref().map(|linkage| linkage.temporal_workflow_id.clone()),
            "temporal_run_id": temporal_linkage.as_ref().map(|linkage| linkage.temporal_run_id.clone()),
        })),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct WorkflowSignalRequest {
    response: String,
}

/// POST /v1/workflows/executions/:execution_id/signal
pub(crate) async fn signal_workflow_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<String>,
    Json(request): Json<WorkflowSignalRequest>,
) -> impl IntoResponse {
    let guard = state.temporal_client_container.read().await;
    let client = match guard.as_ref() {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "Temporal client not yet connected"
                })),
            )
                .into_response();
        }
    };
    drop(guard);

    match client
        .send_human_signal(&execution_id, request.response)
        .await
    {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "signal_sent",
                "execution_id": execution_id
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// GET /v1/workflows/executions/:execution_id/logs - List workflow log events
pub(crate) async fn get_workflow_logs_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
    Query(params): Query<WorkflowLogQuery>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let execution_id = ExecutionId(execution_id);
    let execution = match state
        .workflow_execution_repo
        .find_by_id_for_tenant(&tenant_id, execution_id)
        .await
    {
        Ok(Some(execution)) => execution,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "workflow execution not found"})),
            )
                .into_response();
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": error.to_string()})),
            )
                .into_response();
        }
    };

    let workflow_name = match state
        .workflow_repo
        .find_by_id_for_tenant(&tenant_id, execution.workflow_id)
        .await
    {
        Ok(Some(workflow)) => Some(workflow.metadata.name),
        _ => None,
    };
    let temporal_linkage = workflow_execution_temporal_linkage(&state.config, execution.id.0)
        .await
        .ok()
        .flatten();
    let limit = params.limit.unwrap_or(100);
    let offset = params.offset.unwrap_or(0);

    match state
        .workflow_execution_repo
        .find_events_by_execution(execution.id, limit, offset)
        .await
    {
        Ok(events) => {
            let transformed: Vec<WorkflowLogEventView> = events
                .into_iter()
                .map(|record| {
                    let payload = serde_json::from_value::<TemporalEventPayload>(record.payload)
                        .unwrap_or(TemporalEventPayload {
                            event_type: record.event_type.clone(),
                            execution_id: execution.id.to_string(),
                            temporal_sequence_number: record.sequence,
                            workflow_id: Some(execution.workflow_id.to_string()),
                            state_name: record.state_name.clone(),
                            output: None,
                            error: None,
                            iteration_number: record.iteration_number,
                            final_blackboard: None,
                            artifacts: None,
                            agent_id: None,
                            code_diff: None,
                            parent_execution_id: None,
                            child_execution_id: None,
                            child_workflow_id: None,
                            mode: None,
                            result_key: None,
                            parent_state_name: None,
                            timestamp: record.recorded_at.to_rfc3339(),
                        });
                    workflow_log_event_from_payload(
                        execution.id,
                        execution.workflow_id.0,
                        workflow_name.clone(),
                        payload,
                        temporal_linkage.as_ref(),
                    )
                })
                .collect();

            let count = transformed.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "execution_id": execution.id.0,
                    "events": transformed,
                    "count": count,
                    "limit": limit,
                    "offset": offset,
                })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

/// GET /v1/workflows/executions/:execution_id/logs/stream - Stream workflow log events
pub(crate) async fn stream_workflow_logs_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
) -> impl IntoResponse {
    let mk_sse = |stream: std::pin::Pin<
        Box<
            dyn futures::Stream<
                    Item = std::result::Result<axum::response::sse::Event, anyhow::Error>,
                > + Send,
        >,
    >| { Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()) };

    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let execution_id = ExecutionId(execution_id);
    let execution = match state
        .workflow_execution_repo
        .find_by_id_for_tenant(&tenant_id, execution_id)
        .await
    {
        Ok(Some(execution)) => execution,
        _ => {
            let stream = async_stream::stream! {
                yield Ok::<_, anyhow::Error>(Event::default().data(
                    serde_json::json!({"error": "workflow execution not found"}).to_string()
                ));
            };
            return mk_sse(Box::pin(stream));
        }
    };

    let workflow_name = match state
        .workflow_repo
        .find_by_id_for_tenant(&tenant_id, execution.workflow_id)
        .await
    {
        Ok(Some(workflow)) => Some(workflow.metadata.name),
        _ => None,
    };
    let temporal_linkage = workflow_execution_temporal_linkage(&state.config, execution.id.0)
        .await
        .ok()
        .flatten();
    let event_bus = state.event_bus.clone();
    let stream = async_stream::stream! {
        let mut receiver = event_bus.subscribe_workflow_execution(execution.id);
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let payload = serde_json::to_string(&workflow_event_view_from_domain(
                        &event,
                        workflow_name.clone(),
                        Some(execution.workflow_id.0),
                        temporal_linkage.as_ref(),
                    ))?;
                    let terminal = matches!(
                        event,
                        WorkflowEvent::WorkflowExecutionCompleted { .. }
                            | WorkflowEvent::WorkflowExecutionFailed { .. }
                            | WorkflowEvent::WorkflowExecutionCancelled { .. }
                    );
                    yield Ok::<_, anyhow::Error>(Event::default().data(payload));
                    if terminal {
                        break;
                    }
                }
                Err(aegis_orchestrator_core::infrastructure::event_bus::EventBusError::Closed) => break,
                Err(_) => continue,
            }
        }
    };

    mk_sse(Box::pin(stream))
}

pub(crate) async fn cancel_workflow_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> impl IntoResponse {
    let namespace = match temporal_namespace(&state.config) {
        Ok(namespace) => namespace,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": error.to_string()})),
            )
                .into_response();
        }
    };

    match connect_temporal_workflow_client(&state.config).await {
        Ok(mut client) => {
            let request = RequestCancelWorkflowExecutionRequest {
                namespace,
                workflow_execution: Some(TemporalWorkflowExecution {
                    workflow_id: execution_id.to_string(),
                    run_id: String::new(),
                }),
                identity: "aegis-daemon".to_string(),
                request_id: Uuid::new_v4().to_string(),
                first_execution_run_id: String::new(),
                reason: "Cancelled by aegis workflow cancel".to_string(),
                links: Vec::new(),
            };
            match client.request_cancel_workflow_execution(request).await {
                Ok(_) => (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({
                        "status": "cancel_requested",
                        "execution_id": execution_id
                    })),
                )
                    .into_response(),
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": error.to_string()})),
                )
                    .into_response(),
            }
        }
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": error.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn remove_workflow_execution_handler(
    State(state): State<Arc<AppState>>,
    Path(execution_id): Path<Uuid>,
) -> impl IntoResponse {
    let namespace = match temporal_namespace(&state.config) {
        Ok(namespace) => namespace,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": error.to_string()})),
            )
                .into_response();
        }
    };

    let temporal_result = match connect_temporal_workflow_client(&state.config).await {
        Ok(mut client) => {
            let request = DeleteWorkflowExecutionRequest {
                namespace,
                workflow_execution: Some(TemporalWorkflowExecution {
                    workflow_id: execution_id.to_string(),
                    run_id: String::new(),
                }),
            };
            client.delete_workflow_execution(request).await.map(|_| ())
        }
        Err(error) => Err(tonic::Status::unavailable(error.to_string())),
    };

    if let Err(error) = temporal_result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error.to_string()})),
        )
            .into_response();
    }

    let db_cleanup = if let Some(database) = &state.config.spec.database {
        let database_url = match resolve_env_value(&database.url) {
            Ok(url) => url,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": error.to_string()})),
                )
                    .into_response();
            }
        };
        match sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
        {
            Ok(pool) => sqlx::query("DELETE FROM workflow_executions WHERE id = $1")
                .bind(execution_id)
                .execute(&pool)
                .await
                .map(|_| ()),
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };

    match db_cleanup {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "removed",
                "execution_id": execution_id
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Workflow execution deleted in Temporal but local cleanup failed: {error}")
            })),
        )
            .into_response(),
    }
}
