// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Agent Repository — ADR-025
//!
//! Production `AgentRepository` implementation backed by the `agents` table
//! in PostgreSQL via `sqlx`. Translates between the `Agent` domain aggregate
//! and the `agents` / `agent_manifests` relational schema.
//!
//! PostgreSQL-backed `AgentRepository` used when persistence is enabled.
//!
//! See ADR-025 (PostgreSQL Schema Design and Migration Strategy).

use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
use crate::domain::repository::{AgentRepository, RepositoryError};
use crate::domain::tenant::TenantId;
use anyhow::Result;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;

pub struct PostgresAgentRepository {
    pool: PgPool,
}

impl PostgresAgentRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AgentRepository for PostgresAgentRepository {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent: &Agent,
    ) -> Result<(), RepositoryError> {
        let manifest_json = serde_json::to_value(&agent.manifest)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let manifest_yaml = serde_yaml::to_string(&agent.manifest)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let status_str = match agent.status {
            AgentStatus::Active => "active",
            AgentStatus::Paused => "paused",
            AgentStatus::Archived => "archived",
            AgentStatus::Failed => "active", // Table stores active/paused/archived states only.
        };

        // Extract security policy (from spec.security)
        let security_policy = serde_json::to_value(&agent.manifest.spec.security)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO agents (
                id, tenant_id, name, version, manifest_yaml, manifest_json,
                runtime, timeout_seconds, security_policy, status,
                description, tags, created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            ON CONFLICT (id) DO UPDATE SET
                tenant_id = EXCLUDED.tenant_id,
                name = EXCLUDED.name,
                version = EXCLUDED.version,
                manifest_yaml = EXCLUDED.manifest_yaml,
                manifest_json = EXCLUDED.manifest_json,
                runtime = EXCLUDED.runtime,
                timeout_seconds = EXCLUDED.timeout_seconds,
                security_policy = EXCLUDED.security_policy,
                status = EXCLUDED.status,
                description = EXCLUDED.description,
                tags = EXCLUDED.tags,
                updated_at = EXCLUDED.updated_at
            "#,
        )
        .bind(agent.id.0)
        .bind(tenant_id.as_str())
        .bind(&agent.name)
        .bind(agent.manifest.metadata.version.clone())
        .bind(&manifest_yaml)
        .bind(&manifest_json)
        .bind(agent.manifest.runtime_string())
        .bind(300_i32) // Default timeout, can be extracted from spec.security.resources.timeout if present
        .bind(security_policy)
        .bind(status_str)
        .bind(agent.manifest.metadata.description.as_deref())
        .bind(&agent.manifest.metadata.tags)
        .bind(agent.created_at)
        .bind(agent.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save agent: {e}")))?;

        // Append to agent_versions history (append-only log of all deployed versions)
        sqlx::query(
            r#"
            INSERT INTO agent_versions (agent_id, tenant_id, version, manifest_yaml, manifest_json)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (agent_id, version) DO UPDATE SET
                manifest_yaml = EXCLUDED.manifest_yaml,
                manifest_json = EXCLUDED.manifest_json,
                created_at = now()
            "#,
        )
        .bind(agent.id.0)
        .bind(tenant_id.as_str())
        .bind(&agent.manifest.metadata.version)
        .bind(&manifest_yaml)
        .bind(&manifest_json)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("Failed to save agent version history: {e}"))
        })?;

        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<Option<Agent>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            WHERE tenant_id = $1 AND id = $2
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let id: uuid::Uuid = row.get("id");
            let name: String = row.get("name");
            let manifest_val: serde_json::Value = row.get("manifest_json");
            let status_str: String = row.get("status");
            let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
            let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");

            let status = match status_str.as_str() {
                "active" => AgentStatus::Active,
                "paused" => AgentStatus::Paused,
                "archived" => AgentStatus::Archived,
                _ => AgentStatus::Active,
            };

            let manifest: AgentManifest = serde_json::from_value(manifest_val).map_err(|e| {
                RepositoryError::Serialization(format!("Failed to deserialize manifest: {e}"))
            })?;

            Ok(Some(Agent {
                id: AgentId(id),
                tenant_id: tenant_id.clone(),
                name,
                manifest,
                status,
                created_at,
                updated_at,
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            WHERE tenant_id = $1 AND name = $2
            ORDER BY version DESC
            LIMIT 1
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let id: uuid::Uuid = row.get("id");
            let name: String = row.get("name");
            let manifest_val: serde_json::Value = row.get("manifest_json");
            let status_str: String = row.get("status");
            let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
            let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");

            let status = match status_str.as_str() {
                "active" => AgentStatus::Active,
                "paused" => AgentStatus::Paused,
                "archived" => AgentStatus::Archived,
                _ => AgentStatus::Active,
            };

            let manifest: AgentManifest = serde_json::from_value(manifest_val).map_err(|e| {
                RepositoryError::Serialization(format!("Failed to deserialize manifest: {e}"))
            })?;

            Ok(Some(Agent {
                id: AgentId(id),
                tenant_id: tenant_id.clone(),
                name,
                manifest,
                status,
                created_at,
                updated_at,
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
    ) -> Result<Option<Agent>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            WHERE tenant_id = $1 AND name = $2 AND version = $3
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(name)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let id: uuid::Uuid = row.get("id");
            let name: String = row.get("name");
            let manifest_val: serde_json::Value = row.get("manifest_json");
            let status_str: String = row.get("status");
            let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
            let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");

            let status = match status_str.as_str() {
                "active" => AgentStatus::Active,
                "paused" => AgentStatus::Paused,
                "archived" => AgentStatus::Archived,
                _ => AgentStatus::Active,
            };

            let manifest: AgentManifest = serde_json::from_value(manifest_val).map_err(|e| {
                RepositoryError::Serialization(format!("Failed to deserialize manifest: {e}"))
            })?;

            Ok(Some(Agent {
                id: AgentId(id),
                tenant_id: tenant_id.clone(),
                name,
                manifest,
                status,
                created_at,
                updated_at,
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Agent>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            WHERE tenant_id = $1
            ORDER BY name ASC
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut agents = Vec::new();
        for row in rows {
            let id: uuid::Uuid = row.get("id");
            let name: String = row.get("name");
            let manifest_val: serde_json::Value = row.get("manifest_json");
            let status_str: String = row.get("status");
            let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
            let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");

            let status = match status_str.as_str() {
                "active" => AgentStatus::Active,
                "paused" => AgentStatus::Paused,
                "archived" => AgentStatus::Archived,
                _ => AgentStatus::Active,
            };

            let manifest: AgentManifest = serde_json::from_value(manifest_val).map_err(|e| {
                RepositoryError::Serialization(format!("Failed to deserialize manifest: {e}"))
            })?;

            agents.push(Agent {
                id: AgentId(id),
                tenant_id: tenant_id.clone(),
                name,
                manifest,
                status,
                created_at,
                updated_at,
            });
        }
        Ok(agents)
    }

    async fn delete_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM agents WHERE tenant_id = $1 AND id = $2")
            .bind(tenant_id.as_str())
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn list_versions_for_tenant(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<crate::domain::repository::AgentVersion>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT id, version, created_at, manifest_yaml
            FROM agent_versions
            WHERE tenant_id = $1 AND agent_id = $2
            ORDER BY created_at DESC
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(agent_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut versions = Vec::new();
        for row in rows {
            let id: uuid::Uuid = row.get("id");
            let version: String = row.get("version");
            let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
            let manifest_yaml: String = row.get("manifest_yaml");
            versions.push(crate::domain::repository::AgentVersion {
                id,
                version,
                created_at,
                manifest_yaml,
            });
        }
        Ok(versions)
    }
}
