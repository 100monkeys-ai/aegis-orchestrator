// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Workflow CRUD handlers: register, execute, list, get, delete, scope, run (legacy).

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use uuid::Uuid;

use aegis_orchestrator_core::application::register_workflow::RegisterWorkflowUseCase;
use aegis_orchestrator_core::application::start_workflow_execution::{
    StartWorkflowExecutionRequest, StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::tenant::TenantId;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

#[derive(serde::Deserialize, Default)]
pub(crate) struct RegisterWorkflowQuery {
    #[serde(default)]
    force: bool,
    /// Optional workflow scope override (user, tenant, global). Default: tenant.
    scope: Option<String>,
}

/// POST /v1/workflows/temporal/register - Register a workflow with Temporal
pub(crate) async fn register_temporal_workflow_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(query): axum::extract::Query<RegisterWorkflowQuery>,
    body: String,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .register_workflow_use_case
        .register_workflow_for_tenant(&tenant_id, &body, query.force)
        .await
    {
        Ok(res) => {
            // If a scope was requested, update the workflow scope after registration
            if let Some(scope_str) = &query.scope {
                use aegis_orchestrator_core::domain::workflow::WorkflowScope;
                let user_id = identity
                    .as_ref()
                    .map(|ext| ext.0.sub.clone())
                    .unwrap_or_default();
                let target_scope = match scope_str.as_str() {
                    "global" => WorkflowScope::Global,
                    _ => WorkflowScope::Tenant, // default / "tenant"
                };
                if let Ok(Some(workflow)) = state
                    .workflow_repo
                    .find_by_name_for_tenant(&tenant_id, &res.name)
                    .await
                {
                    let new_tenant_id = match &target_scope {
                        WorkflowScope::Global => TenantId::system(),
                        _ => tenant_id.clone(),
                    };
                    if let Err(e) = state
                        .workflow_repo
                        .update_scope(workflow.id, target_scope, &new_tenant_id)
                        .await
                    {
                        tracing::warn!("Failed to set workflow scope after registration: {e}");
                    }
                }
            }
            (StatusCode::OK, Json(res)).into_response()
        }
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to register workflow: {}", e)
            })),
        )
            .into_response(),
    }
}

/// POST /v1/workflows/temporal/execute - Start a workflow execution
pub(crate) async fn execute_temporal_workflow_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(mut request): Json<StartWorkflowExecutionRequest>,
) -> impl IntoResponse {
    let tenant_id = request
        .tenant_id
        .get_or_insert_with(|| {
            tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0))
        })
        .clone();
    match state
        .start_workflow_execution_use_case
        .start_execution_for_tenant(&tenant_id, request, identity.as_ref().map(|ext| &ext.0))
        .await
    {
        Ok(res) => (StatusCode::OK, Json(res)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to start workflow execution: {}", e)
            })),
        )
            .into_response(),
    }
}

/// POST /v1/workflows/:name/run - Execute a workflow (Legacy endpoint for CLI)
#[derive(serde::Deserialize)]
pub(crate) struct RunWorkflowLegacyRequest {
    input: serde_json::Value,
    #[serde(default)]
    blackboard: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct RunWorkflowQuery {
    version: Option<String>,
}

pub(crate) async fn run_workflow_legacy_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
    Query(query): Query<RunWorkflowQuery>,
    Json(request): Json<RunWorkflowLegacyRequest>,
) -> impl IntoResponse {
    let req = StartWorkflowExecutionRequest {
        workflow_id: name,
        input: request.input,
        blackboard: request.blackboard,
        version: query.version,
        tenant_id: Some(tenant_id_from_identity(
            identity.as_ref().map(|identity| &identity.0),
        )),
        security_context_name: identity
            .as_ref()
            .map(|ext| ext.0.to_security_context_name()),
        intent: None,
    };
    execute_temporal_workflow_handler(State(state), identity, Json(req)).await
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct ListWorkflowsQuery {
    scope: Option<String>,
    #[serde(default)]
    visible: Option<bool>,
}

/// GET /v1/workflows - List all workflows
pub(crate) async fn list_workflows_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(query): axum::extract::Query<ListWorkflowsQuery>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    let workflows = if query.scope.as_deref() == Some("global") {
        state.workflow_repo.list_global().await.unwrap_or_default()
    } else if query.visible.unwrap_or(false) {
        state
            .workflow_repo
            .list_visible(&tenant_id)
            .await
            .unwrap_or_default()
    } else {
        state
            .workflow_repo
            .list_all_for_tenant(&tenant_id)
            .await
            .unwrap_or_default()
    };

    let counts: Vec<i64> = {
        let futs: Vec<_> = workflows
            .iter()
            .map(|w| {
                let repo = state.workflow_execution_repo.clone();
                let tid = w.tenant_id.clone();
                let wid = w.id;
                async move {
                    repo.count_by_workflow_for_tenant(&tid, wid)
                        .await
                        .unwrap_or(0)
                }
            })
            .collect();
        futures::future::join_all(futs).await
    };
    let workflow_list: Vec<serde_json::Value> = workflows
        .iter()
        .enumerate()
        .map(|(idx, w)| {
            serde_json::json!({
                "id": w.id.0,
                "name": w.metadata.name,
                "version": w.metadata.version,
                "description": w.metadata.description,
                "scope": w.scope.to_string(),
                "status": "active",
                "tags": w.metadata.tags,
                "labels": w.metadata.labels,
                "created_at": w.created_at.to_rfc3339(),
                "updated_at": w.updated_at.map(|t| t.to_rfc3339()),
                "tenant_id": w.tenant_id.as_str(),
                "input_schema": w.metadata.input_schema,
                "execution_count": counts[idx],
            })
        })
        .collect();

    (StatusCode::OK, Json(workflow_list))
}

/// GET /v1/workflows/:name - Get workflow YAML
pub(crate) async fn get_workflow_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    match state
        .workflow_repo
        .find_by_name_visible(&tenant_id, &name)
        .await
    {
        Ok(Some(workflow)) => match WorkflowParser::to_yaml(&workflow) {
            Ok(yaml) => (StatusCode::OK, yaml),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to serialize workflow: {e}"),
            ),
        },
        _ => (
            StatusCode::NOT_FOUND,
            format!("Workflow '{name}' not found"),
        ),
    }
}

/// DELETE /v1/workflows/:name - Delete workflow
pub(crate) async fn delete_workflow_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .workflow_repo
        .find_by_name_for_tenant(&tenant_id, &name)
        .await
    {
        Ok(Some(workflow)) => {
            let workflow_id = workflow.id;
            let workflow_name = workflow.metadata.name.clone();
            if let Err(e) = state
                .workflow_repo
                .delete_for_tenant(&tenant_id, workflow_id)
                .await
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                );
            }
            state.event_bus.publish_workflow_event(
                aegis_orchestrator_core::domain::events::WorkflowEvent::WorkflowRemoved {
                    workflow_id,
                    workflow_name,
                    removed_at: Utc::now(),
                },
            );
            (StatusCode::OK, Json(serde_json::json!({"success": true})))
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        ),
    }
}

/// GET /v1/workflows/:name/versions - List all versions of a workflow
pub(crate) async fn list_workflow_versions_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    match state
        .workflow_repo
        .list_by_name_for_tenant(&tenant_id, &name)
        .await
    {
        Ok(workflows) => {
            let versions: Vec<serde_json::Value> = workflows
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "id": w.id.0,
                        "name": w.metadata.name,
                        "version": w.metadata.version,
                        "description": w.metadata.description,
                        "scope": w.scope.to_string(),
                        "created_at": w.created_at.to_rfc3339(),
                        "tenant_id": w.tenant_id.as_str(),
                    })
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!(versions)))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// POST /v1/workflows/:id_or_name/scope - Change workflow scope (promote/demote)
pub(crate) async fn update_workflow_scope_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(id_or_name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    use aegis_orchestrator_core::application::workflow_scope::ScopeChangeRequester;
    use aegis_orchestrator_core::domain::workflow::WorkflowScope;

    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    let user_id = identity
        .as_ref()
        .map(|ext| ext.0.sub.clone())
        .unwrap_or_default();

    // Derive roles from identity kind
    let roles: Vec<String> = identity
        .as_ref()
        .map(|ext| match &ext.0.identity_kind {
            IdentityKind::Operator { aegis_role } => vec![aegis_role.as_claim_str().to_string()],
            IdentityKind::ServiceAccount { .. } => vec!["aegis:operator".to_string()],
            IdentityKind::TenantUser { .. } => vec!["tenant:admin".to_string()],
            _ => vec!["user".to_string()],
        })
        .unwrap_or_else(|| vec!["aegis:operator".to_string()]); // local daemon defaults to operator

    // Parse target_scope from body
    let target_scope_str = match body.get("target_scope").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing 'target_scope' field"})),
            )
                .into_response();
        }
    };

    let target_scope = match target_scope_str.as_str() {
        "global" => WorkflowScope::Global,
        "tenant" => WorkflowScope::Tenant,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid scope: '{other}'. Valid values: global, tenant")})),
            )
                .into_response();
        }
    };

    // Resolve workflow by UUID or name
    let workflow = if let Ok(uuid) = id_or_name.parse::<Uuid>() {
        let wf_id = aegis_orchestrator_core::domain::workflow::WorkflowId(uuid);
        state
            .workflow_repo
            .find_by_id_for_tenant(&tenant_id, wf_id)
            .await
    } else {
        state
            .workflow_repo
            .find_by_name_for_tenant(&tenant_id, &id_or_name)
            .await
    };

    let workflow = match workflow {
        Ok(Some(w)) => w,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("workflow '{}' not found", id_or_name)})),
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

    let previous_scope = workflow.scope.to_string();
    let workflow_id = workflow.id;

    let requester = ScopeChangeRequester {
        user_id,
        roles,
        tenant_id,
    };

    match state
        .workflow_scope_service
        .change_scope(workflow_id, target_scope.clone(), &requester)
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "workflow_id": workflow_id.0,
                "previous_scope": previous_scope,
                "new_scope": target_scope.to_string(),
            })),
        )
            .into_response(),
        Err(
            aegis_orchestrator_core::application::workflow_scope::ScopeChangeError::NameCollision {
                existing_id,
                name,
                version,
            },
        ) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("Name collision: workflow '{name}' v{version} already exists at target scope"),
                "existing_id": existing_id.0,
            })),
        )
            .into_response(),
        Err(
            aegis_orchestrator_core::application::workflow_scope::ScopeChangeError::Unauthorized {
                reason,
            },
        ) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": reason})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
