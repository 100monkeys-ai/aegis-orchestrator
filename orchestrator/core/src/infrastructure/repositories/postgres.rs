use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use anyhow::Result;
use crate::domain::repository::{WorkflowRepository, RepositoryError};
use crate::domain::workflow::{Workflow, WorkflowId};
use crate::infrastructure::workflow_parser::WorkflowParser;

pub struct PostgresWorkflowRepository {
    pool: PgPool,
}

impl PostgresWorkflowRepository {
    pub fn new(_connection_string: String) -> Self {
        // NOTE: This assumes connection_string isn't needed here if we pass the pool, 
        // but the factory interface passes connection string.
        // For simplicity, let's change the factory to panic or fix it properly later. 
        // Ideally we should reuse the pool.
        // But `create_workflow_repository` factory in `repository.rs` takes a `StorageBackend` enum which has config.
        // We will need to create a pool here.
        // BLOCKING ISSUE: Creating a pool is async, but `create_workflow_repository` is synchronous.
        // We might need to change how repositories are created or pass a lazy pool.
        
        // However, `server.rs` already creates a pool.
        // Let's assume for now we construct it with a pool, but we need to change standard factory pattern or just let binding layer handle it.
        // For this file, let's provide a constructor that takes the pool, and we'll fix the factory usage or simply instantiate it directly in `server.rs`.
        unimplemented!("Use new_with_pool instad")
    }

    pub fn new_with_pool(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WorkflowRepository for PostgresWorkflowRepository {
    async fn save(&self, workflow: &Workflow) -> Result<(), RepositoryError> {
        let definition_json = serde_json::to_value(workflow)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        
        let yaml_source = WorkflowParser::to_yaml(workflow)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        // Generate Temporal definition
        let temporal_def = crate::application::temporal_mapper::TemporalWorkflowMapper::to_temporal_definition(workflow)
            .map_err(|e| RepositoryError::Serialization(format!("Failed to map temporal definition: {}", e)))?;
        let temporal_def_json = serde_json::to_value(temporal_def)
            .map_err(|e| RepositoryError::Serialization(format!("Failed to serialize temporal definition: {}", e)))?;
        
        // Version 
        let version = workflow.metadata.version.clone().unwrap_or_else(|| "0.0.1".to_string());
        let description = workflow.metadata.description.clone();

        sqlx::query(
            r#"
            INSERT INTO workflows (id, name, version, description, yaml_source, domain_json, temporal_def_json, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
            ON CONFLICT (name) DO UPDATE SET
                version = EXCLUDED.version,
                description = EXCLUDED.description,
                yaml_source = EXCLUDED.yaml_source,
                domain_json = EXCLUDED.domain_json,
                temporal_def_json = EXCLUDED.temporal_def_json,
                updated_at = NOW()
            "#
        )
        .bind(workflow.id.0)
        .bind(&workflow.metadata.name)
        .bind(version)
        .bind(description)
        .bind(yaml_source)
        .bind(definition_json)
        .bind(temporal_def_json)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    async fn find_by_id(&self, id: WorkflowId) -> Result<Option<Workflow>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT domain_json
            FROM workflows
            WHERE id = $1
            "#
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row.try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            Ok(Some(workflow))
        } else {
            Ok(None)
        }
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<Workflow>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT domain_json
            FROM workflows
            WHERE name = $1
            "#
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row.try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            Ok(Some(workflow))
        } else {
            Ok(None)
        }
    }

    async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT domain_json
            FROM workflows
            ORDER BY name ASC
            "#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut workflows = Vec::new();
        for row in rows {
            let domain_json: serde_json::Value = row.try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            workflows.push(workflow);
        }
        Ok(workflows)
    }

    async fn delete(&self, id: WorkflowId) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            DELETE FROM workflows
            WHERE id = $1
            "#
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }
}


