// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Postgres Volume
//!
//! Provides postgres volume functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements postgres volume

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use crate::domain::repository::{VolumeRepository, RepositoryError};
use crate::domain::volume::{Volume, VolumeId, TenantId, VolumeStatus, StorageClass, FilerEndpoint, VolumeOwnership};
use chrono::{DateTime, Utc};

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
        
        let filer_endpoint_json = serde_json::to_value(&volume.filer_endpoint)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        
        let status_json = serde_json::to_value(&volume.status)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        
        let ownership_json = serde_json::to_value(&volume.ownership)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO volumes (
                id, name, tenant_id, storage_class, filer_endpoint,
                remote_path, size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            ON CONFLICT (id) DO UPDATE SET
                storage_class = EXCLUDED.storage_class,
                filer_endpoint = EXCLUDED.filer_endpoint,
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
        .bind(volume.tenant_id.0)
        .bind(storage_class_json)
        .bind(filer_endpoint_json)
        .bind(&volume.remote_path)
        .bind(volume.size_limit_bytes as i64)
        .bind(status_json)
        .bind(ownership_json)
        .bind(volume.created_at)
        .bind(volume.attached_at)
        .bind(volume.detached_at)
        .bind(volume.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save volume: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: VolumeId) -> Result<Option<Volume>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, name, tenant_id, storage_class, filer_endpoint,
                remote_path, size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE id = $1
            "#
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
                id, name, tenant_id, storage_class, filer_endpoint,
                remote_path, size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE tenant_id = $1
            ORDER BY created_at DESC
            "#
        )
        .bind(tenant_id.0)
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
                id, name, tenant_id, storage_class, filer_endpoint,
                remote_path, size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE expires_at IS NOT NULL AND expires_at < $1
              AND status NOT IN ('"deleted"', '"deleting"')
            ORDER BY expires_at ASC
            "#
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

    async fn find_by_ownership(&self, ownership: &VolumeOwnership) -> Result<Vec<Volume>, RepositoryError> {
        // Convert ownership to JSON for comparison
        let ownership_json = serde_json::to_value(ownership)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let rows = sqlx::query(
            r#"
            SELECT 
                id, name, tenant_id, storage_class, filer_endpoint,
                remote_path, size_limit_bytes, status, ownership,
                created_at, attached_at, detached_at, expires_at
            FROM volumes
            WHERE ownership @> $1
            ORDER BY created_at DESC
            "#
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
            "#
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(RepositoryError::NotFound(format!("Volume {} not found", id)));
        }

        Ok(())
    }
}

/// Parse a volume from a database row
fn parse_volume_row(row: sqlx::postgres::PgRow) -> Result<Volume, RepositoryError> {
    let id: uuid::Uuid = row.get("id");
    let name: String = row.get("name");
    let tenant_id: uuid::Uuid = row.get("tenant_id");
    let storage_class_val: serde_json::Value = row.get("storage_class");
    let filer_endpoint_val: serde_json::Value = row.get("filer_endpoint");
    let remote_path: String = row.get("remote_path");
    let size_limit_bytes: i64 = row.get("size_limit_bytes");
    let status_val: serde_json::Value = row.get("status");
    let ownership_val: serde_json::Value = row.get("ownership");
    let created_at: DateTime<Utc> = row.get("created_at");
    let attached_at: Option<DateTime<Utc>> = row.get("attached_at");
    let detached_at: Option<DateTime<Utc>> = row.get("detached_at");
    let expires_at: Option<DateTime<Utc>> = row.get("expires_at");

    let storage_class: StorageClass = serde_json::from_value(storage_class_val)
        .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize storage_class: {}", e)))?;
    
    let filer_endpoint: FilerEndpoint = serde_json::from_value(filer_endpoint_val)
        .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize filer_endpoint: {}", e)))?;
    
    let status: VolumeStatus = serde_json::from_value(status_val)
        .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize status: {}", e)))?;
    
    let ownership: VolumeOwnership = serde_json::from_value(ownership_val)
        .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize ownership: {}", e)))?;

    Ok(Volume {
        id: VolumeId(id),
        name,
        tenant_id: TenantId(tenant_id),
        storage_class,
        filer_endpoint,
        remote_path,
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
