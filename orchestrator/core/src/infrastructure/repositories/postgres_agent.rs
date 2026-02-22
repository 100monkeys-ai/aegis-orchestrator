// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Agent Repository — ADR-025
//!
//! Production `AgentRepository` implementation backed by the `agents` table
//! in PostgreSQL via `sqlx`. Translates between the `Agent` domain aggregate
//! and the `agents` / `agent_manifests` relational schema.
//!
//! ⚠️ Phase 2 — In-memory repository (`InMemoryAgentRepository`) is used in
//! Phase 1. This implementation requires an active `PgPool` connection.
//!
//! See ADR-025 (PostgreSQL Schema Design and Migration Strategy).

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use anyhow::Result;
use crate::domain::repository::{AgentRepository, RepositoryError};
use crate::domain::agent::{Agent, AgentId, AgentStatus, AgentManifest};

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
    async fn save(&self, agent: &Agent) -> Result<(), RepositoryError> {
        let manifest_json = serde_json::to_value(&agent.manifest)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        
        let manifest_yaml = serde_yaml::to_string(&agent.manifest)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let status_str = match agent.status {
            AgentStatus::Active => "active",
            AgentStatus::Paused => "paused",
            AgentStatus::Archived => "archived",
            AgentStatus::Failed => "active", // Map Failed to active for now if table doesn't support it
        };

        // Extract security policy (from spec.security)
        let security_policy = serde_json::to_value(&agent.manifest.spec.security)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO agents (
                id, name, version, manifest_yaml, manifest_json, 
                runtime, timeout_seconds, security_policy, status, 
                created_at, updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (id) DO UPDATE SET
                name = EXCLUDED.name,
                version = EXCLUDED.version,
                manifest_yaml = EXCLUDED.manifest_yaml,
                manifest_json = EXCLUDED.manifest_json,
                runtime = EXCLUDED.runtime,
                timeout_seconds = EXCLUDED.timeout_seconds,
                security_policy = EXCLUDED.security_policy,
                status = EXCLUDED.status,
                updated_at = EXCLUDED.updated_at
            "#
        )
        .bind(agent.id.0)
        .bind(&agent.name)
        .bind(agent.manifest.metadata.version.clone())
        .bind(manifest_yaml)
        .bind(manifest_json)
        .bind(&agent.manifest.runtime_string())
        .bind(300 as i32) // Default timeout, can be extracted from spec.security.resources.timeout if present
        .bind(security_policy)
        .bind(status_str)
        .bind(agent.created_at)
        .bind(agent.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save agent: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: AgentId) -> Result<Option<Agent>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            WHERE id = $1
            "#
        )
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

            let manifest: AgentManifest = serde_json::from_value(manifest_val)
                .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize manifest: {}", e)))?;

            Ok(Some(Agent {
                id: AgentId(id),
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

    async fn find_by_name(&self, name: &str) -> Result<Option<Agent>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            WHERE name = $1
            "#
        )
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

            let manifest: AgentManifest = serde_json::from_value(manifest_val)
                .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize manifest: {}", e)))?;

            Ok(Some(Agent {
                id: AgentId(id),
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

    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, name, manifest_json, status, created_at, updated_at
            FROM agents
            ORDER BY name ASC
            "#
        )
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

            let manifest: AgentManifest = serde_json::from_value(manifest_val)
                .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize manifest: {}", e)))?;

            agents.push(Agent {
                id: AgentId(id),
                name,
                manifest,
                status,
                created_at,
                updated_at,
            });
        }
        Ok(agents)
    }

    async fn delete(&self, id: AgentId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
}
