// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL store of webhook sources (ADR-157 D4; migration 068).

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::TenantId;
use crate::domain::webhook_source::{WebhookSource, WebhookSourceRepository, WebhookSourceSpec};

pub const UPSERT_SQL: &str = "INSERT INTO webhook_sources (tenant_id, name, spec, owner, created_by, created_at) \
     VALUES ($1, $2, $3, $4, $5, now()) \
     ON CONFLICT (tenant_id, name) DO UPDATE SET spec = EXCLUDED.spec, owner = EXCLUDED.owner, created_by = EXCLUDED.created_by \
     RETURNING created_at";
pub const DELETE_SQL: &str = "DELETE FROM webhook_sources WHERE tenant_id = $1 AND name = $2";

pub struct PgWebhookSourceRepository {
    pool: PgPool,
}

impl PgWebhookSourceRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn db(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Database(e.to_string())
}

fn row_to_source(r: &sqlx::postgres::PgRow) -> Result<WebhookSource, RepositoryError> {
    let tenant: String = r.get("tenant_id");
    let spec: serde_json::Value = r.get("spec");
    let owner: serde_json::Value = r.get("owner");
    Ok(WebhookSource {
        tenant_id: TenantId::from_string(&tenant)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?,
        name: r.get("name"),
        spec: serde_json::from_value::<WebhookSourceSpec>(spec)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?,
        owner: serde_json::from_value(owner)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?,
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
    })
}

#[async_trait]
impl WebhookSourceRepository for PgWebhookSourceRepository {
    async fn save(
        &self,
        tenant: &TenantId,
        name: &str,
        spec: &WebhookSourceSpec,
        owner: &crate::domain::schedule::ScheduleOwner,
        created_by: &str,
    ) -> Result<WebhookSource, RepositoryError> {
        let json = serde_json::to_value(spec)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        let owner_json = serde_json::to_value(owner)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        let created_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(UPSERT_SQL)
            .bind(tenant.as_str())
            .bind(name)
            .bind(json)
            .bind(owner_json)
            .bind(created_by)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        Ok(WebhookSource {
            tenant_id: tenant.clone(),
            name: name.to_string(),
            spec: spec.clone(),
            owner: owner.clone(),
            created_by: created_by.to_string(),
            created_at,
        })
    }

    async fn get(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<Option<WebhookSource>, RepositoryError> {
        let row = sqlx::query(
            "SELECT tenant_id, name, spec, owner, created_by, created_at FROM webhook_sources WHERE tenant_id = $1 AND name = $2",
        )
        .bind(tenant.as_str())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.as_ref().map(row_to_source).transpose()
    }

    async fn list(&self, tenant: &TenantId) -> Result<Vec<WebhookSource>, RepositoryError> {
        let rows = sqlx::query(
            "SELECT tenant_id, name, spec, owner, created_by, created_at FROM webhook_sources WHERE tenant_id = $1 ORDER BY name",
        )
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(row_to_source).collect()
    }

    async fn delete(&self, tenant: &TenantId, name: &str) -> Result<bool, RepositoryError> {
        let done = sqlx::query(DELETE_SQL)
            .bind(tenant.as_str())
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(done.rows_affected() > 0)
    }

    async fn tenants_for(&self, name: &str) -> Result<Vec<TenantId>, RepositoryError> {
        let rows =
            sqlx::query("SELECT tenant_id FROM webhook_sources WHERE name = $1 ORDER BY tenant_id")
                .bind(name)
                .fetch_all(&self.pool)
                .await
                .map_err(db)?;
        rows.iter()
            .map(|r| {
                TenantId::from_string(&r.get::<String, _>("tenant_id"))
                    .map_err(|e| RepositoryError::Serialization(e.to_string()))
            })
            .collect()
    }
}
