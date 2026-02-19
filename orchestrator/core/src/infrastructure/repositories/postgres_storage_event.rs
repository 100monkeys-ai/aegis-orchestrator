// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! PostgreSQL implementation of StorageEventRepository (ADR-036)
//!
//! Persists file-level operation audit trail for forensic analysis.
//! Subscribes to in-memory event bus and writes to storage_events table.

use crate::domain::repository::{StorageEventRepository, RepositoryError};
use crate::domain::events::StorageEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::volume::VolumeId;
use async_trait::async_trait;
use sqlx::PgPool;
use tracing::{debug, error};

pub struct PostgresStorageEventRepository {
    pool: PgPool,
}

impl PostgresStorageEventRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl StorageEventRepository for PostgresStorageEventRepository {
    async fn save(&self, event: &StorageEvent) -> Result<(), RepositoryError> {
        // Extract common fields and convert event to database format
        let (event_type, execution_id, volume_id, path, details) = match event {
            StorageEvent::FileOpened { execution_id, volume_id, path, open_mode, opened_at } => (
                "FileOpened",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "open_mode": open_mode,
                    "timestamp": opened_at.to_rfc3339(),
                }),
            ),
            StorageEvent::FileRead { execution_id, volume_id, path, offset, bytes_read, duration_ms, read_at } => (
                "FileRead",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "offset": offset,
                    "bytes_read": bytes_read,
                    "duration_ms": duration_ms,
                    "timestamp": read_at.to_rfc3339(),
                }),
            ),
            StorageEvent::FileWritten { execution_id, volume_id, path, offset, bytes_written, duration_ms, written_at } => (
                "FileWritten",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "offset": offset,
                    "bytes_written": bytes_written,
                    "duration_ms": duration_ms,
                    "timestamp": written_at.to_rfc3339(),
                }),
            ),
            StorageEvent::FileClosed { execution_id, volume_id, path, closed_at } => (
                "FileClosed",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "timestamp": closed_at.to_rfc3339(),
                }),
            ),
            StorageEvent::DirectoryListed { execution_id, volume_id, path, entry_count, listed_at } => (
                "DirectoryListed",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "entry_count": entry_count,
                    "timestamp": listed_at.to_rfc3339(),
                }),
            ),
            StorageEvent::FileCreated { execution_id, volume_id, path, created_at } => (
                "FileCreated",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "timestamp": created_at.to_rfc3339(),
                }),
            ),
            StorageEvent::FileDeleted { execution_id, volume_id, path, deleted_at } => (
                "FileDeleted",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "timestamp": deleted_at.to_rfc3339(),
                }),
            ),
            StorageEvent::PathTraversalBlocked { execution_id, attempted_path, blocked_at } => (
                "PathTraversalBlocked",
                *execution_id,
                VolumeId::default(), // No volume ID for security violations
                attempted_path.clone(),
                serde_json::json!({
                    "timestamp": blocked_at.to_rfc3339(),
                }),
            ),
            StorageEvent::FilesystemPolicyViolation { execution_id, volume_id, operation, path, policy_rule, violated_at } => (
                "FilesystemPolicyViolation",
                *execution_id,
                *volume_id,
                path.clone(),
                serde_json::json!({
                    "operation": operation,
                    "policy_rule": policy_rule,
                    "timestamp": violated_at.to_rfc3339(),
                }),
            ),
            StorageEvent::QuotaExceeded { execution_id, volume_id, requested_bytes, available_bytes, exceeded_at } => (
                "QuotaExceeded",
                *execution_id,
                *volume_id,
                "".to_string(), // No specific path
                serde_json::json!({
                    "requested_bytes": requested_bytes,
                    "available_bytes": available_bytes,
                    "timestamp": exceeded_at.to_rfc3339(),
                }),
            ),
            StorageEvent::UnauthorizedVolumeAccess { execution_id, volume_id, attempted_at } => (
                "UnauthorizedVolumeAccess",
                *execution_id,
                *volume_id,
                "".to_string(),
                serde_json::json!({
                    "timestamp": attempted_at.to_rfc3339(),
                }),
            ),
        };

        // Insert into database
        sqlx::query(
            r#"
            INSERT INTO storage_events (execution_id, volume_id, event_type, path, operation_details)
            VALUES ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(execution_id.0)
        .bind(volume_id.0)
        .bind(event_type)
        .bind(path)
        .bind(details)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            error!("Failed to save storage event: {}", e);
            RepositoryError::Database(e.to_string())
        })?;

        debug!("Saved storage event: {} for execution {}", event_type, execution_id.0);
        Ok(())
    }

    async fn find_by_execution(&self, execution_id: ExecutionId, limit: Option<usize>) -> Result<Vec<StorageEvent>, RepositoryError> {
        let limit = limit.unwrap_or(1000) as i64;

        let _rows = sqlx::query(
            r#"
            SELECT event_type, execution_id, volume_id, path, operation_details, timestamp
            FROM storage_events
            WHERE execution_id = $1
            ORDER BY timestamp DESC
            LIMIT $2
            "#,
        )
        .bind(execution_id.0)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // For now, return empty vec - full deserialization would require mapping back to enum variants
        // This can be enhanced later if needed
        Ok(Vec::new())
    }

    async fn find_by_volume(&self, volume_id: VolumeId, limit: Option<usize>) -> Result<Vec<StorageEvent>, RepositoryError> {
        let limit = limit.unwrap_or(1000) as i64;

        let _rows = sqlx::query(
            r#"
            SELECT event_type, execution_id, volume_id, path, operation_details, timestamp
            FROM storage_events
            WHERE volume_id = $1
            ORDER BY timestamp DESC
            LIMIT $2
            "#,
        )
        .bind(volume_id.0)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Vec::new())
    }

    async fn find_violations(&self, execution_id: Option<ExecutionId>) -> Result<Vec<StorageEvent>, RepositoryError> {
        let rows = if let Some(exec_id) = execution_id {
            sqlx::query(
                r#"
                SELECT event_type, execution_id, volume_id, path, operation_details, timestamp
                FROM storage_events
                WHERE execution_id = $1
                  AND event_type IN ('PathTraversalBlocked', 'FilesystemPolicyViolation', 'UnauthorizedVolumeAccess')
                ORDER BY timestamp DESC
                "#,
            )
            .bind(exec_id.0)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT event_type, execution_id, volume_id, path, operation_details, timestamp
                FROM storage_events
                WHERE event_type IN ('PathTraversalBlocked', 'FilesystemPolicyViolation', 'UnauthorizedVolumeAccess')
                ORDER BY timestamp DESC
                LIMIT 100
                "#,
            )
            .fetch_all(&self.pool)
            .await
        };

        rows.map(|_| Vec::new())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }
}

