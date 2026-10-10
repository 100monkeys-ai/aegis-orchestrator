// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `aegis.schedule.*` tools (AEGIS ADR-139 N11): a person makes, reads,
//! changes, pauses, resumes, runs now and deletes their schedules, and reads
//! a schedule's runs, through the schedule service the `/v1/schedules` routes
//! use, each tool answering its route's body.
//!
//! `contexts` and `repositories` are reserved dispatch keys, in no
//! model-facing schema: the client writes them from the person's choices,
//! as it does for the starting tools (Zaru ADR-0055 D10). A `create` that
//! carries no `contexts` takes the call's `_meta.contexts` when its session
//! has no execution record (AEGIS ADR-132 S7), so a person's MCP client
//! with chosen contexts schedules with them. No schedule tool is gated:
//! each run's outward calls are (N9).

use super::*;
use crate::application::schedule_service::{
    ScheduleError, ScheduleReader, ScheduleRunView, ScheduleService, ScheduleView,
};
use crate::domain::execution::{ExecutionContexts, ServerChoice};
use crate::domain::iam::{IdentityKind, TenantScope, UserIdentity};
use crate::domain::schedule::{
    ScheduleDraft, ScheduleFire, ScheduleId, SchedulePatch, Timing, OVERLAP_REFUSAL, OWNER_REFUSAL,
    UNAVAILABLE_REFUSAL,
};
use serde_json::json;

/// The schedule tools.
pub(super) const SCHEDULE_TOOLS: [&str; 9] = [
    "aegis.schedule.create",
    "aegis.schedule.list",
    "aegis.schedule.get",
    "aegis.schedule.update",
    "aegis.schedule.pause",
    "aegis.schedule.resume",
    "aegis.schedule.run_now",
    "aegis.schedule.delete",
    "aegis.schedule.runs",
];

/// The refusal of a `limit` out of range, as the runs route answers it.
const LIMIT_REFUSAL: &str = "'limit' must be between 1 and 100.";
/// The refusal of a `schedule_id` that names no schedule of the caller's.
const NOT_FOUND: &str = "Not found";

/// A schedule as `GET /v1/schedules/{id}` answers it.
pub fn schedule_view(view: &ScheduleView) -> Value {
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
            "time": fire.scheduled_time.unwrap_or(fire.fired_at),
            "outcome": fire.outcome.as_str(),
            "execution_id": fire.execution_id.map(|e| e.to_string()),
        })),
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
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

/// One run as `GET /v1/schedules/{id}/runs` answers it.
pub fn schedule_run_view(run: &ScheduleRunView) -> Value {
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

/// The call's `_meta.contexts`, as the facade parsed it, written back as a
/// `contexts` value: a set of one as its binding id, a larger set as the
/// list, none as `null`.
fn contexts_value(contexts: &ExecutionContexts) -> Value {
    let mut map = serde_json::Map::new();
    for (server, choice) in contexts.servers() {
        let value = match choice {
            ServerChoice::NotGiven => continue,
            ServerChoice::None => Value::Null,
            ServerChoice::Bindings(ids) if ids.len() == 1 => json!(ids[0].0.to_string()),
            ServerChoice::Bindings(ids) => {
                Value::Array(ids.iter().map(|id| json!(id.0.to_string())).collect())
            }
        };
        map.insert(server.to_string(), value);
    }
    Value::Object(map)
}

impl ToolInvocationService {
    fn schedules(&self) -> Result<&Arc<ScheduleService>, String> {
        self.schedule_service
            .as_ref()
            .ok_or_else(|| UNAVAILABLE_REFUSAL.to_string())
    }

    /// Run one `aegis.schedule.*` tool for the call's person.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn invoke_aegis_schedule_tool(
        &self,
        tool_name: &str,
        args: &mut Value,
        execution_id: crate::domain::execution::ExecutionId,
        caller_identity: Option<&UserIdentity>,
        scope: &TenantScope,
        call_contexts: Option<&ExecutionContexts>,
        // The call's `_meta.profile` (AEGIS ADR-140 D12), which a `create`
        // carrying neither `profile` nor `contexts` takes before the
        // contexts it resolved into.
        call_profile: Option<uuid::Uuid>,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant = Self::enforce_tenant_arg(args, scope)?;
        if tool_name == "aegis.schedule.create"
            && args.get("contexts").is_none()
            && args.get("profile").is_none()
            && (call_contexts.is_some() || call_profile.is_some())
        {
            // S7: a session whose execution has a record ignores the
            // call's `_meta.contexts`, and its `_meta.profile`.
            let has_record = self
                .execution_service
                .get_execution_unscoped(execution_id)
                .await
                .is_ok();
            if !has_record {
                if let Some(profile) = call_profile {
                    args["profile"] = json!(profile.to_string());
                } else if let Some(contexts) = call_contexts {
                    args["contexts"] = contexts_value(contexts);
                }
            }
        }
        let answer = self
            .schedule_tool_answer(tool_name, args, caller_identity, &tenant)
            .await;
        Ok(ToolInvocationResult::Direct(match answer {
            Ok(body) => body,
            Err(sentence) => json!({ "tool": tool_name, "error": sentence }),
        }))
    }

    async fn schedule_tool_answer(
        &self,
        tool_name: &str,
        args: &Value,
        caller_identity: Option<&UserIdentity>,
        tenant: &TenantId,
    ) -> Result<Value, String> {
        let service = self.schedules()?;
        match tool_name {
            "aegis.schedule.create" => {
                let owner = person(caller_identity)?;
                let draft: ScheduleDraft = arguments(tool_name, args)?;
                let view = service
                    .create(owner, tenant, draft)
                    .await
                    .map_err(sentence)?;
                Ok(json!({ "schedule": schedule_view(&view) }))
            }
            "aegis.schedule.list" => {
                let reader = reader(caller_identity, tenant)?;
                let views = service.list(&reader).await.map_err(sentence)?;
                Ok(json!({
                    "count": views.len(),
                    "schedules": views.iter().map(schedule_view).collect::<Vec<_>>(),
                }))
            }
            "aegis.schedule.get" => {
                let reader = reader(caller_identity, tenant)?;
                let view = service
                    .get(&reader, schedule_id(args)?)
                    .await
                    .map_err(sentence)?;
                Ok(json!({ "schedule": schedule_view(&view) }))
            }
            "aegis.schedule.update" => {
                let owner = person(caller_identity)?;
                let id = schedule_id(args)?;
                let patch: SchedulePatch = arguments(tool_name, args)?;
                let view = service.update(owner, id, patch).await.map_err(sentence)?;
                Ok(json!({ "schedule": schedule_view(&view) }))
            }
            "aegis.schedule.pause" => {
                let owner = person(caller_identity)?;
                let view = service
                    .pause(owner, schedule_id(args)?)
                    .await
                    .map_err(sentence)?;
                Ok(json!({ "schedule": schedule_view(&view) }))
            }
            "aegis.schedule.resume" => {
                let owner = person(caller_identity)?;
                let view = service
                    .resume(owner, schedule_id(args)?)
                    .await
                    .map_err(sentence)?;
                Ok(json!({ "schedule": schedule_view(&view) }))
            }
            "aegis.schedule.run_now" => {
                let owner = person(caller_identity)?;
                let run = service
                    .run_now(tenant, owner, schedule_id(args)?)
                    .await
                    .map_err(sentence)?;
                Ok(json!({ "run": schedule_run_view(&run) }))
            }
            "aegis.schedule.delete" => {
                let owner = person(caller_identity)?;
                let id = schedule_id(args)?;
                service.delete(owner, id).await.map_err(sentence)?;
                Ok(json!({ "deleted": id.to_string() }))
            }
            "aegis.schedule.runs" => {
                let reader = reader(caller_identity, tenant)?;
                let id = schedule_id(args)?;
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => 20,
                    Some(value) => value
                        .as_i64()
                        .filter(|n| (1..=100).contains(n))
                        .ok_or_else(|| LIMIT_REFUSAL.to_string())?,
                };
                let runs = service
                    .runs(&reader, id, limit as usize)
                    .await
                    .map_err(sentence)?;
                Ok(json!({
                    "count": runs.len(),
                    "runs": runs.iter().map(schedule_run_view).collect::<Vec<_>>(),
                }))
            }
            other => Err(format!("Unknown schedule tool '{other}'")),
        }
    }
}

/// The sentence a refusal of the service is answered with, as its route
/// words it.
fn sentence(e: ScheduleError) -> String {
    match e {
        ScheduleError::Refused(sentence) | ScheduleError::Forbidden(sentence) => sentence,
        ScheduleError::NotFound => NOT_FOUND.to_string(),
        ScheduleError::Unavailable => UNAVAILABLE_REFUSAL.to_string(),
        ScheduleError::Overlap => OVERLAP_REFUSAL.to_string(),
        ScheduleError::Repository(detail) => {
            tracing::error!(error = %detail, "Schedule store failed");
            "Schedule store failed".to_string()
        }
    }
}

/// The person a change is made for; anyone else is refused (N3).
fn person(identity: Option<&UserIdentity>) -> Result<&UserIdentity, String> {
    match identity {
        Some(identity)
            if matches!(
                identity.identity_kind,
                IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. }
            ) =>
        {
            Ok(identity)
        }
        _ => Err(OWNER_REFUSAL.to_string()),
    }
}

/// Who reads: the owner, or an operator reading the call's tenant.
fn reader(identity: Option<&UserIdentity>, tenant: &TenantId) -> Result<ScheduleReader, String> {
    match identity.map(|i| (&i.identity_kind, i)) {
        Some((IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. }, identity)) => {
            Ok(ScheduleReader::Owner {
                sub: identity.sub.clone(),
            })
        }
        Some((IdentityKind::Operator { .. }, _)) => Ok(ScheduleReader::Operator {
            tenant: tenant.clone(),
        }),
        _ => Err(OWNER_REFUSAL.to_string()),
    }
}

fn schedule_id(args: &Value) -> Result<ScheduleId, String> {
    args.get("schedule_id")
        .and_then(Value::as_str)
        .and_then(ScheduleId::parse)
        .ok_or_else(|| NOT_FOUND.to_string())
}

/// The call's arguments read as the route reads its body.
fn arguments<T: serde::de::DeserializeOwned>(tool_name: &str, args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("{tool_name} arguments: {e}"))
}

#[cfg(test)]
mod tests {
    use super::super::approval_gate_tests::{harness, Harness, USER};
    use super::*;
    use crate::application::ports::{
        ScheduleEnginePort, TemporalScheduleDescription, TemporalScheduleSpec,
    };
    use crate::application::schedule_service::ScheduledRunPort;
    use crate::domain::execution::{ExecutionId, ExecutionStatus};
    use crate::domain::schedule::{Schedule, TargetKind, OVERLAP_REFUSAL, TIMING_REFUSAL};
    use crate::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
    const OTHER_BINDING: &str = "5a6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";

    /// Temporal's schedules, held as given.
    #[derive(Default)]
    struct HeldSchedules(StdMutex<HashMap<String, TemporalScheduleSpec>>);

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

    /// Starts nothing: no tool here fires a schedule.
    struct NoRuns;

    #[async_trait::async_trait]
    impl ScheduledRunPort for NoRuns {
        async fn start(&self, _: &Schedule, _: &UserIdentity) -> Result<ExecutionId, String> {
            Err("not exercised".into())
        }
        async fn run_status(
            &self,
            _: TargetKind,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Option<ExecutionStatus>> {
            Ok(None)
        }
    }

    /// The gate harness with a schedule service over the in-memory store.
    async fn with_schedules() -> (Harness, Arc<ScheduleService>) {
        let mut h = harness().await;
        let schedules = Arc::new(ScheduleService::new(
            Arc::new(InMemoryScheduleRepository::new()),
            Arc::new(HeldSchedules::default()),
            Arc::new(NoRuns),
        ));
        h.service = h.service.with_schedule_service(schedules.clone());
        (h, schedules)
    }

    /// Starts every run it is asked to; each reads `Running` while
    /// `running` is set, else `Completed`.
    #[derive(Default)]
    struct StartingRuns(StdMutex<Vec<String>>, std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl ScheduledRunPort for StartingRuns {
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
            Ok(Some(if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                ExecutionStatus::Running
            } else {
                ExecutionStatus::Completed
            }))
        }
    }

    fn draft(extra: Value) -> Value {
        let mut draft = json!({
            "name": "Weekday triage",
            "target_kind": "agent",
            "target": "mail-triage",
            "intent": "triage my inbox",
            "recurrence": { "cron": "0 15 * * 1-5", "timezone": "Europe/Berlin" },
        });
        for (key, value) in extra.as_object().unwrap() {
            draft[key] = value.clone();
        }
        draft
    }

    async fn stored(schedules: &ScheduleService, id: &str) -> Schedule {
        schedules
            .get(
                &ScheduleReader::Owner { sub: USER.into() },
                ScheduleId::parse(id).unwrap(),
            )
            .await
            .expect("the schedule is stored")
            .schedule
    }

    /// N11: each of the eight tools does what its route does, for the call's
    /// person, and answers the route's body.
    #[tokio::test]
    async fn the_eight_tools_make_read_change_pause_resume_and_delete_a_schedule() {
        let (h, schedules) = with_schedules().await;
        let token = h.session_for(ExecutionId::new()).await;
        let call = |tool: &'static str, args: Value| {
            let h = &h;
            let token = token.clone();
            async move {
                h.route_tool(&token, tool, args, None)
                    .await
                    .unwrap_or_else(|e| json!({ "refused": e.to_string() }))
            }
        };

        let made = call("aegis.schedule.create", draft(json!({}))).await;
        let id = made["schedule"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let view = |tool: &str, body: &Value| {
            (
                tool.to_string(),
                body["schedule"]["name"].clone(),
                body["schedule"]["state"].clone(),
                body["schedule"]["next_run_at"].clone(),
            )
        };
        let next = json!("2026-10-09T15:00:00Z");
        let mut answered = vec![view("create", &made)];
        let listed = call("aegis.schedule.list", json!({})).await;
        answered.push((
            "list".into(),
            listed["count"].clone(),
            listed["schedules"][0]["id"].clone(),
            Value::Null,
        ));
        answered.push(view(
            "get",
            &call("aegis.schedule.get", json!({ "schedule_id": id })).await,
        ));
        answered.push(view(
            "update",
            &call(
                "aegis.schedule.update",
                json!({ "schedule_id": id, "name": "Weekday triage, late" }),
            )
            .await,
        ));
        answered.push(view(
            "pause",
            &call("aegis.schedule.pause", json!({ "schedule_id": id })).await,
        ));
        answered.push(view(
            "resume",
            &call("aegis.schedule.resume", json!({ "schedule_id": id })).await,
        ));
        let runs = call("aegis.schedule.runs", json!({ "schedule_id": id })).await;
        answered.push((
            "runs".into(),
            runs["count"].clone(),
            runs["runs"].clone(),
            Value::Null,
        ));
        let deleted = call("aegis.schedule.delete", json!({ "schedule_id": id })).await;
        answered.push((
            "delete".into(),
            deleted["deleted"].clone(),
            Value::Null,
            Value::Null,
        ));
        let after = call("aegis.schedule.list", json!({})).await;
        answered.push((
            "list".into(),
            after["count"].clone(),
            Value::Null,
            Value::Null,
        ));

        assert_eq!(
            answered,
            vec![
                (
                    "create".into(),
                    json!("Weekday triage"),
                    json!("active"),
                    next.clone()
                ),
                ("list".into(), json!(1), json!(id), Value::Null),
                (
                    "get".into(),
                    json!("Weekday triage"),
                    json!("active"),
                    next.clone()
                ),
                (
                    "update".into(),
                    json!("Weekday triage, late"),
                    json!("active"),
                    next.clone()
                ),
                (
                    "pause".into(),
                    json!("Weekday triage, late"),
                    json!("paused"),
                    Value::Null
                ),
                (
                    "resume".into(),
                    json!("Weekday triage, late"),
                    json!("active"),
                    next.clone()
                ),
                ("runs".into(), json!(0), json!([]), Value::Null),
                ("delete".into(), json!(id), Value::Null, Value::Null),
                ("list".into(), json!(0), Value::Null, Value::Null),
            ],
            "the schedule tools did not each do and answer what their routes do"
        );
        let _ = schedules;
    }

    /// N11: `contexts` and `repositories` are kept from the call; a `create`
    /// with no `contexts` takes the call's `_meta.contexts` when its session
    /// has no execution record, and none when it has one (S7).
    #[tokio::test]
    async fn contexts_come_from_the_call_else_from_meta_contexts_on_a_session_with_no_record() {
        let (h, schedules) = with_schedules().await;
        let conversation = h.session_for(ExecutionId::new()).await;
        let agent_run = h.session_for(h.execution).await;
        let meta = Some(json!({ "contexts": { "imap": BINDING } }));
        let repositories = json!([{ "binding_id": OTHER_BINDING }]);

        let mut stored_contexts = Vec::new();
        for (token, args) in [
            (&conversation, draft(json!({}))),
            (
                &conversation,
                draft(
                    json!({ "contexts": { "imap": OTHER_BINDING }, "repositories": repositories }),
                ),
            ),
            (&agent_run, draft(json!({}))),
        ] {
            let made = h
                .route_tool(token, "aegis.schedule.create", args, meta.clone())
                .await
                .expect("create answers");
            let id = made["schedule"]["id"]
                .as_str()
                .unwrap_or_else(|| panic!("no schedule made: {made}"));
            let schedule = stored(&schedules, id).await;
            stored_contexts.push((schedule.contexts, schedule.repositories));
        }
        assert_eq!(
            stored_contexts,
            vec![
                (Some(json!({ "imap": BINDING })), None),
                (
                    Some(json!({ "imap": OTHER_BINDING })),
                    Some(repositories.clone())
                ),
                (None, None),
            ],
            "a schedule's contexts were not the call's, else its _meta.contexts on a session \
             with no record"
        );
    }

    /// A refusal of the service is answered with its sentence, and a node
    /// with no schedule service answers every tool as unavailable.
    #[tokio::test]
    async fn a_refusal_is_answered_with_its_sentence_and_no_service_is_unavailable() {
        let (h, _) = with_schedules().await;
        let token = h.session_for(ExecutionId::new()).await;
        let mut no_timing = draft(json!({}));
        no_timing.as_object_mut().unwrap().remove("recurrence");
        let refused = h
            .route_tool(&token, "aegis.schedule.create", no_timing, None)
            .await
            .expect("create answers");

        let bare = harness().await;
        let bare_token = bare.session_for(ExecutionId::new()).await;
        let unavailable = bare
            .route_tool(&bare_token, "aegis.schedule.list", json!({}), None)
            .await
            .expect("list answers");
        assert_eq!(
            (refused, unavailable),
            (
                json!({ "tool": "aegis.schedule.create", "error": TIMING_REFUSAL }),
                json!({ "tool": "aegis.schedule.list", "error": UNAVAILABLE_REFUSAL }),
            ),
            "a refusal was not answered with its sentence"
        );
    }

    /// Run now: `aegis.schedule.run_now` starts one run of the caller's own
    /// paused schedule and answers the run as the runs route lists it
    /// (outcome `started`, no scheduled time); pressed again while that run
    /// is still running it is refused with the overlap sentence.
    #[tokio::test]
    async fn run_now_starts_the_callers_schedule_and_answers_the_run_view() {
        let mut h = harness().await;
        let runs = Arc::new(StartingRuns::default());
        let schedules = Arc::new(ScheduleService::new(
            Arc::new(InMemoryScheduleRepository::new()),
            Arc::new(HeldSchedules::default()),
            runs.clone(),
        ));
        h.service = h.service.with_schedule_service(schedules.clone());
        let token = h.session_for(ExecutionId::new()).await;
        let made = h
            .route_tool(&token, "aegis.schedule.create", draft(json!({})), None)
            .await
            .expect("create answers");
        let id = made["schedule"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        h.route_tool(
            &token,
            "aegis.schedule.pause",
            json!({ "schedule_id": id }),
            None,
        )
        .await
        .expect("pause answers");

        let pressed = h
            .route_tool(
                &token,
                "aegis.schedule.run_now",
                json!({ "schedule_id": id }),
                None,
            )
            .await
            .unwrap_or_else(|e| json!({ "refused": e.to_string() }));
        let listed = schedules
            .runs(
                &ScheduleReader::Owner { sub: USER.into() },
                ScheduleId::parse(&id).unwrap_or_else(ScheduleId::new),
                20,
            )
            .await
            .unwrap_or_default();
        runs.1.store(true, std::sync::atomic::Ordering::SeqCst);
        let again = h
            .route_tool(
                &token,
                "aegis.schedule.run_now",
                json!({ "schedule_id": id }),
                None,
            )
            .await
            .unwrap_or_else(|e| json!({ "refused": e.to_string() }));
        assert_eq!(
            (
                pressed["run"]["outcome"].clone(),
                pressed["run"]["scheduled_time"].clone(),
                pressed.clone(),
                again,
                runs.0.lock().unwrap().clone(),
            ),
            (
                json!("started"),
                Value::Null,
                json!({ "run": listed.first().map(schedule_run_view) }),
                json!({ "tool": "aegis.schedule.run_now", "error": OVERLAP_REFUSAL }),
                vec![USER.to_string()],
            ),
            "aegis.schedule.run_now did not start the caller's schedule and answer its run"
        );
    }

    #[test]
    fn meta_contexts_are_written_back_as_a_contexts_value() {
        let one = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
        let two = "5a6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
        let given = json!({ "imap": one, "nuclear-notes": [one, two], "github": null });
        let contexts = ExecutionContexts::from_value(Some(&given));
        assert_eq!(
            contexts_value(&contexts),
            given,
            "the call's _meta.contexts was not written back as it was given"
        );
    }
}
