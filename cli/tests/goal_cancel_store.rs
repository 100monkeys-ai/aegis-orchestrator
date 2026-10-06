// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A goal the person ends (AEGIS ADR-131 U33, U33a) against a real
//! PostgreSQL, with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: migration 043 over goals from before it leaves them reading
//! as they did, with no reason; a goal closes `cancelled` with its reason
//! once; the goal an execution or a workflow execution is bound to is found
//! by the execution's id, and none for an execution bound to none; and
//! migration 043 run again changes nothing.

use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::goal::{Goal, GoalChannel, GoalId, GoalRepository, GoalState};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_goal::PostgresGoalRepository;
use chrono::{Duration, DurationRound, Utc};
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
    /// A database holding every migration before `version` (or every one,
    /// with `None`).
    async fn create(before: Option<i64>) -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_goal_cancel_{}", Uuid::new_v4().simple());
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
        match before {
            None => MIGRATOR.run(&pool).await.expect("apply every migration"),
            Some(version) => {
                for migration in MIGRATOR.iter().filter(|m| m.version < version) {
                    sqlx::raw_sql(&migration.sql)
                        .execute(&pool)
                        .await
                        .unwrap_or_else(|e| panic!("migration {}: {e}", migration.version));
                }
            }
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

fn now() -> chrono::DateTime<Utc> {
    Utc::now()
        .duration_trunc(Duration::microseconds(1))
        .unwrap()
}

fn goal() -> Goal {
    Goal {
        id: GoalId::new(),
        tenant_id: TenantId::new(TENANT).unwrap(),
        user_sub: "owner-sub".to_string(),
        statement: "Solve the routing problem.".to_string(),
        client_ref: "conversation-1".to_string(),
        channel: GoalChannel::Web,
        state: GoalState::Open,
        rounds: 0,
        created_at: now(),
        closed_at: None,
        closed_reason: None,
    }
}

async fn migration_043(db: &TestDb) {
    let migration = MIGRATOR
        .iter()
        .find(|m| m.version == 43)
        .expect("migration 043 ships");
    sqlx::raw_sql(&migration.sql)
        .execute(&db.pool)
        .await
        .expect("migration 043");
}

#[tokio::test]
async fn migration_043_over_goals_from_before_it_leaves_them_reading_as_they_did() {
    let Some(db) = TestDb::create(Some(43)).await else {
        return;
    };
    let goal_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO goals (id, tenant_id, user_sub, statement, client_ref, channel, state, \
         rounds, created_at, closed_at) VALUES ($1, $2, 'owner-sub', 'the request', \
         'conversation-1', 'web', 'stopped', 2, NOW(), NOW())",
    )
    .bind(goal_id)
    .bind(TENANT)
    .execute(&db.pool)
    .await
    .unwrap();
    let refused = sqlx::query("UPDATE goals SET state = 'cancelled' WHERE id = $1")
        .bind(goal_id)
        .execute(&db.pool)
        .await;
    assert!(
        refused.is_err(),
        "before 043 the state check refuses cancelled"
    );

    migration_043(&db).await;

    let repo = PostgresGoalRepository::new(db.pool.clone());
    let read = repo.find_goal(GoalId(goal_id)).await.unwrap().unwrap();
    assert_eq!((read.state, read.rounds), (GoalState::Stopped, 2));
    assert_eq!(read.closed_reason, None, "an old goal has no reason");
    db.remove().await;
}

#[tokio::test]
async fn a_goal_closes_cancelled_with_its_reason_once() {
    let Some(db) = TestDb::create(None).await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal();
    repo.insert_goal(&g).await.unwrap();
    let closed_at = now();
    assert!(repo
        .close_goal(g.id, GoalState::Cancelled, closed_at, Some("Stop it."))
        .await
        .unwrap());
    assert!(
        !repo
            .close_goal(g.id, GoalState::Expired, closed_at, None)
            .await
            .unwrap(),
        "a closed goal is closed once"
    );
    let read = repo.find_goal(g.id).await.unwrap().unwrap();
    println!(
        "PostgreSQL: the goal reads {} at {:?} ({:?})",
        read.state.as_str(),
        read.closed_at,
        read.closed_reason
    );
    assert_eq!(
        (read.state, read.closed_at, read.closed_reason),
        (
            GoalState::Cancelled,
            Some(closed_at),
            Some("Stop it.".to_string())
        )
    );
    db.remove().await;
}

#[tokio::test]
async fn the_goal_of_an_execution_is_found_by_its_id_for_both_kinds() {
    let Some(db) = TestDb::create(None).await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    let g = goal();
    repo.insert_goal(&g).await.unwrap();

    let agent = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, security_policy) \
         VALUES ($1, $2, 'vrp-solver-agent', 'x', '{}', 'python:3.11', '{}')",
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
    let unbound = Uuid::new_v4();
    for id in [execution, unbound] {
        sqlx::query(
            "INSERT INTO executions (id, tenant_id, agent_id, input, security_context_name, started_at) \
             VALUES ($1, $2, $3, '{}', 'zaru-free', NOW())",
        )
        .bind(id)
        .bind(TENANT)
        .bind(agent)
        .execute(&db.pool)
        .await
        .unwrap();
    }
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

    assert_eq!(
        repo.find_goal_of_execution(ExecutionId(execution))
            .await
            .unwrap(),
        Some(g.id)
    );
    assert_eq!(
        repo.find_goal_of_execution(ExecutionId(pipeline))
            .await
            .unwrap(),
        Some(g.id)
    );
    assert_eq!(
        repo.find_goal_of_execution(ExecutionId(unbound))
            .await
            .unwrap(),
        None,
        "an execution bound to no goal"
    );
    assert_eq!(
        repo.find_goal_of_execution(ExecutionId::new())
            .await
            .unwrap(),
        None,
        "no execution by that id"
    );
    db.remove().await;
}

#[tokio::test]
async fn migration_043_run_again_changes_nothing() {
    let Some(db) = TestDb::create(None).await else {
        return;
    };
    let repo = PostgresGoalRepository::new(db.pool.clone());
    repo.insert_goal(&goal()).await.unwrap();
    let snapshot = || async {
        sqlx::query(
            "SELECT (SELECT count(*) FROM goals) AS goals, \
                    (SELECT count(*) FROM information_schema.columns \
                        WHERE table_name = 'goals') AS columns, \
                    (SELECT count(*) FROM pg_constraint WHERE conrelid = 'goals'::regclass) AS constraints",
        )
        .fetch_one(&db.pool)
        .await
        .map(|row| {
            (
                row.get::<i64, _>("goals"),
                row.get::<i64, _>("columns"),
                row.get::<i64, _>("constraints"),
            )
        })
        .unwrap()
    };
    let before = snapshot().await;
    migration_043(&db).await;
    assert_eq!(snapshot().await, before);
    db.remove().await;
}
