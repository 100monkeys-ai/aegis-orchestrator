// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Execution handlers: get, cancel, list, delete, stream events, file retrieval.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use futures::StreamExt;
use uuid::Uuid;

use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::file_operations_service::{
    FileContent, FileOperationsError, FileOperationsService,
};
use aegis_orchestrator_core::application::goal_service::{
    cancel_ending_its_goal, CancelPathError, GoalService,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::repository::{
    ExecutionRepository, WorkflowExecutionRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::volume::VolumeId;
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
    Ok(execution_or_workflow_status(
        state.execution_repo.as_ref(),
        state.workflow_execution_repo.as_ref(),
        state.workflow_repo.as_ref(),
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
///
/// The answer is `{kind: "agent", id, agent_id, status, tenant_id}`. Once
/// the execution has ended (Completed, Failed or Cancelled) it also carries
/// `final_output`, the output of the last iteration that has one, and
/// `error`, the execution's error, each `null` when there is none, so a
/// caller that finds the run already over (the worker re-attaching to it)
/// reads what it produced. A Pending or Running execution carries neither
/// key.
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
        Ok(Some(exec)) => {
            use aegis_orchestrator_core::domain::execution::ExecutionStatus;
            let mut body = serde_json::json!({
                "kind": "agent",
                "id": exec.id.0,
                "agent_id": exec.agent_id.0,
                "status": format!("{:?}", exec.status),
                "tenant_id": exec.tenant_id.as_str(),
            });
            if matches!(
                exec.status,
                ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
            ) {
                let final_output = exec
                    .iterations
                    .iter()
                    .rev()
                    .find_map(|iteration| iteration.output.as_deref());
                body["final_output"] = serde_json::json!(final_output);
                body["error"] = serde_json::json!(exec.error);
            }
            (StatusCode::OK, axum::Json(body))
        }
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

/// `GET /v1/executions/{id}`'s answer for an id of either kind.
///
/// An agent execution answers as [`execution_status`] gives it, with
/// `"kind": "agent"`. When no agent execution has the id, a workflow run of
/// the tenant the request resolves to (the same resolution as the agent
/// read, [`tenant_id_from_request`]) answers with `"kind": "workflow"` and
/// the fields `GET /v1/workflows/executions/{id}` gives: `execution_id`,
/// `workflow_id`, `workflow_name`, `status` (lower case, as that route
/// gives it), `current_state`, `started_at`, `last_transition_at` and
/// `tenant_id`; once the run has ended (completed, failed or cancelled) it
/// also carries `final_output`, `null` when the run produced none. Neither
/// found answers 404 "Execution not found".
pub(crate) async fn execution_or_workflow_status(
    execution_repo: &dyn ExecutionRepository,
    workflow_execution_repo: &dyn WorkflowExecutionRepository,
    workflow_repo: &dyn WorkflowRepository,
    identity: Option<&UserIdentity>,
    delegation: Option<&str>,
    execution_id: Uuid,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    let agent_answer = execution_status(execution_repo, identity, delegation, execution_id).await;
    if agent_answer.0 != StatusCode::NOT_FOUND {
        return agent_answer;
    }
    let tenant_id = tenant_id_from_request(identity, delegation);
    let run = match workflow_execution_repo
        .find_by_id_for_tenant(&tenant_id, ExecutionId(execution_id))
        .await
    {
        Ok(Some(run)) => run,
        Ok(None) => return agent_answer,
        Err(e) => {
            tracing::debug!(error = %e, %execution_id, "workflow execution lookup failed");
            return agent_answer;
        }
    };
    let workflow_name = match workflow_repo
        .find_by_id_visible(&tenant_id, run.workflow_id)
        .await
    {
        Ok(Some(workflow)) => Some(workflow.metadata.name),
        _ => None,
    };
    use aegis_orchestrator_core::domain::execution::ExecutionStatus;
    let mut body = serde_json::json!({
        "kind": "workflow",
        "id": run.id.0,
        "execution_id": run.id.0,
        "workflow_id": run.workflow_id.0,
        "workflow_name": workflow_name,
        "status": format!("{:?}", run.status).to_lowercase(),
        "current_state": run.current_state.as_str(),
        "started_at": run.started_at,
        "last_transition_at": run.last_transition_at,
        "tenant_id": run.tenant_id.as_str(),
    });
    if matches!(
        run.status,
        ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
    ) {
        body["final_output"] = run.final_output.unwrap_or(serde_json::Value::Null);
    }
    (StatusCode::OK, axum::Json(body))
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
) -> Result<(), CancelPathError<anyhow::Error>> {
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
    headers: HeaderMap,
    Path(execution_id): Path<Uuid>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("execution:cancel")?;
    let delegation = headers
        .get(TENANT_DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());
    Ok(execution_cancel(
        state.execution_service.as_ref(),
        state
            .tool_invocation_service
            .goal_service()
            .map(|goals| goals.as_ref()),
        identity.as_ref().map(|identity| &identity.0),
        delegation,
        execution_id,
    )
    .await)
}

/// The cancel route's answer for `execution_id` as `identity` asks it.
///
/// The tenant is resolved as on the status route ([`tenant_id_from_request`],
/// ADR-100): a service account takes the tenant it names in `X-Tenant-Id` —
/// the Temporal worker's cancel of a user's agent execution — and every other
/// identity keeps its own tenant whatever the header says. An execution
/// outside that tenant is not cancelled.
pub(crate) async fn execution_cancel(
    executions: &dyn ExecutionService,
    goals: Option<&GoalService>,
    identity: Option<&UserIdentity>,
    delegation: Option<&str>,
    execution_id: Uuid,
) -> (StatusCode, axum::Json<serde_json::Value>) {
    let tenant_id = tenant_id_from_request(identity, delegation);
    match cancel_execution_ending_its_goal(executions, goals, &tenant_id, ExecutionId(execution_id))
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({"success": true})),
        ),
        // Audit 002 §4.37.6 — return 500 on backend error instead of 200.
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
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

    // Determine the maximum allowed page size from configuration, with a
    // backward-compatible default of 1000 if not set.
    let max_limit = state
        .config
        .spec
        .max_execution_list_limit
        .unwrap_or(DEFAULT_MAX_EXECUTION_LIST_LIMIT);

    Ok(axum::Json(
        list_executions(
            state.execution_repo.as_ref(),
            state.execution_service.as_ref(),
            state.workflow_repo.as_ref(),
            max_limit,
            identity.as_ref().map(|identity| &identity.0),
            &query,
        )
        .await,
    ))
}

/// `GET /v1/executions`'s answer for `query` as `identity` sees it: an
/// array of executions, newest first, at most `limit` (20 when absent,
/// capped at `max_limit`), or `{"error": …}`.
pub(crate) async fn list_executions(
    execution_repo: &dyn ExecutionRepository,
    execution_service: &dyn ExecutionService,
    workflow_repo: &dyn WorkflowRepository,
    max_limit: usize,
    identity: Option<&UserIdentity>,
    query: &ListExecutionsQuery,
) -> serde_json::Value {
    let agent_id = query.agent_id.map(AgentId);
    let limit = query.limit.unwrap_or(20).min(max_limit);
    let tenant_id = tenant_id_from_identity(identity);

    // Resolve workflow_name to a WorkflowId if provided
    let workflow_id = if let Some(ref wf_name) = query.workflow_name {
        match workflow_repo
            .find_by_name_visible(&tenant_id, wf_name)
            .await
        {
            Ok(Some(wf)) => Some(wf.id),
            Ok(None) => {
                return serde_json::json!({"error": format!("Workflow '{}' not found", wf_name)});
            }
            Err(e) => {
                return serde_json::json!({"error": e.to_string()});
            }
        }
    } else {
        None
    };

    // A workflow name's executions roll up across its versions, active and
    // archived (AEGIS ADR-148).
    let workflow_ids = match (&query.workflow_name, workflow_id) {
        (Some(wf_name), Some(wid)) => {
            let mut ids = workflow_repo
                .version_ids_visible(&tenant_id, wf_name)
                .await
                .unwrap_or_default();
            if !ids.contains(&wid) {
                ids.push(wid);
            }
            ids
        }
        _ => Vec::new(),
    };

    // Operator cross-tenant aggregation (ADR-097). When agent/workflow
    // filters are present, fall through to the tenant-scoped path —
    // operators wanting to filter cross-tenant by agent/workflow can use
    // the future `?tenant=<slug>` pivot.
    let executions_result = if is_operator(identity) && agent_id.is_none() && workflow_id.is_none()
    {
        execution_repo
            .list_recent_all_paginated(limit, 0)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    } else if workflow_ids.len() > 1 {
        execution_service
            .list_executions_for_workflows_for_tenant(&tenant_id, &workflow_ids, limit, 0)
            .await
    } else {
        execution_service
            .list_executions_for_tenant(&tenant_id, agent_id, workflow_id, limit, 0)
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
            serde_json::json!(json_executions)
        }
        Err(e) => serde_json::json!({"error": e.to_string()}),
    }
}

/// The files route's query: `volume_id` reads the file from that volume of
/// the caller's tenant in place of the execution's workspace (the volume an
/// `aegis.image.generate` answer names).
#[derive(serde::Deserialize)]
pub(crate) struct ExecutionFileQuery {
    pub(crate) volume_id: Option<Uuid>,
}

/// GET /v1/executions/:execution_id/files/*path
///
/// Read a single file from a completed execution's workspace volume post-mortem,
/// or, with `?volume_id=<id>`, from that volume of the caller's tenant, the
/// execution id then not read. The path segment is normalized: a `/workspace/`
/// prefix is stripped if present.
pub(crate) async fn get_execution_file_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path((execution_id, file_path)): Path<(Uuid, String)>,
    Query(query): Query<ExecutionFileQuery>,
) -> Result<impl IntoResponse, (StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("execution:read")?;
    let tenant_id = tenant_id_from_identity(identity.as_ref().map(|identity| &identity.0));
    execution_file(
        state.file_operations_service.as_ref(),
        &tenant_id,
        ExecutionId(execution_id),
        &file_path,
        query.volume_id.map(VolumeId),
    )
    .await
}

/// The files route past its scope check: the file's bytes from `volume_id`
/// when the query names one (tenant-checked: another tenant's volume is
/// answered as one that does not exist), otherwise from the execution.
async fn execution_file(
    file_operations: &FileOperationsService,
    tenant_id: &TenantId,
    execution_id: ExecutionId,
    file_path: &str,
    volume_id: Option<VolumeId>,
) -> Result<axum::response::Response, (StatusCode, axum::Json<serde_json::Value>)> {
    // Normalize: strip /workspace/ prefix if present
    let normalized = file_path
        .strip_prefix("workspace/")
        .or_else(|| file_path.strip_prefix("/workspace/"))
        .unwrap_or(file_path);

    let read = match volume_id {
        Some(volume_id) => {
            file_operations
                .read_file_in_volume_for_tenant(&volume_id, tenant_id, normalized)
                .await
        }
        None => {
            file_operations
                .read_file_for_execution(execution_id, tenant_id, normalized)
                .await
        }
    };
    read.map(execution_file_response).map_err(|e| {
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
mod file_volume_tests {
    //! The files route with `?volume_id=`: the bytes of a file an
    //! `aegis.image.generate` answer names, read from the volume it names
    //! (row `image-generate-file-unfetchable`), tenant-checked.

    use super::execution_file;
    use aegis_orchestrator_core::application::file_operations_service::FileOperationsService;
    use aegis_orchestrator_core::domain::events::StorageEvent;
    use aegis_orchestrator_core::domain::execution::ExecutionId;
    use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
    use aegis_orchestrator_core::domain::repository::{ExecutionRepository, VolumeRepository};
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::domain::volume::{
        StorageClass, Volume, VolumeBackend, VolumeId, VolumeOwnership,
    };
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
    };
    use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// The first bytes of a JPEG file, then a few more.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIFgenerated-image";
    const PATH: &str = "generated/54a33efd/1.jpg";

    struct NoStorageEvents;

    #[async_trait::async_trait]
    impl EventPublisher for NoStorageEvents {
        async fn publish_storage_event(&self, _event: StorageEvent) {}
    }

    /// A file service over a scratch root holding one person's
    /// `chat-attachments` volume in `tenant` with [`JPEG`] at [`PATH`].
    async fn person_volume(
        tenant: &TenantId,
    ) -> (tempfile::TempDir, FileOperationsService, VolumeId) {
        let root = tempfile::tempdir().expect("scratch root");
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(root.path()).expect("storage root")),
            volumes.clone() as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let mut volume = Volume::new(
            "chat-attachments".to_string(),
            tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: std::path::PathBuf::from("/placeholder"),
            },
            64 * 1024 * 1024,
            VolumeOwnership::persistent("image-person"),
        )
        .expect("volume");
        volume.backend = VolumeBackend::HostPath {
            path: std::path::PathBuf::from(format!("/hosts/{}", volume.id)),
        };
        volume.mark_available().expect("available");
        volumes.save(&volume).await.expect("saved");
        let on_disk = root.path().join(format!("hosts/{}", volume.id)).join(PATH);
        std::fs::create_dir_all(on_disk.parent().expect("a parent")).expect("dirs");
        std::fs::write(on_disk, JPEG).expect("written");
        let service = FileOperationsService::new(
            fsal,
            Arc::new(InMemoryExecutionRepository::new()) as Arc<dyn ExecutionRepository>,
            Arc::new(InMemoryAgentRepository::new()),
        );
        (root, service, volume.id)
    }

    /// The route answers the file's bytes from the volume its query names,
    /// with its content type and length; the execution id is not read.
    #[tokio::test]
    async fn the_route_answers_the_bytes_of_the_volume_it_names() {
        let tenant = TenantId::for_consumer_user("image-person").expect("tenant");
        let (_root, files, volume) = person_volume(&tenant).await;
        let response = execution_file(&files, &tenant, ExecutionId(volume.0), PATH, Some(volume))
            .await
            .expect("the file is answered");
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body");
        println!(
            "files route with volume_id: content-type {:?}, content-length {:?}, {} bytes",
            headers.get(axum::http::header::CONTENT_TYPE),
            headers.get(axum::http::header::CONTENT_LENGTH),
            body.len()
        );
        assert_eq!(body.as_ref(), JPEG);
        assert_eq!(headers[axum::http::header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(
            headers[axum::http::header::CONTENT_LENGTH],
            JPEG.len().to_string()
        );
    }

    /// Another tenant's volume is answered as one that does not exist: 404
    /// with the same body form, no byte of the file.
    #[tokio::test]
    async fn the_route_answers_another_tenants_volume_as_missing() {
        let owner = TenantId::for_consumer_user("image-person").expect("tenant");
        let caller = TenantId::for_consumer_user("intruder").expect("tenant");
        let (_root, files, volume) = person_volume(&owner).await;
        let missing = VolumeId::new();
        let answer = |volume: VolumeId| {
            let files = &files;
            let caller = &caller;
            async move {
                match execution_file(files, caller, ExecutionId::new(), PATH, Some(volume)).await {
                    Ok(_) => panic!("another tenant's file was answered"),
                    Err((status, axum::Json(body))) => (status, body),
                }
            }
        };
        let (refused_status, refused) = answer(volume).await;
        let (absent_status, absent) = answer(missing).await;
        println!("another tenant's volume: {refused_status} {refused}; a missing one: {absent_status} {absent}");
        assert_eq!(refused_status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(absent_status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(
            refused,
            serde_json::json!({"error": format!("not found: no file {PATH} in volume {volume}")})
        );
        assert_eq!(
            absent,
            serde_json::json!({"error": format!("not found: no file {PATH} in volume {missing}")})
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
        consumer, identity_provider, operator, serve, service_account,
    };
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::execution::{Execution, ExecutionId, ExecutionInput};
    use aegis_orchestrator_core::domain::iam::{AegisRole, UserIdentity};
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

    /// The owner's running execution, ended by `end`, as the Temporal
    /// worker's poll (a service account naming the tenant) and an
    /// operator's cross-tenant read each see it.
    async fn ended_status_reads(end: impl FnOnce(&mut Execution)) -> [(u16, serde_json::Value); 2] {
        let repo = InMemoryExecutionRepository::new();
        let (tenant, id) = owner_execution(&repo).await;
        let mut execution = repo
            .find_by_id_for_tenant(&tenant, id)
            .await
            .unwrap()
            .expect("the owner's execution");
        end(&mut execution);
        repo.save_for_tenant(&tenant, &execution).await.unwrap();
        let worker =
            execution_status(&repo, Some(&service_account()), Some(tenant.as_str()), id.0).await;
        let operator_read =
            execution_status(&repo, Some(&operator(AegisRole::Operator)), None, id.0).await;
        [
            (worker.0.as_u16(), worker.1 .0),
            (operator_read.0.as_u16(), operator_read.1 .0),
        ]
    }

    /// A completed execution answers the output of its last iteration and
    /// no error, so a worker re-attaching to it after it ended answers what
    /// it produced.
    #[tokio::test]
    async fn a_completed_execution_answers_its_last_iterations_output() {
        for (status, body) in ended_status_reads(|execution| {
            execution.max_iterations = 2;
            execution.start_iteration("first".to_string()).unwrap();
            execution.complete_iteration("first answer".to_string());
            execution.start_iteration("second".to_string()).unwrap();
            execution.complete_iteration("second answer".to_string());
            execution.complete();
        })
        .await
        {
            assert_eq!(status, 200, "body: {body}");
            assert_eq!(body["status"], "Completed");
            assert_eq!(body["final_output"], "second answer", "body: {body}");
            assert!(
                body.get("error").is_some_and(|error| error.is_null()),
                "a completed execution answers `error: null`: {body}"
            );
        }
    }

    /// A failed execution answers its error.
    #[tokio::test]
    async fn a_failed_execution_answers_its_error() {
        for (status, body) in ended_status_reads(|execution| {
            execution.fail("the model refused the request".to_string());
        })
        .await
        {
            assert_eq!(status, 200, "body: {body}");
            assert_eq!(body["status"], "Failed");
            assert_eq!(
                body["error"], "the model refused the request",
                "body: {body}"
            );
            assert!(
                body.get("final_output")
                    .is_some_and(|output| output.is_null()),
                "a failed execution with no output answers `final_output: null`: {body}"
            );
        }
    }

    /// A running execution carries neither key, so a poller sees the shape
    /// it saw before until the run ends.
    #[tokio::test]
    async fn a_running_execution_carries_no_output_and_no_error() {
        let repo = InMemoryExecutionRepository::new();
        let (tenant, id) = owner_execution(&repo).await;
        for identity in [service_account(), operator(AegisRole::Operator)] {
            let (status, body) =
                execution_status(&repo, Some(&identity), Some(tenant.as_str()), id.0).await;
            assert_eq!(status.as_u16(), 200, "body: {:?}", body.0);
            assert_eq!(body.0["status"], "Running");
            assert!(body.0.get("final_output").is_none(), "body: {:?}", body.0);
            assert!(body.0.get("error").is_none(), "body: {:?}", body.0);
        }
    }

    // ── A workflow run's id on the executions route ──
    //
    // `GET /v1/executions/{id}` answers a workflow run of the caller's tenant
    // when no agent execution has the id, with `"kind": "workflow"` and the
    // fields the workflow route gives; an agent execution answers
    // `"kind": "agent"`; another tenant's workflow run stays 404.

    use super::execution_or_workflow_status;
    use aegis_orchestrator_core::domain::execution::ExecutionStatus;
    use aegis_orchestrator_core::domain::repository::{
        WorkflowExecutionRepository, WorkflowRepository,
    };
    use aegis_orchestrator_core::domain::workflow::{
        StateKind, StateName, Workflow, WorkflowExecution, WorkflowMetadata, WorkflowSpec,
        WorkflowState,
    };
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
    };
    use std::collections::HashMap;

    #[derive(Clone)]
    struct BothKinds {
        executions: Arc<InMemoryExecutionRepository>,
        workflow_executions: Arc<InMemoryWorkflowExecutionRepository>,
        workflows: Arc<InMemoryWorkflowRepository>,
    }

    impl BothKinds {
        fn new() -> Self {
            Self {
                executions: Arc::new(InMemoryExecutionRepository::new()),
                workflow_executions: Arc::new(InMemoryWorkflowExecutionRepository::new()),
                workflows: Arc::new(InMemoryWorkflowRepository::new()),
            }
        }
    }

    async fn both_kinds_route(
        State(repos): State<BothKinds>,
        identity: Option<Extension<UserIdentity>>,
        headers: HeaderMap,
        Path(execution_id): Path<Uuid>,
    ) -> impl axum::response::IntoResponse {
        let delegation = headers
            .get(super::TENANT_DELEGATION_HEADER)
            .and_then(|v| v.to_str().ok());
        execution_or_workflow_status(
            repos.executions.as_ref(),
            repos.workflow_executions.as_ref(),
            repos.workflows.as_ref(),
            identity.as_ref().map(|identity| &identity.0),
            delegation,
            execution_id,
        )
        .await
    }

    async fn get_any_status(
        repos: BothKinds,
        caller: UserIdentity,
        id: Uuid,
        tenant_header: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let router = Router::new()
            .route("/v1/executions/{execution_id}", get(both_kinds_route))
            .with_state(repos);
        let base = serve(
            router,
            Some(identity_provider(&[("caller", caller, SCOPES)])),
            None,
        )
        .await;
        let mut request = reqwest::Client::new()
            .get(format!("{base}/v1/executions/{id}"))
            .bearer_auth("caller");
        if let Some(tenant) = tenant_header {
            request = request.header("x-tenant-id", tenant);
        }
        let response = request.send().await.expect("loopback request");
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    fn two_state_workflow() -> Workflow {
        let mut states = HashMap::new();
        for name in ["ANALYZE", "BUILD"] {
            states.insert(
                StateName::new(name).unwrap(),
                WorkflowState {
                    kind: StateKind::System {
                        command: "echo".to_string(),
                        env: HashMap::new(),
                        workdir: None,
                    },
                    transitions: vec![],
                    timeout: None,
                    max_state_visits: None,
                },
            );
        }
        Workflow::new(
            WorkflowMetadata {
                name: "the-forge".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                labels: HashMap::new(),
                annotations: HashMap::new(),
                input_schema: None,
                output_schema: None,
                output_template: None,
            },
            WorkflowSpec {
                initial_state: StateName::new("ANALYZE").unwrap(),
                context: HashMap::new(),
                states,
                storage: Default::default(),
                max_total_transitions: None,
                default_schedule: None,
                repositories: None,
            },
        )
        .unwrap()
    }

    /// A workflow run of the consumer `owner-sub`, moved to `BUILD` and
    /// shaped by `shape`, with its workflow saved in the same tenant.
    async fn owner_workflow_run(
        repos: &BothKinds,
        shape: impl FnOnce(&mut WorkflowExecution),
    ) -> (TenantId, ExecutionId) {
        let tenant = TenantId::for_consumer_user("owner-sub").expect("owner tenant");
        let workflow = two_state_workflow();
        repos
            .workflows
            .save_for_tenant(&tenant, &workflow)
            .await
            .unwrap();
        let mut run = WorkflowExecution::new(&workflow, ExecutionId::new(), serde_json::json!({}));
        run.tenant_id = tenant.clone();
        run.transition_to(StateName::new("BUILD").unwrap());
        shape(&mut run);
        repos
            .workflow_executions
            .save_for_tenant(&tenant, &run)
            .await
            .unwrap();
        (tenant, run.id)
    }

    /// A workflow run's id answers 200 with `kind: workflow` and the run's
    /// state (today: 404 "Execution not found", the route reading agent
    /// executions only), for its owner and for the worker's delegated poll.
    #[tokio::test]
    async fn a_workflow_runs_id_answers_kind_workflow_and_its_current_state() {
        let repos = BothKinds::new();
        let (tenant, id) = owner_workflow_run(&repos, |_| {}).await;
        for (caller, header) in [
            (consumer("owner-sub"), None),
            (service_account(), Some(tenant.as_str())),
        ] {
            let (status, body) = get_any_status(repos.clone(), caller, id.0, header).await;
            assert_eq!(status, 200, "a workflow run's id answered: {body}");
            assert_eq!(body["kind"], "workflow", "body: {body}");
            assert_eq!(body["current_state"], "BUILD", "body: {body}");
            assert_eq!(body["status"], "running", "body: {body}");
            assert_eq!(body["workflow_name"], "the-forge", "body: {body}");
            assert_eq!(body["execution_id"], id.0.to_string(), "body: {body}");
            assert!(body["started_at"].is_string(), "body: {body}");
            assert!(body["last_transition_at"].is_string(), "body: {body}");
            assert!(
                body.get("final_output").is_none(),
                "a running workflow run carries no final output: {body}"
            );
        }
    }

    /// An ended workflow run answers its final output.
    #[tokio::test]
    async fn an_ended_workflow_run_answers_its_final_output() {
        let repos = BothKinds::new();
        let (_tenant, id) = owner_workflow_run(&repos, |run| {
            run.status = ExecutionStatus::Completed;
            run.final_output = Some(serde_json::json!({"summary": "built and tested"}));
        })
        .await;
        let (status, body) = get_any_status(repos, consumer("owner-sub"), id.0, None).await;
        assert_eq!(status, 200, "body: {body}");
        assert_eq!(body["kind"], "workflow", "body: {body}");
        assert_eq!(body["status"], "completed", "body: {body}");
        assert_eq!(
            body["final_output"],
            serde_json::json!({"summary": "built and tested"}),
            "body: {body}"
        );
    }

    /// An agent execution's id still answers as an agent execution, now
    /// with `kind: agent`.
    #[tokio::test]
    async fn an_agent_executions_id_answers_kind_agent() {
        let repos = BothKinds::new();
        let (tenant, id) = owner_execution(&repos.executions).await;
        let (status, body) = get_any_status(repos, consumer("owner-sub"), id.0, None).await;
        assert_eq!(status, 200, "body: {body}");
        assert_eq!(body["kind"], "agent", "body: {body}");
        assert_eq!(body["status"], "Running", "body: {body}");
        assert_eq!(body["tenant_id"], tenant.as_str(), "body: {body}");
    }

    /// Another tenant's workflow run is not found, as another tenant's
    /// agent execution is not.
    #[tokio::test]
    async fn another_tenants_workflow_run_answers_404() {
        let repos = BothKinds::new();
        let (_tenant, id) = owner_workflow_run(&repos, |_| {}).await;
        let (status, body) = get_any_status(repos, consumer("intruder-sub"), id.0, None).await;
        assert_eq!(status, 404, "body: {body}");
        assert_eq!(body["error"], "Execution not found", "body: {body}");
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
    pub(super) struct Executions(pub(super) Mutex<HashMap<ExecutionId, Execution>>);

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

#[cfg(test)]
mod cancel_tenant_tests {
    //! The cancel route `POST /v1/executions/{id}/cancel` resolves its tenant
    //! as the status route does (ADR-100): a service account that names a
    //! tenant in `X-Tenant-Id` cancels that tenant's execution — the Temporal
    //! worker's cancel of a user's agent execution — and any other caller
    //! stays in its own tenant. Driven through the daemon's real
    //! authentication stack (`test_support::serve`).

    use super::cancel_ends_goal_tests::Executions;
    use super::execution_cancel;
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, serve, service_account,
    };
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::execution::{
        Execution, ExecutionId, ExecutionInput, ExecutionStatus,
    };
    use aegis_orchestrator_core::domain::iam::UserIdentity;
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use axum::extract::{Extension, Path, State};
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;
    use uuid::Uuid;

    const SCOPES: &str = "execution:cancel";

    async fn cancel_route(
        State(executions): State<Arc<Executions>>,
        identity: Option<Extension<UserIdentity>>,
        headers: HeaderMap,
        Path(execution_id): Path<Uuid>,
    ) -> impl axum::response::IntoResponse {
        let delegation = headers
            .get(super::TENANT_DELEGATION_HEADER)
            .and_then(|v| v.to_str().ok());
        execution_cancel(
            executions.as_ref(),
            None,
            identity.as_ref().map(|identity| &identity.0),
            delegation,
            execution_id,
        )
        .await
    }

    /// One running execution in the tenant of the consumer `owner-sub`.
    fn owner_execution(executions: &Executions) -> (TenantId, ExecutionId) {
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
        let id = execution.id;
        executions.0.lock().unwrap().insert(id, execution);
        (tenant, id)
    }

    fn status_of(executions: &Executions, id: ExecutionId) -> ExecutionStatus {
        executions.0.lock().unwrap()[&id].status.clone()
    }

    async fn post_cancel(
        executions: Arc<Executions>,
        caller: UserIdentity,
        execution_id: ExecutionId,
        tenant_header: Option<&str>,
    ) -> (u16, serde_json::Value) {
        let router = Router::new()
            .route("/v1/executions/{execution_id}/cancel", post(cancel_route))
            .with_state(executions);
        let base = serve(
            router,
            Some(identity_provider(&[("caller", caller, SCOPES)])),
            None,
        )
        .await;
        let mut request = reqwest::Client::new()
            .post(format!("{base}/v1/executions/{}/cancel", execution_id.0))
            .bearer_auth("caller");
        if let Some(tenant) = tenant_header {
            request = request.header("x-tenant-id", tenant);
        }
        let response = request.send().await.expect("loopback request");
        let status = response.status().as_u16();
        let body = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    /// The Temporal worker's cancel: a service account naming the
    /// execution's tenant cancels it (before: 500, "Execution not found",
    /// the tenant taken from the service account alone).
    #[tokio::test]
    async fn a_service_account_naming_the_tenant_cancels_its_execution() {
        let executions = Arc::new(Executions::default());
        let (tenant, id) = owner_execution(&executions);
        let (status, body) = post_cancel(
            executions.clone(),
            service_account(),
            id,
            Some(tenant.as_str()),
        )
        .await;
        assert_eq!(
            status, 200,
            "a service account naming the tenant was not let cancel its execution; body: {body}"
        );
        assert_eq!(body["success"], true);
        assert_eq!(status_of(&executions, id), ExecutionStatus::Cancelled);
    }

    /// A service account that names no tenant stays in the system tenant,
    /// as before: another tenant's execution is not found and keeps running.
    #[tokio::test]
    async fn a_service_account_without_the_header_keeps_its_answer() {
        let executions = Arc::new(Executions::default());
        let (_tenant, id) = owner_execution(&executions);
        let (status, body) = post_cancel(executions.clone(), service_account(), id, None).await;
        assert_eq!(status, 500, "body: {body}");
        assert_eq!(body["error"], "Execution not found");
        assert_eq!(status_of(&executions, id), ExecutionStatus::Running);
    }

    /// A user naming another user's tenant is refused by the tenant
    /// middleware (`forbidden_tenant_switch`), and the execution keeps
    /// running.
    #[tokio::test]
    async fn a_user_naming_another_tenant_is_refused() {
        let executions = Arc::new(Executions::default());
        let (tenant, id) = owner_execution(&executions);
        let (status, body) = post_cancel(
            executions.clone(),
            consumer("intruder-sub"),
            id,
            Some(tenant.as_str()),
        )
        .await;
        assert_eq!(status, 403, "body: {body}");
        assert_eq!(status_of(&executions, id), ExecutionStatus::Running);
    }

    /// Past the middleware, the cancel itself ignores the header for a
    /// caller that may not delegate (`resolve_effective_tenant`): a user
    /// naming another tenant is scoped to its own and cancels nothing.
    #[tokio::test]
    async fn the_cancel_scopes_a_user_naming_another_tenant_to_its_own() {
        let executions = Executions::default();
        let (tenant, id) = owner_execution(&executions);
        let (status, _) = execution_cancel(
            &executions,
            None,
            Some(&consumer("intruder-sub")),
            Some(tenant.as_str()),
            id.0,
        )
        .await;
        assert_eq!(status.as_u16(), 500);
        assert_eq!(status_of(&executions, id), ExecutionStatus::Running);
    }
}

#[cfg(test)]
mod list_offset_tests {
    //! `GET /v1/executions` pages by `offset` as the workflow list does:
    //! `?limit=20&offset=20` answers the twenty executions after the first
    //! twenty, newest first, and a missing offset answers from the newest
    //! (row `executions-list-no-offset-2026-10-21`: the offset was dropped,
    //! so every page was the first). Driven through the daemon's real
    //! authentication stack (`test_support::serve`).

    use super::{list_executions, ListExecutionsQuery, DEFAULT_MAX_EXECUTION_LIST_LIMIT};
    use crate::daemon::handlers::test_support::{consumer, identity_provider, operator, serve};
    use aegis_orchestrator_core::application::execution::ExecutionService;
    use aegis_orchestrator_core::domain::agent::AgentId;
    use aegis_orchestrator_core::domain::events::ExecutionEvent;
    use aegis_orchestrator_core::domain::execution::{
        Execution, ExecutionId, ExecutionInput, Iteration,
    };
    use aegis_orchestrator_core::domain::iam::{AegisRole, UserIdentity};
    use aegis_orchestrator_core::domain::repository::ExecutionRepository;
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::domain::workflow::WorkflowId;
    use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryExecutionRepository, InMemoryWorkflowRepository,
    };
    use anyhow::Result;
    use axum::extract::{Extension, Query, State};
    use axum::routing::get;
    use axum::Router;
    use futures::Stream;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;

    /// Lists the in-memory store's executions the way the daemon's service
    /// does: one agent's, one workflow's (by the ids `workflows` names) or
    /// all of the tenant's, newest first, the `limit` after the first
    /// `offset`.
    #[derive(Default)]
    struct StoreListed {
        repo: Arc<InMemoryExecutionRepository>,
        workflows: HashMap<WorkflowId, Vec<ExecutionId>>,
    }

    #[async_trait::async_trait]
    impl ExecutionService for StoreListed {
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
        async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
            anyhow::bail!("not exercised")
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
            tenant: &TenantId,
            agent_id: Option<AgentId>,
            workflow_id: Option<WorkflowId>,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<Execution>> {
            if let Some(workflow_id) = workflow_id {
                let members = self
                    .workflows
                    .get(&workflow_id)
                    .cloned()
                    .unwrap_or_default();
                let mut rows: Vec<Execution> = self
                    .repo
                    .find_recent_for_tenant(tenant, usize::MAX, 0)
                    .await?
                    .into_iter()
                    .filter(|e| members.contains(&e.id))
                    .collect();
                rows.sort_by_key(|e| std::cmp::Reverse(e.started_at));
                return Ok(rows.into_iter().skip(offset).take(limit).collect());
            }
            Ok(match agent_id {
                Some(agent_id) => {
                    self.repo
                        .find_by_agent_for_tenant(tenant, agent_id, limit, offset)
                        .await?
                }
                None => {
                    self.repo
                        .find_recent_for_tenant(tenant, limit, offset)
                        .await?
                }
            })
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

    #[derive(Clone)]
    struct ListState {
        service: Arc<StoreListed>,
        workflows: Arc<InMemoryWorkflowRepository>,
    }

    async fn list_route(
        State(state): State<ListState>,
        identity: Option<Extension<UserIdentity>>,
        Query(query): Query<ListExecutionsQuery>,
    ) -> axum::Json<serde_json::Value> {
        axum::Json(
            list_executions(
                state.service.repo.as_ref(),
                state.service.as_ref(),
                state.workflows.as_ref(),
                DEFAULT_MAX_EXECUTION_LIST_LIMIT,
                identity.as_ref().map(|identity| &identity.0),
                &query,
            )
            .await,
        )
    }

    /// One execution of `agent` in `tenant`, started `second` seconds after
    /// a fixed instant.
    fn execution(tenant: &TenantId, agent: AgentId, second: i64) -> Execution {
        let mut execution = Execution::new(
            agent,
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
        execution.started_at =
            chrono::DateTime::from_timestamp(1_760_000_000 + second, 0).expect("an instant");
        execution
    }

    /// Store `executions` and answer their ids newest first.
    async fn stored(repo: &InMemoryExecutionRepository, executions: &[Execution]) -> Vec<String> {
        for execution in executions {
            repo.save_for_tenant(&execution.tenant_id, execution)
                .await
                .unwrap();
        }
        let mut newest_first: Vec<&Execution> = executions.iter().collect();
        newest_first.sort_by_key(|e| std::cmp::Reverse(e.started_at));
        newest_first.iter().map(|e| e.id.0.to_string()).collect()
    }

    /// `GET /v1/executions{query}` as `caller`: the status and the ids
    /// answered, in order.
    async fn list(service: StoreListed, caller: UserIdentity, query: &str) -> (u16, Vec<String>) {
        let router = Router::new()
            .route("/v1/executions", get(list_route))
            .with_state(ListState {
                service: Arc::new(service),
                workflows: Arc::new(InMemoryWorkflowRepository::new()),
            });
        let base = serve(
            router,
            Some(identity_provider(&[("caller", caller, "execution:list")])),
            None,
        )
        .await;
        let response = reqwest::Client::new()
            .get(format!("{base}/v1/executions{query}"))
            .bearer_auth("caller")
            .send()
            .await
            .expect("loopback request");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        let ids = body
            .as_array()
            .unwrap_or_else(|| panic!("an array of executions, not {body}"))
            .iter()
            .map(|e| e["id"].as_str().expect("an id").to_string())
            .collect();
        (status, ids)
    }

    /// A consumer with forty-five executions: no offset answers the newest
    /// twenty, `offset=20` the next twenty, `offset=40` the last five.
    #[tokio::test]
    async fn a_tenants_list_answers_the_rows_after_the_offset() {
        let owner = consumer("owner-sub");
        let tenant = TenantId::for_consumer_user("owner-sub").expect("owner tenant");
        let agent = AgentId::new();
        let mut pages = Vec::new();
        for query in ["?limit=20", "?limit=20&offset=20", "?limit=20&offset=40"] {
            let service = StoreListed::default();
            let executions: Vec<Execution> =
                (0..45).map(|i| execution(&tenant, agent, i)).collect();
            let expected = stored(&service.repo, &executions).await;
            let (status, ids) = list(service, owner.clone(), query).await;
            println!("{query}: {status}, {} rows", ids.len());
            assert_eq!(status, 200);
            pages.push((query, ids, expected));
        }
        for ((query, ids, expected), range) in pages.into_iter().zip([0..20, 20..40, 40..45]) {
            assert_eq!(
                ids,
                expected[range.clone()].to_vec(),
                "{query} answers the rows {range:?}, newest first"
            );
        }
    }

    /// An operator's list of every tenant pages the same way.
    #[tokio::test]
    async fn an_operators_list_of_every_tenant_answers_the_rows_after_the_offset() {
        let tenant_a = TenantId::for_consumer_user("a-sub").expect("tenant a");
        let tenant_b = TenantId::for_consumer_user("b-sub").expect("tenant b");
        let agent = AgentId::new();
        let mut answered = Vec::new();
        for query in ["?limit=20", "?limit=20&offset=20"] {
            let service = StoreListed::default();
            let executions: Vec<Execution> = (0..30)
                .map(|i| {
                    let tenant = if i % 2 == 0 { &tenant_a } else { &tenant_b };
                    execution(tenant, agent, i)
                })
                .collect();
            let expected = stored(&service.repo, &executions).await;
            let (status, ids) = list(service, operator(AegisRole::Admin), query).await;
            println!("operator {query}: {status}, {} rows", ids.len());
            assert_eq!(status, 200);
            answered.push((query, ids, expected));
        }
        for ((query, ids, expected), range) in answered.into_iter().zip([0..20, 20..30]) {
            assert_eq!(
                ids,
                expected[range.clone()].to_vec(),
                "{query} answers the rows {range:?}, newest first"
            );
        }
    }

    /// One agent's executions page by offset among another agent's.
    #[tokio::test]
    async fn one_agents_list_answers_the_rows_after_the_offset() {
        let owner = consumer("owner-sub");
        let tenant = TenantId::for_consumer_user("owner-sub").expect("owner tenant");
        let agent = AgentId::new();
        let other = AgentId::new();
        let service = StoreListed::default();
        let mine: Vec<Execution> = (0..6).map(|i| execution(&tenant, agent, 2 * i)).collect();
        let theirs: Vec<Execution> = (0..6)
            .map(|i| execution(&tenant, other, 2 * i + 1))
            .collect();
        let expected = stored(&service.repo, &mine).await;
        stored(&service.repo, &theirs).await;
        let query = format!("?agent_id={}&limit=2&offset=2", agent.0);
        let (status, ids) = list(service, owner, &query).await;
        println!("{query}: {status}, {ids:?}");
        assert_eq!(status, 200);
        assert_eq!(ids, expected[2..4].to_vec(), "the agent's third and fourth");
    }

    /// A workflow name's executions across two versions page by offset over
    /// the merged list, newest first.
    #[tokio::test]
    async fn a_workflow_names_list_across_versions_answers_the_rows_after_the_offset() {
        let tenant = TenantId::for_consumer_user("owner-sub").expect("owner tenant");
        let agent = AgentId::new();
        let (v1, v2) = (WorkflowId::new(), WorkflowId::new());
        let mut service = StoreListed::default();
        let executions: Vec<Execution> = (0..10).map(|i| execution(&tenant, agent, i)).collect();
        let expected = stored(&service.repo, &executions).await;
        for (i, e) in executions.iter().enumerate() {
            let version = if i % 2 == 0 { v1 } else { v2 };
            service.workflows.entry(version).or_default().push(e.id);
        }
        let rows = service
            .list_executions_for_workflows_for_tenant(&tenant, &[v1, v2], 3, 3)
            .await
            .expect("the merged list");
        let ids: Vec<String> = rows.iter().map(|e| e.id.0.to_string()).collect();
        println!("versions v1, v2, limit 3, offset 3: {ids:?}");
        assert_eq!(ids, expected[3..6].to_vec(), "the merged fourth to sixth");
    }
}
