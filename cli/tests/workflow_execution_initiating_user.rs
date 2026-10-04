// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The workflow execution's starter (AEGIS ADR-132's Update, G2; migration
//! 041) against a real PostgreSQL, with the migrations this binary ships.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. Each test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere the tests say they were skipped and pass.
//!
//! They cover: the starter round-trips through the repository and a later
//! save that carries none leaves it in place; a row written before migration
//! 041 reads as it did, with no starter; migration 041 run again changes
//! nothing.

use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
use aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    Blackboard, StateName, WorkflowExecution, WorkflowId,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository;
use chrono::Utc;
use serde_json::json;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::Row;
use std::collections::HashMap;
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
    /// A database holding every migration before `version` (all of them
    /// when `None`).
    async fn create(before: Option<i64>) -> Option<Self> {
        let url = postgres_url()?;
        let server = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("connect to the test PostgreSQL");
        let name = format!("aegis_wfstarter_{}", Uuid::new_v4().simple());
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

    async fn apply(&self, version: i64) {
        let migration = MIGRATOR
            .iter()
            .find(|m| m.version == version)
            .expect("the migration ships");
        sqlx::raw_sql(&migration.sql)
            .execute(&self.pool)
            .await
            .unwrap_or_else(|e| panic!("migration {version}: {e}"));
    }

    async fn remove(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("drop the test database");
    }
}

async fn insert_workflow(pool: &PgPool) -> Uuid {
    let workflow = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflows (id, tenant_id, name, version, yaml_source, domain_json, temporal_def_json) \
         VALUES ($1, $2, 'outreach', '1.0.0', 'x', '{}', '{}')",
    )
    .bind(workflow)
    .bind(TENANT)
    .execute(pool)
    .await
    .unwrap();
    workflow
}

fn execution(workflow: Uuid, starter: Option<&str>) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        id: ExecutionId::new(),
        workflow_id: WorkflowId(workflow),
        tenant_id: TenantId::new(TENANT).unwrap(),
        status: ExecutionStatus::Running,
        current_state: StateName::new("START").unwrap(),
        blackboard: Blackboard::new(),
        input: json!({}),
        state_outputs: HashMap::new(),
        final_output: None,
        started_at: now,
        last_transition_at: now,
        initiating_user_sub: starter.map(str::to_string),
    }
}

#[tokio::test]
async fn the_starter_round_trips_and_a_later_save_without_one_leaves_it() {
    let Some(db) = TestDb::create(None).await else {
        return;
    };
    let repo = PostgresWorkflowExecutionRepository::new(db.pool.clone());
    let tenant = TenantId::new(TENANT).unwrap();
    let workflow = insert_workflow(&db.pool).await;

    let mut started = execution(workflow, Some("u-starter"));
    repo.save_for_tenant(&tenant, &started).await.unwrap();
    let read = repo
        .find_by_id_for_tenant(&tenant, started.id)
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(read.initiating_user_sub.as_deref(), Some("u-starter"));

    // A transition saved from an aggregate that carries no starter.
    started.initiating_user_sub = None;
    started.status = ExecutionStatus::Completed;
    repo.save_for_tenant(&tenant, &started).await.unwrap();
    let read = repo
        .find_by_id_for_tenant(&tenant, started.id)
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(read.initiating_user_sub.as_deref(), Some("u-starter"));
    assert!(matches!(read.status, ExecutionStatus::Completed));

    let unowned = execution(workflow, None);
    repo.save_for_tenant(&tenant, &unowned).await.unwrap();
    let read = repo
        .find_by_id_for_tenant(&tenant, unowned.id)
        .await
        .unwrap()
        .expect("stored");
    assert_eq!(read.initiating_user_sub, None);
    db.remove().await;
}

#[tokio::test]
async fn a_row_from_before_migration_041_reads_as_it_did_with_no_starter() {
    let Some(db) = TestDb::create(Some(41)).await else {
        return;
    };
    let workflow = insert_workflow(&db.pool).await;
    let row = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflow_executions (id, tenant_id, workflow_id, temporal_workflow_id, temporal_run_id, \
         input_params, status, current_state, started_at) \
         VALUES ($1, $2, $3, 't', 'r', '{\"topic\":\"rc cars\"}', 'running', 'START', NOW())",
    )
    .bind(row)
    .bind(TENANT)
    .bind(workflow)
    .execute(&db.pool)
    .await
    .unwrap();

    db.apply(41).await;

    let repo = PostgresWorkflowExecutionRepository::new(db.pool.clone());
    let read = repo
        .find_by_id_for_tenant(&TenantId::new(TENANT).unwrap(), ExecutionId(row))
        .await
        .unwrap()
        .expect("the old row is still there");
    assert_eq!(read.initiating_user_sub, None);
    assert_eq!(read.input, json!({"topic": "rc cars"}));
    assert_eq!(read.current_state.as_str(), "START");
    assert!(matches!(read.status, ExecutionStatus::Running));
    db.remove().await;
}

#[tokio::test]
async fn migration_041_run_again_changes_nothing() {
    let Some(db) = TestDb::create(None).await else {
        return;
    };
    let columns = || async {
        sqlx::query(
            "SELECT string_agg(column_name || ':' || data_type, ',' ORDER BY column_name) AS c \
             FROM information_schema.columns WHERE table_name = 'workflow_executions'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap()
        .get::<String, _>("c")
    };
    let before = columns().await;
    assert!(before.contains("initiating_user_sub:text"), "{before}");
    db.apply(41).await;
    assert_eq!(columns().await, before);
    db.remove().await;
}
