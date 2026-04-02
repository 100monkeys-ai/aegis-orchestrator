// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agent lifecycle handlers: deploy, execute, list, get, delete, lookup, stream.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::StreamExt;
use uuid::Uuid;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionInput;

use aegis_orchestrator_core::domain::iam::UserIdentity;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

#[derive(serde::Deserialize, Default)]
pub(crate) struct DeployAgentQuery {
    /// Set to `true` to overwrite an existing agent that has the same name and version.
    #[serde(default)]
    force: bool,
}

#[derive(serde::Deserialize)]
pub(crate) struct ExecuteRequest {
    input: serde_json::Value,
    #[serde(default)]
    context_overrides: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct ExecuteAgentQuery {
    version: Option<String>,
}

pub(crate) async fn deploy_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(query): axum::extract::Query<DeployAgentQuery>,
    Json(manifest): Json<aegis_orchestrator_sdk::AgentManifest>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    // SDK now re-exports core types, so no conversion needed
    match state
        .agent_service
        .deploy_agent_for_tenant(&tenant_id, manifest, query.force)
        .await
    {
        Ok(id) => (StatusCode::OK, Json(serde_json::json!({"agent_id": id.0}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

pub(crate) async fn execute_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(agent_id): Path<Uuid>,
    Query(query): Query<ExecuteAgentQuery>,
    Json(request): Json<ExecuteRequest>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    // If a version query parameter is provided, verify the agent's manifest version matches
    if let Some(ref requested_version) = query.version {
        match state
            .agent_service
            .get_agent_for_tenant(&tenant_id, AgentId(agent_id))
            .await
        {
            Ok(agent) => {
                if agent.manifest.metadata.version != *requested_version {
                    return (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": format!(
                                "Version mismatch: requested '{}' but agent has '{}'",
                                requested_version, agent.manifest.metadata.version
                            )
                        })),
                    );
                }
            }
            Err(e) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": e.to_string()})),
                );
            }
        }
    }

    let payload = serde_json::json!({
        "input": request.input,
        "context_overrides": request.context_overrides,
        "tenant_id": tenant_id.to_string(),
    });
    let input = ExecutionInput {
        intent: Some(payload.to_string()),
        payload,
    };

    // ADR-083: derive security context from authenticated identity
    let security_context_name = identity
        .as_ref()
        .map(|ext| ext.0.to_security_context_name())
        .unwrap_or_else(|| "aegis-system-operator".to_string());

    match state
        .execution_service
        .start_execution(
            AgentId(agent_id),
            input,
            security_context_name,
            identity.as_ref().map(|ext| &ext.0),
        )
        .await
    {
        Ok(id) => (
            StatusCode::OK,
            Json(serde_json::json!({"execution_id": id.0})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

pub(crate) async fn stream_agent_events_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(agent_id): Path<Uuid>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let follow = params.get("follow").map(|v| v != "false").unwrap_or(false);
    let verbose = params.get("verbose").map(|v| v == "true").unwrap_or(false);
    let aid = aegis_orchestrator_core::domain::agent::AgentId(agent_id);
    let _tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let activity_service = state.correlated_activity_stream_service.clone();

    let stream = async_stream::stream! {
        if follow {
            let mut activity_stream = activity_service.stream_agent_activity(aid, verbose).await?;
            while let Some(activity) = activity_stream.next().await {
                let payload = serde_json::to_string(&activity?)?;
                yield Ok::<_, anyhow::Error>(Event::default().data(payload));
            }
        } else {
            for activity in activity_service.agent_history(aid, verbose).await? {
                let payload = serde_json::to_string(&activity)?;
                yield Ok::<_, anyhow::Error>(Event::default().data(payload));
            }
        }
    };

    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

pub(crate) async fn list_agents_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> Json<serde_json::Value> {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state.agent_service.list_agents_for_tenant(&tenant_id).await {
        Ok(agents) => {
            let json_agents: Vec<serde_json::Value> = agents
                .into_iter()
                .map(|agent| {
                    serde_json::json!({
                        "id": agent.id.0,
                        "name": agent.manifest.metadata.name,
                        "version": agent.manifest.metadata.version,
                        "description": agent.manifest.metadata.description.clone().unwrap_or_default(),
                        "status": format!("{:?}", agent.status).to_lowercase(),
                        "tags": agent.manifest.metadata.tags,
                        "labels": agent.manifest.metadata.labels,
                        "created_at": agent.created_at.to_rfc3339(),
                        "updated_at": agent.updated_at.to_rfc3339(),
                        "tenant_id": agent.tenant_id.as_str(),
                    })
                })
                .collect();
            Json(serde_json::json!(json_agents))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

pub(crate) async fn delete_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(agent_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .agent_service
        .delete_agent_for_tenant(&tenant_id, AgentId(agent_id))
        .await
    {
        Ok(_) => Json(serde_json::json!({"success": true})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

pub(crate) async fn get_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Json<serde_json::Value> {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .agent_service
        .get_agent_for_tenant(&tenant_id, AgentId(id))
        .await
    {
        Ok(agent) => {
            let manifest_yaml = serde_yaml::to_string(&agent.manifest).unwrap_or_default();
            Json(serde_json::json!({
                "id": agent.id.0,
                "name": agent.manifest.metadata.name,
                "version": agent.manifest.metadata.version,
                "description": agent.manifest.metadata.description.clone().unwrap_or_default(),
                "status": format!("{:?}", agent.status).to_lowercase(),
                "tags": agent.manifest.metadata.tags,
                "labels": agent.manifest.metadata.labels,
                "created_at": agent.created_at.to_rfc3339(),
                "updated_at": agent.updated_at.to_rfc3339(),
                "tenant_id": agent.tenant_id.as_str(),
                "manifest": serde_json::to_value(&agent.manifest).unwrap_or_default(),
                "manifest_yaml": manifest_yaml,
            }))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

pub(crate) async fn lookup_agent_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .agent_service
        .lookup_agent_for_tenant(&tenant_id, &name)
        .await
    {
        Ok(Some(id)) => (StatusCode::OK, Json(serde_json::json!({"id": id.0}))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}
