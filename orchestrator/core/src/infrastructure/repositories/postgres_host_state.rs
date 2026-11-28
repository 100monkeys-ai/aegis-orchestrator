// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL store of host states (ADR-157 D2; migration 066).

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use crate::domain::host_state::{
    HostState, HostStateManifest, HostStateRepository, HostStateStatus,
};
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::TenantId;

pub const ARCHIVE_ACTIVE_SQL: &str = "UPDATE host_states SET status = 'archived' \
     WHERE tenant_id = $1 AND name = $2 AND status = 'active'";
pub const NEXT_VERSION_SQL: &str =
    "SELECT COALESCE(MAX(version), 0) + 1 FROM host_states WHERE tenant_id = $1 AND name = $2";
pub const INSERT_SQL: &str = "INSERT INTO host_states \
     (id, tenant_id, name, version, status, manifest_yaml, spec, created_by, created_at) \
     VALUES ($1, $2, $3, $4, 'active', $5, $6, $7, now()) \
     RETURNING created_at";
const SELECT_COLUMNS: &str =
    "id, tenant_id, name, version, status, manifest_yaml, created_by, created_at";

pub struct PgHostStateRepository {
    pool: PgPool,
}

impl PgHostStateRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn db(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Database(e.to_string())
}

fn row_to_state(r: &sqlx::postgres::PgRow) -> Result<HostState, RepositoryError> {
    let yaml: String = r.get("manifest_yaml");
    let manifest: HostStateManifest =
        serde_yaml::from_str(&yaml).map_err(|e| RepositoryError::Serialization(e.to_string()))?;
    let tenant: String = r.get("tenant_id");
    Ok(HostState {
        id: r.get("id"),
        tenant_id: TenantId::from_string(&tenant)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?,
        name: r.get("name"),
        version: r.get("version"),
        status: if r.get::<String, _>("status") == "active" {
            HostStateStatus::Active
        } else {
            HostStateStatus::Archived
        },
        manifest_yaml: yaml,
        manifest,
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
    })
}

#[async_trait]
impl HostStateRepository for PgHostStateRepository {
    async fn save_version(
        &self,
        tenant: &TenantId,
        manifest_yaml: &str,
        manifest: &HostStateManifest,
        created_by: &str,
    ) -> Result<HostState, RepositoryError> {
        let name = &manifest.metadata.name;
        let spec = serde_json::to_value(&manifest.spec)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(ARCHIVE_ACTIVE_SQL)
            .bind(tenant.as_str())
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let version: i32 = sqlx::query_scalar(NEXT_VERSION_SQL)
            .bind(tenant.as_str())
            .bind(name)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        let id = uuid::Uuid::new_v4();
        let created_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(INSERT_SQL)
            .bind(id)
            .bind(tenant.as_str())
            .bind(name)
            .bind(version)
            .bind(manifest_yaml)
            .bind(spec)
            .bind(created_by)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(HostState {
            id,
            tenant_id: tenant.clone(),
            name: name.clone(),
            version,
            status: HostStateStatus::Active,
            manifest_yaml: manifest_yaml.to_string(),
            manifest: manifest.clone(),
            created_by: created_by.to_string(),
            created_at,
        })
    }

    async fn get_active(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<Option<HostState>, RepositoryError> {
        let row = sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM host_states WHERE tenant_id = $1 AND name = $2 AND status = 'active'"
        ))
        .bind(tenant.as_str())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        row.as_ref().map(row_to_state).transpose()
    }

    async fn list_active(&self, tenant: &TenantId) -> Result<Vec<HostState>, RepositoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM host_states WHERE tenant_id = $1 AND status = 'active' ORDER BY name"
        ))
        .bind(tenant.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.iter().map(row_to_state).collect()
    }

    async fn archive(&self, tenant: &TenantId, name: &str) -> Result<bool, RepositoryError> {
        let done = sqlx::query(ARCHIVE_ACTIVE_SQL)
            .bind(tenant.as_str())
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(done.rows_affected() > 0)
    }
}
