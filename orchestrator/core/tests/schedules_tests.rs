// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Schedules (AEGIS ADR-139 N1 to N5)
//!
//! The schedule service against a Temporal schedule stand-in that records
//! every call and can be made to fail, over the in-memory store; and,
//! when `AEGIS_TEST_POSTGRES_URL` names a database, migration 048 applied
//! twice and the Postgres store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aegis_orchestrator_core::application::ports::{
    ScheduleEnginePort, TemporalScheduleDescription, TemporalScheduleSpec,
};
use aegis_orchestrator_core::application::schedule_service::{
    ScheduleError, ScheduleReader, ScheduleService,
};
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::schedule::{
    RecurrenceInput, Schedule, ScheduleDraft, ScheduleId, SchedulePatch, ScheduleRepository,
    ScheduleState, TargetKind, Timing, AT_REFUSAL, CRON_REFUSAL, OWNER_REFUSAL, TIMEZONE_REFUSAL,
    UNAVAILABLE_REFUSAL,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::InMemoryScheduleRepository;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
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

struct Fixture {
    service: ScheduleService,
    store: Arc<InMemoryScheduleRepository>,
    engine: Arc<StandInEngine>,
}

fn fixture() -> Fixture {
    let store = Arc::new(InMemoryScheduleRepository::new());
    let engine = Arc::new(StandInEngine::default());
    *engine.store.lock().unwrap() = Some(store.clone());
    Fixture {
        service: ScheduleService::new(store.clone(), engine.clone()),
        store,
        engine,
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

// ── Migration 048 and the Postgres store ─────────────────────────────────────

mod postgres {
    use super::*;
    use aegis_orchestrator_core::domain::schedule::FireClaim;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_schedule::PostgresScheduleRepository;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use sqlx::{Executor, Row};

    const MIGRATION_036: &str = include_str!("../../../cli/migrations/036_tool_approvals.sql");
    const MIGRATION_048: &str = include_str!("../../../cli/migrations/048_schedules.sql");

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
