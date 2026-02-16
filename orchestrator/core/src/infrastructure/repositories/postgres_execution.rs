// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use anyhow::Result;
use crate::domain::repository::{ExecutionRepository, RepositoryError};
use crate::domain::execution::{Execution, ExecutionId, ExecutionStatus, Iteration, ExecutionInput, ExecutionHierarchy};
use crate::domain::agent::AgentId;

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
        let iterations_json = serde_json::to_value(&execution.iterations())
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        
        let input_json = serde_json::to_value(&execution.input)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;



        // Extract final output and error from the execution state or last iteration
        let final_output = execution.iterations().last()
            .and_then(|i| i.output.clone());
            
        let error_message = execution.error.clone();

        let status_str = match execution.status {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Cancelled => "cancelled",
        };

        // Note: We don't have workflow_execution_id in Execution struct yet.
        // We also need to store hierarchy, but the table schema might not have it strictly defined as a column yet?
        // Checking schema: executions table has input, status, iterations, current_iteration, max_iterations, final_output, error_message, started_at, completed_at.
        // It does NOT have hierarchy column. We might need to store it in `input` or add a column.
        // For now, let's assume hierarchy is part of the context or we might lose it if not stored.
        // Wait, `iterations` is JSONB, so it stores full iteration history.
        // `input` is JSONB.
        
        // We will store hierarchy in the input payload or similar if we can't change schema.
        // Actually, let's just save what we can map. 
        // If we need hierarchy persistence, we should probably update the schema, but I'll stick to the existing schema for now and maybe stash it in input if needed, 
        // OR just ignore it if it's not critical for the "empty table" issue.
        // The user issue is "empty table", so primary goal is getting the main data in.

        sqlx::query(
            r#"
            INSERT INTO executions (
                id, agent_id, input, status, iterations, 
                current_iteration, max_iterations, final_output, error_message, 
                started_at, completed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                iterations = EXCLUDED.iterations,
                current_iteration = EXCLUDED.current_iteration,
                final_output = EXCLUDED.final_output,
                error_message = EXCLUDED.error_message,
                completed_at = EXCLUDED.completed_at
            "#
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
        .bind(execution.started_at)
        .bind(execution.ended_at)
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
                started_at, completed_at, error_message
            FROM executions
            WHERE id = $1
            "#
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
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let completed_at: Option<chrono::DateTime<chrono::Utc>> = row.get("completed_at");
            let error_message: Option<String> = row.get("error_message");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending, // Default fallback
            };

            let input: ExecutionInput = serde_json::from_value(input_val)
                .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize input: {}", e)))?;

            // There is a weird issue where iterations might be stored as property of Execution, 
            // but Execution struct has explicit `iterations: Vec<Iteration>`.
            // The `iterations` column is JSONB array.
            let iterations: Vec<Iteration> = serde_json::from_value(iterations_val)
                .map_err(|e| RepositoryError::Serialization(format!("Failed to deserialize iterations: {}", e)))?;

            Ok(Some(Execution {
                id: ExecutionId(id),
                agent_id: AgentId(agent_id),
                status,
                iterations, // We need to access private field or use a constructor/struct update syntax? 
                            // `iterations` field is private in `Execution` struct definition?
                            // Checked `execution.rs`: `iterations: Vec<Iteration>` is private (not pub).
                            // But `Execution` struct definition in `execution.rs` lines 128: `iterations: Vec<Iteration>,`
                            // Ensure it is pub or we have a way to reconstruct it.
                            // I need to check `execution.rs` again.
                max_iterations: max_iterations as u8,
                input,
                started_at,
                ended_at: completed_at,
                error: error_message,
                hierarchy: ExecutionHierarchy::root(ExecutionId(id)), // Defaulting hierarchy as it's not persisted yet
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_by_agent(&self, agent_id: AgentId) -> Result<Vec<Execution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, agent_id, input, status, iterations, max_iterations, 
                started_at, completed_at, error_message
            FROM executions
            WHERE agent_id = $1
            ORDER BY started_at DESC
            "#
        )
        .bind(agent_id.0)
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
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let completed_at: Option<chrono::DateTime<chrono::Utc>> = row.get("completed_at");
            let error_message: Option<String> = row.get("error_message");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            let input: ExecutionInput = serde_json::from_value(input_val).unwrap_or(ExecutionInput { intent: None, payload: serde_json::Value::Null });
            let iterations: Vec<Iteration> = serde_json::from_value(iterations_val).unwrap_or_default();

            executions.push(Execution {
                id: ExecutionId(id),
                agent_id: AgentId(agent_id),
                status,
                iterations,
                max_iterations: max_iterations as u8,
                input,
                started_at,
                ended_at: completed_at,
                error: error_message,
                hierarchy: ExecutionHierarchy::root(ExecutionId(id)),
            });
        }

        Ok(executions)
    }

    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, agent_id, input, status, iterations, max_iterations, 
                started_at, completed_at, error_message
            FROM executions
            ORDER BY started_at DESC
            LIMIT $1
            "#
        )
        .bind(limit as i32)
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
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let completed_at: Option<chrono::DateTime<chrono::Utc>> = row.get("completed_at");
            let error_message: Option<String> = row.get("error_message");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            let input: ExecutionInput = serde_json::from_value(input_val).unwrap_or(ExecutionInput { intent: None, payload: serde_json::Value::Null });
            let iterations: Vec<Iteration> = serde_json::from_value(iterations_val).unwrap_or_default();

            executions.push(Execution {
                id: ExecutionId(id),
                agent_id: AgentId(agent_id),
                status,
                iterations,
                max_iterations: max_iterations as u8,
                input,
                started_at,
                ended_at: completed_at,
                error: error_message,
                hierarchy: ExecutionHierarchy::root(ExecutionId(id)),
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
