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

use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentScope, AgentStatus};
use crate::domain::repository::{AgentListing, AgentRepository, RepositoryError, StatusFilter};
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

/// The columns every agent read selects.
const AGENT_COLUMNS: &str =
    "id, tenant_id, name, manifest_json, status, scope, created_at, updated_at";

/// An agent from a row of [`AGENT_COLUMNS`].
fn agent_of_row(row: &sqlx::postgres::PgRow) -> Result<Agent, RepositoryError> {
    let id: uuid::Uuid = row.get("id");
    let tenant_str: String = row.get("tenant_id");
    let tenant_id = TenantId::from_string(&tenant_str)
        .map_err(|e| RepositoryError::Serialization(format!("Invalid tenant_id in row: {e}")))?;
    let name: String = row.get("name");
    let manifest_val: serde_json::Value = row.get("manifest_json");
    let status_str: String = row.get("status");
    let scope_str: String = row.get("scope");
    let created_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
    let updated_at: chrono::DateTime<chrono::Utc> = row.get("updated_at");
    let status = match status_str.as_str() {
        "paused" => AgentStatus::Paused,
        "archived" => AgentStatus::Archived,
        _ => AgentStatus::Active,
    };
    let scope = match scope_str.as_str() {
        "global" => AgentScope::Global,
        _ => AgentScope::Tenant,
    };
    let manifest: AgentManifest = serde_json::from_value(manifest_val).map_err(|e| {
        RepositoryError::Serialization(format!("Failed to deserialize manifest: {e}"))
    })?;
    Ok(Agent {
        id: AgentId(id),
        tenant_id,
        scope,
        name,
        manifest,
        status,
        created_at,
        updated_at,
    })
}

/// The `agents.status` text of a status.
fn agent_status_str(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Active => "active",
        AgentStatus::Paused => "paused",
        AgentStatus::Archived => "archived",
        AgentStatus::Failed => "active", // Table stores active/paused/archived states only.
    }
}

/// The SQL predicate of a status filter on `agents.status`, bound at `$n`:
/// `active` reads every row not archived, so a paused agent stays listed.
fn agent_status_predicate(n: usize) -> String {
    format!(
        "(${n} = 'all' OR (${n} = 'active' AND status <> 'archived') OR (${n} = 'archived' AND status = 'archived'))"
    )
}

impl PostgresAgentRepository {
    async fn fetch_one(&self, sql: &str, binds: &[&str]) -> Result<Option<Agent>, RepositoryError> {
        let mut query = sqlx::query(sql);
        for bind in binds {
            query = query.bind(*bind);
        }
        let row = query
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        row.as_ref().map(agent_of_row).transpose()
    }

    async fn fetch_all(&self, sql: &str, binds: &[&str]) -> Result<Vec<Agent>, RepositoryError> {
        let mut query = sqlx::query(sql);
        for bind in binds {
            query = query.bind(*bind);
        }
        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        rows.iter().map(agent_of_row).collect()
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
                description, scope, created_at, updated_at
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
                scope = EXCLUDED.scope,
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
        .bind(agent.scope.as_db_str())
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
                manifest_json = EXCLUDED.manifest_json
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

    /// Any status: an archived row is read by its id (AEGIS ADR-148, A10).
    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<Option<Agent>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents WHERE tenant_id = $1 AND id = $2"
        ))
        .bind(tenant_id.as_str())
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        row.as_ref().map(agent_of_row).transpose()
    }

    /// The newest live row of the name by `created_at`, never by version
    /// text (AEGIS ADR-148).
    async fn find_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        self.fetch_one(
            &format!(
                r#"
                SELECT {AGENT_COLUMNS}
                FROM agents
                WHERE tenant_id = $1 AND name = $2 AND status <> 'archived'
                ORDER BY created_at DESC
                LIMIT 1
                "#
            ),
            &[tenant_id.as_str(), name],
        )
        .await
    }

    /// Any status: a version named exactly is read even when archived.
    async fn find_by_name_and_version_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        self.fetch_one(
            &format!(
                r#"
                SELECT {AGENT_COLUMNS}
                FROM agents
                WHERE tenant_id = $1 AND name = $2 AND version = $3
                ORDER BY created_at DESC
                LIMIT 1
                "#
            ),
            &[tenant_id.as_str(), name, version],
        )
        .await
    }

    async fn list_all_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Agent>, RepositoryError> {
        self.list_by_status(AgentListing::Tenant(tenant_id), StatusFilter::Active)
            .await
    }

    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError> {
        self.list_by_status(AgentListing::All, StatusFilter::Active)
            .await
    }

    async fn list_by_name_for_tenant(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Vec<Agent>, RepositoryError> {
        self.fetch_all(
            &format!(
                r#"
                SELECT {AGENT_COLUMNS}
                FROM agents
                WHERE tenant_id = $1 AND name = $2
                ORDER BY created_at DESC
                "#
            ),
            &[tenant_id.as_str(), name],
        )
        .await
    }

    async fn list_by_status(
        &self,
        listing: AgentListing<'_>,
        filter: StatusFilter,
    ) -> Result<Vec<Agent>, RepositoryError> {
        let status = filter.as_db_str();
        match listing {
            AgentListing::All => {
                self.fetch_all(
                    &format!(
                        "SELECT {AGENT_COLUMNS} FROM agents WHERE {} \
                         ORDER BY tenant_id ASC, name ASC, created_at DESC",
                        agent_status_predicate(1)
                    ),
                    &[status],
                )
                .await
            }
            AgentListing::Tenant(tenant_id) => {
                self.fetch_all(
                    &format!(
                        "SELECT {AGENT_COLUMNS} FROM agents WHERE tenant_id = $1 AND {} \
                         ORDER BY name ASC, created_at DESC",
                        agent_status_predicate(2)
                    ),
                    &[tenant_id.as_str(), status],
                )
                .await
            }
            AgentListing::Visible(tenant_id) => {
                self.fetch_all(
                    &format!(
                        "SELECT {AGENT_COLUMNS} FROM agents \
                         WHERE (tenant_id = $1 OR (scope = 'global' AND tenant_id = 'aegis-system')) \
                         AND {} ORDER BY name ASC, created_at DESC",
                        agent_status_predicate(2)
                    ),
                    &[tenant_id.as_str(), status],
                )
                .await
            }
        }
    }

    async fn set_status_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
        status: AgentStatus,
    ) -> Result<(), RepositoryError> {
        let result = sqlx::query(
            "UPDATE agents SET status = $3, updated_at = NOW() WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id.as_str())
        .bind(id.0)
        .bind(agent_status_str(&status))
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to set agent status: {e}")))?;
        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!(
                "Agent {} not found",
                id.0
            )));
        }
        Ok(())
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
            WHERE agent_id = $2
              AND (tenant_id = $1 OR tenant_id = 'aegis-system')
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

    async fn list_visible_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<Agent>, RepositoryError> {
        self.list_by_status(AgentListing::Visible(tenant_id), StatusFilter::Active)
            .await
    }

    async fn update_scope(
        &self,
        id: AgentId,
        new_scope: AgentScope,
        new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            r#"
            UPDATE agents SET scope = $1, tenant_id = $2, updated_at = NOW()
            WHERE id = $3
            "#,
        )
        .bind(new_scope.as_db_str())
        .bind(new_tenant_id.as_str())
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    async fn find_by_id_visible(
        &self,
        tenant_id: &TenantId,
        id: AgentId,
    ) -> Result<Option<Agent>, RepositoryError> {
        if let Some(agent) = self.find_by_id_for_tenant(tenant_id, id).await? {
            return Ok(Some(agent));
        }
        let system_tenant = TenantId::system();
        if tenant_id.as_str() != "aegis-system" {
            return self.find_by_id_for_tenant(&system_tenant, id).await;
        }
        Ok(None)
    }

    async fn count_active(&self, tenant_id: &TenantId) -> Result<u64, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*) AS cnt FROM agents
            WHERE tenant_id = $1 AND status = 'active'
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let count: i64 = row.get("cnt");
        Ok(count.max(0) as u64)
    }

    /// The live row of the name: the tenant's before a global one, newest by
    /// `created_at` (AEGIS ADR-148).
    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Agent>, RepositoryError> {
        self.fetch_one(
            &format!(
                r#"
                SELECT {AGENT_COLUMNS}
                FROM agents
                WHERE name = $2 AND status <> 'archived'
                  AND ((scope = 'tenant' AND tenant_id = $1)
                    OR (scope = 'global' AND tenant_id = 'aegis-system'))
                ORDER BY CASE WHEN scope='tenant' THEN 0 ELSE 1 END, created_at DESC
                LIMIT 1
                "#
            ),
            &[tenant_id.as_str(), name],
        )
        .await
    }
}
