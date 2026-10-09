// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedules (AEGIS ADR-139 N1 to N8, N12)
//!
//! The schedule service against a Temporal schedule stand-in that records
//! every call and can be made to fail, over the in-memory store; the run
//! starter against recording start services; the two manifest parsers; and,
//! when `AEGIS_TEST_POSTGRES_URL` names a database, migration 048 applied
//! twice and the Postgres store.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::ports::{
    ScheduleEnginePort, TemporalScheduleDescription, TemporalScheduleSpec,
};
use aegis_orchestrator_core::application::schedule_service::{
    ScheduleError, ScheduleReader, ScheduleService, ScheduledRunPort, ServiceRunStarter,
};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StartWorkflowExecutionRequest, StartWorkflowExecutionUseCase, StartedWorkflowExecution,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentScope};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, ExecutionStatus, Iteration,
};
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::repository::AgentVersion;
use aegis_orchestrator_core::domain::schedule::{
    FireOutcome, RecurrenceInput, Schedule, ScheduleDraft, ScheduleId, SchedulePatch,
    ScheduleRepository, ScheduleState, TargetKind, Timing, AT_REFUSAL, CRON_REFUSAL, OWNER_REFUSAL,
    SPEC_SCHEDULE_REFUSAL, TIMEZONE_REFUSAL, UNAVAILABLE_REFUSAL,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;
use aegis_orchestrator_core::infrastructure::event_bus::DomainEvent;
use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use futures::Stream;
use serde_json::json;

const BINDING: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";

// ── Identities ───────────────────────────────────────────────────────────────

fn consumer(sub: &str) -> UserIdentity {
    UserIdentity {
        sub: sub.into(),
        realm_slug: "zaru-consumer".into(),
        email: Some(format!("{sub}@example.test")),
        email_verified: true,
        name: None,
        identity_kind: IdentityKind::ConsumerUser {
            zaru_tier: ZaruTier::Pro,
            tenant_id: TenantId::for_consumer_user(sub).unwrap(),
        },
    }
}

fn tenant_of(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).unwrap()
}

fn operator() -> UserIdentity {
    UserIdentity {
        sub: "op".into(),
        realm_slug: "aegis-system".into(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::Operator {
            aegis_role: AegisRole::Admin,
        },
    }
}

fn service_account() -> UserIdentity {
    UserIdentity {
        sub: "svc".into(),
        realm_slug: "aegis-system".into(),
        email: None,
        email_verified: false,
        name: None,
        identity_kind: IdentityKind::ServiceAccount {
            client_id: "aegis-temporal-worker".into(),
        },
    }
}

// ── The Temporal schedule stand-in ──────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum EngineCall {
    Create(TemporalScheduleSpec),
    Update(TemporalScheduleSpec),
    Paused(String, bool),
    Delete(String),
}

/// Records every call; holds the schedules it was given; refuses every
/// call while `failing` is set. On create it reads the store, so a test
/// can see whether the row was written first.
#[derive(Default)]
struct StandInEngine {
    calls: Mutex<Vec<EngineCall>>,
    held: Mutex<HashMap<String, TemporalScheduleSpec>>,
    failing: Mutex<bool>,
    next_action: Mutex<Option<DateTime<Utc>>>,
    store: Mutex<Option<Arc<InMemoryScheduleRepository>>>,
    row_present_at_create: Mutex<Vec<bool>>,
}

impl StandInEngine {
    fn calls(&self) -> Vec<EngineCall> {
        self.calls.lock().unwrap().clone()
    }
    fn fail(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }
    fn refuse_if_failing(&self) -> Result<()> {
        if *self.failing.lock().unwrap() {
            anyhow::bail!("connection refused");
        }
        Ok(())
    }
}

#[async_trait]
impl ScheduleEnginePort for StandInEngine {
    async fn create_schedule(&self, spec: &TemporalScheduleSpec) -> Result<()> {
        let store = self.store.lock().unwrap().clone();
        if let Some(store) = store {
            let id = ScheduleId::parse(&spec.schedule_id).unwrap();
            let present = store.get(id).await.unwrap().is_some();
            self.row_present_at_create.lock().unwrap().push(present);
        }
        self.calls
            .lock()
            .unwrap()
            .push(EngineCall::Create(spec.clone()));
        self.refuse_if_failing()?;
        self.held
            .lock()
            .unwrap()
            .insert(spec.temporal_schedule_id.clone(), spec.clone());
        Ok(())
    }
    async fn update_schedule(&self, spec: &TemporalScheduleSpec) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(EngineCall::Update(spec.clone()));
        self.refuse_if_failing()?;
        self.held
            .lock()
            .unwrap()
            .insert(spec.temporal_schedule_id.clone(), spec.clone());
        Ok(())
    }
    async fn set_schedule_paused(&self, id: &str, paused: bool) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(EngineCall::Paused(id.to_string(), paused));
        self.refuse_if_failing()?;
        if let Some(spec) = self.held.lock().unwrap().get_mut(id) {
            spec.paused = paused;
        }
        Ok(())
    }
    async fn delete_schedule(&self, id: &str) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(EngineCall::Delete(id.to_string()));
        self.refuse_if_failing()?;
        self.held.lock().unwrap().remove(id);
        Ok(())
    }
    async fn describe_schedule(&self, id: &str) -> Result<Option<TemporalScheduleDescription>> {
        self.refuse_if_failing()?;
        Ok(self
            .held
            .lock()
            .unwrap()
            .get(id)
            .map(|spec| TemporalScheduleDescription {
                paused: spec.paused,
                next_action_times: self.next_action.lock().unwrap().iter().copied().collect(),
            }))
    }
}

// ── A run starter stand-in, for the fire's own decisions ────────────────────

#[derive(Default)]
struct StandInRuns {
    started: Mutex<Vec<(ScheduleId, String)>>,
    refuse_with: Mutex<Option<String>>,
    status: Mutex<HashMap<ExecutionId, ExecutionStatus>>,
}

#[async_trait]
impl ScheduledRunPort for StandInRuns {
    async fn start(
        &self,
        schedule: &Schedule,
        owner: &UserIdentity,
    ) -> std::result::Result<ExecutionId, String> {
        if let Some(sentence) = self.refuse_with.lock().unwrap().clone() {
            return Err(sentence);
        }
        self.started
            .lock()
            .unwrap()
            .push((schedule.id, owner.sub.clone()));
        let id = ExecutionId::new();
        self.status
            .lock()
            .unwrap()
            .insert(id, ExecutionStatus::Running);
        Ok(id)
    }
    async fn run_status(
        &self,
        _: TargetKind,
        _: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<ExecutionStatus>> {
        Ok(self.status.lock().unwrap().get(&id).cloned())
    }
}

struct Fixture {
    service: ScheduleService,
    store: Arc<InMemoryScheduleRepository>,
    engine: Arc<StandInEngine>,
    runs: Arc<StandInRuns>,
}

fn fixture() -> Fixture {
    let store = Arc::new(InMemoryScheduleRepository::new());
    let engine = Arc::new(StandInEngine::default());
    *engine.store.lock().unwrap() = Some(store.clone());
    let runs = Arc::new(StandInRuns::default());
    Fixture {
        service: ScheduleService::new(store.clone(), engine.clone(), runs.clone()),
        store,
        engine,
        runs,
    }
}

fn recurring(name: &str, cron: &str) -> ScheduleDraft {
    ScheduleDraft {
        name: Some(name.into()),
        target_kind: Some("agent".into()),
        target: Some("mail-triage".into()),
        intent: Some("triage my inbox".into()),
        input: Some(json!({"folder": "inbox"})),
        contexts: Some(json!({ "imap": BINDING })),
        recurrence: Some(RecurrenceInput {
            cron: Some(cron.into()),
            timezone: Some("Europe/Berlin".into()),
            jitter_seconds: Some(120),
        }),
        ..Default::default()
    }
}

fn once(name: &str, at: DateTime<Utc>) -> ScheduleDraft {
    ScheduleDraft {
        name: Some(name.into()),
        target_kind: Some("workflow".into()),
        target: Some("email-inbox-triage".into()),
        at: Some(at.to_rfc3339()),
        ..Default::default()
    }
}

async fn create(f: &Fixture, sub: &str, draft: ScheduleDraft) -> Schedule {
    f.service
        .create(&consumer(sub), &tenant_of(sub), draft)
        .await
        .expect("create")
        .schedule
}

fn at(minutes: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-09T15:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::minutes(minutes)
}

// ── N1 to N5: create, the Temporal Schedule, the refusals ───────────────────

/// N5: the row is written first and the Temporal Schedule second, holding
/// the schedule's id, tenant and recurrence, active.
#[tokio::test]
async fn create_writes_the_row_then_the_temporal_schedule() {
    let f = fixture();
    let schedule = create(&f, "owner", recurring("Weekday triage", "0 15 * * 1-5")).await;
    assert_eq!(
        f.engine.calls(),
        vec![EngineCall::Create(TemporalScheduleSpec {
            temporal_schedule_id: format!("aegis-schedule-{}", schedule.id),
            schedule_id: schedule.id.to_string(),
            tenant_id: tenant_of("owner").as_str().to_string(),
            timing: schedule.timing.clone(),
            paused: false,
        })],
        "the Temporal Schedule made for the schedule"
    );
    assert_eq!(
        *f.engine.row_present_at_create.lock().unwrap(),
        vec![true],
        "the row was not stored before the Temporal Schedule was made"
    );
    assert_eq!(schedule.state, ScheduleState::Active);
    assert_eq!(schedule.owner.sub, "owner");
    assert_eq!(schedule.owner.zaru_tier.as_deref(), Some("pro"));
}

/// N5: when Temporal refuses, the row is taken back and the caller is told
/// nothing was saved.
#[tokio::test]
async fn a_temporal_failure_answers_503_and_leaves_no_row() {
    let f = fixture();
    f.engine.fail(true);
    let refused = f
        .service
        .create(
            &consumer("owner"),
            &tenant_of("owner"),
            recurring("x", "0 15 * * *"),
        )
        .await
        .expect_err("created while Temporal was down");
    assert_eq!(refused, ScheduleError::Unavailable);
    assert_eq!(refused.to_string(), UNAVAILABLE_REFUSAL);
    assert!(
        f.store.list_for_owner("owner").await.unwrap().is_empty(),
        "a row was left behind after Temporal refused"
    );
}

/// N2: every refusal of the timing, byte for byte, before anything is
/// stored or reaches Temporal.
#[tokio::test]
async fn the_timing_is_refused_in_n2s_words_before_anything_is_stored() {
    let f = fixture();
    let mut cases: Vec<(ScheduleDraft, String)> = Vec::new();
    let mut d = recurring("x", "0 15 * *");
    cases.push((d.clone(), CRON_REFUSAL.to_string()));
    d = recurring("x", "0 15 * * *");
    d.recurrence.as_mut().unwrap().timezone = Some("Atlantis/Lost".into());
    cases.push((d.clone(), TIMEZONE_REFUSAL.to_string()));
    d = recurring("x", "*/4 * * * *");
    cases.push((
        d.clone(),
        "A schedule runs at most once every 5 minutes.".to_string(),
    ));
    d = recurring("x", "0 15 * * *");
    d.recurrence.as_mut().unwrap().jitter_seconds = Some(3_601);
    cases.push((
        d.clone(),
        "'jitter_seconds' must be between 0 and 3600.".to_string(),
    ));
    d.recurrence.as_mut().unwrap().jitter_seconds = Some(-1);
    cases.push((
        d.clone(),
        "'jitter_seconds' must be between 0 and 3600.".to_string(),
    ));
    cases.push((
        once("x", Utc::now() + Duration::seconds(30)),
        AT_REFUSAL.to_string(),
    ));
    cases.push((
        once("x", Utc::now() + Duration::days(367)),
        AT_REFUSAL.to_string(),
    ));
    let mut both = once("x", Utc::now() + Duration::hours(1));
    both.recurrence = recurring("x", "0 15 * * *").recurrence;
    cases.push((
        both,
        "Give exactly one of 'at' or 'recurrence'.".to_string(),
    ));

    let mut wrong = Vec::new();
    for (draft, sentence) in cases {
        match f
            .service
            .create(&consumer("owner"), &tenant_of("owner"), draft)
            .await
        {
            Err(ScheduleError::Refused(said)) if said == sentence => {}
            other => wrong.push(format!("expected {sentence:?}, got {other:?}")),
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert!(
        f.engine.calls().is_empty(),
        "a refused schedule reached Temporal"
    );
    assert!(f.store.list_for_owner("owner").await.unwrap().is_empty());
}

/// N3: a service account or an operator acting for no person is refused.
#[tokio::test]
async fn only_a_person_may_own_a_schedule() {
    let f = fixture();
    for identity in [service_account(), operator()] {
        let refused = f
            .service
            .create(&identity, &TenantId::system(), recurring("x", "0 15 * * *"))
            .await
            .expect_err("a non-person made a schedule");
        assert_eq!(
            refused,
            ScheduleError::Refused(OWNER_REFUSAL.to_string()),
            "{:?}",
            identity.identity_kind
        );
    }
}

/// N4: the 26th schedule that is not deleted is refused; deleting one makes
/// room.
#[tokio::test]
async fn a_person_holds_at_most_25_schedules() {
    let f = fixture();
    let mut first = None;
    for n in 0..25 {
        let s = create(&f, "owner", recurring(&format!("s{n}"), "0 15 * * *")).await;
        first.get_or_insert(s.id);
    }
    let refused = f
        .service
        .create(
            &consumer("owner"),
            &tenant_of("owner"),
            recurring("s25", "0 15 * * *"),
        )
        .await
        .expect_err("a 26th schedule was made");
    assert_eq!(
        refused,
        ScheduleError::Refused("You have 25 schedules; delete one to make another.".to_string())
    );
    f.service
        .delete(&consumer("owner"), first.unwrap())
        .await
        .expect("delete");
    create(&f, "owner", recurring("s25", "0 15 * * *")).await;
}

/// N5: at boot every active or paused schedule whose Temporal Schedule is
/// missing is made again, paused as it was; a present one and a completed
/// one are left alone.
#[tokio::test]
async fn boot_recreates_each_missing_temporal_schedule() {
    let f = fixture();
    let kept = create(&f, "owner", recurring("kept", "0 15 * * *")).await;
    let lost = create(&f, "owner", recurring("lost", "0 16 * * *")).await;
    let paused = create(&f, "owner", recurring("paused", "0 17 * * *")).await;
    f.service
        .pause(&consumer("owner"), paused.id)
        .await
        .expect("pause");
    let mut done = create(&f, "owner", once("done", Utc::now() + Duration::hours(2))).await;
    done.state = ScheduleState::Completed;
    f.store.update(&done).await.unwrap();
    {
        let mut held = f.engine.held.lock().unwrap();
        held.remove(&lost.temporal_schedule_id);
        held.remove(&paused.temporal_schedule_id);
        held.remove(&done.temporal_schedule_id);
    }
    f.engine.calls.lock().unwrap().clear();

    let made = f.service.recreate_missing().await.expect("recreate");

    let mut created: Vec<(String, bool)> = f
        .engine
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            EngineCall::Create(spec) => Some((spec.temporal_schedule_id, spec.paused)),
            _ => None,
        })
        .collect();
    created.sort();
    let mut expected = vec![
        (lost.temporal_schedule_id.clone(), false),
        (paused.temporal_schedule_id.clone(), true),
    ];
    expected.sort();
    assert_eq!(
        (made, created),
        (2, expected),
        "the boot re-creation (kept {} untouched)",
        kept.id
    );
}

// ── N10: change, pause, resume, delete, and what a schedule answers ─────────

#[tokio::test]
async fn pause_resume_update_and_delete_follow_temporal() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    let owner = consumer("owner");
    f.engine.calls.lock().unwrap().clear();

    let paused = f.service.pause(&owner, s.id).await.expect("pause");
    assert_eq!(paused.schedule.state, ScheduleState::Paused);
    assert_eq!(
        paused.next_run_at, None,
        "a paused schedule shows a next run"
    );
    let resumed = f.service.resume(&owner, s.id).await.expect("resume");
    assert_eq!(resumed.schedule.state, ScheduleState::Active);
    let updated = f
        .service
        .update(
            &owner,
            s.id,
            SchedulePatch {
                name: Some("Renamed".into()),
                recurrence: Some(RecurrenceInput {
                    cron: Some("30 9 * * 1".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .expect("update");
    assert_eq!(updated.schedule.name, "Renamed");
    f.service.delete(&owner, s.id).await.expect("delete");

    let calls = f.engine.calls();
    assert_eq!(
        calls[..2],
        [
            EngineCall::Paused(s.temporal_schedule_id.clone(), true),
            EngineCall::Paused(s.temporal_schedule_id.clone(), false),
        ]
    );
    match &calls[2] {
        EngineCall::Update(spec) => assert!(
            matches!(&spec.timing, Timing::Recurrence(r) if r.cron == "30 9 * * 1" && r.timezone == "UTC"),
            "{spec:?}"
        ),
        other => panic!("expected an update, got {other:?}"),
    }
    assert_eq!(calls[3], EngineCall::Delete(s.temporal_schedule_id.clone()));
    let row = f.store.get(s.id).await.unwrap().expect("the row stays");
    assert!(
        row.deleted_at.is_some(),
        "the deleted row has no deleted_at"
    );
    assert!(f
        .service
        .list(&ScheduleReader::Owner {
            sub: "owner".into()
        })
        .await
        .unwrap()
        .is_empty());
}

/// A change Temporal refuses puts the row back and saves nothing.
#[tokio::test]
async fn a_pause_temporal_refuses_leaves_the_schedule_active() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    f.engine.fail(true);
    let refused = f
        .service
        .pause(&consumer("owner"), s.id)
        .await
        .expect_err("paused while Temporal was down");
    assert_eq!(refused, ScheduleError::Unavailable);
    assert_eq!(
        f.store.get(s.id).await.unwrap().unwrap().state,
        ScheduleState::Active
    );
}

/// N10: another person's schedule is not found; an operator reads the
/// tenant's schedules.
#[tokio::test]
async fn a_schedule_answers_only_its_owner_and_the_tenants_operator() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    let stranger = consumer("stranger");
    assert_eq!(
        f.service
            .get(
                &ScheduleReader::Owner {
                    sub: "stranger".into()
                },
                s.id
            )
            .await
            .unwrap_err(),
        ScheduleError::NotFound
    );
    assert_eq!(
        f.service.pause(&stranger, s.id).await.unwrap_err(),
        ScheduleError::NotFound
    );
    assert_eq!(
        f.service.delete(&stranger, s.id).await.unwrap_err(),
        ScheduleError::NotFound
    );
    let read = f
        .service
        .get(
            &ScheduleReader::Operator {
                tenant: tenant_of("owner"),
            },
            s.id,
        )
        .await
        .expect("operator read");
    assert_eq!(read.schedule.id, s.id);
    assert_eq!(
        f.service
            .get(
                &ScheduleReader::Operator {
                    tenant: TenantId::system()
                },
                s.id
            )
            .await
            .unwrap_err(),
        ScheduleError::NotFound
    );
}

/// N10: `next_run_at` is Temporal's next action time; `last_run` is the
/// newest fire.
#[tokio::test]
async fn a_schedule_answers_its_next_run_and_its_last_run() {
    let f = fixture();
    let next = at(60);
    *f.engine.next_action.lock().unwrap() = Some(next);
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    f.service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .expect("fire");
    let view = f
        .service
        .get(
            &ScheduleReader::Owner {
                sub: "owner".into(),
            },
            s.id,
        )
        .await
        .unwrap();
    assert_eq!(view.next_run_at, Some(next));
    let last = view.last_run.expect("no last run");
    assert_eq!(
        (last.scheduled_time, last.outcome),
        (at(0), FireOutcome::Started)
    );
}

// ── N6, N7: the fire ─────────────────────────────────────────────────────────

/// N6: a repeated fire of one scheduled time answers the first one's row
/// and starts nothing more.
#[tokio::test]
async fn a_fire_is_decided_once_per_scheduled_time() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    let first = f
        .service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .unwrap();
    let again = f
        .service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .unwrap();
    assert_eq!(first, again, "the repeated fire answered another row");
    assert_eq!(f.runs.started.lock().unwrap().len(), 1, "two runs started");
    assert_eq!(first.outcome, FireOutcome::Started);
}

/// A fire in another tenant than the schedule's is not found.
#[tokio::test]
async fn a_fire_in_another_tenant_is_not_found() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    assert_eq!(
        f.service
            .fire(&tenant_of("stranger"), s.id, at(0))
            .await
            .unwrap_err(),
        ScheduleError::NotFound
    );
}

/// N7: a schedule not active answers `skipped_paused`.
#[tokio::test]
async fn a_paused_schedules_fire_is_skipped() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    f.service.pause(&consumer("owner"), s.id).await.unwrap();
    let fire = f
        .service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .unwrap();
    assert_eq!(fire.outcome, FireOutcome::SkippedPaused);
    assert!(f.runs.started.lock().unwrap().is_empty());
}

/// N7: while the last run is still running the fire answers
/// `skipped_overlap`; once it has ended the next fire starts a run.
#[tokio::test]
async fn a_fire_while_the_last_run_runs_is_skipped() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    let first = f
        .service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .unwrap();
    let second = f
        .service
        .fire(&tenant_of("owner"), s.id, at(30))
        .await
        .unwrap();
    assert_eq!(second.outcome, FireOutcome::SkippedOverlap);
    f.runs
        .status
        .lock()
        .unwrap()
        .insert(first.execution_id.unwrap(), ExecutionStatus::Completed);
    let third = f
        .service
        .fire(&tenant_of("owner"), s.id, at(60))
        .await
        .unwrap();
    assert_eq!(third.outcome, FireOutcome::Started);
}

/// N7: a refused start is recorded with its sentence; after three in a row
/// the schedule pauses itself, in Temporal too, and says why.
#[tokio::test]
async fn three_refused_fires_in_a_row_pause_the_schedule() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    let sentence = "agent 'mail-triage' requires a imap context and none was chosen";
    *f.runs.refuse_with.lock().unwrap() = Some(sentence.to_string());
    let mut outcomes = Vec::new();
    for n in 0..3 {
        let fire = f
            .service
            .fire(&tenant_of("owner"), s.id, at(n * 60))
            .await
            .unwrap();
        outcomes.push((fire.outcome, fire.detail));
        let state = f.store.get(s.id).await.unwrap().unwrap().state;
        if n < 2 {
            assert_eq!(
                state,
                ScheduleState::Active,
                "paused after {} refusals",
                n + 1
            );
        }
    }
    assert_eq!(
        outcomes,
        vec![(FireOutcome::Refused, Some(sentence.to_string())); 3]
    );
    let row = f.store.get(s.id).await.unwrap().unwrap();
    assert_eq!(
        (row.state, row.paused_reason.as_deref()),
        (
            ScheduleState::Paused,
            Some("Paused after three runs could not start: agent 'mail-triage' requires a imap context and none was chosen.")
        )
    );
    assert!(f
        .engine
        .calls()
        .contains(&EngineCall::Paused(s.temporal_schedule_id.clone(), true)));
}

/// N2: a one-time schedule is completed once it has fired.
#[tokio::test]
async fn a_one_time_schedule_completes_after_its_fire() {
    let f = fixture();
    let s = create(&f, "owner", once("once", Utc::now() + Duration::hours(1))).await;
    f.service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .unwrap();
    assert_eq!(
        f.store.get(s.id).await.unwrap().unwrap().state,
        ScheduleState::Completed
    );
}

/// N7: a started run's record carries the schedule.
#[tokio::test]
async fn a_started_run_is_bound_to_its_schedule() {
    let f = fixture();
    let s = create(&f, "owner", recurring("x", "0 15 * * *")).await;
    let fire = f
        .service
        .fire(&tenant_of("owner"), s.id, at(0))
        .await
        .unwrap();
    assert_eq!(
        f.store.bound_schedule(fire.execution_id.unwrap()).await,
        Some((TargetKind::Agent, s.id))
    );
}

// ── N7, N8: the run starter, through the start services ─────────────────────

#[derive(Default)]
struct RecordingExecutions {
    started: Mutex<Vec<(AgentId, ExecutionInput, String, Option<UserIdentity>)>>,
}

#[async_trait]
impl ExecutionService for RecordingExecutions {
    async fn start_execution(
        &self,
        agent_id: AgentId,
        input: ExecutionInput,
        security_context_name: String,
        identity: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        self.started.lock().unwrap().push((
            agent_id,
            input,
            security_context_name,
            identity.cloned(),
        ));
        Ok(ExecutionId::new())
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
    async fn get_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<Execution> {
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

/// One agent, `mail-triage`, visible in every tenant.
struct OneAgent(AgentId);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: AgentScope,
        _: Option<&UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        anyhow::bail!("not exercised")
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        name: &str,
    ) -> Result<Option<AgentId>> {
        Ok((name == "mail-triage").then_some(self.0))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

#[derive(Default)]
struct RecordingWorkflowStarts {
    started: Mutex<
        Vec<(
            TenantId,
            StartWorkflowExecutionRequest,
            Option<UserIdentity>,
        )>,
    >,
}

#[async_trait]
impl StartWorkflowExecutionUseCase for RecordingWorkflowStarts {
    async fn start_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        request: StartWorkflowExecutionRequest,
        identity: Option<&UserIdentity>,
    ) -> Result<StartedWorkflowExecution> {
        let workflow_id = request.workflow_id.clone();
        self.started
            .lock()
            .unwrap()
            .push((tenant_id.clone(), request, identity.cloned()));
        Ok(StartedWorkflowExecution {
            execution_id: uuid::Uuid::new_v4().to_string(),
            workflow_id,
            temporal_run_id: "run".into(),
            status: "running".into(),
            started_at: Utc::now(),
        })
    }
}

fn scheduled(kind: &str, target: &str) -> Schedule {
    Schedule::create(
        ScheduleDraft {
            name: Some("run".into()),
            target_kind: Some(kind.into()),
            target: Some(target.into()),
            intent: Some("triage".into()),
            input: Some(json!({"folder": "inbox", "conversation_id": "c-1"})),
            contexts: Some(json!({ "imap": BINDING })),
            repositories: Some(json!([{ "binding_id": BINDING, "branch": "work" }])),
            recurrence: Some(RecurrenceInput {
                cron: Some("0 15 * * *".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        &consumer("owner"),
        tenant_of("owner"),
        Utc::now(),
    )
    .unwrap()
}

/// N7, N8: an agent's run starts as the owner, in the owner's tenant, with
/// the schedule's intent, input, contexts and repositories, no
/// conversation, under the starting tools' security context.
#[tokio::test]
async fn an_agent_run_starts_as_the_owner_with_the_schedules_choices() {
    let executions = Arc::new(RecordingExecutions::default());
    let agent = AgentId::new();
    let starter = ServiceRunStarter::new(executions.clone(), Arc::new(OneAgent(agent)), None, None);
    let schedule = scheduled("agent", "mail-triage");
    let owner = schedule.owner.to_identity(&schedule.tenant_id).unwrap();
    starter.start(&schedule, &owner).await.expect("started");

    let started = executions.started.lock().unwrap();
    let (agent_id, input, context, identity) = &started[0];
    let identity = identity.as_ref().expect("no identity");
    assert_eq!(*agent_id, agent);
    assert_eq!(context, "aegis-system-agent-runtime");
    assert_eq!(identity.sub, "owner");
    assert!(matches!(
        &identity.identity_kind,
        IdentityKind::ConsumerUser { tenant_id, zaru_tier: ZaruTier::Pro } if *tenant_id == tenant_of("owner")
    ));
    assert_eq!(input.intent.as_deref(), Some("triage"));
    assert_eq!(
        input.input,
        json!({
            "folder": "inbox",
            "contexts": { "imap": BINDING },
            "repositories": [{ "binding_id": BINDING, "branch": "work" }],
            "tenant_id": tenant_of("owner").as_str(),
        }),
        "the run's input"
    );
}

/// N7: a target that does not exist refuses the start with the starting
/// tool's sentence.
#[tokio::test]
async fn an_unknown_agent_refuses_the_start() {
    let starter = ServiceRunStarter::new(
        Arc::new(RecordingExecutions::default()),
        Arc::new(OneAgent(AgentId::new())),
        None,
        None,
    );
    let schedule = scheduled("agent", "gone-agent");
    let owner = schedule.owner.to_identity(&schedule.tenant_id).unwrap();
    assert_eq!(
        starter.start(&schedule, &owner).await,
        Err("Agent 'gone-agent' not found".to_string())
    );
}

/// N7, N8: a workflow's run starts through the workflow start service as
/// the owner, in the schedule's tenant.
#[tokio::test]
async fn a_workflow_run_starts_as_the_owner_with_the_schedules_choices() {
    let workflows = Arc::new(RecordingWorkflowStarts::default());
    let starter = ServiceRunStarter::new(
        Arc::new(RecordingExecutions::default()),
        Arc::new(OneAgent(AgentId::new())),
        Some(workflows.clone()),
        None,
    );
    let schedule = scheduled("workflow", "email-inbox-triage");
    let owner = schedule.owner.to_identity(&schedule.tenant_id).unwrap();
    starter.start(&schedule, &owner).await.expect("started");
    let started = workflows.started.lock().unwrap();
    let (tenant, request, identity) = &started[0];
    assert_eq!(*tenant, tenant_of("owner"));
    assert_eq!(identity.as_ref().map(|i| i.sub.as_str()), Some("owner"));
    assert_eq!(
        (
            request.workflow_id.as_str(),
            request.security_context_name.as_deref(),
            request.intent.as_deref(),
            request.tenant_id.clone(),
        ),
        (
            "email-inbox-triage",
            Some("aegis-system-agent-runtime"),
            Some("triage"),
            Some(tenant_of("owner")),
        )
    );
    assert_eq!(
        request.input,
        json!({
            "folder": "inbox",
            "contexts": { "imap": BINDING },
            "repositories": [{ "binding_id": BINDING, "branch": "work" }],
        })
    );
}

// ── N12: what a manifest may say ─────────────────────────────────────────────

const AGENT_YAML: &str = r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: mail-triage
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Triage the inbox.
"#;

const WORKFLOW_YAML: &str = r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: email-inbox-triage
  version: "1.0.0"
spec:
  initial_state: done
  states:
    done:
      kind: System
      command: echo done
      transitions: []
"#;

/// N12: `spec.schedule` is refused at parse with its sentence, on an agent
/// and on a workflow.
#[test]
fn a_manifest_carrying_spec_schedule_is_refused() {
    let agent = format!(
        "{AGENT_YAML}  schedule:\n    type: cron\n    cron: \"0 * * * *\"\n    timezone: UTC\n"
    );
    let workflow = format!("{WORKFLOW_YAML}  schedule:\n    cron: \"0 * * * *\"\n");
    let agent_error = AgentManifestParser::parse_yaml(&agent)
        .expect_err("an agent with spec.schedule parsed")
        .to_string();
    let workflow_error = WorkflowParser::parse_yaml(&workflow)
        .expect_err("a workflow with spec.schedule parsed")
        .to_string();
    let mut wrong = Vec::new();
    if !agent_error.contains(SPEC_SCHEDULE_REFUSAL) {
        wrong.push(format!("agent: {agent_error}"));
    }
    if !workflow_error.contains(SPEC_SCHEDULE_REFUSAL) {
        wrong.push(format!("workflow: {workflow_error}"));
    }
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// N12: `spec.default_schedule` is parsed on an agent and on a workflow,
/// and held to N2's rules.
#[test]
fn spec_default_schedule_is_parsed_on_both_manifests() {
    let default = "  default_schedule:\n    cron: \"0 15 * * 1-5\"\n    timezone: Europe/Berlin\n    jitter_seconds: 600\n";
    let agent = AgentManifestParser::parse_yaml(&format!("{AGENT_YAML}{default}"))
        .expect("the agent parses");
    let workflow = WorkflowParser::parse_yaml(&format!("{WORKFLOW_YAML}{default}"))
        .expect("the workflow parses");
    let expected = Some(aegis_orchestrator_core::domain::schedule::DefaultSchedule {
        cron: "0 15 * * 1-5".into(),
        timezone: "Europe/Berlin".into(),
        jitter_seconds: 600,
    });
    assert_eq!(agent.spec.default_schedule, expected);
    assert_eq!(workflow.spec.default_schedule, expected);
    let round_trip = WorkflowParser::parse_yaml(&WorkflowParser::to_yaml(&workflow).unwrap())
        .expect("the workflow's YAML parses again");
    assert_eq!(round_trip.spec.default_schedule, expected);

    let too_often = "  default_schedule:\n    cron: \"* * * * *\"\n";
    let agent_error = AgentManifestParser::parse_yaml(&format!("{AGENT_YAML}{too_often}"))
        .expect_err("a too-frequent default parsed")
        .to_string();
    let workflow_error = WorkflowParser::parse_yaml(&format!("{WORKFLOW_YAML}{too_often}"))
        .expect_err("a too-frequent default parsed")
        .to_string();
    for error in [agent_error, workflow_error] {
        assert!(
            error.contains("A schedule runs at most once every 5 minutes."),
            "{error}"
        );
    }
}

// ── Migration 048 and the Postgres store ─────────────────────────────────────

mod postgres {
    use super::*;
    use aegis_orchestrator_core::domain::schedule::FireClaim;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::PostgresScheduleRepository;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::{Executor, Row};

    const MIGRATION_036: &str = include_str!("../../../cli/migrations/036_tool_approvals.sql");
    const MIGRATION_048: &str = include_str!("../../../cli/migrations/048_schedules.sql");
    const MIGRATION_050: &str = include_str!("../../../cli/migrations/050_profile_scoping.sql");

    /// The two run tables as migration 001 made them, in the columns this
    /// migration touches.
    const RUN_TABLES: &str = "
        CREATE TABLE executions (id UUID PRIMARY KEY, status VARCHAR(50) NOT NULL);
        CREATE TABLE workflow_executions (id UUID PRIMARY KEY, status VARCHAR(50) NOT NULL);
    ";

    async fn pool_in_fresh_schema(url: &str) -> (PgPool, String) {
        let schema = format!("sched_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("connect");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        let search_path = format!("SET search_path TO {schema}, public");
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |conn, _| {
                let sql = search_path.clone();
                Box::pin(async move {
                    conn.execute(sql.as_str()).await?;
                    Ok(())
                })
            })
            .connect(url)
            .await
            .expect("connect in schema");
        (pool, schema)
    }

    async fn columns(pool: &PgPool, schema: &str) -> Vec<String> {
        sqlx::query(
            "SELECT table_name || '.' || column_name || ':' || data_type || ':' || is_nullable AS c \
             FROM information_schema.columns WHERE table_schema = $1 ORDER BY 1",
        )
        .bind(schema)
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>("c"))
        .collect()
    }

    /// Migration 048 applied twice changes nothing, leaves an existing run
    /// untouched with a null `schedule_id`, and the Postgres store keeps a
    /// schedule, its fires (once per scheduled time) and a run's schedule.
    #[tokio::test]
    async fn migration_048_applied_twice_changes_nothing_and_the_store_round_trips() {
        let Ok(url) = std::env::var("AEGIS_TEST_POSTGRES_URL") else {
            eprintln!("skipped: no AEGIS_TEST_POSTGRES_URL");
            return;
        };
        let (pool, schema) = pool_in_fresh_schema(&url).await;
        pool.execute(RUN_TABLES).await.expect("run tables");
        pool.execute(MIGRATION_036).await.expect("migration 036");
        let existing = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO executions (id, status) VALUES ($1, 'running')")
            .bind(existing)
            .execute(&pool)
            .await
            .unwrap();

        pool.execute(MIGRATION_048).await.expect("migration 048");
        let once = columns(&pool, &schema).await;
        pool.execute(MIGRATION_048)
            .await
            .expect("migration 048 applied a second time");
        let twice = columns(&pool, &schema).await;
        let mut wrong = Vec::new();
        if once != twice {
            wrong.push(format!(
                "the second run changed the schema: {twice:?} (was {once:?})"
            ));
        }
        for added in [
            "executions.schedule_id:uuid:YES",
            "workflow_executions.schedule_id:uuid:YES",
            "tool_approval_requests.schedule_id:uuid:YES",
            "schedules.owner_sub:text:NO",
            "schedule_fires.scheduled_time:timestamp with time zone:NO",
        ] {
            if !once.iter().any(|c| c == added) {
                wrong.push(format!("missing {added}"));
            }
        }
        let row = sqlx::query("SELECT status, schedule_id FROM executions WHERE id = $1")
            .bind(existing)
            .fetch_one(&pool)
            .await
            .unwrap();
        if row.get::<String, _>("status") != "running"
            || row.get::<Option<uuid::Uuid>, _>("schedule_id").is_some()
        {
            wrong.push("the existing run changed".to_string());
        }
        assert!(wrong.is_empty(), "{wrong:#?}");

        // The store reads and writes the columns of every migration it
        // ships: 050 adds `schedules.profile_id` (AEGIS ADR-140 D8).
        pool.execute(MIGRATION_050).await.expect("migration 050");
        let store = PostgresScheduleRepository::new(pool.clone());
        // Times at a whole second, as PostgreSQL keeps microseconds.
        let mut schedule = scheduled("agent", "mail-triage");
        schedule.created_at = at(0);
        schedule.updated_at = at(0);
        store.insert(&schedule).await.expect("insert");
        assert_eq!(
            store.get(schedule.id).await.unwrap(),
            Some(schedule.clone())
        );
        let fired = Utc::now();
        let first = match store.claim_fire(schedule.id, at(0), fired).await.unwrap() {
            FireClaim::Claimed(fire) => fire,
            other => panic!("expected a claim, got {other:?}"),
        };
        match store.claim_fire(schedule.id, at(0), fired).await.unwrap() {
            FireClaim::Repeated(fire) => assert_eq!(fire.id, first.id),
            other => panic!("expected the first fire's row, got {other:?}"),
        }
        let run = ExecutionId(existing);
        store
            .bind_execution(TargetKind::Agent, run, schedule.id)
            .await
            .unwrap();
        let bound: Option<uuid::Uuid> =
            sqlx::query("SELECT schedule_id FROM executions WHERE id = $1")
                .bind(existing)
                .fetch_one(&pool)
                .await
                .unwrap()
                .get("schedule_id");
        assert_eq!(bound, Some(schedule.id.0));
    }
}

/// AEGIS ADR-140 D8, D10: a schedule saved with a profile fires a run whose
/// input carries the profile and no contexts of its own (the start reads
/// the profile as the owner's); one saved with a profile and contexts is
/// refused with D10's sentence and nothing is stored.
#[tokio::test]
async fn a_schedule_on_a_profile_fires_a_run_on_it_and_one_with_both_is_refused() {
    const PROFILE: &str = "2b7e4c1a-9d3f-4e5a-8b6c-7d8e9f0a1b2c";
    let f = fixture();
    let both = f
        .service
        .create(
            &consumer("owner"),
            &tenant_of("owner"),
            ScheduleDraft {
                profile: Some(json!(PROFILE)),
                ..recurring("both", "0 15 * * *")
            },
        )
        .await;
    let refused = match both {
        Err(e) => e.to_string(),
        Ok(_) => "stored".to_string(),
    };
    let schedule = create(
        &f,
        "owner",
        ScheduleDraft {
            profile: Some(json!(PROFILE)),
            contexts: None,
            ..recurring("on a profile", "0 15 * * *")
        },
    )
    .await;
    let executions = Arc::new(RecordingExecutions::default());
    let starter = ServiceRunStarter::new(
        executions.clone(),
        Arc::new(OneAgent(AgentId::new())),
        None,
        None,
    );
    let owner = schedule.owner.to_identity(&schedule.tenant_id).unwrap();
    starter.start(&schedule, &owner).await.expect("started");
    let started = executions.started.lock().unwrap();
    assert_eq!(
        (refused.contains(aegis_orchestrator_core::domain::execution::PROFILE_WITH_CONTEXTS), started[0].1.input.clone()),
        (
            true,
            json!({
                "folder": "inbox",
                "profile": PROFILE,
                "tenant_id": tenant_of("owner").as_str(),
            })
        ),
        "a schedule with both was {refused}, or the fired run's input did not carry the profile alone"
    );
}
