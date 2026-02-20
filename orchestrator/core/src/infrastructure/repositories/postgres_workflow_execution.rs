// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! PostgreSQL Workflow Execution Repository
//!
//! Provides PostgreSQL-backed persistence for workflow execution state tracking
//! including FSM state transitions, blackboard data, and integration with Temporal.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Track workflow execution state and FSM progress
//! - **Integration:** Domain WorkflowExecutionRepository → PostgreSQL workflow_executions table
//!
//! # Schema
//!
//! The `workflow_executions` table stores:
//! - Workflow execution metadata (ID, workflow ID, status)
//! - Temporal integration IDs (workflow ID, run ID)
//! - Input parameters (JSONB)
//! - Current FSM state and state history
//! - Blackboard data (shared state between workflow steps)
//! - Execution timestamps
//!
//! # FSM State Tracking
//!
//! Workflow executions follow a finite state machine model:
//! - **Current State**: Active state in the workflow graph
//! - **State History**: Ordered list of visited states
//! - **Blackboard**: Key-value storage for passing data between states
//! - **Transitions**: State transitions evaluated based on conditions
//!
//! # Temporal Integration
//!
//! Links AEGIS workflow executions to Temporal.io workflow runs:
//! - `temporal_workflow_id`: Temporal workflow type identifier
//! - `temporal_run_id`: Unique run identifier for status tracking
//!
//! # Usage
//!
//! ```ignore
//! use sqlx::PgPool;
//! use repositories::PostgresWorkflowExecutionRepository;
//!
//! let pool = PgPool::connect(&database_url).await?;
//! let repo = PostgresWorkflowExecutionRepository::new(pool);
//!
//! // Save workflow execution state
//! repo.save(&workflow_execution).await?;
//!
//! // Query by ID
//! let execution = repo.find_by_id(execution_id).await?;
//! ```

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use anyhow::Result;
use crate::domain::repository::{WorkflowExecutionRepository, RepositoryError};
use crate::domain::workflow::{WorkflowExecution, WorkflowId, StateName, Blackboard};
use crate::domain::execution::{ExecutionId, ExecutionStatus};
use std::collections::HashMap;

pub struct PostgresWorkflowExecutionRepository {
    pool: PgPool,
}

impl PostgresWorkflowExecutionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl WorkflowExecutionRepository for PostgresWorkflowExecutionRepository {
    async fn save(&self, execution: &WorkflowExecution) -> Result<(), RepositoryError> {
        let input_json = serde_json::to_value(&execution.input)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        // Derive final output from state outputs if we are completed?
        // Or just map what we have.
        // We will store the full state in a future "internal_state" column, 
        // but for now we map to the existing schema.
        
        let status_str = match execution.status {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Cancelled => "cancelled",
        };

        // We use subquery to get workflow name for temporal_workflow_id placeholder
        // and use execution.id for temporal_run_id
        
        sqlx::query(
            r#"
            INSERT INTO workflow_executions (
                id, workflow_id, temporal_workflow_id, temporal_run_id, 
                input_params, status, started_at, completed_at
            )
            VALUES (
                $1, $2, 
                COALESCE((SELECT name FROM workflows WHERE id = $2), 'unknown-workflow'), 
                $3, 
                $4, $5, $6, 
                CASE WHEN $5 IN ('completed', 'failed', 'cancelled') THEN NOW() ELSE NULL END
            )
            ON CONFLICT (id) DO UPDATE SET
                status = EXCLUDED.status,
                input_params = EXCLUDED.input_params,
                completed_at = EXCLUDED.completed_at
            "#
        )
        .bind(execution.id.0)
        .bind(execution.workflow_id.0)
        .bind(execution.id.0.to_string())
        .bind(input_json)
        .bind(status_str)
        .bind(execution.started_at)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to save workflow execution: {}", e)))?;

        Ok(())
    }

    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<WorkflowExecution>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, workflow_id, input_params, status, started_at
            FROM workflow_executions
            WHERE id = $1
            "#
        )
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let id: uuid::Uuid = row.get("id");
            let workflow_id: uuid::Uuid = row.get("workflow_id");
            let input_val: serde_json::Value = row.get("input_params");
            let status_str: String = row.get("status");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            
            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            // NOTE: This reconstruction is PARTIAL. Blackboard and state_outputs are lost.
            // This is acceptable for listing/querying but NOT for resuming execution.
            // Valid assumption for MVP persistence.
            
            Ok(Some(WorkflowExecution {
                id: ExecutionId(id),
                workflow_id: WorkflowId(workflow_id),
                status,
                current_state: StateName::new("UNKNOWN").unwrap_or(StateName::new("start").unwrap()), // Placeholder
                blackboard: Blackboard::new(),
                input: input_val,
                state_outputs: HashMap::new(),
                started_at,
                last_transition_at: started_at, // Approximation
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_active(&self) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, workflow_id, input_params, status, started_at
            FROM workflow_executions
            WHERE status = 'running'
            "#
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut executions = Vec::new();
        for row in rows {
             let id: uuid::Uuid = row.get("id");
            let workflow_id: uuid::Uuid = row.get("workflow_id");
            let input_val: serde_json::Value = row.get("input_params");
            let status_str: String = row.get("status");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            
            let status = match status_str.as_str() {
                "running" => ExecutionStatus::Running,
                _ => ExecutionStatus::Running,
            };

            executions.push(WorkflowExecution {
                id: ExecutionId(id),
                workflow_id: WorkflowId(workflow_id),
                status,
                current_state: StateName::new("UNKNOWN").unwrap_or(StateName::new("start").unwrap()),
                blackboard: Blackboard::new(),
                input: input_val,
                state_outputs: HashMap::new(),
                started_at,
                last_transition_at: started_at,
            });
        }
        Ok(executions)
    }
}
