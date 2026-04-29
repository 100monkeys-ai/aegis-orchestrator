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
    /// Structured attachments (ADR-113). Carried into `ExecutionInput.attachments`
    /// verbatim so the dispatch path matches the SEAL JSON-RPC invoke shape.
    #[serde(default)]
    attachments: Vec<aegis_orchestrator_core::domain::execution::AttachmentRef>,
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
        attachments: request.attachments,
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
            Ok((StatusCode::OK, Json(serde_json::json!(json_agents))))
        }
        // Audit 002 §4.37.6 — surface backend failures as 500, not 200 with
        // an `{"error":...}` body. Returning 200 makes every client treat
        // database/repository errors as a successful (empty) response.
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
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
            let authorized = agent_admin_authorized(&agent.scope, &roles);

            if !authorized {
                // Audit 002 §4.37.7 — for Global-scope agents, an unauthorised
                // tenant user cannot prove the agent exists by probing the
                // delete endpoint. Returning 403 leaks the existence of the
                // Global agent (the only branch that returns 404 is "agent
                // not visible to this tenant"). Collapse the unauthorised
                // case into the same 404 the lookup path returns so an
                // attacker cannot distinguish "exists but I can't touch it"
                // from "doesn't exist".
                tracing::warn!(
                    agent_id = %agent_id,
                    scope = ?agent.scope,
                    "rejected delete on Global agent without operator/admin role; surfacing as 404"
                );
                return Ok((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
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

/// Audit 002 §4.37.7 — decide whether `roles` are sufficient to administer an
/// agent of the given `scope`. Pure function so the policy can be unit-tested
/// without spinning up the full handler. A `Tenant`-scoped agent is always
/// administrable by the caller (tenant isolation already enforced upstream by
/// `get_agent_for_tenant`); a `Global` agent requires `aegis:operator` or
/// `aegis:admin`.
fn agent_admin_authorized(scope: &AgentScope, roles: &[String]) -> bool {
    match scope {
        AgentScope::Global => roles
            .iter()
            .any(|r| r == "aegis:operator" || r == "aegis:admin"),
        AgentScope::Tenant => true,
    }
}

/// Build the list of roles asserted by the request's authenticated identity.
///
/// SECURITY (audit 4.36, BC-13): when identity is absent — i.e. a route was
/// reached without identity middleware attaching a `UserIdentity` — this
/// function MUST return an empty role set. Any caller that gates an
/// operator-tier action must therefore fail closed against the empty set.
/// Defaulting to `aegis:operator` (the prior behavior) silently grants
/// operator privileges to any misconfigured route and is a privilege-
/// escalation surface.
fn build_roles(identity: &Option<Extension<UserIdentity>>) -> Vec<String> {
    let Some(ext) = identity.as_ref() else {
        // Fail closed. The absence of an identity extension is a
        // configuration anomaly worth surfacing in logs, but it MUST NOT
        // grant any roles.
        tracing::warn!(
            "build_roles called without identity extension; returning empty role set \
             (route is missing identity middleware or token validation failed silently)"
        );
        return Vec::new();
    };
    match &ext.0.identity_kind {
        IdentityKind::Operator { aegis_role } => vec![aegis_role.as_claim_str().to_string()],
        IdentityKind::ServiceAccount { .. } => vec!["aegis:operator".to_string()],
        IdentityKind::TenantUser { .. } => vec!["tenant:admin".to_string()],
        IdentityKind::ConsumerUser { .. } => vec!["user".to_string()],
    }
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
            Ok((StatusCode::OK, Json(response)))
        }
        // Audit 002 §4.37.6 — return 404 (not-found / not-visible folds into
        // the same opaque code per the visibility-isolation contract) so the
        // client sees a real HTTP error code rather than a 200 with an error
        // body.
        Err(e) => {
            tracing::debug!(error = %e, agent_id = %id, "get_agent failed");
            Ok((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Agent not found"})),
            ))
        }
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
        Ok(versions) => Ok((
            StatusCode::OK,
            Json(serde_json::to_value(versions).unwrap_or_default()),
        )),
        // Audit 002 §4.37.6 — return 500 on backend errors instead of 200.
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
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
            let authorized = agent_admin_authorized(&agent.scope, &roles);

            if !authorized {
                // Audit 002 §4.37.7 — same info-leak fix as `delete_agent_handler`.
                // Collapse 403-on-Global to 404 so the existence of a Global
                // agent cannot be probed by an unauthorised tenant.
                tracing::warn!(
                    agent_id = %agent_id,
                    scope = ?agent.scope,
                    "rejected update on Global agent without operator/admin role; surfacing as 404"
                );
                return Ok((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "Agent not found"})),
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

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::iam::AegisRole;

    /// Audit finding 4.36 (BC-13 / BC-1) regression test.
    ///
    /// `build_roles` previously returned `vec!["aegis:operator"]` when the
    /// `Option<Extension<UserIdentity>>` was `None`. That meant any route
    /// that reached this function without identity middleware silently
    /// granted operator privileges. After the fix, missing identity MUST
    /// resolve to the empty role set, and any operator-tier check must
    /// fail closed against it.
    #[test]
    fn build_roles_without_identity_returns_empty_role_set_not_operator() {
        let roles = build_roles(&None);
        assert!(
            roles.is_empty(),
            "audit 4.36: missing identity MUST yield empty role set, got {:?}",
            roles
        );
        assert!(
            !roles.iter().any(|r| r == "aegis:operator"),
            "audit 4.36: missing identity MUST NOT grant aegis:operator (privilege escalation)"
        );
    }

    // ─── Audit 002 §4.37.7 — Global-agent existence info-leak ───────────────
    //
    // The previous code returned 403 FORBIDDEN when a tenant user without
    // operator/admin role attempted to delete or update a Global-scope
    // agent. That distinguished "agent exists but you can't touch it" from
    // "agent doesn't exist", letting an attacker enumerate Global agent ids
    // by probing for 403s. The fix collapses unauthorised Global access to
    // 404 NOT_FOUND so the response is indistinguishable from a missing
    // agent. The pure-function `agent_admin_authorized` carries the policy
    // so the regression test pins the decision matrix without spinning up
    // the full HTTP handler.

    #[test]
    fn agent_admin_authorized_global_requires_operator_or_admin() {
        // Tenant user with no operator-tier role MUST NOT be able to admin
        // a Global agent. Caller folds this `false` into a 404 response.
        assert!(!agent_admin_authorized(&AgentScope::Global, &[]));
        assert!(!agent_admin_authorized(
            &AgentScope::Global,
            &["tenant:admin".to_string()]
        ));
        assert!(!agent_admin_authorized(
            &AgentScope::Global,
            &["user".to_string()]
        ));

        // Operator and admin are both allowed.
        assert!(agent_admin_authorized(
            &AgentScope::Global,
            &["aegis:operator".to_string()]
        ));
        assert!(agent_admin_authorized(
            &AgentScope::Global,
            &["aegis:admin".to_string()]
        ));
    }

    #[test]
    fn agent_admin_authorized_tenant_always_passes_after_isolation_check() {
        // Tenant-scope agents are gated by tenant isolation in the
        // repository call; once the agent is loaded for the caller's
        // tenant, no additional role check is needed.
        assert!(agent_admin_authorized(&AgentScope::Tenant, &[]));
        assert!(agent_admin_authorized(
            &AgentScope::Tenant,
            &["user".to_string()]
        ));
    }

    /// Confirms the operator path still resolves correctly when identity IS
    /// attached — the fix must only close the empty-identity hole.
    #[test]
    fn build_roles_with_operator_identity_returns_operator_role() {
        let identity = Extension(UserIdentity {
            sub: "op-sub".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Operator,
            },
        });
        let roles = build_roles(&Some(identity));
        assert_eq!(roles, vec!["aegis:operator".to_string()]);
    }
}
