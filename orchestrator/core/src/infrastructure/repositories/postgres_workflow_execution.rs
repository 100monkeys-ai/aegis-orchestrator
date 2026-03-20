// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! PostgreSQL Workflow Execution Repository
//!
//! Provides PostgreSQL-backed persistence for workflow execution state tracking
//! including Temporal run linkage, final blackboard capture, and state history.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Track workflow execution state and Temporal run linkage
//! - **Integration:** Domain WorkflowExecutionRepository → PostgreSQL workflow_executions table
//!
//! # Schema
//!
//! The `workflow_executions` table stores:
//! - Workflow execution metadata (ID, workflow ID, status)
//! - Temporal integration IDs (workflow ID, run ID)
//! - Input parameters (JSONB)
//! - Current FSM state and state history
//! - Final blackboard snapshot (captured from Temporal on WorkflowExecutionCompleted)
//! - Execution timestamps
//!
//! # Execution State Tracking
//!
//! Execution state is driven by the TypeScript `aegis_workflow` Temporal worker.
//! Rust mirrors high-level progress from Temporal event callbacks:
//! - **Current State**: Last reported active state (updated by `TemporalEventListener`)
//! - **State History**: Ordered list of visited states (for audit/Cortex)
//! - **Final Blackboard**: Snapshot captured from `WorkflowExecutionCompleted` (not mutated mid-run)
//! - **Transitions**: Evaluated inside the TypeScript worker, not by this repository
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

use crate::domain::execution::{ExecutionId, ExecutionStatus};
use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
use crate::domain::tenant::TenantId;
use crate::domain::workflow::{Blackboard, StateName, WorkflowExecution, WorkflowId};
use anyhow::Result;
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
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
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        let input_json = serde_json::to_value(&execution.input)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let blackboard_json = serde_json::to_value(&execution.blackboard)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let state_outputs_json = serde_json::to_value(&execution.state_outputs)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;

        let status_str = match execution.status {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Cancelled => "cancelled",
        };

        sqlx::query(
            r#"
            INSERT INTO workflow_executions (
                id, tenant_id, workflow_id, temporal_workflow_id, temporal_run_id,
                input_params, status, 
                current_state, blackboard, state_outputs, state_history,
                started_at, last_transition_at, completed_at
            )
            VALUES (
                $1, $2, $3,
                COALESCE((SELECT name FROM workflows WHERE tenant_id = $2 AND id = $3), 'unknown-workflow'),
                $3, 
                $4, $5,
                $6, $7, $8, $9,
                $10, $11,
                CASE WHEN $6 IN ('completed', 'failed', 'cancelled') THEN NOW() ELSE NULL END
            )
            ON CONFLICT (id) DO UPDATE SET
                tenant_id = EXCLUDED.tenant_id,
                status = EXCLUDED.status,
                input_params = EXCLUDED.input_params,
                current_state = EXCLUDED.current_state,
                blackboard = EXCLUDED.blackboard,
                state_outputs = EXCLUDED.state_outputs,
                state_history = workflow_executions.state_history || EXCLUDED.state_history,
                last_transition_at = EXCLUDED.last_transition_at,
                completed_at = EXCLUDED.completed_at
            "#,
        )
        .bind(execution.id.0)
        .bind(tenant_id.as_str())
        .bind(execution.workflow_id.0)
        .bind(execution.id.0.to_string()) // temporal_run_id is execution_id
        .bind(input_json)
        .bind(status_str)
        .bind(execution.current_state.as_str())
        .bind(blackboard_json)
        .bind(state_outputs_json)
        .bind(serde_json::json!(vec![execution.current_state.as_str()])) // state_history
        .bind(execution.started_at)
        .bind(execution.last_transition_at)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            RepositoryError::Database(format!("Failed to save workflow execution: {e}"))
        })?;

        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<WorkflowExecution>, RepositoryError> {
        let row = sqlx::query(
            r#"
            SELECT 
                id, workflow_id, input_params, status, 
                current_state, blackboard, state_outputs,
                started_at, last_transition_at
            FROM workflow_executions
            WHERE tenant_id = $1 AND id = $2
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Some(row) = row {
            let id: uuid::Uuid = row.get("id");
            let workflow_id: uuid::Uuid = row.get("workflow_id");
            let input_val: serde_json::Value = row.get("input_params");
            let status_str: String = row.get("status");
            let current_state_str: String = row.get("current_state");
            let blackboard_val: serde_json::Value = row.get("blackboard");
            let state_outputs_val: serde_json::Value = row.get("state_outputs");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let last_transition_at: chrono::DateTime<chrono::Utc> = row.get("last_transition_at");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            // Reconstructs blackboard and state_outputs from JSONB
            let blackboard =
                Blackboard::from_json(&blackboard_val).unwrap_or_else(|_| Blackboard::new());

            let state_outputs: HashMap<StateName, serde_json::Value> = state_outputs_val
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| {
                            StateName::new(k)
                                .ok()
                                .map(|state_name| (state_name, v.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();

            Ok(Some(WorkflowExecution {
                id: ExecutionId(id),
                workflow_id: WorkflowId(workflow_id),
                status,
                current_state: StateName::new(&current_state_str)
                    .unwrap_or_else(|_| StateName::new("start").unwrap()),
                blackboard,
                input: input_val,
                state_outputs,
                started_at,
                last_transition_at,
            }))
        } else {
            Ok(None)
        }
    }

    async fn find_active_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, workflow_id, input_params, status, 
                current_state, blackboard, state_outputs,
                started_at, last_transition_at
            FROM workflow_executions
            WHERE tenant_id = $1 AND status = 'running'
            "#,
        )
        .bind(tenant_id.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut executions = Vec::new();
        for row in rows {
            let id: uuid::Uuid = row.get("id");
            let workflow_id: uuid::Uuid = row.get("workflow_id");
            let input_val: serde_json::Value = row.get("input_params");
            let status_str: String = row.get("status");
            let current_state_str: String = row.get("current_state");
            let blackboard_val: serde_json::Value = row.get("blackboard");
            let state_outputs_val: serde_json::Value = row.get("state_outputs");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let last_transition_at: chrono::DateTime<chrono::Utc> = row.get("last_transition_at");

            let status = match status_str.as_str() {
                "running" => ExecutionStatus::Running,
                _ => ExecutionStatus::Running,
            };

            let blackboard =
                Blackboard::from_json(&blackboard_val).unwrap_or_else(|_| Blackboard::new());

            let state_outputs: HashMap<StateName, serde_json::Value> = state_outputs_val
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| {
                            StateName::new(k)
                                .ok()
                                .map(|state_name| (state_name, v.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();

            executions.push(WorkflowExecution {
                id: ExecutionId(id),
                workflow_id: WorkflowId(workflow_id),
                status,
                current_state: StateName::new(&current_state_str)
                    .unwrap_or_else(|_| StateName::new("start").unwrap()),
                blackboard,
                input: input_val,
                state_outputs,
                started_at,
                last_transition_at,
            });
        }
        Ok(executions)
    }

    async fn append_event(
        &self,
        execution_id: ExecutionId,
        temporal_sequence_number: i64,
        event_type: String,
        payload: serde_json::Value,
        iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        let iteration_val = iteration_number.map(|n| n as i16);

        sqlx::query(
            r#"
            INSERT INTO execution_events (
                execution_id, temporal_sequence_number, event_type, event_payload, iteration_number
            )
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (execution_id, temporal_sequence_number) DO NOTHING
            "#,
        )
        .bind(execution_id.0)
        .bind(temporal_sequence_number)
        .bind(event_type)
        .bind(payload)
        .bind(iteration_val)
        .execute(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to append execution event: {e}")))?;

        // Also update iteration_count if this is an iteration event
        if iteration_number.is_some() {
            sqlx::query(
                r#"
                UPDATE workflow_executions 
                SET iteration_count = GREATEST(iteration_count, $2)
                WHERE id = $1
                "#,
            )
            .bind(execution_id.0)
            .bind(iteration_val)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                RepositoryError::Database(format!("Failed to update iteration_count: {e}"))
            })?;
        }

        Ok(())
    }

    async fn find_events_by_execution(
        &self,
        id: ExecutionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::domain::workflow::WorkflowExecutionEventRecord>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT temporal_sequence_number, event_type, event_payload,
                   iteration_number, created_at
            FROM execution_events
            WHERE execution_id = $1
            ORDER BY temporal_sequence_number ASC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(id.0)
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(format!("Failed to query execution events: {e}")))?;

        let records = rows
            .into_iter()
            .map(|row| {
                let sequence: i64 = row.get("temporal_sequence_number");
                let event_type: String = row.get("event_type");
                let payload: serde_json::Value = row.get("event_payload");
                let iteration_number: Option<i16> = row.get("iteration_number");
                let recorded_at: chrono::DateTime<chrono::Utc> = row.get("created_at");
                crate::domain::workflow::WorkflowExecutionEventRecord {
                    sequence,
                    event_type,
                    state_name: None,
                    iteration_number: iteration_number.map(|n| n as u8),
                    payload,
                    recorded_at,
                }
            })
            .collect();

        Ok(records)
    }

    async fn list_paginated_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        let rows = sqlx::query(
            r#"
            SELECT 
                id, workflow_id, input_params, status, 
                current_state, blackboard, state_outputs,
                started_at, last_transition_at
            FROM workflow_executions
            WHERE tenant_id = $1
            ORDER BY started_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(tenant_id.as_str())
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut executions = Vec::new();
        for row in rows {
            let id: uuid::Uuid = row.get("id");
            let workflow_id: uuid::Uuid = row.get("workflow_id");
            let input_val: serde_json::Value = row.get("input_params");
            let status_str: String = row.get("status");
            let current_state_str: String = row.get("current_state");
            let blackboard_val: serde_json::Value = row.get("blackboard");
            let state_outputs_val: serde_json::Value = row.get("state_outputs");
            let started_at: chrono::DateTime<chrono::Utc> = row.get("started_at");
            let last_transition_at: chrono::DateTime<chrono::Utc> = row.get("last_transition_at");

            let status = match status_str.as_str() {
                "pending" => ExecutionStatus::Pending,
                "running" => ExecutionStatus::Running,
                "completed" => ExecutionStatus::Completed,
                "failed" => ExecutionStatus::Failed,
                "cancelled" => ExecutionStatus::Cancelled,
                _ => ExecutionStatus::Pending,
            };

            let blackboard =
                Blackboard::from_json(&blackboard_val).unwrap_or_else(|_| Blackboard::new());

            let state_outputs: HashMap<StateName, serde_json::Value> = state_outputs_val
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| {
                            StateName::new(k)
                                .ok()
                                .map(|state_name| (state_name, v.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();

            executions.push(WorkflowExecution {
                id: ExecutionId(id),
                workflow_id: WorkflowId(workflow_id),
                status,
                current_state: StateName::new(&current_state_str)
                    .unwrap_or_else(|_| StateName::new("start").unwrap()),
                blackboard,
                input: input_val,
                state_outputs,
                started_at,
                last_transition_at,
            });
        }
        Ok(executions)
    }
}
