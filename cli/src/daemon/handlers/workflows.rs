// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Workflow CRUD handlers: register, execute, list, get, delete, scope, run (legacy).

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use tracing::warn;
use uuid::Uuid;

use aegis_orchestrator_core::application::register_workflow::RegisterWorkflowUseCase;
use aegis_orchestrator_core::application::start_workflow_execution::{
    StartWorkflowExecutionRequest, StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

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
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(query): axum::extract::Query<RegisterWorkflowQuery>,
    body: String,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:deploy")?;
    use aegis_orchestrator_core::domain::workflow::WorkflowScope;

    // Resolve scope and tenant_id together before registration so the correct
    // values are baked into domain_json at INSERT time. The post-hoc update_scope
    // pattern is removed because it left domain_json with the wrong scope.
    let (tenant_id, scope) = match query.scope.as_deref() {
        Some("global") => (TenantId::system(), WorkflowScope::Global),
        _ => (
            tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0)),
            WorkflowScope::Tenant,
        ),
    };

    match state
        .register_workflow_use_case
        .register_workflow_for_tenant(&tenant_id, &body, query.force, scope)
        .await
    {
        Ok(res) => Ok((StatusCode::OK, Json(res)).into_response()),
        Err(e) => Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Failed to register workflow: {}", e)
            })),
        )
            .into_response()),
    }
}

/// POST /v1/workflows/temporal/execute - Start a workflow execution
pub(crate) async fn execute_temporal_workflow_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Json(mut request): Json<StartWorkflowExecutionRequest>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:run")?;
    // Audit 002 §4.8: NEVER trust body-supplied `tenant_id`. The authenticated
    // JWT identity is the only source of truth for the executing tenant.
    // A body that carries `tenant_id` for tenant-B while the JWT belongs to
    // tenant-A previously executed under tenant-B's quota and security
    // context — a cross-tenant privilege escalation. Overwrite
    // unconditionally with the JWT-derived tenant.
    let tenant_id =
        resolve_execute_tenant(identity.as_ref().map(|identity| &identity.0), &mut request);
    match state
        .start_workflow_execution_use_case
        .start_execution_for_tenant(&tenant_id, request, identity.as_ref().map(|ext| &ext.0))
        .await
    {
        Ok(res) => Ok((StatusCode::OK, Json(res)).into_response()),
        Err(e) => {
            let error_str = e.to_string();
            let status = if error_str.contains("InvalidExecutionInput") {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_REQUEST
            };
            Ok((
                status,
                Json(serde_json::json!({
                    "error": format!("Failed to start workflow execution: {}", error_str)
                })),
            )
                .into_response())
        }
    }
}

/// Resolve the executing tenant for `execute_temporal_workflow_handler`.
///
/// Audit 002 §4.8: The JWT identity's tenant is the only source of truth.
/// Any `tenant_id` carried in the request body is *unconditionally
/// overwritten* with the JWT-derived tenant. Returning early without
/// overwriting (e.g. via `get_or_insert_with`) lets a tenant-A caller
/// execute under tenant-B's quota and security context.
fn resolve_execute_tenant(
    identity: Option<&UserIdentity>,
    request: &mut StartWorkflowExecutionRequest,
) -> TenantId {
    let tenant_id = tenant_id_from_identity(identity);
    request.tenant_id = Some(tenant_id.clone());
    tenant_id
}

/// POST /v1/workflows/:name/run - Execute a workflow (Legacy endpoint for CLI)
#[derive(serde::Deserialize)]
pub(crate) struct RunWorkflowLegacyRequest {
    input: serde_json::Value,
    #[serde(default)]
    blackboard: Option<serde_json::Value>,
    #[serde(default)]
    intent: Option<String>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct RunWorkflowQuery {
    version: Option<String>,
}

pub(crate) async fn run_workflow_legacy_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
    Query(query): Query<RunWorkflowQuery>,
    Json(request): Json<RunWorkflowLegacyRequest>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:run")?;
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
        intent: request.intent,
    };
    // Re-use execute handler but bypass the scope check (already checked above)
    let bypass_guard = ScopeGuard(vec!["workflow:run".to_string()]);
    execute_temporal_workflow_handler(State(state), bypass_guard, identity, Json(req)).await
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
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    axum::extract::Query(query): axum::extract::Query<ListWorkflowsQuery>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:list")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    let workflows = if query.scope.as_deref() == Some("global") {
        match state.workflow_repo.list_global().await {
            Ok(workflows) => workflows,
            Err(err) => {
                warn!("Failed to list global workflows: {}", err);
                Vec::new()
            }
        }
    } else if query.visible.unwrap_or(false) {
        match state.workflow_repo.list_visible(&tenant_id).await {
            Ok(workflows) => workflows,
            Err(err) => {
                warn!(
                    "Failed to list visible workflows for tenant_id={}: {}",
                    tenant_id, err
                );
                Vec::new()
            }
        }
    } else {
        match state.workflow_repo.list_all_for_tenant(&tenant_id).await {
            Ok(workflows) => workflows,
            Err(err) => {
                warn!(
                    "Failed to list all workflows for tenant_id={}: {}",
                    tenant_id, err
                );
                Vec::new()
            }
        }
    };

    let counts: Vec<i64> = {
        let futs: Vec<_> = workflows
            .iter()
            .map(|w| {
                let repo = state.workflow_execution_repo.clone();
                let tid = w.tenant_id.clone();
                let wid = w.id;
                async move {
                    match repo.count_by_workflow_for_tenant(&tid, wid).await {
                        Ok(count) => count,
                        Err(err) => {
                            warn!(
                                workflow_id = %wid.0,
                                tenant_id = %tid,
                                error = %err,
                                "Failed to fetch workflow execution count; defaulting to 0"
                            );
                            0
                        }
                    }
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
                "labels": w.metadata.labels,
                "created_at": w.created_at.to_rfc3339(),
                "updated_at": w.updated_at.map(|t| t.to_rfc3339()),
                "tenant_id": w.tenant_id.as_str(),
                "input_schema": w.metadata.input_schema,
                "execution_count": counts[idx],
            })
        })
        .collect();

    Ok((StatusCode::OK, Json(workflow_list)))
}

/// GET /v1/workflows/:name - Get workflow details
pub(crate) async fn get_workflow_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:read")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));

    match state
        .workflow_repo
        .find_by_name_visible(&tenant_id, &name)
        .await
    {
        Ok(Some(workflow)) => {
            let manifest_yaml = match WorkflowParser::to_yaml(&workflow) {
                Ok(yaml) => yaml,
                Err(err) => {
                    warn!(
                        workflow_name = %workflow.metadata.name,
                        workflow_id = %workflow.id.0,
                        error = %err,
                        "Failed to serialize workflow to YAML in get_workflow_handler"
                    );
                    String::new()
                }
            };
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": workflow.id.0,
                    "name": workflow.metadata.name,
                    "version": workflow.metadata.version,
                    "description": workflow.metadata.description,
                    "scope": workflow.scope.to_string(),
                    "labels": workflow.metadata.labels,
                    "created_at": workflow.created_at.to_rfc3339(),
                    "updated_at": workflow.updated_at.map(|t| t.to_rfc3339()),
                    "tenant_id": workflow.tenant_id.as_str(),
                    "input_schema": workflow.metadata.input_schema,
                    "manifest_yaml": manifest_yaml,
                })),
            )
                .into_response())
        }
        _ => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("Workflow '{name}' not found")
            })),
        )
            .into_response()),
    }
}

/// DELETE /v1/workflows/:name - Delete workflow
pub(crate) async fn delete_workflow_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:delete")?;
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
                return Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                ));
            }
            state.event_bus.publish_workflow_event(
                aegis_orchestrator_core::domain::events::WorkflowEvent::WorkflowRemoved {
                    workflow_id,
                    tenant_id: tenant_id.clone(),
                    workflow_name,
                    removed_at: Utc::now(),
                },
            );
            Ok((StatusCode::OK, Json(serde_json::json!({"success": true}))))
        }
        _ => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )),
    }
}

/// GET /v1/workflows/:name/versions - List all versions of a workflow
pub(crate) async fn list_workflow_versions_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:list")?;
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
            Ok((StatusCode::OK, Json(serde_json::json!(versions))))
        }
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// POST /v1/workflows/:id_or_name/scope - Change workflow scope (promote/demote)
pub(crate) async fn update_workflow_scope_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id_or_name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("workflow:deploy")?;
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
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "missing 'target_scope' field"})),
            )
                .into_response());
        }
    };

    let target_scope = match target_scope_str.as_str() {
        "global" => WorkflowScope::Global,
        "tenant" => WorkflowScope::Tenant,
        other => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid scope: '{other}'. Valid values: global, tenant")})),
            )
                .into_response());
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
            return Ok((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("workflow '{}' not found", id_or_name)})),
            )
                .into_response());
        }
        Err(e) => {
            return Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response());
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
        Ok(()) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "workflow_id": workflow_id.0,
                "previous_scope": previous_scope,
                "new_scope": target_scope.to_string(),
            })),
        )
            .into_response()),
        Err(
            aegis_orchestrator_core::application::workflow_scope::ScopeChangeError::NameCollision {
                existing_id,
                name,
                version,
            },
        ) => Ok((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("Name collision: workflow '{name}' v{version} already exists at target scope"),
                "existing_id": existing_id.0,
            })),
        )
            .into_response()),
        Err(
            aegis_orchestrator_core::application::workflow_scope::ScopeChangeError::Unauthorized {
                reason,
            },
        ) => Ok((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": reason})),
        )
            .into_response()),
        Err(e) => Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

#[cfg(test)]
mod tests {
    //! Audit 002 §4.8 regression tests: `execute_temporal_workflow_handler`
    //! must NEVER trust body-supplied `tenant_id`. The JWT identity is the
    //! only source of truth.

    use super::*;
    use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
    use aegis_orchestrator_core::domain::tenant::TenantId;

    fn tenant_user_identity(slug: &str) -> UserIdentity {
        UserIdentity {
            sub: format!("user-{slug}"),
            realm_slug: format!("tenant-{slug}"),
            email: None,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: slug.into(),
            },
        }
    }

    fn consumer_user_identity(sub: &str) -> UserIdentity {
        UserIdentity {
            sub: sub.into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        }
    }

    fn empty_request_with_body_tenant(t: TenantId) -> StartWorkflowExecutionRequest {
        StartWorkflowExecutionRequest {
            workflow_id: "wf-x".into(),
            input: serde_json::json!({}),
            blackboard: None,
            version: None,
            tenant_id: Some(t),
            security_context_name: None,
            intent: None,
        }
    }

    /// Audit 002 §4.8: tenant-A caller submits a body claiming tenant-B.
    /// The handler MUST overwrite the body's tenant_id with tenant-A
    /// (the JWT-derived tenant) and MUST return tenant-A as the dispatch
    /// tenant. Before the fix, `get_or_insert_with` accepted the body
    /// value and tenant-A would have executed against tenant-B's quota.
    #[test]
    fn execute_handler_ignores_body_tenant_id_when_jwt_is_tenant_user() {
        let identity = tenant_user_identity("acme");
        let attacker_target = TenantId::from_realm_slug("victim").unwrap();
        let mut request = empty_request_with_body_tenant(attacker_target.clone());
        // Sanity: the body claims tenant-B.
        assert_eq!(request.tenant_id.as_ref().unwrap(), &attacker_target);

        let resolved = resolve_execute_tenant(Some(&identity), &mut request);

        // Resolved dispatch tenant is tenant-A (from JWT), not tenant-B (from body).
        assert_eq!(resolved.as_str(), "acme");
        // Body's tenant_id has been *overwritten* with tenant-A — downstream
        // `start_execution_for_tenant` MUST receive only the JWT tenant.
        assert_eq!(request.tenant_id.as_ref().unwrap().as_str(), "acme");
        assert_ne!(request.tenant_id.as_ref().unwrap(), &attacker_target);
    }

    /// Audit 002 §4.8: tenant-A consumer caller submits a body claiming
    /// another consumer's per-user tenant. The handler MUST overwrite with
    /// the JWT-derived per-user tenant (ADR-097) — never the body value.
    #[test]
    fn execute_handler_ignores_body_tenant_id_when_jwt_is_consumer() {
        let identity = consumer_user_identity("user-alice");
        let alice_tenant = TenantId::for_consumer_user("user-alice").unwrap();
        let bob_tenant = TenantId::for_consumer_user("user-bob").unwrap();
        let mut request = empty_request_with_body_tenant(bob_tenant.clone());

        let resolved = resolve_execute_tenant(Some(&identity), &mut request);

        assert_eq!(resolved, alice_tenant);
        assert_eq!(request.tenant_id.as_ref().unwrap(), &alice_tenant);
        assert_ne!(request.tenant_id.as_ref().unwrap(), &bob_tenant);
    }

    /// Audit 002 §4.8: even when the body carries no tenant_id at all, the
    /// handler must populate it from the JWT (so the inner use case never
    /// hits its `tenant_id is required` guard for an authenticated path).
    #[test]
    fn execute_handler_populates_tenant_from_jwt_when_body_is_missing() {
        let identity = tenant_user_identity("acme");
        let mut request = StartWorkflowExecutionRequest {
            workflow_id: "wf-x".into(),
            input: serde_json::json!({}),
            blackboard: None,
            version: None,
            tenant_id: None,
            security_context_name: None,
            intent: None,
        };

        let resolved = resolve_execute_tenant(Some(&identity), &mut request);

        assert_eq!(resolved.as_str(), "acme");
        assert_eq!(request.tenant_id.as_ref().unwrap().as_str(), "acme");
    }
}
