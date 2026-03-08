// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL Execution Repository
//!
//! Provides PostgreSQL-backed persistence for agent execution state and history.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Persist execution state, iterations, and results
//! - **Integration:** Domain ExecutionRepository → PostgreSQL executions table
//!
//! # Schema
//!
//! The `executions` table stores:
//! - Execution metadata (ID, agent ID, status, timestamps)
//! - Input parameters (JSONB)
//! - Iteration history (JSONB array with LLM interactions)
//! - Final output and error messages
//! - Execution hierarchy (parent/child relationships)
//!
//! # Features
//!
//! - **Full History Tracking**: All iterations with token usage
//! - **Status Management**: Lifecycle state (pending → running → completed/failed)
//! - **Hierarchical Queries**: Support for agent-as-judge recursive execution trees
//! - **JSONB Indexing**: Efficient queries on structured execution data
//!
//! # Usage
//!
//! ```ignore
//! use sqlx::PgPool;
//! use repositories::PostgresExecutionRepository;
//!
//! let pool = PgPool::connect(&database_url).await?;
//! let repo = PostgresExecutionRepository::new(pool);
//!
//! // Save execution state
//! repo.save(&execution).await?;
//!
//! // Query by ID
//! let execution = repo.find_by_id(execution_id).await?;
//! ```

use crate::domain::agent::AgentId;
use crate::domain::execution::{
    Execution, ExecutionHierarchy, ExecutionId, ExecutionInput, ExecutionStatus, Iteration,
};
use crate::domain::repository::{ExecutionRepository, RepositoryError};
use anyhow::Result;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;

pub struct PostgresExecutionRepository {
    pool: PgPool,
}

impl PostgresExecutionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ExecutionRepository for PostgresExecutionRepository {
    async fn save(&self, execution: &Execution) -> Result<(), RepositoryError> {
        let iterations_json = serde_json::to_value(execution.iterations())
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let input_json = serde_json::to_value(&execution.input)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        // Extract final output and error from the execution state or last iteration
        let final_output = execution.iterations().last().and_then(|i| i.output.clone());

        let error_message = execution.error.clone();

        let status_str = match execution.status {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Cancelled => "cancelled",
        };

        // Note: the `executions` table currently has columns such as input, status, iterations,
        // current_iteration, max_iterations, final_output, error_message, started_at, and completed_at,
        // but does not have fields for `workflow_execution_id` or `hierarchy`.
        //
        // TODO: If workflow execution IDs or execution hierarchy need to be persisted, extend the schema
        // and `Execution` struct accordingly and map those fields here.

        let parent_execution_id = execution.hierarchy.parent_execution_id.map(|id| id.0);

        sqlx::query(
            r#"
            INSERT INTO executions (
                id, agent_id, input, status, iterations, 
                current_iteration, max_iterations, final_output, error_message, 
                container_uid, container_gid,
                started_at, completed_at, parent_execution_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                iterations = EXCLUDED.iterations,
                current_iteration = EXCLUDED.current_iteration,
                final_output = EXCLUDED.final_output,
                error_message = EXCLUDED.error_message,
                container_uid = EXCLUDED.container_uid,
                container_gid = EXCLUDED.container_gid,
                completed_at = EXCLUDED.completed_at,
                parent_execution_id = EXCLUDED.parent_execution_id
            "#,
        )
        .bind(execution.id.0)
        .bind(execution.agent_id.0)
        .bind(input_json)
        .bind(status_str)
        .bind(iterations_json)
        .bind(execution.iterations().len() as i32)
        .bind(execution.max_iterations as i32)
        .bind(final_output)
        .bind(error_message)
        .bind(execution.container_uid as i32)
        .bind(execution.container_gid as i32)
        .bind(execution.started_at)
        .bind(execution.ended_at)
        .bind(parent_execution_id)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save execution: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<Execution>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, agent_id, input, status, iterations, max_iterations, 
                container_uid, container_gid,
                started_at, completed_at, error_message,
                parent_execution_id
            FROM executions
            WHERE id = $1
            "#,
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let id: uuid::Uuid = row.get("id");
            let agent_id: uuid::Uuid = row.get("agent_id");
            let status_str: String = row.get("status");
            let input_val: serde_json::Value = row.get("input");
            let iterations_val: serde_json::Value = row.get("iterations");
            let max_iterations: i32 = row.get("max_iterations");
            let container_uid: i32 = row.get("container_uid");
            let container_gid: i32 = row.get("container_gid");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let completed_at: Option<chrono::DateTime<chrono::Utc>> = row.get("completed_at");
            let error_message: Option<String> = row.get("error_message");
            let parent_execution_id: Option<uuid::Uuid> = row.get("parent_execution_id");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending, // Default fallback
            };

            let input: ExecutionInput = serde_json::from_value(input_val).map_err(|e| {
                RepositoryError::Serialization(format!("Failed to deserialize input: {}", e))
            })?;

            // There is a weird issue where iterations might be stored as property of Execution,
            // but Execution struct has explicit `iterations: Vec<Iteration>`.
            // The `iterations` column is JSONB array.
            let iterations: Vec<Iteration> =
                serde_json::from_value(iterations_val).map_err(|e| {
                    RepositoryError::Serialization(format!(
                        "Failed to deserialize iterations: {}",
                        e
                    ))
                })?;

            let hierarchy = match parent_execution_id {
                None => ExecutionHierarchy::root(ExecutionId(id)),
                Some(parent_id) => {
                    // NOTE: Only `parent_execution_id` is persisted, so depth and path
                    // are approximated here as a single-level child hierarchy. To support
                    // full multi-level reconstruction, store `depth` and `path` in the
                    // database or traverse ancestry recursively.
                    ExecutionHierarchy {
                        parent_execution_id: Some(ExecutionId(parent_id)),
                        depth: 1,
                        path: vec![ExecutionId(parent_id), ExecutionId(id)],
                    }
                }
            };

            Ok(Some(Execution {
                id: ExecutionId(id),
                agent_id: AgentId(agent_id),
                status,
                iterations,
                max_iterations: max_iterations as u8,
                container_uid: container_uid as u32,
                container_gid: container_gid as u32,
                input,
                started_at,
                ended_at: completed_at,
                error: error_message,
                hierarchy,
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_by_agent(
        &self,
        agent_id: AgentId,
        limit: usize,
    ) -> Result<Vec<Execution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, agent_id, input, status, iterations, max_iterations, 
                container_uid, container_gid,
                started_at, completed_at, error_message, parent_execution_id
            FROM executions
            WHERE agent_id = $1
            ORDER BY started_at DESC
            LIMIT $2
            "#,
        )
        .bind(agent_id.0)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut executions = Vec::new();
        for row in rows {
            // Mapping logic same as find_by_id ...
            // Duplicating logic here for now due to async context
            let id: uuid::Uuid = row.get("id");
            let agent_id: uuid::Uuid = row.get("agent_id");
            let status_str: String = row.get("status");
            let input_val: serde_json::Value = row.get("input");
            let iterations_val: serde_json::Value = row.get("iterations");
            let max_iterations: i32 = row.get("max_iterations");
            let container_uid: i32 = row.get("container_uid");
            let container_gid: i32 = row.get("container_gid");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let completed_at: Option<chrono::DateTime<chrono::Utc>> = row.get("completed_at");
            let error_message: Option<String> = row.get("error_message");
            let parent_execution_id: Option<uuid::Uuid> = row.get("parent_execution_id");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            let input: ExecutionInput =
                serde_json::from_value(input_val).map_err(RepositoryError::from)?;
            let iterations: Vec<Iteration> =
                serde_json::from_value(iterations_val).map_err(|e| {
                    RepositoryError::Serialization(format!(
                        "Failed to deserialize iterations: {}",
                        e
                    ))
                })?;

            let hierarchy = match parent_execution_id {
                Some(parent_id) => ExecutionHierarchy {
                    parent_execution_id: Some(ExecutionId(parent_id)),
                    depth: 1,
                    path: vec![ExecutionId(parent_id), ExecutionId(id)],
                },
                None => ExecutionHierarchy::root(ExecutionId(id)),
            };

            executions.push(Execution {
                id: ExecutionId(id),
                agent_id: AgentId(agent_id),
                status,
                iterations,
                max_iterations: max_iterations as u8,
                container_uid: container_uid as u32,
                container_gid: container_gid as u32,
                input,
                started_at,
                ended_at: completed_at,
                error: error_message,
                hierarchy,
            });
        }

        Ok(executions)
    }

    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, agent_id, input, status, iterations, max_iterations, 
                container_uid, container_gid,
                started_at, completed_at, error_message, parent_execution_id
            FROM executions
            ORDER BY started_at DESC
            LIMIT $1
            "#,
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut executions = Vec::new();
        for row in rows {
            let id: uuid::Uuid = row.get("id");
            let agent_id: uuid::Uuid = row.get("agent_id");
            let status_str: String = row.get("status");
            let input_val: serde_json::Value = row.get("input");
            let iterations_val: serde_json::Value = row.get("iterations");
            let max_iterations: i32 = row.get("max_iterations");
            let container_uid: i32 = row.get("container_uid");
            let container_gid: i32 = row.get("container_gid");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let completed_at: Option<chrono::DateTime<chrono::Utc>> = row.get("completed_at");
            let error_message: Option<String> = row.get("error_message");
            let parent_execution_id: Option<uuid::Uuid> = row.get("parent_execution_id");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            let input: ExecutionInput =
                serde_json::from_value(input_val).map_err(RepositoryError::from)?;
            let iterations: Vec<Iteration> =
                serde_json::from_value(iterations_val).map_err(|e| {
                    RepositoryError::Serialization(format!(
                        "Failed to deserialize iterations: {}",
                        e
                    ))
                })?;

            let hierarchy = match parent_execution_id {
                Some(parent_id) => ExecutionHierarchy {
                    parent_execution_id: Some(ExecutionId(parent_id)),
                    depth: 1,
                    path: vec![ExecutionId(parent_id), ExecutionId(id)],
                },
                None => ExecutionHierarchy::root(ExecutionId(id)),
            };

            executions.push(Execution {
                id: ExecutionId(id),
                agent_id: AgentId(agent_id),
                status,
                iterations,
                max_iterations: max_iterations as u8,
                container_uid: container_uid as u32,
                container_gid: container_gid as u32,
                input,
                started_at,
                ended_at: completed_at,
                error: error_message,
                hierarchy,
            });
        }
        Ok(executions)
    }

    async fn delete(&self, id: ExecutionId) -> Result<(), RepositoryError> {
        sqlx::query("DELETE FROM executions WHERE id = $1")
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
}
