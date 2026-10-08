// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The schedule fire route (AEGIS ADR-139 N6)
//!
//! `POST /v1/internal/schedules/{id}/fire` with `{scheduled_time}`, called
//! by the Temporal worker when a Temporal Schedule acts. It admits only the
//! worker's service account and decides each scheduled time once.

use std::sync::Arc;

use aegis_orchestrator_core::application::schedule_service::{ScheduleError, ScheduleService};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::schedule::{
    ScheduleFire, ScheduleId, FIRE_CLIENT_ID, UNAVAILABLE_REFUSAL,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

type Refusal = (StatusCode, Json<Value>);

/// The refusal of a fire whose `scheduled_time` is not an RFC 3339 time.
const SCHEDULED_TIME_REFUSAL: &str = "'scheduled_time' must be an RFC 3339 time.";

/// State of the schedule sub-router.
#[derive(Clone)]
pub(crate) struct SchedulesState {
    /// `None` when the node has no schedule service: every route answers 503.
    pub(crate) service: Option<Arc<ScheduleService>>,
}

/// The worker's fire route, merged into the daemon router by
/// `router::create_router` beneath the same authentication layers as every
/// other route.
pub(crate) fn schedules_router(state: SchedulesState) -> Router {
    Router::new()
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
#[cfg(test)]
mod tests {
    //! Driven through the daemon's real authentication stack against the
    //! real schedule service over the in-memory store, a Temporal stand-in
    //! that holds what it is given, and a run starter that records starts.

    use super::{schedules_router, SchedulesState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, serve, service_account,
    };
    use aegis_orchestrator_core::application::ports::{
        ScheduleEnginePort, TemporalScheduleDescription, TemporalScheduleSpec,
    };
    use aegis_orchestrator_core::application::schedule_service::{
        ScheduleService, ScheduledRunPort,
    };
    use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
    use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
    use aegis_orchestrator_core::domain::schedule::{
        RecurrenceInput, Schedule, ScheduleDraft, TargetKind,
    };
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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
        service: Arc<ScheduleService>,
        starts: Arc<RecordedStarts>,
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
        let base = serve(
            schedules_router(SchedulesState {
                service: Some(service.clone()),
            }),
            Some(identity_provider(&[
                ("owner", consumer("owner-sub"), ""),
                ("worker", service_account(), ""),
                ("sdk", other_service_account(), ""),
            ])),
            None,
        )
        .await;
        Fixture {
            base,
            service,
            starts,
        }
    }

    async fn create(f: &Fixture) -> String {
        let owner = consumer("owner-sub");
        let view = f
            .service
            .create(
                &owner,
                &TenantId::for_consumer_user("owner-sub").unwrap(),
                ScheduleDraft {
                    name: Some("Weekday triage".into()),
                    target_kind: Some("agent".into()),
                    target: Some("mail-triage".into()),
                    recurrence: Some(RecurrenceInput {
                        cron: Some("0 15 * * 1-5".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .expect("create");
        view.schedule.id.to_string()
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

    /// N6: the fire route admits only the worker's service account and is
    /// idempotent per scheduled time.
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
    }
}
