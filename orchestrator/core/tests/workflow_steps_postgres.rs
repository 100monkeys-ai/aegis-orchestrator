// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! AEGIS ADR-131 U32a against a real PostgreSQL, with the migrations the
//! daemon ships (`cli/migrations`, applied in order): the step executions of
//! a workflow execution are read each on its own, so a step whose row cannot
//! be read is answered as its id with the error, between the steps read
//! whole, in start order; the one-call query fails as a whole on that row,
//! which is why the goal judge reads the steps this way.
//!
//! CI starts a PostgreSQL and sets `AEGIS_TEST_POSTGRES_URL` to a database a
//! superuser can connect to. The test creates its own database there and
//! drops it at the end. In CI (`CI` set) a missing URL fails the test;
//! elsewhere it says it was skipped and passes.

use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::{Execution, ExecutionId, ExecutionInput};
use aegis_orchestrator_core::domain::repository::{ExecutionRepository, RepositoryError};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_execution::PostgresExecutionRepository;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use uuid::Uuid;

fn postgres_url() -> Option<String> {
    match std::env::var("AEGIS_TEST_POSTGRES_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ if std::env::var_os("CI").is_some() => {
            panic!("AEGIS_TEST_POSTGRES_URL is not set; in CI this test must reach PostgreSQL")
        }
        _ => {
            eprintln!("skipped: AEGIS_TEST_POSTGRES_URL is not set");
            None
        }
    }
}

/// A database of its own on the test server, migrated as the daemon
/// migrates its own.
async fn migrated_database(url: &str) -> (PgPool, String, PgPool) {
    let server = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("connect to the test PostgreSQL");
    let name = format!("aegis_steps_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&server)
        .await
        .expect("create the test database");
    let options: PgConnectOptions = url.parse::<PgConnectOptions>().unwrap().database(&name);
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("connect to the test database");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/migrations");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("read cli/migrations")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    files.sort();
    for file in files {
        let sql = std::fs::read_to_string(&file).unwrap();
        sqlx::raw_sql(&sql)
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("apply {}: {e}", file.display()));
    }
    (server, name, pool)
}

fn step(agent: AgentId, workflow_execution: Uuid, offset: i64) -> Execution {
    let input = ExecutionInput {
        intent: None,
        input: serde_json::json!({}),
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: Some(workflow_execution),
        attachments: Vec::new(),
    };
    let mut exec = Execution::new_with_id(ExecutionId::new(), agent, input, 5, "default".into());
    exec.tenant_id = TenantId::system();
    exec.start();
    exec.started_at += chrono::Duration::seconds(offset);
    exec.start_iteration("the step".to_string()).unwrap();
    exec.complete_iteration("done".to_string());
    exec.complete();
    exec
}

/// U32a: three steps of one workflow execution, the middle one's row
/// unreadable (its iterations no longer a list): the steps are answered in start
/// order, the middle one as its id with the error, the others whole.
#[tokio::test]
async fn an_unreadable_step_row_is_answered_as_its_id_with_the_error_between_the_others() {
    let Some(url) = postgres_url() else {
        return;
    };
    let (server, name, pool) = migrated_database(&url).await;
    let tenant = TenantId::system();
    let agent = AgentId::new();
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, name, manifest_yaml, manifest_json, runtime, \
         security_policy) VALUES ($1, $2, 'step-agent', '', '{}', 'python:3.11', '{}')",
    )
    .bind(agent.0)
    .bind(tenant.as_str())
    .execute(&pool)
    .await
    .expect("insert the steps' agent");
    let repo = PostgresExecutionRepository::new(pool.clone());
    let workflow_execution = Uuid::new_v4();
    let steps = [
        step(agent, workflow_execution, 1),
        step(agent, workflow_execution, 2),
        step(agent, workflow_execution, 3),
    ];
    // Stored latest first: the order read is the steps' start order.
    for exec in steps.iter().rev() {
        repo.save_for_tenant(&tenant, exec).await.unwrap();
    }
    sqlx::query(r#"UPDATE executions SET iterations = '{"not": "a list"}'::jsonb WHERE id = $1"#)
        .bind(steps[1].id.0)
        .execute(&pool)
        .await
        .unwrap();

    let whole = repo
        .find_by_workflow_execution_for_tenant(&tenant, workflow_execution)
        .await;
    let read = repo
        .read_steps_of_workflow_execution_for_tenant(&tenant, workflow_execution)
        .await;
    let shown: Vec<String> = match &read {
        Ok(steps) => steps
            .iter()
            .map(|step| match step {
                Ok(exec) => format!("read {}", exec.id),
                Err((id, error)) => format!("unread {id}: {error}"),
            })
            .collect(),
        Err(error) => vec![format!("the call failed: {error}")],
    };
    println!(
        "U32a over PostgreSQL: the one-call query answers {}; the steps read one by one: {shown:#?}",
        match &whole {
            Ok(steps) => format!("{} steps", steps.len()),
            Err(error) => format!("an error for them all: {error}"),
        }
    );

    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&server)
        .await
        .expect("drop the test database");

    let mut complaints = Vec::new();
    match read {
        Ok(read) => {
            if read.len() != 3 {
                complaints.push(format!("{} steps answered, not 3", read.len()));
            }
            for (index, (answered, stored)) in read.iter().zip(&steps).enumerate() {
                match (index, answered) {
                    (1, Err((id, RepositoryError::Serialization(_)))) if *id == stored.id => {}
                    (1, other) => complaints.push(format!(
                        "the unreadable step is answered {:?}, not its id with the \
                         serialization error",
                        other.as_ref().map(|e| e.id)
                    )),
                    (_, Ok(exec)) if exec.id == stored.id => {}
                    (_, other) => complaints.push(format!(
                        "step {index} is answered {:?}, not read whole as {}",
                        other
                            .as_ref()
                            .map(|e| e.id)
                            .map_err(|(id, e)| (id, e.to_string())),
                        stored.id
                    )),
                }
            }
        }
        Err(error) => complaints.push(format!(
            "the steps were not read one by one: the call failed as a whole: {error}"
        )),
    }
    assert!(complaints.is_empty(), "U32a: {complaints:#?}");
}
