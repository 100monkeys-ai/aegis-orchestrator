// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agent lifecycle handlers: deploy, execute, list, get, delete, lookup, stream.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::StreamExt;
use uuid::Uuid;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::scope_requester::ScopeChangeRequester;
use aegis_orchestrator_core::domain::agent::{AgentId, AgentScope};
use aegis_orchestrator_core::domain::execution::ExecutionInput;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::{
    tenant_id_from_identity, tenant_id_from_request, TENANT_DELEGATION_HEADER,
};
use crate::daemon::state::AppState;

#[derive(serde::Deserialize, Default)]
pub(crate) struct DeployAgentQuery {
    /// Set to `true` to overwrite an existing agent that has the same name and version.
    #[serde(default)]
    force: bool,
    /// Optional scope override (user, tenant, global). Default: tenant.
    scope: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct ExecuteRequest {
    input: serde_json::Value,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    context_overrides: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct ExecuteAgentQuery {
    version: Option<String>,
}

pub(crate) async fn deploy_agent_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<DeployAgentQuery>,
    Json(manifest): Json<aegis_orchestrator_sdk::AgentManifest>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:deploy")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);

    let agent_scope = match query.scope.as_deref() {
        Some("global") => AgentScope::Global,
        _ => AgentScope::Tenant,
    };

    let effective_tenant_id = if matches!(agent_scope, AgentScope::Global) {
        TenantId::system()
    } else {
        tenant_id
    };

    match state
        .agent_service
        .deploy_agent_for_tenant(
            &effective_tenant_id,
            manifest,
            query.force,
            agent_scope,
            identity.as_ref().map(|e| &e.0),
        )
        .await
    {
        Ok(id) => Ok((StatusCode::OK, Json(serde_json::json!({"agent_id": id.0})))),
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

pub(crate) async fn execute_agent_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
    Query(query): Query<ExecuteAgentQuery>,
    Json(request): Json<ExecuteRequest>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:execute")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);

    // If a version query parameter is provided, verify the agent's manifest version matches
    if let Some(ref requested_version) = query.version {
        match state
            .agent_service
            .get_agent_for_tenant(&tenant_id, AgentId(agent_id))
            .await
        {
            Ok(agent) => {
                if agent.manifest.metadata.version != *requested_version {
                    return Ok((
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": format!(
                                "Version mismatch: requested '{}' but agent has '{}'",
                                requested_version, agent.manifest.metadata.version
                            )
                        })),
                    ));
                }
            }
            Err(e) => {
                return Ok((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": e.to_string()})),
                ));
            }
        }
    }

    let input = ExecutionInput {
        intent: request.intent,
        input: serde_json::json!({
            "input": request.input,
            "context_overrides": request.context_overrides,
            "tenant_id": tenant_id.to_string(),
        }),
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: None,
        attachments: Vec::new(),
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
        Ok(id) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({"execution_id": id.0})),
        )),
        Err(e) => {
            let error_str = e.to_string();
            let status = if error_str.contains("InvalidExecutionInput") {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Ok((status, Json(serde_json::json!({"error": error_str}))))
        }
    }
}

pub(crate) async fn stream_agent_events_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(agent_id): Path<Uuid>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    if let Err(e) = scope_guard.require("agent:logs") {
        return e.into_response();
    }
    let follow = params.get("follow").map(|v| v != "false").unwrap_or(false);
    let verbose = params.get("verbose").map(|v| v == "true").unwrap_or(false);
    let aid = aegis_orchestrator_core::domain::agent::AgentId(agent_id);
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let activity_service = state.correlated_activity_stream_service.clone();

    let stream = async_stream::stream! {
        if follow {
            let mut activity_stream = activity_service.stream_agent_activity(aid, &tenant_id, verbose).await?;
            while let Some(activity) = activity_stream.next().await {
                let payload = serde_json::to_string(&activity?)?;
                yield Ok::<_, anyhow::Error>(Event::default().data(payload));
            }
        } else {
            for activity in activity_service.agent_history(aid, &tenant_id, verbose).await? {
                let payload = serde_json::to_string(&activity)?;
                yield Ok::<_, anyhow::Error>(Event::default().data(payload));
            }
        }
    };

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct ListAgentsQuery {
    scope: Option<String>,
    #[serde(default)]
    visible: Option<bool>,
}

pub(crate) async fn list_agents_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<ListAgentsQuery>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:list")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);

    let list_result = if query.scope.as_deref() == Some("global") {
        state
            .agent_service
            .list_agents_for_tenant(&TenantId::system())
            .await
    } else if query.visible.unwrap_or(true) {
        state
            .agent_service
            .list_agents_visible_for_tenant(&tenant_id)
            .await
    } else {
        state.agent_service.list_agents_for_tenant(&tenant_id).await
    };

    match list_result {
        Ok(agents) => {
            let counts: Vec<i64> = {
                let futs: Vec<_> = agents
                    .iter()
                    .map(|agent| {
                        let repo = state.execution_repo.clone();
                        let tid = agent.tenant_id.clone();
                        let aid = agent.id;
                        async move { repo.count_by_agent_for_tenant(&tid, aid).await.unwrap_or(0) }
                    })
                    .collect();
                futures::future::join_all(futs).await
            };
            let json_agents: Vec<serde_json::Value> = agents
                .iter()
                .enumerate()
                .map(|(idx, agent)| {
                    let mut entry = serde_json::json!({
                        "id": agent.id.0,
                        "name": agent.manifest.metadata.name,
                        "version": agent.manifest.metadata.version,
                        "description": agent.manifest.metadata.description.clone().unwrap_or_default(),
                        "status": format!("{:?}", agent.status).to_lowercase(),
                        "labels": agent.manifest.metadata.labels,
                        "scope": agent.scope.to_string(),
                        "created_at": agent.created_at.to_rfc3339(),
                        "updated_at": agent.updated_at.to_rfc3339(),
                        "tenant_id": agent.tenant_id.as_str(),
                        "execution_count": counts[idx],
                    });
                    if let Some(schema) = &agent.manifest.spec.input_schema {
                        entry["input_schema"] = serde_json::json!(schema);
                    }
                    entry
                })
                .collect();
            Ok(Json(serde_json::json!(json_agents)))
        }
        Err(e) => Ok(Json(serde_json::json!({"error": e.to_string()}))),
    }
}

pub(crate) async fn delete_agent_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:delete")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);

    // Check scope authorization before deleting
    let aid = AgentId(agent_id);
    match state
        .agent_service
        .get_agent_for_tenant(&tenant_id, aid)
        .await
    {
        Ok(agent) => {
            let roles = build_roles(&identity);
            let authorized = match &agent.scope {
                AgentScope::Global => roles
                    .iter()
                    .any(|r| r == "aegis:operator" || r == "aegis:admin"),
                AgentScope::Tenant => true,
            };

            if !authorized {
                return Ok((
                    StatusCode::FORBIDDEN,
                    Json(
                        serde_json::json!({"error": "Unauthorized: insufficient permissions to delete this agent"}),
                    ),
                ));
            }

            match state
                .agent_service
                .delete_agent_for_tenant(&tenant_id, aid)
                .await
            {
                Ok(_) => Ok((StatusCode::OK, Json(serde_json::json!({"success": true})))),
                Err(e) => Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )),
            }
        }
        Err(_) => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )),
    }
}

fn build_roles(identity: &Option<Extension<UserIdentity>>) -> Vec<String> {
    identity
        .as_ref()
        .map(|ext| match &ext.0.identity_kind {
            IdentityKind::Operator { aegis_role } => vec![aegis_role.as_claim_str().to_string()],
            IdentityKind::ServiceAccount { .. } => vec!["aegis:operator".to_string()],
            IdentityKind::TenantUser { .. } => vec!["tenant:admin".to_string()],
            _ => vec!["user".to_string()],
        })
        .unwrap_or_else(|| vec!["aegis:operator".to_string()]) // local daemon defaults to operator
}

pub(crate) async fn get_agent_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:read")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);
    match state
        .agent_service
        .get_agent_visible(&tenant_id, AgentId(id))
        .await
    {
        Ok(agent) => {
            let manifest_yaml = serde_yaml::to_string(&agent.manifest).unwrap_or_default();
            let mut response = serde_json::json!({
                "id": agent.id.0,
                "name": agent.manifest.metadata.name,
                "version": agent.manifest.metadata.version,
                "description": agent.manifest.metadata.description.clone().unwrap_or_default(),
                "status": format!("{:?}", agent.status).to_lowercase(),
                "labels": agent.manifest.metadata.labels,
                "scope": agent.scope.to_string(),
                "created_at": agent.created_at.to_rfc3339(),
                "updated_at": agent.updated_at.to_rfc3339(),
                "tenant_id": agent.tenant_id.as_str(),
                "manifest": serde_json::to_value(&agent.manifest).unwrap_or_default(),
                "manifest_yaml": manifest_yaml,
            });
            if let Some(schema) = &agent.manifest.spec.input_schema {
                response["input_schema"] = serde_json::json!(schema);
            }
            Ok(Json(response))
        }
        Err(e) => Ok(Json(serde_json::json!({"error": e.to_string()}))),
    }
}

pub(crate) async fn list_agent_versions_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:read")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);
    match state
        .agent_service
        .list_versions_for_tenant(&tenant_id, AgentId(agent_id))
        .await
    {
        Ok(versions) => Ok(Json(serde_json::to_value(versions).unwrap_or_default())),
        Err(e) => Ok(Json(serde_json::json!({"error": e.to_string()}))),
    }
}

pub(crate) async fn lookup_agent_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:read")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);
    match state
        .agent_service
        .lookup_agent_visible_for_tenant(&tenant_id, &name)
        .await
    {
        Ok(Some(id)) => Ok((StatusCode::OK, Json(serde_json::json!({"id": id.0})))),
        Ok(None) => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )),
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// PATCH /v1/agents/:id - Update agent manifest with scope authorization check
pub(crate) async fn update_agent_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
    Json(manifest): Json<aegis_orchestrator_sdk::AgentManifest>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:deploy")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);
    let aid = AgentId(agent_id);

    // Load agent to check scope authorization
    match state
        .agent_service
        .get_agent_for_tenant(&tenant_id, aid)
        .await
    {
        Ok(agent) => {
            let roles = build_roles(&identity);
            let authorized = match &agent.scope {
                AgentScope::Global => roles
                    .iter()
                    .any(|r| r == "aegis:operator" || r == "aegis:admin"),
                AgentScope::Tenant => true,
            };

            if !authorized {
                return Ok((
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "Unauthorized: insufficient permissions to update this agent"})),
                )
                    .into_response());
            }

            match state
                .agent_service
                .update_agent_for_tenant(&tenant_id, aid, manifest)
                .await
            {
                Ok(_) => Ok(
                    (StatusCode::OK, Json(serde_json::json!({"success": true}))).into_response()
                ),
                Err(e) => Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response()),
            }
        }
        Err(_) => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Agent not found"})),
        )
            .into_response()),
    }
}

/// POST /v1/agents/:id/scope - Change agent scope (promote/demote)
pub(crate) async fn update_agent_scope_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    headers: HeaderMap,
    Path(agent_id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("agent:deploy")?;
    use aegis_orchestrator_core::application::agent_scope::AgentScopeChangeError;

    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    let tenant_id = tenant_id_from_request(identity.as_ref().map(|e| &e.0), delegation);
    let user_id = identity
        .as_ref()
        .map(|ext| ext.0.sub.clone())
        .unwrap_or_default();

    let roles = build_roles(&identity);

    // Parse target_scope from body
    let target_scope_str = match body.get("target_scope").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing 'target_scope' field"})),
            )
                .into_response());
        }
    };

    let target_scope = match target_scope_str.as_str() {
        "global" => AgentScope::Global,
        "tenant" => AgentScope::Tenant,
        other => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid scope: '{other}'. Valid values: global, tenant")})),
            )
                .into_response());
        }
    };

    let aid = AgentId(agent_id);
    let requester = ScopeChangeRequester {
        user_id,
        roles,
        tenant_id,
    };

    match state
        .agent_scope_service
        .change_scope(aid, target_scope.clone(), &requester)
        .await
    {
        Ok(()) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "new_scope": target_scope.to_string(),
            })),
        )
            .into_response()),
        Err(AgentScopeChangeError::NotFound) => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "agent not found"})),
        )
            .into_response()),
        Err(AgentScopeChangeError::Unauthorized { reason }) => Ok((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": reason})),
        )
            .into_response()),
        Err(AgentScopeChangeError::InvalidTransition { from, to }) => Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("invalid transition from '{from}' to '{to}': must traverse through Tenant")})),
        )
            .into_response()),
        Err(AgentScopeChangeError::NameCollision { name, .. }) => Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("name collision: agent '{name}' already exists at target scope")})),
        )
            .into_response()),
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}
