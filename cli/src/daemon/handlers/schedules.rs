// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedule routes (AEGIS ADR-139 N6, N10)
//!
//! | Route | Caller | Scope |
//! |-------|--------|-------|
//! | `POST /v1/schedules` | a person | `schedule:write` |
//! | `GET /v1/schedules` | the owner (own); an operator (the tenant's) | `schedule:read` |
//! | `GET /v1/schedules/{id}` | the owner; an operator (the tenant's) | `schedule:read` |
//! | `PATCH /v1/schedules/{id}` | the owner | `schedule:write` |
//! | `POST /v1/schedules/{id}/pause` | the owner | `schedule:write` |
//! | `POST /v1/schedules/{id}/resume` | the owner | `schedule:write` |
//! | `DELETE /v1/schedules/{id}` | the owner | `schedule:write` |
//! | `GET /v1/schedules/{id}/runs?limit=` | the owner; an operator (the tenant's) | `schedule:read` |
//! | `GET /v1/schedules/defaults?target_kind=&target=` | a person; an operator | `schedule:read` |
//! | `POST /v1/internal/schedules/{id}/fire` | the Temporal worker's service account only | none |
//!
//! Another person's schedule is answered 404, exactly as one that does not
//! exist. An operator reads a tenant's schedules and never changes one; a
//! service account is refused every route but the fire, with N3's sentence.

use std::sync::Arc;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::schedule_service::{
    ScheduleError, ScheduleReader, ScheduleRunView, ScheduleService, ScheduleView,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::repository::WorkflowRepository;
use aegis_orchestrator_core::domain::schedule::{
    DefaultSchedule, ScheduleDraft, ScheduleFire, ScheduleId, SchedulePatch, Timing,
    FIRE_CLIENT_ID, OWNER_REFUSAL, TARGET_KIND_REFUSAL, TARGET_REFUSAL, UNAVAILABLE_REFUSAL,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::daemon::handlers::tenant_id_from_identity;

type Refusal = (StatusCode, Json<Value>);

/// The refusal of a `limit` out of range.
const LIMIT_REFUSAL: &str = "'limit' must be between 1 and 100.";
/// The refusal of a fire whose `scheduled_time` is not an RFC 3339 time.
const SCHEDULED_TIME_REFUSAL: &str = "'scheduled_time' must be an RFC 3339 time.";

/// State of the schedule sub-router.
#[derive(Clone)]
pub(crate) struct SchedulesState {
    /// `None` when the node has no schedule service: every route answers 503.
    pub(crate) service: Option<Arc<ScheduleService>>,
    /// The agents a defaults read looks a target up in (N12).
    pub(crate) agents: Arc<dyn AgentLifecycleService>,
    /// The workflows a defaults read looks a target up in (N12).
    pub(crate) workflows: Arc<dyn WorkflowRepository>,
}

/// The `/v1/schedules*` routes and the worker's fire route, merged into the
/// daemon router by `router::create_router` beneath the same authentication
/// layers as every other route.
pub(crate) fn schedules_router(state: SchedulesState) -> Router {
    Router::new()
        .route(
            "/v1/schedules",
            post(create_schedule_handler).get(list_schedules_handler),
        )
        .route(
            "/v1/schedules/{id}",
            get(get_schedule_handler)
                .patch(update_schedule_handler)
                .delete(delete_schedule_handler),
        )
        .route("/v1/schedules/{id}/pause", post(pause_schedule_handler))
        .route("/v1/schedules/{id}/resume", post(resume_schedule_handler))
        .route("/v1/schedules/{id}/runs", get(list_schedule_runs_handler))
        .route("/v1/schedules/defaults", get(schedule_defaults_handler))
        .route(
            "/v1/internal/schedules/{id}/fire",
            post(fire_schedule_handler),
        )
        .with_state(state)
}

fn refusal(status: StatusCode, error: &str) -> Refusal {
    (status, Json(json!({ "error": error })))
}

fn service(state: &SchedulesState) -> Result<&Arc<ScheduleService>, Refusal> {
    state
        .service
        .as_ref()
        .ok_or_else(|| refusal(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE_REFUSAL))
}

fn from_service_error(e: ScheduleError) -> Refusal {
    match e {
        ScheduleError::Refused(sentence) => refusal(StatusCode::BAD_REQUEST, &sentence),
        ScheduleError::NotFound => refusal(StatusCode::NOT_FOUND, "Not found"),
        ScheduleError::Forbidden(sentence) => refusal(StatusCode::FORBIDDEN, &sentence),
        ScheduleError::Unavailable => refusal(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE_REFUSAL),
        ScheduleError::Repository(detail) => {
            tracing::error!(error = %detail, "Schedule store failed");
            refusal(StatusCode::INTERNAL_SERVER_ERROR, "Schedule store failed")
        }
    }
}

/// Who is asking, for a read.
fn reader(
    identity: Option<&UserIdentity>,
    tenant: Option<&TenantId>,
) -> Result<ScheduleReader, Refusal> {
    let identity =
        identity.ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. } => {
            Ok(ScheduleReader::Owner {
                sub: identity.sub.clone(),
            })
        }
        IdentityKind::Operator { .. } => Ok(ScheduleReader::Operator {
            tenant: tenant
                .cloned()
                .unwrap_or_else(|| tenant_id_from_identity(Some(identity))),
        }),
        IdentityKind::ServiceAccount { .. } => Err(refusal(StatusCode::FORBIDDEN, OWNER_REFUSAL)),
    }
}

/// The person asking, for a change, with the tenant their schedule lives
/// in; an operator or a service account is refused (N3).
fn person<'a>(
    identity: Option<&'a UserIdentity>,
    tenant: Option<&TenantId>,
) -> Result<(&'a UserIdentity, TenantId), Refusal> {
    let identity =
        identity.ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. } => Ok((
            identity,
            tenant
                .cloned()
                .unwrap_or_else(|| tenant_id_from_identity(Some(identity))),
        )),
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => {
            Err(refusal(StatusCode::FORBIDDEN, OWNER_REFUSAL))
        }
    }
}

fn parse_id(id: &str) -> Result<ScheduleId, Refusal> {
    ScheduleId::parse(id).ok_or_else(|| refusal(StatusCode::NOT_FOUND, "Not found"))
}

fn fire_view(fire: &ScheduleFire) -> Value {
    json!({
        "scheduled_time": fire.scheduled_time,
        "fired_at": fire.fired_at,
        "outcome": fire.outcome.as_str(),
        "execution_id": fire.execution_id.map(|e| e.to_string()),
        "detail": fire.detail,
    })
}

fn schedule_view(view: &ScheduleView) -> Value {
    let s = &view.schedule;
    let (at, recurrence) = match &s.timing {
        Timing::Once { at } => (json!(at), Value::Null),
        Timing::Recurrence(r) => (
            Value::Null,
            json!({
                "cron": r.cron,
                "timezone": r.timezone,
                "jitter_seconds": r.jitter_seconds,
            }),
        ),
    };
    json!({
        "id": s.id.to_string(),
        "name": s.name,
        "target_kind": s.target_kind.as_str(),
        "target": s.target,
        "version": s.target_version,
        "intent": s.intent,
        "input": s.input,
        "attachments": s.attachments,
        "contexts": s.contexts,
        "profile_id": s.profile_id.map(|p| p.to_string()),
        "repositories": s.repositories,
        "at": at,
        "recurrence": recurrence,
        "state": s.state.as_str(),
        "paused_reason": s.paused_reason,
        "next_run_at": view.next_run_at,
        "last_run": view.last_run.as_ref().map(|fire| json!({
            "time": fire.scheduled_time,
            "outcome": fire.outcome.as_str(),
            "execution_id": fire.execution_id.map(|e| e.to_string()),
        })),
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

fn run_view(run: &ScheduleRunView) -> Value {
    let mut view = fire_view(&run.fire);
    view["execution"] = match run.fire.execution_id {
        Some(id) => json!({
            "id": id.to_string(),
            "kind": run.kind.as_str(),
            "status": run.status.as_ref().map(|s| format!("{s:?}").to_lowercase()),
        }),
        None => Value::Null,
    };
    view
}

/// `POST /v1/schedules`.
pub(crate) async fn create_schedule_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Json(draft): Json<ScheduleDraft>,
) -> Result<(StatusCode, Json<Value>), Refusal> {
    scope_guard.require("schedule:write")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let view = service(&state)?
        .create(owner, &tenant_id, draft)
        .await
        .map_err(from_service_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "schedule": schedule_view(&view) })),
    ))
}

/// `GET /v1/schedules`.
pub(crate) async fn list_schedules_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:read")?;
    let reader = reader(identity.as_deref(), tenant.as_deref())?;
    let views = service(&state)?
        .list(&reader)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({
        "count": views.len(),
        "schedules": views.iter().map(schedule_view).collect::<Vec<_>>(),
    })))
}

/// `GET /v1/schedules/{id}`.
pub(crate) async fn get_schedule_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:read")?;
    let reader = reader(identity.as_deref(), tenant.as_deref())?;
    let view = service(&state)?
        .get(&reader, parse_id(&id)?)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "schedule": schedule_view(&view) })))
}

/// `PATCH /v1/schedules/{id}`.
pub(crate) async fn update_schedule_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
    Json(patch): Json<SchedulePatch>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:write")?;
    let (owner, _) = person(identity.as_deref(), tenant.as_deref())?;
    let view = service(&state)?
        .update(owner, parse_id(&id)?, patch)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "schedule": schedule_view(&view) })))
}

/// `POST /v1/schedules/{id}/pause`.
pub(crate) async fn pause_schedule_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:write")?;
    let (owner, _) = person(identity.as_deref(), tenant.as_deref())?;
    let view = service(&state)?
        .pause(owner, parse_id(&id)?)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "schedule": schedule_view(&view) })))
}

/// `POST /v1/schedules/{id}/resume`.
pub(crate) async fn resume_schedule_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:write")?;
    let (owner, _) = person(identity.as_deref(), tenant.as_deref())?;
    let view = service(&state)?
        .resume(owner, parse_id(&id)?)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "schedule": schedule_view(&view) })))
}

/// `DELETE /v1/schedules/{id}`.
pub(crate) async fn delete_schedule_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:write")?;
    let (owner, _) = person(identity.as_deref(), tenant.as_deref())?;
    let id = parse_id(&id)?;
    service(&state)?
        .delete(owner, id)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "deleted": id.to_string() })))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct RunsQuery {
    limit: Option<i64>,
}

/// `GET /v1/schedules/{id}/runs?limit=`: newest first, 1 to 100, default 20.
pub(crate) async fn list_schedule_runs_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
    Query(query): Query<RunsQuery>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:read")?;
    let reader = reader(identity.as_deref(), tenant.as_deref())?;
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(refusal(StatusCode::BAD_REQUEST, LIMIT_REFUSAL));
    }
    let runs = service(&state)?
        .runs(&reader, parse_id(&id)?, limit as usize)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({
        "count": runs.len(),
        "runs": runs.iter().map(run_view).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct DefaultsQuery {
    target_kind: Option<String>,
    target: Option<String>,
}

/// The recommended timing the target's manifest carries in
/// `spec.default_schedule`, or `None`; a 404 when no such target is
/// visible to the tenant.
async fn default_schedule_of(
    state: &SchedulesState,
    tenant: &TenantId,
    target_kind: &str,
    target: &str,
) -> Result<Option<DefaultSchedule>, Refusal> {
    let not_found = || refusal(StatusCode::NOT_FOUND, "Not found");
    match target_kind {
        "agent" => {
            let id = match uuid::Uuid::parse_str(target) {
                Ok(uuid) => AgentId(uuid),
                Err(_) => state
                    .agents
                    .lookup_agent_visible_for_tenant(tenant, target)
                    .await
                    .ok()
                    .flatten()
                    .ok_or_else(not_found)?,
            };
            let agent = state
                .agents
                .get_agent_visible(tenant, id)
                .await
                .map_err(|_| not_found())?;
            Ok(agent.manifest.spec.default_schedule)
        }
        "workflow" => {
            let workflow = state
                .workflows
                .find_by_name_visible(tenant, target)
                .await
                .map_err(|e| {
                    tracing::error!(error = %e, "Workflow lookup failed");
                    refusal(StatusCode::INTERNAL_SERVER_ERROR, "Workflow lookup failed")
                })?
                .ok_or_else(not_found)?;
            Ok(workflow.spec.default_schedule)
        }
        _ => Err(refusal(StatusCode::BAD_REQUEST, TARGET_KIND_REFUSAL)),
    }
}

/// `GET /v1/schedules/defaults?target_kind=&target=`: the timing the
/// target's manifest recommends, or `null` (N12). It makes nothing.
pub(crate) async fn schedule_defaults_handler(
    State(state): State<SchedulesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Query(query): Query<DefaultsQuery>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("schedule:read")?;
    let tenant = match reader(identity.as_deref(), tenant.as_deref())? {
        ScheduleReader::Operator { tenant } => tenant,
        ScheduleReader::Owner { .. } => tenant
            .as_deref()
            .cloned()
            .unwrap_or_else(|| tenant_id_from_identity(identity.as_deref())),
    };
    let target_kind = query.target_kind.as_deref().unwrap_or_default();
    if !matches!(target_kind, "agent" | "workflow") {
        return Err(refusal(StatusCode::BAD_REQUEST, TARGET_KIND_REFUSAL));
    }
    let target = query
        .target
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| refusal(StatusCode::BAD_REQUEST, TARGET_REFUSAL))?;
    let recommended = default_schedule_of(&state, &tenant, target_kind, target).await?;
    Ok(Json(json!({ "default_schedule": recommended })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct FireBody {
    scheduled_time: String,
}

/// `POST /v1/internal/schedules/{id}/fire` with `{scheduled_time}`: only
/// the Temporal worker's service account, in the tenant its `X-Tenant-Id`
/// names; idempotent per scheduled time (N6).
pub(crate) async fn fire_schedule_handler(
    State(state): State<SchedulesState>,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
    Json(body): Json<FireBody>,
) -> Result<Json<Value>, Refusal> {
    let identity = identity
        .as_deref()
        .ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    let is_worker = matches!(
        &identity.identity_kind,
        IdentityKind::ServiceAccount { client_id } if client_id == FIRE_CLIENT_ID
    );
    if !is_worker {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "Only the workflow worker fires a schedule",
        ));
    }
    let tenant = tenant
        .as_deref()
        .cloned()
        .ok_or_else(|| refusal(StatusCode::BAD_REQUEST, "X-Tenant-Id is required"))?;
    let scheduled_time = chrono::DateTime::parse_from_rfc3339(&body.scheduled_time)
        .map_err(|_| refusal(StatusCode::BAD_REQUEST, SCHEDULED_TIME_REFUSAL))?
        .with_timezone(&chrono::Utc);
    let fire = service(&state)?
        .fire(&tenant, parse_id(&id)?, scheduled_time)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "fire": fire_view(&fire) })))
}

#[cfg(test)]
mod tests {
    //! Driven through the daemon's real authentication stack against the
    //! real schedule service over the in-memory store, a Temporal stand-in
    //! that holds what it is given, and a run starter that records starts.

    use super::{schedules_router, SchedulesState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, serve, service_account,
    };
    use aegis_orchestrator_core::application::ports::{
        ScheduleEnginePort, TemporalScheduleDescription, TemporalScheduleSpec,
    };
    use aegis_orchestrator_core::application::schedule_service::ScheduleReader;
    use aegis_orchestrator_core::application::schedule_service::{
        ScheduleService, ScheduledRunPort,
    };
    use aegis_orchestrator_core::application::tool_invocation_service::{
        schedule_run_view, schedule_view,
    };
    use aegis_orchestrator_core::domain::agent::Agent;
    use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
    use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity};
    use aegis_orchestrator_core::domain::repository::{AgentRepository, WorkflowRepository};
    use aegis_orchestrator_core::domain::schedule::{Schedule, TargetKind, OWNER_REFUSAL};
    use aegis_orchestrator_core::domain::schedule::{ScheduleId, TARGET_KIND_REFUSAL};
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryWorkflowRepository,
    };
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
    use reqwest::Method;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const SCOPES: &str = "schedule:read schedule:write";

    #[derive(Default)]
    struct HeldSchedules(Mutex<HashMap<String, TemporalScheduleSpec>>);

    #[async_trait::async_trait]
    impl ScheduleEnginePort for HeldSchedules {
        async fn create_schedule(&self, spec: &TemporalScheduleSpec) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .insert(spec.temporal_schedule_id.clone(), spec.clone());
            Ok(())
        }
        async fn update_schedule(&self, spec: &TemporalScheduleSpec) -> anyhow::Result<()> {
            self.create_schedule(spec).await
        }
        async fn set_schedule_paused(&self, id: &str, paused: bool) -> anyhow::Result<()> {
            if let Some(spec) = self.0.lock().unwrap().get_mut(id) {
                spec.paused = paused;
            }
            Ok(())
        }
        async fn delete_schedule(&self, id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
        async fn describe_schedule(
            &self,
            id: &str,
        ) -> anyhow::Result<Option<TemporalScheduleDescription>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(id)
                .map(|spec| TemporalScheduleDescription {
                    paused: spec.paused,
                    next_action_times: vec![chrono::DateTime::parse_from_rfc3339(
                        "2026-10-09T15:00:00Z",
                    )
                    .unwrap()
                    .with_timezone(&chrono::Utc)],
                }))
        }
    }

    #[derive(Default)]
    struct RecordedStarts(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl ScheduledRunPort for RecordedStarts {
        async fn start(&self, _: &Schedule, owner: &UserIdentity) -> Result<ExecutionId, String> {
            self.0.lock().unwrap().push(owner.sub.clone());
            Ok(ExecutionId::new())
        }
        async fn run_status(
            &self,
            _: TargetKind,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Option<ExecutionStatus>> {
            Ok(Some(ExecutionStatus::Completed))
        }
    }

    struct Fixture {
        base: String,
        starts: Arc<RecordedStarts>,
        service: Arc<ScheduleService>,
        agents: Arc<InMemoryAgentRepository>,
        workflows: Arc<InMemoryWorkflowRepository>,
    }

    fn other_service_account() -> UserIdentity {
        UserIdentity {
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-sdk-python".into(),
            },
            ..service_account()
        }
    }

    async fn fixture() -> Fixture {
        let starts = Arc::new(RecordedStarts::default());
        let service = Arc::new(ScheduleService::new(
            Arc::new(InMemoryScheduleRepository::new()),
            Arc::new(HeldSchedules::default()),
            starts.clone(),
        ));
        let agents = Arc::new(InMemoryAgentRepository::new());
        let workflows = Arc::new(InMemoryWorkflowRepository::new());
        let base = serve(
            schedules_router(SchedulesState {
                service: Some(service.clone()),
                agents: agents.clone(),
                workflows: workflows.clone(),
            }),
            Some(identity_provider(&[
                ("owner", consumer("owner-sub"), SCOPES),
                ("stranger", consumer("stranger-sub"), SCOPES),
                ("operator", operator(AegisRole::Admin), SCOPES),
                ("worker", service_account(), ""),
                ("sdk", other_service_account(), SCOPES),
            ])),
            None,
        )
        .await;
        Fixture {
            base,
            starts,
            service,
            agents,
            workflows,
        }
    }

    fn draft() -> Value {
        json!({
            "name": "Weekday triage",
            "target_kind": "agent",
            "target": "mail-triage",
            "intent": "triage my inbox",
            "recurrence": { "cron": "0 15 * * 1-5", "timezone": "Europe/Berlin" },
        })
    }

    async fn create(f: &Fixture) -> String {
        let (status, body) = send(
            &f.base,
            &Method::POST,
            "/v1/schedules",
            &Some(draft()),
            Some("owner"),
        )
        .await;
        assert_eq!(status, 201, "{body}");
        body["schedule"]["id"].as_str().unwrap().to_string()
    }

    async fn fire(f: &Fixture, token: &str, id: &str, tenant: &str, time: &str) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/internal/schedules/{id}/fire", f.base))
            .bearer_auth(token)
            .header("X-Tenant-Id", tenant)
            .json(&json!({ "scheduled_time": time }))
            .send()
            .await
            .expect("loopback request");
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn owner_tenant() -> String {
        TenantId::for_consumer_user("owner-sub")
            .unwrap()
            .as_str()
            .to_string()
    }

    /// N10: the owner makes, reads, pauses, resumes and deletes; each
    /// answer carries `next_run_at` and `last_run`.
    #[tokio::test]
    async fn the_owner_manages_a_schedule_through_the_routes() {
        let f = fixture().await;
        let id = create(&f).await;
        let (status, body) = send(
            &f.base,
            &Method::GET,
            &format!("/v1/schedules/{id}"),
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let s = &body["schedule"];
        assert_eq!(
            (
                s["state"].clone(),
                s["next_run_at"].clone(),
                s["last_run"].clone(),
                s["recurrence"]["cron"].clone()
            ),
            (
                json!("active"),
                json!("2026-10-09T15:00:00Z"),
                Value::Null,
                json!("0 15 * * 1-5")
            ),
            "{body}"
        );
        for (path, state) in [("pause", "paused"), ("resume", "active")] {
            let (status, body) = send(
                &f.base,
                &Method::POST,
                &format!("/v1/schedules/{id}/{path}"),
                &None,
                Some("owner"),
            )
            .await;
            assert_eq!(
                (status, body["schedule"]["state"].clone()),
                (200, json!(state)),
                "{body}"
            );
        }
        let (status, body) = send(
            &f.base,
            &Method::PATCH,
            &format!("/v1/schedules/{id}"),
            &Some(json!({ "name": "Renamed" })),
            Some("owner"),
        )
        .await;
        assert_eq!(
            (status, body["schedule"]["name"].clone()),
            (200, json!("Renamed"))
        );
        let (status, _) = send(
            &f.base,
            &Method::DELETE,
            &format!("/v1/schedules/{id}"),
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 200);
        let (status, body) =
            send(&f.base, &Method::GET, "/v1/schedules", &None, Some("owner")).await;
        assert_eq!((status, body["count"].clone()), (200, json!(0)), "{body}");
    }

    /// N10: another person's schedule is 404 on every route; an operator
    /// reads the tenant's schedules and changes none; a service account is
    /// refused with N3's sentence.
    #[tokio::test]
    async fn every_route_answers_only_its_owner_and_the_operator_reads_only() {
        let f = fixture().await;
        let id = create(&f).await;
        let mut wrong = Vec::new();
        for (method, path) in [
            (Method::GET, format!("/v1/schedules/{id}")),
            (Method::PATCH, format!("/v1/schedules/{id}")),
            (Method::POST, format!("/v1/schedules/{id}/pause")),
            (Method::POST, format!("/v1/schedules/{id}/resume")),
            (Method::DELETE, format!("/v1/schedules/{id}")),
            (Method::GET, format!("/v1/schedules/{id}/runs")),
        ] {
            let body = (method == Method::PATCH).then(|| json!({ "name": "mine" }));
            let (status, answer) = send(&f.base, &method, &path, &body, Some("stranger")).await;
            if status != 404 {
                wrong.push(format!("stranger {method} {path}: {status} {answer}"));
            }
            let (status, answer) = send(&f.base, &method, &path, &body, Some("sdk")).await;
            if status != 403 || answer["error"] != OWNER_REFUSAL {
                wrong.push(format!(
                    "service account {method} {path}: {status} {answer}"
                ));
            }
        }
        let (status, answer) = send(
            &f.base,
            &Method::POST,
            &format!("/v1/schedules/{id}/pause"),
            &None,
            Some("operator"),
        )
        .await;
        if status != 403 {
            wrong.push(format!("operator pause: {status} {answer}"));
        }
        let (status, answer) = send(
            &f.base,
            &Method::POST,
            "/v1/schedules",
            &Some(draft()),
            Some("operator"),
        )
        .await;
        if status != 403 || answer["error"] != OWNER_REFUSAL {
            wrong.push(format!("operator create: {status} {answer}"));
        }
        let resp = reqwest::Client::new()
            .get(format!("{}/v1/schedules/{id}", f.base))
            .bearer_auth("operator")
            .header("X-Aegis-Tenant", owner_tenant())
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() != 200 {
            wrong.push(format!("operator read in the tenant: {}", resp.status()));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// N6: the fire route admits only the worker's service account, is
    /// idempotent per scheduled time, and its run is listed under the
    /// schedule's runs.
    #[tokio::test]
    async fn only_the_worker_fires_and_a_repeated_fire_starts_nothing_more() {
        let f = fixture().await;
        let id = create(&f).await;
        let time = "2026-10-09T15:00:00Z";
        let tenant = owner_tenant();
        let (status, _) = fire(&f, "sdk", &id, &tenant, time).await;
        assert_eq!(status, 403, "another service account fired");
        let (status, _) = fire(&f, "owner", &id, &tenant, time).await;
        assert_eq!(status, 403, "a person fired");
        let (status, first) = fire(&f, "worker", &id, &tenant, time).await;
        assert_eq!(status, 200, "{first}");
        let (status, again) = fire(&f, "worker", &id, &tenant, time).await;
        assert_eq!(
            (status, &again),
            (200, &first),
            "a repeated fire answered otherwise"
        );
        assert_eq!(
            f.starts.0.lock().unwrap().clone(),
            vec!["owner-sub".to_string()]
        );
        assert_eq!(first["fire"]["outcome"], "started");

        let (status, body) = send(
            &f.base,
            &Method::GET,
            &format!("/v1/schedules/{id}/runs?limit=5"),
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["runs"][0]["execution"]["kind"], "agent", "{body}");
        assert_eq!(
            body["runs"][0]["execution"]["status"], "completed",
            "{body}"
        );
        let (status, _) = send(
            &f.base,
            &Method::GET,
            &format!("/v1/schedules/{id}/runs?limit=101"),
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(status, 400, "a limit over 100 was accepted");
    }

    const AGENT_YAML: &str = "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: NAME\n  version: \"1.0.0\"\nspec:\n  runtime:\n    language: python\n    version: \"3.11\"\n  task:\n    instruction: Triage the inbox.\n";
    const WORKFLOW_YAML: &str = "apiVersion: 100monkeys.ai/v1\nkind: Workflow\nmetadata:\n  name: email-inbox-triage\n  version: \"1.0.0\"\nspec:\n  initial_state: done\n  states:\n    done:\n      kind: System\n      command: echo done\n      transitions: []\n";
    const DEFAULT: &str = "  default_schedule:\n    cron: \"0 15 * * 1-5\"\n    timezone: Europe/Berlin\n    jitter_seconds: 600\n";

    /// Store an agent named `name` in the owner's tenant, recommending
    /// N12's example timing when `recommends`.
    async fn agent_named(f: &Fixture, name: &str, recommends: bool) -> String {
        let yaml = format!(
            "{}{}",
            AGENT_YAML.replace("NAME", name),
            if recommends { DEFAULT } else { "" }
        );
        let agent = Agent::new(AgentManifestParser::parse_yaml(&yaml).expect("agent parses"));
        let tenant = TenantId::for_consumer_user("owner-sub").unwrap();
        f.agents.save_for_tenant(&tenant, &agent).await.unwrap();
        agent.id.0.to_string()
    }

    async fn defaults(f: &Fixture, query: &str) -> (u16, Value) {
        send(
            &f.base,
            &Method::GET,
            &format!("/v1/schedules/defaults?{query}"),
            &None,
            Some("owner"),
        )
        .await
    }

    /// N12: the defaults route answers the target's `spec.default_schedule`,
    /// for an agent by name or UUID and for a workflow, `null` for a target
    /// that recommends none, 404 for one that is not there, and 400 for a
    /// kind that is neither; it makes no schedule.
    #[tokio::test]
    async fn the_defaults_route_answers_the_targets_recommended_timing_or_null() {
        let f = fixture().await;
        let recommending = agent_named(&f, "mail-triage", true).await;
        agent_named(&f, "plain-agent", false).await;
        let workflow = WorkflowParser::parse_yaml(&format!("{WORKFLOW_YAML}{DEFAULT}"))
            .expect("workflow parses");
        f.workflows
            .save_for_tenant(
                &TenantId::for_consumer_user("owner-sub").unwrap(),
                &workflow,
            )
            .await
            .unwrap();
        let expected =
            json!({"cron": "0 15 * * 1-5", "timezone": "Europe/Berlin", "jitter_seconds": 600});

        let mut answered = Vec::new();
        for query in [
            "target_kind=agent&target=mail-triage".to_string(),
            format!("target_kind=agent&target={recommending}"),
            "target_kind=workflow&target=email-inbox-triage".to_string(),
            "target_kind=agent&target=plain-agent".to_string(),
            "target_kind=agent&target=nobody".to_string(),
            "target_kind=pipeline&target=mail-triage".to_string(),
        ] {
            let (status, body) = defaults(&f, &query).await;
            answered.push((status, body));
        }
        assert_eq!(
            answered,
            vec![
                (200, json!({ "default_schedule": expected })),
                (200, json!({ "default_schedule": expected })),
                (200, json!({ "default_schedule": expected })),
                (200, json!({ "default_schedule": null })),
                (404, json!({ "error": "Not found" })),
                (400, json!({ "error": TARGET_KIND_REFUSAL })),
            ],
            "the defaults route did not answer each target's recommended timing"
        );
        let (status, body) =
            send(&f.base, &Method::GET, "/v1/schedules", &None, Some("owner")).await;
        assert_eq!(
            (status, body["count"].clone()),
            (200, json!(0)),
            "a defaults read made a schedule: {body}"
        );
    }

    /// N11: the schedule tools answer their routes' shapes: the route's
    /// schedule and its runs are exactly what the tools' views make of the
    /// same schedule.
    #[tokio::test]
    async fn the_routes_answer_the_shapes_the_tools_answer() {
        let f = fixture().await;
        let id = create(&f).await;
        let (status, fired) =
            fire(&f, "worker", &id, &owner_tenant(), "2026-10-09T15:00:00Z").await;
        assert_eq!(status, 200, "{fired}");
        let reader = ScheduleReader::Owner {
            sub: "owner-sub".into(),
        };
        let schedule_id = ScheduleId::parse(&id).unwrap();
        let view = f.service.get(&reader, schedule_id).await.unwrap();
        let runs = f.service.runs(&reader, schedule_id, 20).await.unwrap();

        let (_, got) = send(
            &f.base,
            &Method::GET,
            &format!("/v1/schedules/{id}"),
            &None,
            Some("owner"),
        )
        .await;
        let (_, listed) = send(
            &f.base,
            &Method::GET,
            &format!("/v1/schedules/{id}/runs"),
            &None,
            Some("owner"),
        )
        .await;
        assert_eq!(
            (got["schedule"].clone(), listed["runs"].clone()),
            (
                schedule_view(&view),
                Value::Array(runs.iter().map(schedule_run_view).collect())
            ),
            "the route and the tool answer a schedule or its runs in different shapes"
        );
    }
}
