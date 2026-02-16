// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use anyhow::Result;
use crate::domain::repository::{WorkflowRepository, RepositoryError};
use crate::domain::workflow::{Workflow, WorkflowId};
use crate::infrastructure::workflow_parser::WorkflowParser;

use sha2::{Sha256, Digest};
use std::fmt::Write;

pub struct PostgresWorkflowRepository {
    pool: PgPool,
}

impl PostgresWorkflowRepository {
    pub fn new(_connection_string: String) -> Self {
        unimplemented!("Use new_with_pool instad")
    }

    pub fn new_with_pool(pool: PgPool) -> Self {
        Self { pool }
    }
    
    fn compute_hash(json: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        hasher.update(json.to_string().as_bytes());
        let result = hasher.finalize();
        let mut hex_string = String::new();
        for byte in result {
            let _ = write!(hex_string, "{:02x}", byte);
        }
        hex_string
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
        .bind(&version)
        .bind(&description)
        .bind(yaml_source)
        .bind(&definition_json)
        .bind(&temporal_def_json)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save to workflows: {}", e)))?;

        // Also save to shared workflow_definitions table for TypeScript worker
        let def_hash = Self::compute_hash(&temporal_def_json);
        
        sqlx::query(
            r#"
            INSERT INTO workflow_definitions (workflow_id, name, definition, definition_hash, registered_at)
            VALUES ($1, $2, $3, $4, NOW())
            ON CONFLICT (workflow_id) DO UPDATE SET
                name = EXCLUDED.name,
                definition = EXCLUDED.definition,
                definition_hash = EXCLUDED.definition_hash,
                registered_at = NOW()
            "#
        )
        .bind(workflow.id.0)
        .bind(&workflow.metadata.name)
        .bind(&temporal_def_json)
        .bind(def_hash)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save to workflow_definitions: {}", e)))?;

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


