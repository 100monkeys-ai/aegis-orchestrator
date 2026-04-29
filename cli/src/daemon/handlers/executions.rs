// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Execution handlers: get, cancel, list, delete, stream events, file retrieval.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use futures::StreamExt;
use uuid::Uuid;

use aegis_orchestrator_core::application::file_operations_service::FileOperationsError;
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

pub(crate) use crate::daemon::handlers::DEFAULT_MAX_EXECUTION_LIST_LIMIT;

#[derive(serde::Deserialize)]
pub(crate) struct ListExecutionsQuery {
    pub(crate) agent_id: Option<Uuid>,
    pub(crate) workflow_name: Option<String>,
    pub(crate) limit: Option<usize>,
}

pub(crate) async fn get_execution_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("execution:read")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .execution_service
        .get_execution_for_tenant(&tenant_id, ExecutionId(execution_id))
        .await
    {
        Ok(exec) => Ok(axum::Json(serde_json::json!({
            "id": exec.id.0,
            "agent_id": exec.agent_id.0,
            "status": format!("{:?}", exec.status),
            // "started_at": exec.started_at,
            // "ended_at": exec.ended_at
        }))),
        Err(e) => Ok(axum::Json(serde_json::json!({"error": e.to_string()}))),
    }
}

pub(crate) async fn cancel_execution_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("execution:cancel")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .execution_service
        .cancel_execution_for_tenant(&tenant_id, ExecutionId(execution_id))
        .await
    {
        Ok(_) => Ok(axum::Json(serde_json::json!({"success": true}))),
        Err(e) => Ok(axum::Json(serde_json::json!({"error": e.to_string()}))),
    }
}

pub(crate) async fn stream_events_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    if let Err(e) = scope_guard.require("execution:stream") {
        return e.into_response();
    }
    let follow = params.get("follow").map(|v| v != "false").unwrap_or(true);
    let verbose = params.get("verbose").map(|v| v == "true").unwrap_or(false);
    let exec_id = aegis_orchestrator_core::domain::execution::ExecutionId(execution_id);
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let activity_service = state.correlated_activity_stream_service.clone();

    let stream = async_stream::stream! {
        if follow {
            let mut activity_stream = activity_service.stream_execution_activity(&tenant_id, exec_id, verbose).await?;
            while let Some(activity) = activity_stream.next().await {
                let payload = serde_json::to_string(&activity?)?;
                yield Ok::<_, anyhow::Error>(Event::default().data(payload));
            }
        } else {
            for activity in activity_service.execution_history(&tenant_id, exec_id, verbose).await? {
                let payload = serde_json::to_string(&activity)?;
                yield Ok::<_, anyhow::Error>(Event::default().data(payload));
            }
        }
    };

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

pub(crate) async fn delete_execution_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(execution_id): Path<Uuid>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("execution:remove")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .execution_service
        .delete_execution_for_tenant(&tenant_id, ExecutionId(execution_id))
        .await
    {
        Ok(_) => Ok(axum::Json(serde_json::json!({"success": true}))),
        Err(e) => Ok(axum::Json(serde_json::json!({"error": e.to_string()}))),
    }
}

/// Maximum number of executions that can be returned by a single
/// `list_executions` request. This upper bound protects the daemon from
/// excessive memory usage and response sizes when clients request very
/// large pages. The effective limit is configurable via NodeConfig to
/// allow tuning based on deployment capacity and client requirements. If
/// not explicitly configured, a safe default of 1000 is used.
pub(crate) async fn list_executions_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(query): axum::extract::Query<ListExecutionsQuery>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("execution:list")?;
    let agent_id = query.agent_id.map(AgentId);

    // Determine the maximum allowed page size from configuration, with a
    // backward-compatible default of 1000 if not set.
    let max_limit = state
        .config
        .spec
        .max_execution_list_limit
        .unwrap_or(DEFAULT_MAX_EXECUTION_LIST_LIMIT);

    let limit = query.limit.unwrap_or(20).min(max_limit);
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    // Resolve workflow_name to a WorkflowId if provided
    let workflow_id = if let Some(ref wf_name) = query.workflow_name {
        match state
            .workflow_repo
            .find_by_name_visible(&tenant_id, wf_name)
            .await
        {
            Ok(Some(wf)) => Some(wf.id),
            Ok(None) => {
                return Ok(axum::Json(
                    serde_json::json!({"error": format!("Workflow '{}' not found", wf_name)}),
                ));
            }
            Err(e) => {
                return Ok(axum::Json(serde_json::json!({"error": e.to_string()})));
            }
        }
    } else {
        None
    };

    match state
        .execution_service
        .list_executions_for_tenant(&tenant_id, agent_id, workflow_id, limit)
        .await
    {
        Ok(executions) => {
            let json_executions: Vec<serde_json::Value> = executions
                .into_iter()
                .map(|exec| {
                    serde_json::json!({
                        "id": exec.id.0,
                        "agent_id": exec.agent_id.0,
                        "status": format!("{:?}", exec.status),
                        "started_at": exec.started_at,
                        "ended_at": exec.ended_at
                    })
                })
                .collect();
            Ok(axum::Json(serde_json::json!(json_executions)))
        }
        Err(e) => Ok(axum::Json(serde_json::json!({"error": e.to_string()}))),
    }
}

/// GET /v1/executions/:execution_id/files/*path
///
/// Read a single file from a completed execution's workspace volume post-mortem.
/// The path segment is normalized: a `/workspace/` prefix is stripped if present.
pub(crate) async fn get_execution_file_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path((execution_id, file_path)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, (StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("execution:read")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    // Normalize: strip /workspace/ prefix if present
    let normalized = file_path
        .strip_prefix("workspace/")
        .or_else(|| file_path.strip_prefix("/workspace/"))
        .unwrap_or(&file_path);

    state
        .file_operations_service
        .read_file_for_execution(ExecutionId(execution_id), &tenant_id, normalized)
        .await
        .map(|content| {
            (
                [(axum::http::header::CONTENT_TYPE, content.content_type)],
                content.data,
            )
                .into_response()
        })
        .map_err(|e| {
            let (status, message) = match &e {
                FileOperationsError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
                FileOperationsError::Unauthorized => (StatusCode::FORBIDDEN, e.to_string()),
                FileOperationsError::InvalidPath(_) => {
                    (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            };
            (status, axum::Json(serde_json::json!({"error": message})))
        })
}
