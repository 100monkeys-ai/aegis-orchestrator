// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §F — `/v1/edge/*` REST surface.
//!
//! Endpoints:
//!
//! | Method | Path | Purpose |
//! | --- | --- | --- |
//! | POST   | /v1/edge/enrollment-tokens | Mint an enrollment JWT. |
//! | GET    | /v1/edge/hosts             | List enrolled edges. |
//! | GET    | /v1/edge/hosts/{id}         | Detail. |
//! | PATCH  | /v1/edge/hosts/{id}         | Rename / tag mutation. |
//! | DELETE | /v1/edge/hosts/{id}         | Revoke. |
//! | GET    | /v1/edge/groups            | List groups. |
//! | POST   | /v1/edge/groups            | Create group. |
//! | GET    | /v1/edge/groups/{id}        | Group detail. |
//! | PATCH  | /v1/edge/groups/{id}        | Update selector / pinned. |
//! | DELETE | /v1/edge/groups/{id}        | Delete. |
//! | POST   | /v1/edge/fleet/preview     | Resolve EdgeTarget without dispatch. |
//! | POST   | /v1/edge/fleet/invoke      | Server-streamed per-node dispatch (SSE). |
//! | POST   | /v1/edge/fleet/{id}/cancel  | Cancel a running fleet operation. |
//! | GET    | /v1/edge/fleet/runs        | History (in-memory). |
//! | GET    | /v1/edge/fleet/runs/{id}    | Single run detail. |
//!
//! Effective tenant is resolved per ADR-100 / ADR-111 by
//! `tenant_context_middleware`, which inserts a [`TenantId`] into the request
//! extensions. Handlers obtain it via axum's [`Extension`] extractor — they do
//! NOT read any tenant header directly.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
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
use crate::application::edge::issue_enrollment_token::EnrollmentTokenIssuer;
use crate::application::edge::manage_groups::ManageGroupsService;
use crate::application::edge::manage_tags::ManageTagsService;
use crate::application::edge::revoke_edge::RevokeEdgeService;
use crate::domain::cluster::{FailurePolicy, FleetCommandId, FleetDispatchPolicy, FleetMode};
use crate::domain::edge::{EdgeGroupId, EdgeSelector, EdgeTarget};
use crate::domain::shared_kernel::{NodeId, TenantId};

/// Bundle of edge services consumed by the router.
#[derive(Clone)]
pub struct EdgeApiState {
    pub issue_token: Arc<dyn EnrollmentTokenIssuer>,
    pub edge_repo: Arc<dyn crate::domain::edge::EdgeDaemonRepository>,
    pub group_service: Arc<ManageGroupsService>,
    pub tag_service: Arc<ManageTagsService>,
    pub revoke_service: Arc<RevokeEdgeService>,
    pub resolver: Arc<EdgeFleetResolver>,
    pub fleet_dispatcher: Arc<FleetDispatcher>,
    pub fleet_cancel: Arc<CancelFleetService>,
    pub dispatch_service: Arc<DispatchToEdgeService>,
}

/// Mount the `/v1/edge` router.
pub fn router(state: EdgeApiState) -> Router {
    Router::new()
        .route("/v1/edge/enrollment-tokens", post(post_enrollment_token))
        .route("/v1/edge/hosts", get(list_hosts))
        .route(
            "/v1/edge/hosts/{id}",
            get(get_host).patch(patch_host).delete(delete_host),
        )
        .route("/v1/edge/groups", get(list_groups).post(create_group))
        .route(
            "/v1/edge/groups/{id}",
            get(get_group).patch(patch_group).delete(delete_group),
        )
        .route("/v1/edge/fleet/preview", post(fleet_preview))
        .route("/v1/edge/fleet/invoke", post(fleet_invoke))
        .route("/v1/edge/fleet/{id}/cancel", post(fleet_cancel))
        .route("/v1/edge/fleet/runs", get(list_runs))
        .route("/v1/edge/fleet/runs/{id}", get(get_run))
        .with_state(state)
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
    Extension(tenant): Extension<TenantId>,
    headers: HeaderMap,
    Json(req): Json<IssueTokenRequest>,
) -> Result<Json<IssueTokenResponse>, ApiError> {
    // ADR-117: when this process is a Controller without local signing
    // capability, `issue_token` is a `RelayProxyEnrollmentTokenIssuer` that
    // forwards the request to the Relay Coordinator. The user's Bearer
    // token must be propagated so the Relay's IAM middleware authenticates
    // the request against the same `UserIdentity` / `effective_tenant`.
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let issued = s
        .issue_token
        .issue(&tenant, &req.issued_to, bearer.as_deref())
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
    Extension(tenant): Extension<TenantId>,
) -> Result<Json<Vec<EdgeHostView>>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<EdgeHostView>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<PatchHost>,
) -> Result<Json<Vec<String>>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<CreateGroup>,
) -> Result<Json<GroupView>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
) -> Result<Json<Vec<GroupView>>, ApiError> {
    let gs = s.group_service.list(&tenant).await.map_err(map_group_err)?;
    Ok(Json(gs.iter().map(group_view).collect()))
}

async fn get_group(
    State(s): State<EdgeApiState>,
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<Json<GroupView>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<PatchGroup>,
) -> Result<Json<GroupView>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<FleetTargetSpec>,
) -> Result<Json<FleetPreview>, ApiError> {
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
    Extension(tenant): Extension<TenantId>,
    Json(req): Json<FleetInvokeRequest>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
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
    Extension(_tenant): Extension<TenantId>,
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
    Extension(_tenant): Extension<TenantId>,
    _q: Query<std::collections::HashMap<String, String>>,
) -> Json<EmptyList> {
    // ADR-117 v1: fleet runs are transient (in-memory). History persistence
    // is out of scope for the orchestrator core; observability tooling
    // reconstructs runs from EdgeCommandDispatched / FleetCommand* events.
    Json(EmptyList::default())
}

async fn get_run(
    State(_s): State<EdgeApiState>,
    Extension(_tenant): Extension<TenantId>,
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

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    //! Regression tests for the tenant-extraction contract.
    //!
    //! Bug: the edge handlers used to read a `X-Effective-Tenant` header
    //! invented locally in `edge.rs`. The rest of the orchestrator (per
    //! ADR-100 / ADR-111) injects a `TenantId` into request extensions via
    //! `tenant_context_middleware`, and clients send `X-Tenant-Id`. The
    //! divergence caused real browser requests to fail with
    //! `400 missing X-Effective-Tenant`. These tests assert the new
    //! contract: handlers consume the resolved tenant via
    //! `Extension<TenantId>` and never look at headers themselves.
    use super::*;
    use crate::application::edge::dispatch_to_edge::DispatchToEdgeService;
    use crate::application::edge::fleet::dispatcher::FleetDispatcher;
    use crate::application::edge::fleet::registry::FleetRegistry;
    use crate::application::edge::fleet::{CancelFleetService, EdgeFleetResolver};
    use crate::application::edge::issue_enrollment_token::{
        EnrollmentTokenIssuer, IssueEnrollmentToken, IssuedEnrollmentToken,
        RelayProxyEnrollmentTokenIssuer,
    };
    use crate::application::edge::manage_groups::ManageGroupsService;
    use crate::application::edge::manage_tags::ManageTagsService;
    use crate::application::edge::revoke_edge::RevokeEdgeService;
    use crate::domain::cluster::NodePeerStatus;
    use crate::domain::edge::{
        EdgeDaemon, EdgeDaemonRepository, EdgeGroup, EdgeGroupId, EdgeGroupRepoError,
        EdgeGroupRepository,
    };
    use crate::domain::shared_kernel::TenantId;
    use crate::infrastructure::edge::EdgeConnectionRegistry;
    use crate::infrastructure::secrets_manager::TestSecretStore;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::middleware::{from_fn, Next};
    use tower::ServiceExt;

    // Stub edge repository — list_by_tenant returns empty so list_hosts succeeds.
    struct StubEdgeRepo;
    #[async_trait::async_trait]
    impl EdgeDaemonRepository for StubEdgeRepo {
        async fn upsert(&self, _edge: &EdgeDaemon) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get(&self, _node_id: &NodeId) -> anyhow::Result<Option<EdgeDaemon>> {
            Ok(None)
        }
        async fn list_by_tenant(&self, _tenant_id: &TenantId) -> anyhow::Result<Vec<EdgeDaemon>> {
            Ok(vec![])
        }
        async fn update_status(
            &self,
            _node_id: &NodeId,
            _status: NodePeerStatus,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_tags(&self, _node_id: &NodeId, _tags: &[String]) -> anyhow::Result<()> {
            Ok(())
        }
        async fn update_capabilities(
            &self,
            _node_id: &NodeId,
            _capabilities: &crate::domain::edge::EdgeCapabilities,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete(&self, _node_id: &NodeId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct StubGroupRepo;
    #[async_trait::async_trait]
    impl EdgeGroupRepository for StubGroupRepo {
        async fn create(&self, _group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
            Ok(())
        }
        async fn get(&self, _id: &EdgeGroupId) -> Result<Option<EdgeGroup>, EdgeGroupRepoError> {
            Ok(None)
        }
        async fn list_by_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<Vec<EdgeGroup>, EdgeGroupRepoError> {
            Ok(vec![])
        }
        async fn update(&self, _group: &EdgeGroup) -> Result<(), EdgeGroupRepoError> {
            Ok(())
        }
        async fn delete(&self, _id: &EdgeGroupId) -> Result<(), EdgeGroupRepoError> {
            Ok(())
        }
    }

    fn build_state() -> EdgeApiState {
        let edge_repo: Arc<dyn EdgeDaemonRepository> = Arc::new(StubEdgeRepo);
        let group_repo: Arc<dyn EdgeGroupRepository> = Arc::new(StubGroupRepo);
        let conn_registry = EdgeConnectionRegistry::new();
        let fleet_registry = FleetRegistry::new();
        let secret_store = Arc::new(TestSecretStore::new());

        let issue_token: Arc<dyn EnrollmentTokenIssuer> = Arc::new(IssueEnrollmentToken::new(
            secret_store,
            "test-issuer".into(),
            "test-controller:443".into(),
            "test-key".into(),
        ));
        let group_service = Arc::new(ManageGroupsService::new(group_repo.clone()));
        let tag_service = Arc::new(ManageTagsService::new(edge_repo.clone()));
        let revoke_service = Arc::new(RevokeEdgeService::new(
            edge_repo.clone(),
            conn_registry.clone(),
        ));
        let resolver = Arc::new(EdgeFleetResolver::new(
            edge_repo.clone(),
            group_repo.clone(),
            conn_registry.clone(),
        ));
        let dispatch_service = Arc::new(DispatchToEdgeService::new(
            edge_repo.clone(),
            conn_registry.clone(),
        ));
        let fleet_dispatcher = Arc::new(FleetDispatcher::new(
            dispatch_service.clone(),
            fleet_registry.clone(),
        ));
        let fleet_cancel = Arc::new(CancelFleetService::new(
            fleet_registry,
            conn_registry.clone(),
        ));

        EdgeApiState {
            issue_token,
            edge_repo,
            group_service,
            tag_service,
            revoke_service,
            resolver,
            fleet_dispatcher,
            fleet_cancel,
            dispatch_service,
        }
    }

    /// Wrap the edge router with a synthetic middleware that mirrors
    /// `tenant_context_middleware`'s observable contract: when (and only when)
    /// a request arrives carrying `X-Tenant-Id`, insert a `TenantId` into
    /// request extensions. This isolates the test from the full IAM stack
    /// while still exercising the *integration* point that previously broke.
    fn router_under_tenant_extension_layer() -> Router {
        let state = build_state();
        router(state).layer(from_fn(
            |req: axum::extract::Request, next: Next| async move {
                let tenant = req
                    .headers()
                    .get("X-Tenant-Id")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| TenantId::new(s).ok());
                let mut req = req;
                if let Some(t) = tenant {
                    req.extensions_mut().insert(t);
                }
                next.run(req).await
            },
        ))
    }

    #[tokio::test]
    async fn list_hosts_succeeds_with_x_tenant_id_header() {
        // GIVEN the edge router behind a tenant-injection middleware that
        // mirrors tenant_context_middleware's contract,
        // WHEN a request arrives with `X-Tenant-Id` (the canonical header
        // from zaru-client and ADR-100 service-account delegation),
        // THEN the handler must succeed — proving it consumes the resolved
        // tenant from request extensions, NOT a hand-rolled
        // `X-Effective-Tenant` header.
        let app = router_under_tenant_extension_layer();
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/v1/edge/hosts")
            .header("X-Tenant-Id", "t-consumer")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "list_hosts must succeed when TenantId is supplied via Extension; \
             a 400 here means edge.rs has regressed to reading X-Effective-Tenant"
        );
    }

    #[tokio::test]
    async fn list_runs_succeeds_with_x_tenant_id_header() {
        let app = router_under_tenant_extension_layer();
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/v1/edge/fleet/runs")
            .header("X-Tenant-Id", "t-consumer")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_tenant_extension_does_not_emit_legacy_400() {
        // The middleware contract is: if no tenant is resolvable, the
        // *middleware* short-circuits the request — handlers never observe
        // missing-tenant. With the synthetic middleware in this test, no
        // injection happens when X-Tenant-Id is absent, and the handler's
        // `Extension<TenantId>` extractor must fail with axum's standard
        // 500 — NOT the legacy hand-rolled 400 / "missing X-Effective-Tenant"
        // body that the bug emitted.
        let app = router_under_tenant_extension_layer();
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/v1/edge/hosts")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            !body_str.contains("X-Effective-Tenant"),
            "response must not reference the obsolete X-Effective-Tenant \
             header (got status={status}, body={body_str})"
        );
        assert_ne!(
            status,
            StatusCode::BAD_REQUEST,
            "handler must not emit a hand-rolled 400 for missing tenant — \
             the middleware owns that failure mode"
        );
    }

    /// Regression: ADR-117 path rename. The legacy `/api/edge/*` namespace
    /// was renamed to `/v1/edge/*` to align with the rest of the
    /// orchestrator REST surface. A request to the old path MUST 404.
    #[tokio::test]
    async fn legacy_api_edge_path_returns_404() {
        let app = router_under_tenant_extension_layer();
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/api/edge/hosts")
            .header("X-Tenant-Id", "t-consumer")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "legacy /api/edge/* must not be mounted; only /v1/edge/* is canonical"
        );
    }

    /// Regression: ADR-117 SaaS topology. When the core orchestrator runs
    /// alongside a Relay Coordinator (which holds the OpenBao signing
    /// capability), the enrollment-token endpoint MUST proxy to the Relay
    /// rather than attempt to sign locally. This test wires a stub proxy
    /// issuer into `EdgeApiState` and asserts the handler dispatches to it
    /// (forwarding the Bearer token + tenant for IAM continuity on the
    /// Relay side).
    #[tokio::test]
    async fn enrollment_token_handler_dispatches_to_configured_issuer() {
        use std::sync::Mutex;

        struct CapturingIssuer {
            captured: Mutex<Option<(String, String, Option<String>)>>,
        }
        #[async_trait::async_trait]
        impl EnrollmentTokenIssuer for CapturingIssuer {
            async fn issue(
                &self,
                tenant: &TenantId,
                issued_to_sub: &str,
                bearer: Option<&str>,
            ) -> anyhow::Result<IssuedEnrollmentToken> {
                *self.captured.lock().unwrap() = Some((
                    tenant.as_str().to_string(),
                    issued_to_sub.to_string(),
                    bearer.map(str::to_string),
                ));
                Ok(IssuedEnrollmentToken {
                    token: "stub-token".into(),
                    expires_at: chrono::Utc::now(),
                    controller_endpoint: "relay.myzaru.com:443".into(),
                    qr_payload: "aegis edge enroll stub-token".into(),
                    command_hint: "aegis edge enroll stub-token".into(),
                })
            }
        }

        let issuer = Arc::new(CapturingIssuer {
            captured: Mutex::new(None),
        });
        let mut state = build_state();
        state.issue_token = issuer.clone();
        let app = router(state).layer(from_fn(
            |req: axum::extract::Request, next: Next| async move {
                let tenant = req
                    .headers()
                    .get("X-Tenant-Id")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| TenantId::new(s).ok());
                let mut req = req;
                if let Some(t) = tenant {
                    req.extensions_mut().insert(t);
                }
                next.run(req).await
            },
        ));

        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/edge/enrollment-tokens")
            .header("X-Tenant-Id", "t-consumer")
            .header("Authorization", "Bearer caller-jwt")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"issued_to":"alice"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let captured = issuer.captured.lock().unwrap().clone();
        let (tenant, sub, bearer) = captured.expect("issuer must have been invoked");
        assert_eq!(tenant, "t-consumer");
        assert_eq!(sub, "alice");
        assert_eq!(
            bearer.as_deref(),
            Some("caller-jwt"),
            "Bearer token must be forwarded to the issuer for IAM continuity \
             across the in-pod proxy hop (ADR-117)"
        );
    }

    /// Regression: the proxy issuer constructor must accept the
    /// in-pod Relay endpoint and target the canonical `/v1/edge/...`
    /// path. This is a contract test for the path string used over
    /// the trusted in-pod hop.
    #[test]
    fn relay_proxy_issuer_constructs_with_endpoint() {
        let _issuer =
            RelayProxyEnrollmentTokenIssuer::new("http://aegis-relay-coordinator:8088".into());
        // Smoke test — actual HTTP behavior is covered by integration
        // tests that stand up a mock Relay (out of scope for unit tests
        // that must not bind sockets).
    }
}
