// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # PostgreSQL Volume Repository — ADR-025/032
//!
//! Production `VolumeRepository` implementation backed by the `volumes` table
//! in PostgreSQL via `sqlx`. Handles `StorageClass` serialisation, TTL
//! persistence for ephemeral volumes, and VolumeEvent reconstruction.
//!
//! PostgreSQL-backed `VolumeRepository` used when persistence is enabled.
//!
//! See ADR-025 (PostgreSQL Schema), ADR-032 (Unified Storage via SeaweedFS).

use crate::domain::repository::{RepositoryError, VolumeRepository};
use crate::domain::volume::{
    StorageClass, TenantId, Volume, VolumeBackend, VolumeId, VolumeOwnership, VolumeStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::PgPool;
use sqlx::Row;

pub struct PostgresVolumeRepository {
    pool: PgPool,
}

impl PostgresVolumeRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl VolumeRepository for PostgresVolumeRepository {
    async fn save(&self, volume: &Volume) -> Result<(), RepositoryError> {
        // Serialize complex types to JSONB
        let storage_class_json = serde_json::to_value(&volume.storage_class)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let backend_json = serde_json::to_value(&volume.backend)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let status_json = serde_json::to_value(volume.status)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let ownership_json = serde_json::to_value(&volume.ownership)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO volumes (
                id, name, tenant_id, storage_class, backend,
                size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (id) DO UPDATE SET
                storage_class = EXCLUDED.storage_class,
                backend = EXCLUDED.backend,
                size_limit_bytes = EXCLUDED.size_limit_bytes,
                status = EXCLUDED.status,
                ownership = EXCLUDED.ownership,
                attached_at = EXCLUDED.attached_at,
                detached_at = EXCLUDED.detached_at,
                expires_at = EXCLUDED.expires_at
            -- Note: name and tenant_id are NOT unique - multiple executions can use same logical name
            -- remote_path is unique and includes volume_id, ensuring true isolation
            "#
        )
        .bind(volume.id.0)
        .bind(&volume.name)
        .bind(volume.tenant_id.as_str())
        .bind(storage_class_json)
        .bind(backend_json)
        .bind(volume.size_limit_bytes as i64)
        .bind(status_json)
        .bind(ownership_json)
        .bind(volume.created_at)
        .bind(volume.attached_at)
        .bind(volume.detached_at)
        .bind(volume.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save volume: {e}")))?;

        Ok(())
    }

    async fn find_by_id(&self, id: VolumeId) -> Result<Option<Volume>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, name, tenant_id, storage_class, backend,
                size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE id = $1
            "#,
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let volume = parse_volume_row(row)?;
            Ok(Some(volume))
        } else {
            Ok(None)
        }
    }

    async fn find_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Volume>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, name, tenant_id, storage_class, backend,
                size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE tenant_id = $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut volumes = Vec::new();
        for row in rows {
            volumes.push(parse_volume_row(row)?);
        }
        Ok(volumes)
    }

    async fn find_expired(&self) -> Result<Vec<Volume>, RepositoryError> {
        let now = Utc::now();
        let rows = sqlx::query(
            r#"
            SELECT 
                id, name, tenant_id, storage_class, backend,
                size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE expires_at IS NOT NULL AND expires_at < $1
              AND status::text NOT IN ('deleted', 'deleting')
            ORDER BY expires_at ASC
            "#,
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut volumes = Vec::new();
        for row in rows {
            volumes.push(parse_volume_row(row)?);
        }
        Ok(volumes)
    }

    async fn find_by_ownership(
        &self,
        ownership: &VolumeOwnership,
    ) -> Result<Vec<Volume>, RepositoryError> {
        // Convert ownership to JSON for comparison
        let ownership_json = serde_json::to_value(ownership)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let rows = sqlx::query(
            r#"
            SELECT 
                id, name, tenant_id, storage_class, backend,
                size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE ownership @> $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(ownership_json)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut volumes = Vec::new();
        for row in rows {
            volumes.push(parse_volume_row(row)?);
        }
        Ok(volumes)
    }

    async fn delete(&self, id: VolumeId) -> Result<(), RepositoryError> {
        let result = sqlx::query(
            r#"
            DELETE FROM volumes
            WHERE id = $1
            "#,
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!("Volume {id} not found")));
        }

        Ok(())
    }
}

/// Parse a volume from a database row
fn parse_volume_row(row: sqlx::postgres::PgRow) -> Result<Volume, RepositoryError> {
    let id: uuid::Uuid = row.get("id");
    let name: String = row.get("name");
    let tenant_id: String = row.get("tenant_id");
    let storage_class: serde_json::Value = row.get("storage_class");
    let backend: serde_json::Value = row.get("backend");
    let size_limit_bytes: i64 = row.get("size_limit_bytes");
    let status: serde_json::Value = row.get("status");
    let ownership: serde_json::Value = row.get("ownership");
    let created_at: DateTime<Utc> = row.get("created_at");
    let attached_at: Option<DateTime<Utc>> = row.get("attached_at");
    let detached_at: Option<DateTime<Utc>> = row.get("detached_at");
    let expires_at: Option<DateTime<Utc>> = row.get("expires_at");

    let storage_class: StorageClass = serde_json::from_value(storage_class).map_err(|e| {
        RepositoryError::Serialization(format!("Failed to deserialize storage_class: {e}"))
    })?;

    let backend: VolumeBackend = serde_json::from_value(backend).map_err(|e| {
        RepositoryError::Serialization(format!("Failed to deserialize backend: {e}"))
    })?;

    let status: VolumeStatus = serde_json::from_value(status).map_err(|e| {
        RepositoryError::Serialization(format!("Failed to deserialize status: {e}"))
    })?;

    let ownership: VolumeOwnership = serde_json::from_value(ownership).map_err(|e| {
        RepositoryError::Serialization(format!("Failed to deserialize ownership: {e}"))
    })?;

    Ok(Volume {
        id: VolumeId(id),
        name,
        tenant_id: TenantId::from_string(&tenant_id)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?,
        storage_class,
        backend,
        size_limit_bytes: size_limit_bytes as u64,
        status,
        ownership,
        created_at,
        attached_at,
        detached_at,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    // Integration tests would go here (require PostgreSQL connection)
    // For now, we'll rely on unit tests in volume.rs and manual integration testing
}
