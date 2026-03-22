// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres contract test for workflow execution persistence.
//!
//! This test exercises the real `PostgresWorkflowExecutionRepository` against
//! a temp schema so placeholder drift and identifier semantics show up as a
//! database-level failure instead of a silent mismatch.

use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
    WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_workflow_execution::PostgresWorkflowExecutionRepository;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::collections::HashMap;

async fn connect_test_pool() -> Option<PgPool> {
    let database_url = std::env::var("AEGIS_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;

    PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .ok()
}

fn build_workflow(name: &str) -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("START").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo start".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("END").unwrap(),
                feedback: None,
            }],
            timeout: None,
        },
    );
    states.insert(
        StateName::new("END").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo end".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );

    Workflow::new(
        WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
            volumes: vec![],
        },
    )
    .unwrap()
}

#[tokio::test]
async fn workflow_execution_repository_persists_expected_temporal_columns() {
    let Some(pool) = connect_test_pool().await else {
        eprintln!(
            "Skipping workflow execution repository contract test: DATABASE_URL/AEGIS_DATABASE_URL not set or unreachable"
        );
        return;
    };

    sqlx::query(
        r#"
        CREATE TEMP TABLE workflows (
            id UUID PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            name TEXT NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await
    .expect("Failed to create temp workflows table");

    sqlx::query(
        r#"
        CREATE TEMP TABLE workflow_executions (
            id UUID PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            workflow_id UUID NOT NULL,
            temporal_workflow_id TEXT NOT NULL,
            temporal_run_id TEXT NOT NULL,
            input_params JSONB NOT NULL,
            status TEXT NOT NULL,
            current_state TEXT NOT NULL,
            blackboard JSONB NOT NULL,
            state_outputs JSONB NOT NULL,
            state_history JSONB NOT NULL,
            started_at TIMESTAMPTZ NOT NULL,
            last_transition_at TIMESTAMPTZ NOT NULL,
            completed_at TIMESTAMPTZ
        )
        "#,
    )
    .execute(&pool)
    .await
    .expect("Failed to create temp workflow_executions table");

    let tenant_id = TenantId::local_default();
    let workflow = build_workflow("copy-generator");
    let execution = WorkflowExecution::new(
        &workflow,
        ExecutionId::new(),
        serde_json::json!({
            "topic": "copy",
            "style": "human"
        }),
    );

    sqlx::query(
        r#"
        INSERT INTO workflows (id, tenant_id, name)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(workflow.id.0)
    .bind(tenant_id.as_str())
    .bind(&workflow.metadata.name)
    .execute(&pool)
    .await
    .expect("Failed to seed temp workflows table");

    let repo = PostgresWorkflowExecutionRepository::new(pool.clone());
    let save_result = repo.save_for_tenant(&tenant_id, &execution).await;

    assert!(
        save_result.is_ok(),
        "workflow execution save should succeed once placeholder bindings are correct: {save_result:?}"
    );

    let row = sqlx::query(
        r#"
        SELECT
            workflow_id,
            temporal_workflow_id,
            temporal_run_id,
            input_params,
            status,
            current_state,
            blackboard,
            state_outputs,
            state_history
        FROM workflow_executions
        WHERE id = $1
        "#,
    )
    .bind(execution.id.0)
    .fetch_one(&pool)
    .await
    .expect("Persisted workflow execution row not found");

    let workflow_id: uuid::Uuid = row.get("workflow_id");
    let temporal_workflow_id: String = row.get("temporal_workflow_id");
    let temporal_run_id: String = row.get("temporal_run_id");
    let input_params: serde_json::Value = row.get("input_params");
    let status: String = row.get("status");
    let current_state: String = row.get("current_state");
    let blackboard: serde_json::Value = row.get("blackboard");
    let state_outputs: serde_json::Value = row.get("state_outputs");
    let state_history: serde_json::Value = row.get("state_history");

    assert_eq!(workflow_id, workflow.id.0);
    assert_eq!(
        temporal_workflow_id,
        execution.id.0.to_string(),
        "temporal_workflow_id should mirror the Temporal workflow ID used to start the workflow"
    );
    assert_eq!(
        temporal_run_id, "",
        "temporal_run_id should be empty until Temporal start returns a real run ID"
    );
    assert_eq!(
        input_params,
        serde_json::json!({
            "topic": "copy",
            "style": "human"
        })
    );
    assert_eq!(status, "running");
    assert_eq!(current_state, "START");
    assert_eq!(blackboard, serde_json::json!({}));
    assert_eq!(state_outputs, serde_json::json!({}));
    assert_eq!(state_history, serde_json::json!(["START"]));
}
