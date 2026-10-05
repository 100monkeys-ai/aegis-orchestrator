// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The goal store (AEGIS ADR-131 D1, Update U1) against a real PostgreSQL,
//! with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: a goal and its evaluations round-trip; a round is decided by
//! one evaluation only, against two racing deciders; a continuation is
//! granted once; `goal_id` is written on an execution's and a workflow
//! execution's rows and listed oldest first, and the executions' own upserts
//! leave it in place; the sweep finds only goals open past the cutoff;
//! migration 039 run again changes nothing; and migration 040 (U16, U17),
//! applied to a database holding goals and evaluations from before it,
//! leaves them reading as they did, stores the `stopped` state and a stop
//! reason, decides a stopped round once, and run again changes nothing; and
//! a wait for running work (U24) is stored in those columns with no
//! migration and decides no round; and the goal service's sweep reads the
//! stored wait rows so that the time a goal waited does not count against
//! its lifetime, and leaves a goal whose judge runs after a wait (U29).

use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::goal::{
    wait_time_text, BoundKind, Goal, GoalChannel, GoalEvaluation, GoalId, GoalOutcome,
    GoalRepository, GoalState, StopReason, WAITING_ON_EXECUTION,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_goal::PostgresGoalRepository;
use chrono::{Duration, DurationRound, Utc};
use serde_json::json;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A tenant every migrated database holds (migration 001).
const TENANT: &str = "aegis-system";

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI these tests must reach PostgreSQL")
        }
        _ => {
            eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
            None
        }
    }
}

struct TestDb {
    server: PgPool,
    name: String,
    pool: PgPool,
}

impl TestDb {
    async fn create() -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_goals_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        MIGRATOR.run(&pool).await.expect("apply every migration");
        Some(Self { server, name, pool })
    }

    /// A database holding every migration before `version` and none after,
    /// as production stood before that migration was deployed.
    async fn create_before(version: i64) -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_goals_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&server)
            .await
            .expect("create the test database");
        let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect to the test database");
        for migration in MIGRATOR.iter().filter(|m| m.version < version) {
            sqlx::raw_sql(&migration.sql)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("migration {}: {e}", migration.version));
        }
        Some(Self { server, name, pool })
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

/// PostgreSQL keeps microseconds; compare times at that precision.
fn now() -> chrono::DateTime<Utc> {
    Utc::now()
        .duration_trunc(Duration::microseconds(1))
        .unwrap()
}

fn goal(client_ref: &str) -> Goal {
    Goal {
        id: GoalId::new(),
        tenant_id: TenantId::new(TENANT).unwrap(),
        user_sub: "owner-sub".to_string(),
        statement: "Create palindrome-checker, then run it on \"racecar\".".to_string(),
        client_ref: client_ref.to_string(),
        channel: GoalChannel::Web,
        state: GoalState::Open,
        rounds: 0,
        created_at: now(),
        closed_at: None,
    }
}

fn evaluation(goal_id: GoalId, round: u32, attempt: u32) -> GoalEvaluation {
    GoalEvaluation {
        id: Uuid::new_v4(),
        goal_id,
        round,
        attempt,
        judge_execution_id: Some(ExecutionId::new()),
        companion_answer: "Dispatching it now.".to_string(),
        verdict: None,
        outcome: None,
        r#continue: false,
        waiting_on: None,
        answer: None,
        created_at: now(),
        decided_at: None,
        stop_reason: None,
        input_digest: Some("d".repeat(64)),
    }
}

fn decided(mut e: GoalEvaluation, outcome: GoalOutcome, r#continue: bool) -> GoalEvaluation {
    e.verdict = Some(json!({"score": 0.4, "confidence": 0.9, "reasoning": "r", "signals": []}));
    e.outcome = Some(outcome);
    e.r#continue = r#continue;
    e.answer = Some(json!({"continue": r#continue}));
    e.decided_at = Some(now());
    e
}

/// A wait for bound work still running (U24): no judge, no verdict, no
/// outcome, `waiting_on` naming the executions and the wait's bound.
fn wait(goal_id: GoalId, round: u32, wait_until: chrono::DateTime<Utc>) -> GoalEvaluation {
    GoalEvaluation {
        judge_execution_id: None,
        waiting_on: Some(json!({
            "kind": WAITING_ON_EXECUTION,
            "execution_ids": [ExecutionId::new().to_string()],
            "wait_until": wait_time_text(wait_until),
        })),
        input_digest: None,
        ..evaluation(goal_id, round, 1)
    }
}

/// U24 with no migration: a wait row is stored and read back in 039's and
/// 040's columns; it decides no round, so the round's verdict is stored
/// beside it under `uq_goal_evaluations_decided_round`; its end is written
/// by `finish_evaluation`.
#[tokio::test]
async fn a_wait_row_round_trips_and_decides_no_round() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-wait");
    repo.insert_goal(&g).await.unwrap();

    let open = wait(g.id, 0, now() + Duration::seconds(2_400));
    repo.insert_evaluation(&open).await.unwrap();
    let stored = repo.list_evaluations(g.id).await.unwrap();
    assert_eq!(stored, vec![open.clone()]);
    assert!(stored[0].is_open_wait());
    assert!(!stored[0].is_running() && !stored[0].is_fault() && !stored[0].decides_round());

    let mut ended = open.clone();
    ended.decided_at = Some(now());
    assert!(repo.finish_evaluation(&ended).await.unwrap());
    let judged = decided(evaluation(g.id, 0, 1), GoalOutcome::NotMet, true);
    repo.insert_evaluation(&judged).await.unwrap();
    let stored = repo.list_evaluations(g.id).await.unwrap();
    let waits: Vec<_> = stored.iter().filter(|e| e.is_wait()).collect();
    assert_eq!(waits, vec![&ended]);
    assert!(!waits[0].is_fault(), "an ended wait is no judge fault");
    assert_eq!(
        stored.iter().filter(|e| e.decides_round()).count(),
        1,
        "the round's verdict decides it, the wait does not"
    );
    db.remove().await;
}

#[tokio::test]
async fn a_goal_and_its_evaluations_round_trip() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-1");
    repo.insert_goal(&g).await.unwrap();
    assert_eq!(repo.find_goal(g.id).await.unwrap(), Some(g.clone()));
    assert_eq!(
        repo.list_open_for_client_ref(&g.tenant_id, "owner-sub", "conversation-1")
            .await
            .unwrap(),
        vec![g.clone()]
    );

    let running = evaluation(g.id, 0, 1);
    repo.insert_evaluation(&running).await.unwrap();
    let done = decided(running.clone(), GoalOutcome::NotMet, true);
    assert!(repo.finish_evaluation(&done).await.unwrap());
    assert_eq!(repo.list_evaluations(g.id).await.unwrap(), vec![done]);

    assert!(repo.grant_round(g.id, 0).await.unwrap());
    assert!(
        !repo.grant_round(g.id, 0).await.unwrap(),
        "a continuation is granted once"
    );
    assert_eq!(repo.find_goal(g.id).await.unwrap().unwrap().rounds, 1);

    let closed_at = now();
    assert!(repo
        .close_goal(g.id, GoalState::Met, closed_at)
        .await
        .unwrap());
    assert!(!repo
        .close_goal(g.id, GoalState::Expired, closed_at)
        .await
        .unwrap());
    let stored = repo.find_goal(g.id).await.unwrap().unwrap();
    assert_eq!(
        (stored.state, stored.closed_at),
        (GoalState::Met, Some(closed_at))
    );
    db.remove().await;
}

/// U1, U2: a round is decided by one evaluation; a waiting evaluation
/// (U3) or a fault decides none.
#[tokio::test]
async fn a_round_is_decided_by_one_evaluation_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-1");
    repo.insert_goal(&g).await.unwrap();

    let mut waiting = evaluation(g.id, 0, 1);
    repo.insert_evaluation(&waiting).await.unwrap();
    waiting = decided(waiting, GoalOutcome::NotMet, false);
    waiting.waiting_on = Some(json!({"kind": "approval", "approval_ids": ["a-1"]}));
    assert!(repo.finish_evaluation(&waiting).await.unwrap());

    let first = evaluation(g.id, 0, 1);
    let second = evaluation(g.id, 0, 1);
    repo.insert_evaluation(&first).await.unwrap();
    repo.insert_evaluation(&second).await.unwrap();
    assert!(repo
        .finish_evaluation(&decided(first, GoalOutcome::NotMet, true))
        .await
        .unwrap());
    assert!(
        !repo
            .finish_evaluation(&decided(second, GoalOutcome::Met, false))
            .await
            .unwrap(),
        "a second decision of round 0 is refused"
    );
    let deciding: Vec<_> = repo
        .list_evaluations(g.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.decides_round())
        .collect();
    assert_eq!(deciding.len(), 1);
    assert_eq!(deciding[0].outcome, Some(GoalOutcome::NotMet));
    db.remove().await;
}

/// D1, U6: `goal_id` on an execution's and a workflow execution's rows,
/// listed oldest first; an upsert of the execution, as the execution
/// repository writes it, leaves it in place.
#[tokio::test]
async fn goal_id_is_written_on_both_kinds_of_row_and_survives_their_upserts() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-1");
    repo.insert_goal(&g).await.unwrap();

    let agent = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, security_policy) \
         VALUES ($1, $2, 'palindrome-checker', 'x', '{}', 'python:3.11', '{}')",
    )
    .bind(agent)
    .bind(TENANT)
    .execute(&db.pool)
    .await
    .unwrap();
    let workflow = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflows (id, tenant_id, name, version, yaml_source, domain_json, temporal_def_json) \
         VALUES ($1, $2, 'builtin-intent-to-execution', '1.0.0', 'x', '{}', '{}')",
    )
    .bind(workflow)
    .bind(TENANT)
    .execute(&db.pool)
    .await
    .unwrap();

    let execution = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO executions (id, tenant_id, agent_id, input, security_context_name, started_at) \
         VALUES ($1, $2, $3, '{}', 'zaru-free', NOW() - INTERVAL '1 minute')",
    )
    .bind(execution)
    .bind(TENANT)
    .bind(agent)
    .execute(&db.pool)
    .await
    .unwrap();
    let pipeline = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflow_executions (id, tenant_id, workflow_id, temporal_workflow_id, temporal_run_id, started_at) \
         VALUES ($1, $2, $3, 't', 'r', NOW())",
    )
    .bind(pipeline)
    .bind(TENANT)
    .bind(workflow)
    .execute(&db.pool)
    .await
    .unwrap();

    assert!(repo
        .bind_execution(g.id, ExecutionId(execution))
        .await
        .unwrap());
    assert!(repo
        .bind_workflow_execution(g.id, ExecutionId(pipeline))
        .await
        .unwrap());
    assert!(
        !repo.bind_execution(g.id, ExecutionId::new()).await.unwrap(),
        "no row, no binding"
    );

    // The execution repository's upsert names its columns; goal_id is not
    // among them.
    sqlx::query(
        "INSERT INTO executions (id, tenant_id, agent_id, input, status, security_context_name) \
         VALUES ($1, $2, $3, '{}', 'completed', 'zaru-free') \
         ON CONFLICT (id) DO UPDATE SET status = EXCLUDED.status",
    )
    .bind(execution)
    .bind(TENANT)
    .bind(agent)
    .execute(&db.pool)
    .await
    .unwrap();

    let bound = repo.list_bound(g.id).await.unwrap();
    let listed: Vec<_> = bound.iter().map(|b| (b.execution_id.0, b.kind)).collect();
    assert_eq!(
        listed,
        vec![
            (execution, BoundKind::Agent),
            (pipeline, BoundKind::Workflow)
        ]
    );
    db.remove().await;
}

#[tokio::test]
async fn the_sweep_finds_only_goals_open_past_the_cutoff() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let mut old = goal("a");
    old.created_at = now() - Duration::seconds(1800);
    let young = goal("b");
    let mut closed = goal("c");
    closed.created_at = old.created_at;
    closed.state = GoalState::Met;
    closed.closed_at = Some(now());
    for g in [&old, &young, &closed] {
        repo.insert_goal(g).await.unwrap();
    }
    let found = repo
        .list_open_created_before(now() - Duration::seconds(1800), now())
        .await
        .unwrap();
    assert_eq!(found.iter().map(|g| g.id).collect::<Vec<_>>(), vec![old.id]);
    db.remove().await;
}

/// U25: the sweep skips a goal open past its lifetime whose current round
/// holds an open wait with its `wait_until` ahead; a wait already ended, an
/// open wait past its `wait_until`, or a wait of an earlier round leaves the
/// goal to the sweep as D7 says.
#[tokio::test]
async fn the_sweep_skips_a_goal_whose_round_waits_inside_its_bound() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let at = now();
    let mut goals = Vec::new();
    for client_ref in ["waiting", "wait-ended", "wait-past-bound", "earlier-round"] {
        let mut g = goal(client_ref);
        g.created_at = at - Duration::seconds(1_801);
        repo.insert_goal(&g).await.unwrap();
        goals.push(g);
    }
    let [waiting, ended, past, earlier] = &goals[..] else {
        unreachable!()
    };
    repo.insert_evaluation(&wait(waiting.id, 0, at + Duration::seconds(600)))
        .await
        .unwrap();
    let mut done = wait(ended.id, 0, at + Duration::seconds(600));
    done.decided_at = Some(at);
    repo.insert_evaluation(&done).await.unwrap();
    repo.insert_evaluation(&wait(past.id, 0, at - Duration::seconds(1)))
        .await
        .unwrap();
    repo.insert_evaluation(&wait(earlier.id, 0, at + Duration::seconds(600)))
        .await
        .unwrap();
    assert!(repo.grant_round(earlier.id, 0).await.unwrap());

    let found: Vec<GoalId> = repo
        .list_open_created_before(at - Duration::seconds(1_800), at)
        .await
        .unwrap()
        .into_iter()
        .map(|g| g.id)
        .collect();
    assert!(
        !found.contains(&waiting.id),
        "an open wait ahead is skipped"
    );
    for g in [ended, past, earlier] {
        assert!(found.contains(&g.id), "{} is swept", g.client_ref);
    }
    db.remove().await;
}

/// Migration 039 applied again over a migrated schema, with rows in it,
/// changes nothing.
#[tokio::test]
async fn migration_039_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-1");
    repo.insert_goal(&g).await.unwrap();
    repo.insert_evaluation(&evaluation(g.id, 0, 1))
        .await
        .unwrap();
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM goals) AS goals, \
                    (SELECT count(*) FROM goal_evaluations) AS evaluations, \
                    (SELECT count(*) FROM pg_indexes WHERE tablename IN \
                        ('goals', 'goal_evaluations', 'executions', 'workflow_executions')) AS indexes, \
                    (SELECT count(*) FROM information_schema.columns \
                        WHERE column_name = 'goal_id' \
                        AND table_name IN ('executions', 'workflow_executions')) AS columns",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("goals"),
                row.get::<i64, _>("evaluations"),
                row.get::<i64, _>("indexes"),
                row.get::<i64, _>("columns"),
            )
        })
        .unwrap()
    };
    let before = snapshot().await;
    assert_eq!(before.3, 2, "goal_id on both tables");
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 39)
        .expect("migration 039 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 039 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}

/// Migration 040 applied to a database holding a goal and an evaluation
/// written before it: both read as they did, with no stop reason and no
/// digest, and the evaluation's answer, cut to 8,192 characters before
/// U14, stays as stored.
#[tokio::test]
async fn migration_040_over_rows_from_before_it_leaves_them_reading_as_they_did() {
    let Some(db) = TestDb::create_before(40).await else {
        return;
    };
    let goal_id = Uuid::new_v4();
    let evaluation_id = Uuid::new_v4();
    let cut = "a".repeat(8_192);
    sqlx::query(
        "INSERT INTO goals (id, tenant_id, user_sub, statement, client_ref, channel, state, \
         rounds, created_at, closed_at) VALUES ($1, $2, 'owner-sub', 'the request', \
         'conversation-1', 'web', 'exhausted', 3, NOW(), NOW())",
    )
    .bind(goal_id)
    .bind(TENANT)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO goal_evaluations (id, goal_id, round, attempt, judge_execution_id, \
         companion_answer, verdict, outcome, continue, waiting_on, answer, created_at, \
         decided_at) VALUES ($1, $2, 3, 1, $3, $4, '{\"score\": 0.5}', 'not_met', false, \
         NULL, '{\"state\": \"exhausted\"}', NOW(), NOW())",
    )
    .bind(evaluation_id)
    .bind(goal_id)
    .bind(Uuid::new_v4())
    .bind(&cut)
    .execute(&db.pool)
    .await
    .unwrap();

    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 40)
        .expect("migration 040 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 040 over the old rows");

    let repo = PostgresGoalRepository::new(db.pool.clone());
    let goal = repo.find_goal(GoalId(goal_id)).await.unwrap().unwrap();
    assert_eq!((goal.state, goal.rounds), (GoalState::Exhausted, 3));
    let evaluations = repo.list_evaluations(GoalId(goal_id)).await.unwrap();
    assert_eq!(evaluations.len(), 1);
    let old = &evaluations[0];
    assert_eq!(old.companion_answer, cut, "the cut answer stays as stored");
    assert_eq!(old.outcome, Some(GoalOutcome::NotMet));
    assert_eq!(old.stop_reason, None);
    assert_eq!(old.input_digest, None);
    assert!(old.decides_round());
    assert!(!old.is_fault());
    assert_eq!(old.answer, Some(json!({"state": "exhausted"})));
    db.remove().await;
}

/// U16, U17: a goal stops, its stopping evaluation keeps its reason and
/// digest, and a stopped round is decided once.
#[tokio::test]
async fn a_stopped_goal_and_its_stop_reason_round_trip_and_stop_a_round_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-1");
    repo.insert_goal(&g).await.unwrap();

    let stopping = |reason| {
        let mut e = evaluation(g.id, 0, 1);
        e.judge_execution_id = None;
        e.verdict = Some(json!({"score": null, "reasoning": "Round 0 brought nothing new."}));
        e.answer = Some(json!({"state": "stopped", "continue": false}));
        e.decided_at = Some(now());
        e.stop_reason = Some(reason);
        e
    };
    let first = stopping(StopReason::RepeatedRound);
    repo.insert_evaluation(&first).await.unwrap();
    assert!(
        repo.insert_evaluation(&stopping(StopReason::TooLarge))
            .await
            .is_err(),
        "a second stop of round 0 is refused"
    );
    assert_eq!(repo.list_evaluations(g.id).await.unwrap(), vec![first]);
    assert!(repo
        .close_goal(g.id, GoalState::Stopped, now())
        .await
        .unwrap());
    assert_eq!(
        repo.find_goal(g.id).await.unwrap().unwrap().state,
        GoalState::Stopped
    );
    db.remove().await;
}

/// Migration 040 applied again over a migrated schema, with rows in it,
/// changes nothing.
#[tokio::test]
async fn migration_040_run_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal("conversation-1");
    repo.insert_goal(&g).await.unwrap();
    repo.insert_evaluation(&evaluation(g.id, 0, 1))
        .await
        .unwrap();
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM goals) AS goals, \
                    (SELECT count(*) FROM goal_evaluations) AS evaluations, \
                    (SELECT count(*) FROM pg_indexes WHERE tablename = 'goal_evaluations') AS indexes, \
                    (SELECT count(*) FROM pg_constraint WHERE conrelid IN \
                        ('goals'::regclass, 'goal_evaluations'::regclass)) AS constraints",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("goals"),
                row.get::<i64, _>("evaluations"),
                row.get::<i64, _>("indexes"),
                row.get::<i64, _>("constraints"),
            )
        })
        .unwrap()
    };
    let before = snapshot().await;
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 40)
        .expect("migration 040 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 040 run again");
    assert_eq!(snapshot().await, before);
    db.remove().await;
}

/// U29 through the goal service over the PostgreSQL store: of goals open
/// 3,000 s, one that waited 2,400 s on its work (a wait row ended) has spent
/// 600 s of its own and is not swept; one that never waited is swept; one
/// whose round waited 600 s (2,400 s of its own) and whose judge started 60 s
/// ago is still running is not swept (the Low row); one whose judge started
/// after such a wait 1,000 s ago and never reported is swept, past
/// goal-judge's 300 s plus the margin.
#[tokio::test]
async fn the_services_sweep_credits_stored_waits_and_leaves_a_judge_in_flight() {
    use aegis_orchestrator_core::application::goal_service::GoalService;
    use aegis_orchestrator_core::domain::node_config::GoalsConfig;
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use std::sync::Arc;

    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = Arc::new(PostgresGoalRepository::new(db.pool.clone()));
    let at = now();
    let created = at - Duration::seconds(3_000);
    let mut goals = Vec::new();
    for client_ref in ["waited", "never-waited", "judge-running", "judge-silent"] {
        let mut g = goal(client_ref);
        g.created_at = created;
        repo.insert_goal(&g).await.unwrap();
        goals.push(g);
    }
    let [waited, never, running, silent] = &goals[..] else {
        unreachable!()
    };
    let ended_wait = |goal_id: GoalId, seconds: i64| {
        let ended = created + Duration::seconds(100 + seconds);
        let mut w = wait(goal_id, 0, ended);
        w.created_at = created + Duration::seconds(100);
        w.decided_at = Some(ended);
        w
    };
    repo.insert_evaluation(&ended_wait(waited.id, 2_400))
        .await
        .unwrap();
    for (g, started) in [(running, 60), (silent, 1_000)] {
        // A 600 s wait: 2,400 s of its own, past the lifetime.
        repo.insert_evaluation(&ended_wait(g.id, 600))
            .await
            .unwrap();
        let mut judge = evaluation(g.id, 0, 1);
        judge.created_at = at - Duration::seconds(started);
        judge.decided_at = None;
        judge.outcome = None;
        judge.verdict = None;
        judge.answer = None;
        judge.input_digest = None;
        assert!(judge.is_running(), "a judge in flight");
        repo.insert_evaluation(&judge).await.unwrap();
    }

    let service = GoalService::new(
        repo.clone(),
        Arc::new(EventBus::new(16)),
        GoalsConfig::default(),
    );
    assert_eq!(service.close_expired(at).await.unwrap(), 2);
    let state = |id: GoalId| {
        let repo = repo.clone();
        async move { repo.find_goal(id).await.unwrap().unwrap().state }
    };
    assert_eq!(state(waited.id).await, GoalState::Open, "600 s of its own");
    assert_eq!(state(never.id).await, GoalState::Expired);
    assert_eq!(state(running.id).await, GoalState::Open, "its judge runs");
    assert_eq!(state(silent.id).await, GoalState::Expired);
    db.remove().await;
}
