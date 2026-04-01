// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres contract test for workflow registration persistence.
//!
//! Redeploying the same tenant/name/version must preserve the canonical
//! workflow UUID across both `workflows` and `workflow_definitions`.

use aegis_orchestrator_core::domain::repository::WorkflowRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowId,
    WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::repositories::postgres_workflow::PostgresWorkflowRepository;
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

async fn setup_temp_tables(pool: &PgPool) {
    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS workflows (
            id UUID PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            description TEXT,
            yaml_source TEXT NOT NULL,
            domain_json JSONB NOT NULL,
            temporal_def_json JSONB NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            CONSTRAINT workflows_tenant_name_version_unique UNIQUE (tenant_id, name, version)
        )
        "#,
    )
    .execute(pool)
    .await
    .expect("Failed to create temp workflows table");

    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS workflow_definitions (
            workflow_id UUID PRIMARY KEY,
            tenant_id TEXT NOT NULL,
            name TEXT NOT NULL,
            definition JSONB NOT NULL,
            registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            version TEXT NOT NULL,
            definition_hash TEXT NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .expect("Failed to create temp workflow_definitions table");

    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_temp_workflow_definitions_tenant_name_version
        ON workflow_definitions (tenant_id, name, version)
        "#,
    )
    .execute(pool)
    .await
    .expect("Failed to create temp workflow_definitions unique index");
}

fn build_workflow_with_description(name: &str, description: Option<&str>) -> Workflow {
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

    let description_owned = description.map(|d| d.to_string());

    Workflow::new(
        WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: description_owned,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
            volumes: vec![],
            storage: None,
        },
    )
    .unwrap()
}

fn build_workflow(name: &str) -> Workflow {
    build_workflow_with_description(name, Some("initial description"))
}

#[tokio::test]
async fn workflow_repository_force_redeploy_keeps_workflow_tables_on_same_id() {
    let Some(pool) = connect_test_pool().await else {
        eprintln!(
            "Skipping workflow registration contract test: DATABASE_URL/AEGIS_DATABASE_URL not set or unreachable"
        );
        return;
    };

    setup_temp_tables(&pool).await;

    let tenant_id = TenantId::local_default();
    let original = build_workflow("copy-generator");
    let original_id = original.id;

    let mut redeployed = original.clone();
    redeployed.id = WorkflowId::new();
    redeployed.metadata.description = Some("updated description".to_string());

    let repo = PostgresWorkflowRepository::new_with_pool(pool.clone());
    repo.save_for_tenant(&tenant_id, &original)
        .await
        .expect("Initial workflow save should succeed");
    repo.save_for_tenant(&tenant_id, &redeployed)
        .await
        .expect("Redeployed workflow save should succeed");

    let row = sqlx::query(
        r#"
        SELECT
            w.id::text AS workflow_id,
            w.domain_json ->> 'id' AS domain_workflow_id,
            w.temporal_def_json ->> 'workflow_id' AS temporal_workflow_id,
            wd.workflow_id::text AS definition_workflow_id,
            wd.definition ->> 'workflow_id' AS definition_payload_workflow_id,
            w.description AS description
        FROM workflows w
        JOIN workflow_definitions wd
          ON wd.tenant_id = w.tenant_id
         AND wd.name = w.name
         AND wd.version = w.version
        WHERE w.tenant_id = $1 AND w.name = $2 AND w.version = $3
        "#,
    )
    .bind(tenant_id.as_str())
    .bind(&original.metadata.name)
    .bind(
        original
            .metadata
            .version
            .as_deref()
            .expect("workflow version should be present"),
    )
    .fetch_one(&pool)
    .await
    .expect("Persisted workflow row not found");

    let workflow_id: String = row.get("workflow_id");
    let domain_workflow_id: String = row.get("domain_workflow_id");
    let temporal_workflow_id: String = row.get("temporal_workflow_id");
    let definition_workflow_id: String = row.get("definition_workflow_id");
    let definition_payload_workflow_id: String = row.get("definition_payload_workflow_id");
    let description: Option<String> = row.get("description");

    assert_eq!(workflow_id, original_id.to_string());
    assert_eq!(domain_workflow_id, original_id.to_string());
    assert_eq!(temporal_workflow_id, original_id.to_string());
    assert_eq!(definition_workflow_id, original_id.to_string());
    assert_eq!(definition_payload_workflow_id, original_id.to_string());
    assert_eq!(description, Some("updated description".to_string()));
    assert_ne!(redeployed.id.to_string(), original_id.to_string());
}

#[tokio::test]
async fn workflow_repository_force_redeploy_can_clear_description() {
    let pool = match connect_test_pool().await {
        Some(pool) => pool,
        None => {
            eprintln!("Skipping test: AEGIS_DATABASE_URL or DATABASE_URL not set or unreachable");
            return;
        }
    };

    setup_temp_tables(&pool).await;

    let tenant_id = TenantId::new("test-tenant-clear-description").unwrap();
    let repo = PostgresWorkflowRepository::new_with_pool(pool.clone());

    let original =
        build_workflow_with_description("clear-desc-workflow", Some("initial description"));
    let original_id = original.id;

    let mut redeployed = build_workflow_with_description("clear-desc-workflow", None);
    redeployed.id = WorkflowId::new();

    repo.save_for_tenant(&tenant_id, &original)
        .await
        .expect("Initial workflow save should succeed");
    repo.save_for_tenant(&tenant_id, &redeployed)
        .await
        .expect("Redeployed workflow save should succeed");

    let row = sqlx::query(
        r#"
        SELECT
            w.id::text AS workflow_id,
            w.domain_json ->> 'id' AS domain_workflow_id,
            w.temporal_def_json ->> 'workflow_id' AS temporal_workflow_id,
            wd.workflow_id::text AS definition_workflow_id,
            wd.definition ->> 'workflow_id' AS definition_payload_workflow_id,
            w.description AS description
        FROM workflows w
        JOIN workflow_definitions wd
          ON wd.tenant_id = w.tenant_id
         AND wd.name = w.name
         AND wd.version = w.version
        WHERE w.tenant_id = $1 AND w.name = $2 AND w.version = $3
        "#,
    )
    .bind(tenant_id.as_str())
    .bind(&original.metadata.name)
    .bind(
        original
            .metadata
            .version
            .as_deref()
            .expect("workflow version should be present"),
    )
    .fetch_one(&pool)
    .await
    .expect("Persisted workflow row not found");

    let workflow_id: String = row.get("workflow_id");
    let domain_workflow_id: String = row.get("domain_workflow_id");
    let temporal_workflow_id: String = row.get("temporal_workflow_id");
    let definition_workflow_id: String = row.get("definition_workflow_id");
    let definition_payload_workflow_id: String = row.get("definition_payload_workflow_id");
    let description: Option<String> = row.get("description");

    assert_eq!(workflow_id, original_id.to_string());
    assert_eq!(domain_workflow_id, original_id.to_string());
    assert_eq!(temporal_workflow_id, original_id.to_string());
    assert_eq!(definition_workflow_id, original_id.to_string());
    assert_eq!(definition_payload_workflow_id, original_id.to_string());
    assert_eq!(description, None);
    assert_ne!(redeployed.id.to_string(), original_id.to_string());
}
