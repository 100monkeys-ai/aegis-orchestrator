// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Execution handlers: get, cancel, list, delete, stream events, file retrieval.

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use futures::StreamExt;
use uuid::Uuid;

use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::file_operations_service::{
    FileContent, FileOperationsError,
};
use aegis_orchestrator_core::application::goal_service::{cancel_ending_its_goal, GoalService};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::repository::ExecutionRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::{
    is_operator, tenant_id_from_identity, tenant_id_from_request, TENANT_DELEGATION_HEADER,
};
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
    headers: HeaderMap,
    Path(execution_id): Path<Uuid>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("execution:read")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    Ok(execution_status(
        state.execution_repo.as_ref(),
        identity.as_ref().map(|identity| &identity.0),
        delegation,
        execution_id,
    )
    .await)
}

/// The status route's answer for `execution_id` as `identity` sees it.
///
/// An operator reads any tenant's execution (ADR-097). Any other caller
/// reads only executions of the tenant its request resolves to, resolved as
/// on every delegated route (ADR-100, [`tenant_id_from_request`]): a service
/// account takes the tenant it names in `X-Tenant-Id` — the Temporal
/// worker's status poll of a user's agent execution — and every other
/// identity keeps its own tenant whatever the header says.
pub(crate) async fn execution_status(
    execution_repo: &dyn ExecutionRepository,
    identity: Option<&UserIdentity>,
    delegation: Option<&str>,
    execution_id: Uuid,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    let tenant_id = tenant_id_from_request(identity, delegation);
    let exec_result = if is_operator(identity) {
        // Operator cross-tenant fetch (ADR-097). Each Execution carries its
        // own `tenant_id` for the projection.
        execution_repo
            .find_by_id_unscoped(ExecutionId(execution_id))
            .await
    } else {
        execution_repo
            .find_by_id_for_tenant(&tenant_id, ExecutionId(execution_id))
            .await
    };
    match exec_result {
        Ok(Some(exec)) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "id": exec.id.0,
                "agent_id": exec.agent_id.0,
                "status": format!("{:?}", exec.status),
                "tenant_id": exec.tenant_id.as_str(),
            })),
        ),
        // Audit 002 §4.37.6 — collapse not-found / not-visible to 404 instead
        // of returning 200 with an error body.
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "Execution not found"})),
        ),
        Err(e) => {
            tracing::debug!(error = %e, %execution_id, "get_execution failed");
            (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({"error": "Execution not found"})),
            )
        }
    }
}

/// `POST /v1/executions/{id}/cancel`'s cancel (AEGIS ADR-131 U33a): the
/// execution is cancelled, and when it is bound to an open goal the goal is
/// closed `cancelled` before the route answers, so no round re-dispatches
/// the work the person stopped.
pub(crate) async fn cancel_execution_ending_its_goal(
    executions: &dyn ExecutionService,
    goals: Option<&GoalService>,
    tenant_id: &TenantId,
    execution_id: ExecutionId,
) -> anyhow::Result<()> {
    cancel_ending_its_goal(
        goals,
        tenant_id,
        execution_id,
        executions.cancel_execution_for_tenant(tenant_id, execution_id),
    )
    .await
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
    match cancel_execution_ending_its_goal(
        state.execution_service.as_ref(),
        state
            .tool_invocation_service
            .goal_service()
            .map(|goals| goals.as_ref()),
        &tenant_id,
        ExecutionId(execution_id),
    )
    .await
    {
        Ok(_) => Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({"success": true})),
        )),
        // Audit 002 §4.37.6 — return 500 on backend error instead of 200.
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )),
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
        Ok(_) => Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({"success": true})),
        )),
        // Audit 002 §4.37.6 — return 500 on backend error instead of 200.
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )),
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
    let identity_ref = identity.as_ref().map(|identity| &identity.0);
    let tenant_id = tenant_id_from_identity(identity_ref);

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

    // Operator cross-tenant aggregation (ADR-097). When agent/workflow
    // filters are present, fall through to the tenant-scoped path —
    // operators wanting to filter cross-tenant by agent/workflow can use
    // the future `?tenant=<slug>` pivot.
    let executions_result =
        if is_operator(identity_ref) && agent_id.is_none() && workflow_id.is_none() {
            state
                .execution_repo
                .list_recent_all_paginated(limit, 0)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
        } else {
            state
                .execution_service
                .list_executions_for_tenant(&tenant_id, agent_id, workflow_id, limit)
                .await
        };

    match executions_result {
        Ok(executions) => {
            let json_executions: Vec<serde_json::Value> = executions
                .into_iter()
                .map(|exec| {
                    serde_json::json!({
                        "id": exec.id.0,
                        "agent_id": exec.agent_id.0,
                        "status": format!("{:?}", exec.status),
                        "started_at": exec.started_at,
                        "ended_at": exec.ended_at,
                        "tenant_id": exec.tenant_id.as_str(),
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
        .map(execution_file_response)
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

/// The files route's answer: the bytes, their content type, and their
/// `Content-Length` from the storage stat (AEGIS ADR-005 I8).
fn execution_file_response(content: FileContent) -> axum::response::Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, content.content_type),
            (
                axum::http::header::CONTENT_LENGTH,
                content.size_bytes.to_string(),
            ),
        ],
        content.data,
    )
        .into_response()
}

#[cfg(test)]
mod file_response_tests {
    //! AEGIS ADR-005 I8: the files route answers `Content-Length` from the
    //! FSAL stat, so a download names its size before its first byte.

    use super::*;

    #[test]
    fn the_files_route_answers_content_length_from_the_stat() {
        let response = execution_file_response(FileContent {
            data: b"%PDF-1.7 the report".to_vec(),
            content_type: "application/pdf".to_string(),
            size_bytes: 19,
        });
        let headers = response.headers();
        println!(
            "files route headers: content-type {:?}, content-length {:?}",
            headers.get(axum::http::header::CONTENT_TYPE),
            headers.get(axum::http::header::CONTENT_LENGTH)
        );
        assert_eq!(
            headers
                .get(axum::http::header::CONTENT_TYPE)
                .map(|v| v.as_bytes()),
            Some(&b"application/pdf"[..]),
        );
        assert_eq!(
            headers
                .get(axum::http::header::CONTENT_LENGTH)
                .map(|v| v.as_bytes()),
            Some(&b"19"[..]),
            "I8: the files route answers no Content-Length from the stat"
        );
    }
}

#[cfg(test)]
mod tests {
    //! The status route `GET /v1/executions/{id}` resolves its tenant as the
    //! other delegated routes do (ADR-100): a service account that names a
    //! tenant in `X-Tenant-Id` reads that tenant's execution — the Temporal
    //! worker's status poll of a user's agent execution — and any other
    //! caller stays in its own tenant. Driven through the daemon's real
    //! authentication stack (`test_support::serve`).

    use super::execution_status;
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, serve, service_account,
    };
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::execution::{Execution, ExecutionId, ExecutionInput};
    use aegis_orchestrator_core::domain::iam::UserIdentity;
    use aegis_orchestrator_core::domain::repository::ExecutionRepository;
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::repositories::InMemoryExecutionRepository;
    use axum::extract::{Extension, Path, State};
    use axum::http::HeaderMap;
    use axum::routing::get;
    use axum::Router;
    use std::sync::Arc;
    use uuid::Uuid;

    const SCOPES: &str = "execution:read";

    async fn status_route(
        State(repo): State<Arc<InMemoryExecutionRepository>>,
        identity: Option<Extension<UserIdentity>>,
        headers: HeaderMap,
        Path(execution_id): Path<Uuid>,
    ) -> impl axum::response::IntoResponse {
        let delegation = headers
            .get(super::TENANT_DELEGATION_HEADER)
            .and_then(|v| v.to_str().ok());
        execution_status(
            repo.as_ref(),
            identity.as_ref().map(|identity| &identity.0),
            delegation,
            execution_id,
        )
        .await
    }

    /// One running execution in the tenant of the consumer `owner-sub`.
    async fn owner_execution(repo: &InMemoryExecutionRepository) -> (TenantId, ExecutionId) {
        let tenant = TenantId::for_consumer_user("owner-sub").expect("owner tenant");
        let mut execution = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: Some("work".to_string()),
                input: serde_json::json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            1,
            "aegis-system-operator".to_string(),
        );
        execution.tenant_id = tenant.clone();
        execution.start();
        repo.save_for_tenant(&tenant, &execution).await.unwrap();
        (tenant, execution.id)
    }

    async fn get_status(
        repo: Arc<InMemoryExecutionRepository>,
        caller: UserIdentity,
        execution_id: ExecutionId,
        tenant_header: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let router = Router::new()
            .route("/v1/executions/{execution_id}", get(status_route))
            .with_state(repo);
        let base = serve(
            router,
            Some(identity_provider(&[("caller", caller, SCOPES)])),
            None,
        )
        .await;
        let mut request = reqwest::Client::new()
            .get(format!("{base}/v1/executions/{}", execution_id.0))
            .bearer_auth("caller");
        if let Some(tenant) = tenant_header {
            request = request.header("x-tenant-id", tenant);
        }
        let response = request.send().await.expect("loopback request");
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// The Temporal worker's status poll: a service account naming the
    /// execution's tenant reads it (today: 404, "execution not found in
    /// tenant scope tenant_id=aegis-system").
    #[tokio::test]
    async fn a_service_account_naming_the_tenant_reads_its_execution() {
        let repo = Arc::new(InMemoryExecutionRepository::new());
        let (tenant, id) = owner_execution(&repo).await;
        let (status, body) = get_status(repo, service_account(), id, Some(tenant.as_str())).await;
        assert_eq!(status, 200, "body: {body}");
        assert_eq!(body["status"], "Running");
        assert_eq!(body["tenant_id"], tenant.as_str());
    }

    /// A service account that names no tenant stays in the system tenant,
    /// as today: another tenant's execution is not found.
    #[tokio::test]
    async fn a_service_account_without_the_header_stays_in_the_system_tenant() {
        let repo = Arc::new(InMemoryExecutionRepository::new());
        let (_tenant, id) = owner_execution(&repo).await;
        let (status, body) = get_status(repo, service_account(), id, None).await;
        assert_eq!(status, 404, "body: {body}");
    }

    /// A user naming another user's tenant is refused by the tenant
    /// middleware (`forbidden_tenant_switch`), as on every delegated route.
    #[tokio::test]
    async fn a_user_naming_another_tenant_is_refused() {
        let repo = Arc::new(InMemoryExecutionRepository::new());
        let (tenant, id) = owner_execution(&repo).await;
        let (status, body) =
            get_status(repo, consumer("intruder-sub"), id, Some(tenant.as_str())).await;
        assert_eq!(status, 403, "body: {body}");
    }

    /// The owner reads its own execution without any header, as today.
    #[tokio::test]
    async fn the_owner_reads_its_execution_without_the_header() {
        let repo = Arc::new(InMemoryExecutionRepository::new());
        let (_tenant, id) = owner_execution(&repo).await;
        let (status, body) = get_status(repo, consumer("owner-sub"), id, None).await;
        assert_eq!(status, 200, "body: {body}");
        assert_eq!(body["status"], "Running");
    }

    /// Past the middleware, the handler itself ignores the header for a
    /// caller that may not delegate (`resolve_effective_tenant`): a user
    /// naming another tenant is scoped to its own and does not see it.
    #[tokio::test]
    async fn the_handler_scopes_a_user_naming_another_tenant_to_its_own() {
        let repo = InMemoryExecutionRepository::new();
        let (tenant, id) = owner_execution(&repo).await;
        let (status, _) = execution_status(
            &repo,
            Some(&consumer("intruder-sub")),
            Some(tenant.as_str()),
            id.0,
        )
        .await;
        assert_eq!(status.as_u16(), 404);
    }
}

#[cfg(test)]
mod cancel_ends_goal_tests {
    //! AEGIS ADR-131 U33a: `POST /v1/executions/{id}/cancel` (Zaru Web's
    //! stop) cancels the execution and closes the open goal it is bound to
    //! `cancelled`, with the reason naming it, before it answers; a following
    //! starting call is refused `goal_not_open`. Driven through the route's
    //! own cancel with a real `GoalService` over the in-memory store.

    use super::cancel_execution_ending_its_goal;
    use aegis_orchestrator_core::application::execution::ExecutionService;
    use aegis_orchestrator_core::application::goal_service::{GoalCaller, GoalService};
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::events::ExecutionEvent;
    use aegis_orchestrator_core::domain::execution::{
        Execution, ExecutionId, ExecutionInput, ExecutionStatus, Iteration,
    };
    use aegis_orchestrator_core::domain::goal::{
        BoundKind, GoalChannel, GoalRepository, GoalState,
    };
    use aegis_orchestrator_core::domain::iam::UserIdentity;
    use aegis_orchestrator_core::domain::node_config::GoalsConfig;
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
    use aegis_orchestrator_core::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
    use anyhow::Result;
    use futures::Stream;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    /// One running execution per id; a cancel ends it as the real service does.
    #[derive(Default)]
    struct Executions(Mutex<HashMap<ExecutionId, Execution>>);

    #[async_trait::async_trait]
    impl ExecutionService for Executions {
        async fn start_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_execution_with_id(
            &self,
            _: ExecutionId,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<Execution> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_unscoped(&self, _: ExecutionId) -> Result<Execution> {
            anyhow::bail!("not exercised")
        }
        async fn get_iterations_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<Vec<Iteration>> {
            anyhow::bail!("not exercised")
        }
        async fn cancel_execution_for_tenant(
            &self,
            tenant: &TenantId,
            id: ExecutionId,
        ) -> Result<()> {
            let mut all = self.0.lock().unwrap();
            let e = all
                .get_mut(&id)
                .filter(|e| &e.tenant_id == tenant)
                .ok_or_else(|| anyhow::anyhow!("Execution not found"))?;
            e.status = ExecutionStatus::Cancelled;
            Ok(())
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
            anyhow::bail!("not exercised")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            anyhow::bail!("not exercised")
        }
        async fn list_executions_for_tenant(
            &self,
            _: &TenantId,
            _: Option<AgentId>,
            _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
            _: usize,
        ) -> Result<Vec<Execution>> {
            anyhow::bail!("not exercised")
        }
        async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: aegis_orchestrator_core::domain::execution::LlmInteraction,
        ) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            anyhow::bail!("not exercised")
        }
    }

    #[tokio::test]
    async fn the_rest_cancel_of_a_bound_execution_closes_its_goal_before_it_answers() {
        let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
        let caller = GoalCaller {
            tenant_id: tenant.clone(),
            user_sub: "owner-sub".to_string(),
        };
        let repo = Arc::new(InMemoryGoalRepository::new());
        let goals = GoalService::new(
            repo.clone(),
            Arc::new(EventBus::new(16)),
            GoalsConfig::default(),
        );
        let goal = goals
            .create(
                &caller,
                "Solve the routing problem.",
                "conversation-1",
                GoalChannel::Web,
            )
            .await
            .unwrap();
        let mut execution = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: Some("solve".to_string()),
                input: serde_json::json!({}),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            5,
            "zaru-free".to_string(),
        );
        execution.tenant_id = tenant.clone();
        execution.start();
        let id = execution.id;
        goals.bind(goal.id, id, BoundKind::Agent).await.unwrap();
        let executions = Executions::default();
        executions.0.lock().unwrap().insert(id, execution);

        cancel_execution_ending_its_goal(&executions, Some(&goals), &tenant, id)
            .await
            .unwrap();

        assert_eq!(
            executions.0.lock().unwrap()[&id].status,
            ExecutionStatus::Cancelled
        );
        let stored = repo.find_goal(goal.id).await.unwrap().unwrap();
        println!(
            "after POST /v1/executions/{id}/cancel: the goal is {} ({:?})",
            stored.state.as_str(),
            stored.closed_reason
        );
        assert_eq!(
            stored.state,
            GoalState::Cancelled,
            "the REST cancel ends the goal"
        );
        assert_eq!(
            stored.closed_reason,
            Some(format!("its execution {id} was cancelled"))
        );
        let refused = goals.open_goal_for(&caller, goal.id).await.unwrap_err();
        assert_eq!(
            refused.code(),
            "goal_not_open",
            "a following starting call is refused"
        );
    }
}
