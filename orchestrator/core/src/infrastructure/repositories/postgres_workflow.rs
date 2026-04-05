// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL Workflow Repository
//!
//! Provides PostgreSQL-backed persistence for workflow definitions with versioning
//! and content-addressable storage via SHA-256 hashing.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Store and retrieve workflow FSM definitions
//! - **Integration:** Domain WorkflowRepository → PostgreSQL workflows table
//!
//! # Schema
//!
//! The `workflows` table stores:
//! - Workflow metadata (ID, name, version, description)
//! - Complete FSM definition (JSONB)
//! - YAML source representation
//! - Temporal workflow mapping (for execution engine integration)
//! - Content hash (SHA-256 for deduplication)
//!
//! # Features
//!
//! - **Version Management**: Multiple versions of the same workflow
//! - **Content Hashing**: SHA-256 based deduplication
//! - **Multi-Format Storage**: YAML source + JSON definition + Temporal mapping
//! - **Definition Storage**: Workflows can be versioned and updated as needed
//!
//! # Usage
//!
//! ```ignore
//! use sqlx::PgPool;
//! use repositories::PostgresWorkflowRepository;
//!
//! let pool = PgPool::connect(&database_url).await?;
//! let repo = PostgresWorkflowRepository::new_with_pool(pool);
//!
//! // Save workflow definition
//! repo.save(&workflow).await?;
//!
//! // Retrieve by ID
//! let workflow = repo.find_by_id(workflow_id).await?;
//! ```

use crate::domain::repository::{RepositoryError, WorkflowRepository};
use crate::domain::tenant::TenantId;
use crate::domain::workflow::{Workflow, WorkflowId, WorkflowScope};
use crate::infrastructure::workflow_parser::WorkflowParser;
use anyhow::Result;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;

use sha2::{Digest, Sha256};
use std::fmt::Write;

pub struct PostgresWorkflowRepository {
    pool: PgPool,
}

impl PostgresWorkflowRepository {
    pub fn new_with_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Recursively canonicalize a JSON value by sorting all object keys.
    fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                entries.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));

                let mut sorted_map = serde_json::Map::with_capacity(entries.len());
                for (key, val) in entries {
                    sorted_map.insert(key.clone(), Self::canonicalize_json(val));
                }

                serde_json::Value::Object(sorted_map)
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(Self::canonicalize_json).collect())
            }
            _ => value.clone(),
        }
    }

    fn compute_hash(json: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        let canonical = Self::canonicalize_json(json);
        hasher.update(canonical.to_string().as_bytes());
        let result = hasher.finalize();
        let mut hex_string = String::new();
        for byte in result {
            let _ = write!(hex_string, "{byte:02x}");
        }
        hex_string
    }
}

#[async_trait]
impl WorkflowRepository for PostgresWorkflowRepository {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
    ) -> Result<(), RepositoryError> {
        let version = workflow
            .metadata
            .version
            .clone()
            .unwrap_or_else(|| "0.0.1".to_string());

        let persisted_workflow_id = sqlx::query_scalar::<_, uuid::Uuid>(
            r#"
            SELECT id
            FROM workflows
            WHERE tenant_id = $1 AND name = $2 AND version = $3
              AND scope = $4
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(&workflow.metadata.name)
        .bind(&version)
        .bind(workflow.scope.as_db_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to load existing workflow: {e}")))?
        .map(WorkflowId)
        .unwrap_or(workflow.id);

        let mut persisted_workflow = workflow.clone();
        persisted_workflow.id = persisted_workflow_id;

        let definition_json = serde_json::to_value(&persisted_workflow)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let yaml_source = WorkflowParser::to_yaml(&persisted_workflow)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        // Generate Temporal definition
        let temporal_def =
            crate::application::temporal_mapper::TemporalWorkflowMapper::to_temporal_definition(
                &persisted_workflow,
                tenant_id,
            )
            .map_err(|e| {
                RepositoryError::Serialization(format!("Failed to map temporal definition: {e}"))
            })?;
        let temporal_def_json = serde_json::to_value(temporal_def).map_err(|e| {
            RepositoryError::Serialization(format!("Failed to serialize temporal definition: {e}"))
        })?;

        let description = workflow.metadata.description.clone();

        let scope_str = workflow.scope.as_db_str();

        sqlx::query(
            r#"
            INSERT INTO workflows (id, tenant_id, name, version, scope, description, tags, yaml_source, domain_json, temporal_def_json, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NOW())
            ON CONFLICT (id) DO UPDATE SET
                description = EXCLUDED.description,
                tags = EXCLUDED.tags,
                yaml_source = EXCLUDED.yaml_source,
                domain_json = EXCLUDED.domain_json,
                temporal_def_json = EXCLUDED.temporal_def_json,
                scope = EXCLUDED.scope,
                updated_at = NOW()
            "#
        )
        .bind(persisted_workflow.id.0)
        .bind(tenant_id.as_str())
        .bind(&workflow.metadata.name)
        .bind(&version)
        .bind(scope_str)
        .bind(&description)
        .bind(&workflow.metadata.tags)
        .bind(yaml_source)
        .bind(&definition_json)
        .bind(&temporal_def_json)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save to workflows: {e}")))?;

        // Also save to shared workflow_definitions table for TypeScript worker
        let def_hash = Self::compute_hash(&temporal_def_json);

        sqlx::query(
            r#"
            INSERT INTO workflow_definitions (workflow_id, tenant_id, name, version, scope, definition, definition_hash, registered_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())
            ON CONFLICT (tenant_id, name, version, scope) DO UPDATE SET
                workflow_id = EXCLUDED.workflow_id,
                definition = EXCLUDED.definition,
                definition_hash = EXCLUDED.definition_hash,
                scope = EXCLUDED.scope,
                registered_at = NOW()
            "#
        )
        .bind(persisted_workflow.id.0)
        .bind(tenant_id.as_str())
        .bind(&workflow.metadata.name)
        .bind(&version)
        .bind(scope_str)
        .bind(&temporal_def_json)
        .bind(def_hash)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save to workflow_definitions: {e}")))?;

        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE id = $2
              AND (tenant_id = $1 OR (scope = 'global' AND tenant_id = 'aegis-system'))
            ORDER BY CASE WHEN tenant_id = $1 THEN 0 ELSE 1 END
            LIMIT 1
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            Ok(Some(Workflow {
                updated_at,
                ..workflow
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE name = $2
              AND (tenant_id = $1 OR (scope = 'global' AND tenant_id = 'aegis-system'))
            ORDER BY
              CASE WHEN tenant_id = $1 THEN 0 ELSE 1 END,
              version DESC
            LIMIT 1
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            Ok(Some(Workflow {
                updated_at,
                ..workflow
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_by_name_and_version_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE name = $2 AND version = $3
              AND (tenant_id = $1 OR (scope = 'global' AND tenant_id = 'aegis-system'))
            ORDER BY CASE WHEN tenant_id = $1 THEN 0 ELSE 1 END
            LIMIT 1
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(name)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            Ok(Some(Workflow {
                updated_at,
                ..workflow
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE tenant_id = $1 AND name = $2
            ORDER BY updated_at DESC
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(name)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut workflows = Vec::new();
        for row in rows {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            workflows.push(Workflow {
                updated_at,
                ..workflow
            });
        }
        Ok(workflows)
    }

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE tenant_id = $1
            ORDER BY name ASC
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut workflows = Vec::new();
        for row in rows {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            workflows.push(Workflow {
                updated_at,
                ..workflow
            });
        }
        Ok(workflows)
    }

    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        // Two-level scope resolution: tenant (0) > global (1)
        let row = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE name = $1
              AND (
                  (scope = 'tenant' AND tenant_id = $2)
                  OR (scope = 'global' AND tenant_id = 'aegis-system')
              )
            ORDER BY
                CASE scope
                    WHEN 'tenant' THEN 0
                    WHEN 'global' THEN 1
                END,
                version DESC
            LIMIT 1
            "#,
        )
        .bind(name)
        .bind(tenant_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            Ok(Some(Workflow {
                updated_at,
                ..workflow
            }))
        } else {
            Ok(None)
        }
    }

    async fn resolve_by_name_and_version(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE name = $1 AND version = $2
              AND (
                  (scope = 'tenant' AND tenant_id = $3)
                  OR (scope = 'global' AND tenant_id = 'aegis-system')
              )
            ORDER BY
                CASE scope
                    WHEN 'tenant' THEN 0
                    WHEN 'global' THEN 1
                END
            LIMIT 1
            "#,
        )
        .bind(name)
        .bind(version)
        .bind(tenant_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            Ok(Some(Workflow {
                updated_at,
                ..workflow
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_visible(&self, tenant_id: &TenantId) -> Result<Vec<Workflow>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE
                (tenant_id = $1)
                OR (scope = 'global' AND tenant_id = 'aegis-system')
            ORDER BY name ASC, version DESC
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut workflows = Vec::new();
        for row in rows {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            workflows.push(Workflow {
                updated_at,
                ..workflow
            });
        }
        Ok(workflows)
    }

    async fn list_global(&self) -> Result<Vec<Workflow>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT domain_json, updated_at
            FROM workflows
            WHERE scope = 'global'
            ORDER BY name ASC, version DESC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut workflows = Vec::new();
        for row in rows {
            let domain_json: serde_json::Value = row
                .try_get("domain_json")
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let workflow: Workflow = serde_json::from_value(domain_json)
                .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
            let updated_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("updated_at").ok();
            workflows.push(Workflow {
                updated_at,
                ..workflow
            });
        }
        Ok(workflows)
    }

    async fn update_scope(
        &self,
        id: WorkflowId,
        new_scope: WorkflowScope,
        new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        let scope_str = new_scope.as_db_str();

        let mut tx =
            self.pool.begin().await.map_err(|e| {
                RepositoryError::Database(format!("Failed to begin transaction: {e}"))
            })?;

        sqlx::query(
            r#"
            UPDATE workflows
            SET scope = $1, tenant_id = $2, updated_at = NOW()
            WHERE id = $3
            "#,
        )
        .bind(scope_str)
        .bind(new_tenant_id.as_str())
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to update workflow scope: {e}")))?;

        sqlx::query(
            r#"
            UPDATE workflow_definitions
            SET scope = $1, tenant_id = $2
            WHERE workflow_id = $3
            "#,
        )
        .bind(scope_str)
        .bind(new_tenant_id.as_str())
        .bind(id.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("Failed to update workflow_definitions scope: {e}"))
        })?;

        tx.commit()
            .await
            .map_err(|e| RepositoryError::Database(format!("Failed to commit transaction: {e}")))?;

        Ok(())
    }

    async fn find_by_name_visible(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        if let Some(workflow) = self.find_by_name_for_tenant(tenant_id, name).await? {
            return Ok(Some(workflow));
        }
        let system_tenant = TenantId::system();
        if tenant_id.as_str() != "aegis-system" {
            return self.find_by_name_for_tenant(&system_tenant, name).await;
        }
        Ok(None)
    }

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: WorkflowId,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            DELETE FROM workflow_definitions
            WHERE tenant_id = $1 AND workflow_id = $2
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        sqlx::query(
            r#"
            DELETE FROM workflows
            WHERE tenant_id = $1 AND id = $2
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }
}
