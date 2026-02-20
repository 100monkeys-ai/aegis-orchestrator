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
use sqlx::{PgPool, Row};
use tracing::{debug, error, warn};
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub struct PostgresStorageEventRepository {
    pool: PgPool,
}

impl PostgresStorageEventRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Helper to deserialize database row into StorageEvent
    fn deserialize_row(row: &sqlx::postgres::PgRow) -> Result<StorageEvent, RepositoryError> {
        let event_type: String = row.try_get("event_type")
            .map_err(|e| RepositoryError::Database(format!("Missing event_type: {}", e)))?;
        let execution_id = ExecutionId(row.try_get::<Uuid, _>("execution_id")
            .map_err(|e| RepositoryError::Database(format!("Missing execution_id: {}", e)))?);
        let volume_id = VolumeId(row.try_get::<Uuid, _>("volume_id")
            .map_err(|e| RepositoryError::Database(format!("Missing volume_id: {}", e)))?);
        let path: String = row.try_get("path")
            .map_err(|e| RepositoryError::Database(format!("Missing path: {}", e)))?;
        let details: serde_json::Value = row.try_get("operation_details")
            .map_err(|e| RepositoryError::Database(format!("Missing operation_details: {}", e)))?;
        let timestamp: DateTime<Utc> = row.try_get("timestamp")
            .map_err(|e| RepositoryError::Database(format!("Missing timestamp: {}", e)))?;

        // Parse timestamp from details (fallback to row timestamp if missing)
        let parse_timestamp = |key: &str| -> DateTime<Utc> {
            details.get(key)
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(timestamp)
        };

        match event_type.as_str() {
            "FileOpened" => {
                let open_mode = details.get("open_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("read")
                    .to_string();
                Ok(StorageEvent::FileOpened {
                    execution_id,
                    volume_id,
                    path,
                    open_mode,
                    opened_at: parse_timestamp("timestamp"),
                })
            }
            "FileRead" => {
                let offset = details.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
                let bytes_read = details.get("bytes_read").and_then(|v| v.as_u64()).unwrap_or(0);
                let duration_ms = details.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(StorageEvent::FileRead {
                    execution_id,
                    volume_id,
                    path,
                    offset,
                    bytes_read,
                    duration_ms,
                    read_at: parse_timestamp("timestamp"),
                })
            }
            "FileWritten" => {
                let offset = details.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
                let bytes_written = details.get("bytes_written").and_then(|v| v.as_u64()).unwrap_or(0);
                let duration_ms = details.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(StorageEvent::FileWritten {
                    execution_id,
                    volume_id,
                    path,
                    offset,
                    bytes_written,
                    duration_ms,
                    written_at: parse_timestamp("timestamp"),
                })
            }
            "FileClosed" => {
                Ok(StorageEvent::FileClosed {
                    execution_id,
                    volume_id,
                    path,
                    closed_at: parse_timestamp("timestamp"),
                })
            }
            "DirectoryListed" => {
                let entry_count = details.get("entry_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                Ok(StorageEvent::DirectoryListed {
                    execution_id,
                    volume_id,
                    path,
                    entry_count,
                    listed_at: parse_timestamp("timestamp"),
                })
            }
            "FileCreated" => {
                Ok(StorageEvent::FileCreated {
                    execution_id,
                    volume_id,
                    path,
                    created_at: parse_timestamp("timestamp"),
                })
            }
            "FileDeleted" => {
                Ok(StorageEvent::FileDeleted {
                    execution_id,
                    volume_id,
                    path,
                    deleted_at: parse_timestamp("timestamp"),
                })
            }
            "PathTraversalBlocked" => {
                Ok(StorageEvent::PathTraversalBlocked {
                    execution_id,
                    attempted_path: path,
                    blocked_at: parse_timestamp("timestamp"),
                })
            }
            "FilesystemPolicyViolation" => {
                let operation = details.get("operation")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let policy_rule = details.get("policy_rule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(StorageEvent::FilesystemPolicyViolation {
                    execution_id,
                    volume_id,
                    operation,
                    path,
                    policy_rule,
                    violated_at: parse_timestamp("timestamp"),
                })
            }
            "QuotaExceeded" => {
                let requested_bytes = details.get("requested_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                let available_bytes = details.get("available_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(StorageEvent::QuotaExceeded {
                    execution_id,
                    volume_id,
                    requested_bytes,
                    available_bytes,
                    exceeded_at: parse_timestamp("timestamp"),
                })
            }
            "UnauthorizedVolumeAccess" => {
                Ok(StorageEvent::UnauthorizedVolumeAccess {
                    execution_id,
                    volume_id,
                    attempted_at: parse_timestamp("timestamp"),
                })
            }
            _ => {
                warn!("Unknown storage event type: {}", event_type);
                Err(RepositoryError::Database(format!("Unknown event type: {}", event_type)))
            }
        }
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

        let rows = sqlx::query(
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

        // Deserialize each row into StorageEvent
        let mut events = Vec::new();
        for row in rows.iter() {
            match Self::deserialize_row(row) {
                Ok(event) => events.push(event),
                Err(e) => {
                    warn!("Failed to deserialize storage event: {}", e);
                    // Continue with other rows rather than failing entire query
                }
            }
        }
        
        debug!("Deserialized {} storage events for execution {}", events.len(), execution_id.0);
        Ok(events)
    }

    async fn find_by_volume(&self, volume_id: VolumeId, limit: Option<usize>) -> Result<Vec<StorageEvent>, RepositoryError> {
        let limit = limit.unwrap_or(1000) as i64;

        let rows = sqlx::query(
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

        // Deserialize each row into StorageEvent
        let mut events = Vec::new();
        for row in rows.iter() {
            match Self::deserialize_row(row) {
                Ok(event) => events.push(event),
                Err(e) => {
                    warn!("Failed to deserialize storage event: {}", e);
                }
            }
        }
        
        debug!("Deserialized {} storage events for volume {}", events.len(), volume_id.0);
        Ok(events)
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

        let rows = rows.map_err(|e| RepositoryError::Database(e.to_string()))?;
        
        // Deserialize each violation event
        let mut events = Vec::new();
        for row in rows.iter() {
            match Self::deserialize_row(&row) {
                Ok(event) => events.push(event),
                Err(e) => {
                    warn!("Failed to deserialize violation event: {}", e);
                }
            }
        }
        
        debug!("Deserialized {} violation events", events.len());
        Ok(events)
    }
}

