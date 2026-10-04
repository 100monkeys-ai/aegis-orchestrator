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
//! leave it in place; the sweep finds only goals open past the cutoff; and
//! migration 039 run again changes nothing.

use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::goal::{
    BoundKind, Goal, GoalChannel, GoalEvaluation, GoalId, GoalOutcome, GoalRepository, GoalState,
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
        .list_open_created_before(now() - Duration::seconds(1800))
        .await
        .unwrap();
    assert_eq!(found.iter().map(|g| g.id).collect::<Vec<_>>(), vec![old.id]);
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
