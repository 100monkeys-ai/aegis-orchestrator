// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §F — `/api/edge/*` REST surface.
//!
//! Endpoints:
//!
//! | Method | Path | Purpose |
//! | --- | --- | --- |
//! | POST   | /api/edge/enrollment-tokens | Mint an enrollment JWT. |
//! | GET    | /api/edge/hosts             | List enrolled edges. |
//! | GET    | /api/edge/hosts/{id}         | Detail. |
//! | PATCH  | /api/edge/hosts/{id}         | Rename / tag mutation. |
//! | DELETE | /api/edge/hosts/{id}         | Revoke. |
//! | GET    | /api/edge/groups            | List groups. |
//! | POST   | /api/edge/groups            | Create group. |
//! | GET    | /api/edge/groups/{id}        | Group detail. |
//! | PATCH  | /api/edge/groups/{id}        | Update selector / pinned. |
//! | DELETE | /api/edge/groups/{id}        | Delete. |
//! | POST   | /api/edge/fleet/preview     | Resolve EdgeTarget without dispatch. |
//! | POST   | /api/edge/fleet/invoke      | Server-streamed per-node dispatch (SSE). |
//! | POST   | /api/edge/fleet/{id}/cancel  | Cancel a running fleet operation. |
//! | GET    | /api/edge/fleet/runs        | History (in-memory). |
//! | GET    | /api/edge/fleet/runs/{id}    | Single run detail. |
//!
//! `effective_tenant` is resolved per ADR-100 by the `TenantId` extractor
//! that is already plugged into the orchestrator's middleware stack; here it
//! is supplied via the `EffectiveTenant` header extractor.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use prost_types::Struct;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::application::edge::dispatch_to_edge::DispatchToEdgeService;
use crate::application::edge::fleet::dispatcher::{FleetDispatcher, FleetInvocation};
use crate::application::edge::fleet::{CancelFleetService, EdgeFleetResolver};
use crate::application::edge::issue_enrollment_token::IssueEnrollmentToken;
use crate::application::edge::manage_groups::ManageGroupsService;
use crate::application::edge::manage_tags::ManageTagsService;
use crate::application::edge::revoke_edge::RevokeEdgeService;
use crate::domain::cluster::{FailurePolicy, FleetCommandId, FleetDispatchPolicy, FleetMode};
use crate::domain::edge::{EdgeGroupId, EdgeSelector, EdgeTarget};
use crate::domain::shared_kernel::{NodeId, TenantId};

/// Bundle of edge services consumed by the router.
#[derive(Clone)]
pub struct EdgeApiState {
    pub issue_token: Arc<IssueEnrollmentToken>,
    pub edge_repo: Arc<dyn crate::domain::edge::EdgeDaemonRepository>,
    pub group_service: Arc<ManageGroupsService>,
    pub tag_service: Arc<ManageTagsService>,
    pub revoke_service: Arc<RevokeEdgeService>,
    pub resolver: Arc<EdgeFleetResolver>,
    pub fleet_dispatcher: Arc<FleetDispatcher>,
    pub fleet_cancel: Arc<CancelFleetService>,
    pub dispatch_service: Arc<DispatchToEdgeService>,
}

/// Mount the `/api/edge` router.
pub fn router(state: EdgeApiState) -> Router {
    Router::new()
        .route("/api/edge/enrollment-tokens", post(post_enrollment_token))
        .route("/api/edge/hosts", get(list_hosts))
        .route(
            "/api/edge/hosts/{id}",
            get(get_host).patch(patch_host).delete(delete_host),
        )
        .route("/api/edge/groups", get(list_groups).post(create_group))
        .route(
            "/api/edge/groups/{id}",
            get(get_group).patch(patch_group).delete(delete_group),
        )
        .route("/api/edge/fleet/preview", post(fleet_preview))
        .route("/api/edge/fleet/invoke", post(fleet_invoke))
        .route("/api/edge/fleet/{id}/cancel", post(fleet_cancel))
        .route("/api/edge/fleet/runs", get(list_runs))
        .route("/api/edge/fleet/runs/{id}", get(get_run))
        .with_state(state)
}

// ── Tenant resolution (ADR-100) ────────────────────────────────────────

const TENANT_HEADER: &str = "X-Effective-Tenant";

fn effective_tenant(headers: &HeaderMap) -> Result<TenantId, ApiError> {
    headers
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad_request("missing X-Effective-Tenant"))
        .and_then(|s| TenantId::new(s).map_err(|e| ApiError::bad_request(e.to_string())))
}

// ── Error envelope ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ApiError {
    status: u16,
    code: String,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, msg: impl Into<String>) -> Self {
        Self {
            status: status.as_u16(),
            code: code.into(),
            message: msg.into(),
        }
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", msg)
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", msg)
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", msg)
    }
    fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

// ── Enrollment tokens ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct IssueTokenRequest {
    issued_to: String,
}

#[derive(Serialize)]
struct IssueTokenResponse {
    token: String,
    expires_at: String,
    controller_endpoint: String,
    qr_payload: String,
    command_hint: String,
}

async fn post_enrollment_token(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Json(req): Json<IssueTokenRequest>,
) -> Result<Json<IssueTokenResponse>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let issued = s
        .issue_token
        .issue(&tenant, &req.issued_to)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(IssueTokenResponse {
        token: issued.token,
        expires_at: issued.expires_at.to_rfc3339(),
        controller_endpoint: issued.controller_endpoint,
        qr_payload: issued.qr_payload,
        command_hint: issued.command_hint,
    }))
}

// ── Hosts ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct EdgeHostView {
    node_id: String,
    tenant_id: String,
    status: String,
    tags: Vec<String>,
    os: String,
    arch: String,
}

async fn list_hosts(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
) -> Result<Json<Vec<EdgeHostView>>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let edges = s
        .edge_repo
        .list_by_tenant(&tenant)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(
        edges
            .into_iter()
            .map(|e| EdgeHostView {
                node_id: e.node_id.to_string(),
                tenant_id: e.tenant_id.as_str().to_string(),
                status: format!("{:?}", e.status).to_lowercase(),
                tags: e.capabilities.tags,
                os: e.capabilities.os,
                arch: e.capabilities.arch,
            })
            .collect(),
    ))
}

async fn get_host(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<EdgeHostView>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let nid = parse_node_id(&id)?;
    let edge = s
        .edge_repo
        .get(&nid)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .ok_or_else(|| ApiError::not_found("edge"))?;
    if edge.tenant_id != tenant {
        return Err(ApiError::not_found("edge"));
    }
    Ok(Json(EdgeHostView {
        node_id: edge.node_id.to_string(),
        tenant_id: edge.tenant_id.as_str().to_string(),
        status: format!("{:?}", edge.status).to_lowercase(),
        tags: edge.capabilities.tags,
        os: edge.capabilities.os,
        arch: edge.capabilities.arch,
    }))
}

#[derive(Deserialize)]
struct PatchHost {
    add_tags: Option<Vec<String>>,
    remove_tags: Option<Vec<String>>,
}

async fn patch_host(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchHost>,
) -> Result<Json<Vec<String>>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let nid = parse_node_id(&id)?;
    let mut tags = Vec::new();
    if let Some(add) = body.add_tags {
        tags = s
            .tag_service
            .add_tags(&tenant, nid, add)
            .await
            .map_err(map_tags_err)?;
    }
    if let Some(rm) = body.remove_tags {
        tags = s
            .tag_service
            .remove_tags(&tenant, nid, rm)
            .await
            .map_err(map_tags_err)?;
    }
    Ok(Json(tags))
}

fn map_tags_err(e: crate::application::edge::manage_tags::ManageTagsError) -> ApiError {
    use crate::application::edge::manage_tags::ManageTagsError;
    match e {
        ManageTagsError::NotFound => ApiError::not_found("edge"),
        ManageTagsError::Forbidden => {
            ApiError::new(StatusCode::FORBIDDEN, "forbidden", "cross-tenant refused")
        }
        ManageTagsError::Repo(msg) => ApiError::internal(msg),
    }
}

fn map_revoke_err(e: crate::application::edge::revoke_edge::RevokeEdgeError) -> ApiError {
    use crate::application::edge::revoke_edge::RevokeEdgeError;
    match e {
        RevokeEdgeError::NotFound => ApiError::not_found("edge"),
        RevokeEdgeError::Forbidden => {
            ApiError::new(StatusCode::FORBIDDEN, "forbidden", "cross-tenant refused")
        }
        RevokeEdgeError::Repo(msg) => ApiError::internal(msg),
    }
}

async fn delete_host(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let nid = parse_node_id(&id)?;
    s.revoke_service
        .revoke(&tenant, nid)
        .await
        .map_err(map_revoke_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Groups ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateGroup {
    name: String,
    selector: EdgeSelector,
    #[serde(default)]
    pinned_members: Vec<String>,
    created_by: String,
}

#[derive(Serialize)]
struct GroupView {
    id: String,
    name: String,
    selector: EdgeSelector,
    pinned_members: Vec<String>,
    created_by: String,
    created_at: String,
}

fn group_view(g: &crate::domain::edge::EdgeGroup) -> GroupView {
    GroupView {
        id: g.id.to_string(),
        name: g.name.clone(),
        selector: g.selector.clone(),
        pinned_members: g.pinned_members.iter().map(|n| n.to_string()).collect(),
        created_by: g.created_by.clone(),
        created_at: g.created_at.to_rfc3339(),
    }
}

async fn create_group(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Json(req): Json<CreateGroup>,
) -> Result<Json<GroupView>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let pinned: Vec<NodeId> = req
        .pinned_members
        .iter()
        .filter_map(|s| NodeId::from_string(s).ok())
        .collect();
    let group = s
        .group_service
        .create(tenant, req.name, req.selector, pinned, req.created_by)
        .await
        .map_err(map_group_err)?;
    Ok(Json(group_view(&group)))
}

async fn list_groups(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
) -> Result<Json<Vec<GroupView>>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let gs = s.group_service.list(&tenant).await.map_err(map_group_err)?;
    Ok(Json(gs.iter().map(group_view).collect()))
}

async fn get_group(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<GroupView>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let gid =
        EdgeGroupId(uuid::Uuid::parse_str(&id).map_err(|e| ApiError::bad_request(e.to_string()))?);
    let g = s
        .group_service
        .get(&tenant, gid)
        .await
        .map_err(map_group_err)?;
    Ok(Json(group_view(&g)))
}

#[derive(Deserialize)]
struct PatchGroup {
    name: Option<String>,
    selector: Option<EdgeSelector>,
    pinned_members: Option<Vec<String>>,
}

async fn patch_group(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PatchGroup>,
) -> Result<Json<GroupView>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let gid =
        EdgeGroupId(uuid::Uuid::parse_str(&id).map_err(|e| ApiError::bad_request(e.to_string()))?);
    let mut g = s
        .group_service
        .get(&tenant, gid)
        .await
        .map_err(map_group_err)?;
    if let Some(n) = body.name {
        g.name = n;
    }
    if let Some(sel) = body.selector {
        g.selector = sel;
    }
    if let Some(p) = body.pinned_members {
        g.pinned_members = p
            .iter()
            .filter_map(|s| NodeId::from_string(s).ok())
            .collect();
    }
    let updated = s
        .group_service
        .update(&tenant, g)
        .await
        .map_err(map_group_err)?;
    Ok(Json(group_view(&updated)))
}

async fn delete_group(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let gid =
        EdgeGroupId(uuid::Uuid::parse_str(&id).map_err(|e| ApiError::bad_request(e.to_string()))?);
    s.group_service
        .delete(&tenant, gid)
        .await
        .map_err(map_group_err)?;
    Ok(StatusCode::NO_CONTENT)
}

fn map_group_err(e: crate::application::edge::manage_groups::ManageGroupError) -> ApiError {
    use crate::application::edge::manage_groups::ManageGroupError;
    match e {
        ManageGroupError::Exists => ApiError::conflict("group exists"),
        ManageGroupError::NotFound => ApiError::not_found("group"),
        ManageGroupError::CrossTenant => ApiError::not_found("group"),
        ManageGroupError::Other(e) => ApiError::internal(e.to_string()),
    }
}

// ── Fleet ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FleetTargetSpec {
    target: EdgeTarget,
}

#[derive(Serialize)]
struct FleetPreview {
    resolved: Vec<String>,
}

async fn fleet_preview(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Json(req): Json<FleetTargetSpec>,
) -> Result<Json<FleetPreview>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let resolved = s
        .resolver
        .resolve(&tenant, &req.target)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(FleetPreview {
        resolved: resolved.into_iter().map(|n| n.to_string()).collect(),
    }))
}

#[derive(Deserialize)]
struct FleetInvokeRequest {
    target: EdgeTarget,
    tool_name: String,
    args: serde_json::Value,
    security_context_name: String,
    #[serde(default)]
    user_security_token: String,
    mode: Option<String>,
    batch: Option<usize>,
    max_concurrency: Option<usize>,
    failure_policy: Option<String>,
    stop_after: Option<usize>,
    require_min_targets: Option<usize>,
    deadline_secs: Option<u64>,
}

async fn fleet_invoke(
    State(s): State<EdgeApiState>,
    headers: HeaderMap,
    Json(req): Json<FleetInvokeRequest>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let tenant = effective_tenant(&headers)?;
    let resolved = s
        .resolver
        .resolve(&tenant, &req.target)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let policy = build_policy(&req)?;
    let args_struct: Struct = crate::application::edge::json_value_to_prost_struct(req.args)
        .map_err(|e| ApiError::bad_request(format!("args must be JSON object: {e}")))?;
    let inv = FleetInvocation {
        fleet_command_id: FleetCommandId::new(),
        tenant_id: tenant.clone(),
        tool_name: req.tool_name,
        args: args_struct,
        security_context_name: req.security_context_name,
        user_seal_envelope: crate::infrastructure::aegis_cluster_proto::SealEnvelope {
            user_security_token: req.user_security_token,
            tenant_id: tenant.as_str().to_string(),
            security_context_name: String::new(),
            payload: None,
            signature: vec![],
        },
        resolved,
        policy,
    };
    let rx = s.fleet_dispatcher.clone().spawn(inv);
    let stream = ReceiverStream::new(rx).map(|ev| {
        Ok::<_, Infallible>(
            Event::default()
                .json_data(&ev)
                .unwrap_or_else(|_| Event::default().data("{}")),
        )
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default().interval(Duration::from_secs(15))))
}

fn build_policy(req: &FleetInvokeRequest) -> Result<FleetDispatchPolicy, ApiError> {
    let mode = match req.mode.as_deref() {
        Some("parallel") => FleetMode::Parallel,
        Some("rolling") => FleetMode::Rolling {
            batch: req.batch.unwrap_or(1).max(1),
        },
        _ => FleetMode::Sequential,
    };
    let failure_policy = match req.failure_policy.as_deref() {
        Some("continue") => FailurePolicy::ContinueOnError,
        Some("stop-after") => FailurePolicy::StopAfter(req.stop_after.unwrap_or(1)),
        _ => FailurePolicy::FailFast,
    };
    Ok(FleetDispatchPolicy {
        mode,
        max_concurrency: req.max_concurrency,
        failure_policy,
        require_min_targets: req.require_min_targets,
        per_target_deadline: Duration::from_secs(req.deadline_secs.unwrap_or(60)),
    })
}

async fn fleet_cancel(
    State(s): State<EdgeApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let fleet_id = FleetCommandId(
        uuid::Uuid::parse_str(&id).map_err(|e| ApiError::bad_request(e.to_string()))?,
    );
    let cancelled = s.fleet_cancel.cancel(fleet_id).await;
    if cancelled {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("fleet command"))
    }
}

#[derive(Serialize, Default)]
struct EmptyList {
    runs: Vec<()>,
}

async fn list_runs(
    State(_s): State<EdgeApiState>,
    _headers: HeaderMap,
    _q: Query<std::collections::HashMap<String, String>>,
) -> Json<EmptyList> {
    // ADR-117 v1: fleet runs are transient (in-memory). History persistence
    // is out of scope for the orchestrator core; observability tooling
    // reconstructs runs from EdgeCommandDispatched / FleetCommand* events.
    Json(EmptyList::default())
}

async fn get_run(
    State(_s): State<EdgeApiState>,
    Path(_id): Path<String>,
) -> Result<Json<EmptyList>, ApiError> {
    Err(ApiError::not_found(
        "fleet run history not retained server-side",
    ))
}

// ── Helpers ────────────────────────────────────────────────────────────

fn parse_node_id(s: &str) -> Result<NodeId, ApiError> {
    NodeId::from_string(s).map_err(|e| ApiError::bad_request(e.to_string()))
}
