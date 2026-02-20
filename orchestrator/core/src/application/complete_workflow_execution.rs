// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Complete Workflow Execution Use Case
//!
//! Application service for completing workflow executions and triggering post-execution activities.
//!
//! # DDD Pattern: Application Service
//!
//! - **Layer:** Application
//! - **Responsibility:** Handle workflow completion, persist final state, trigger learning
//! - **Collaborators:**
//!   - Domain: WorkflowExecution aggregate updates
//!   - Infrastructure: WorkflowExecutionRepository, EventBus, CortexService
//!
//! # Flow
//!
//! 1. Load workflow execution from repository
//! 2. Update status to completed/failed
//! 3. Persist final state to repository
//! 4. Publish DomainEvent::WorkflowExecutionCompleted
//! 5. If had refinements: trigger Cortex pattern learning
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for complete workflow execution

use crate::domain::execution::ExecutionId;
use crate::domain::execution::ExecutionStatus;
use crate::domain::repository::WorkflowExecutionRepository;
use crate::infrastructure::event_bus::EventBus;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Arc;
use tracing::info;

/// Workflow execution completion status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    Success,
    Failed,
    Cancelled,
}

/// Workflow execution completion request
#[derive(Debug, Clone)]
pub struct CompleteWorkflowExecutionRequest {
    pub execution_id: String,
    pub status: CompletionStatus,
    pub final_blackboard: Option<serde_json::Value>,
    pub error_reason: Option<String>,
    pub artifacts: Option<serde_json::Value>,
}

/// Completed workflow execution response
#[derive(Debug, Clone)]
pub struct CompletedWorkflowExecution {
    pub execution_id: String,
    pub status: String,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}

/// Complete Workflow Execution Use Case
#[async_trait]
pub trait CompleteWorkflowExecutionUseCase: Send + Sync {
    /// Complete a workflow execution
    ///
    /// # Arguments
    ///
    /// * `request` - Completion request with final state
    ///
    /// # Returns
    ///
    /// Completion confirmation
    ///
    /// # Errors
    ///
    /// - ExecutionNotFound: Specified execution_id doesn't exist
    /// - PersistenceError: Database save failed
    async fn complete_execution(
        &self,
        request: CompleteWorkflowExecutionRequest,
    ) -> Result<CompletedWorkflowExecution>;
}

/// Standard implementation of CompleteWorkflowExecutionUseCase
pub struct StandardCompleteWorkflowExecutionUseCase {
    execution_repository: Arc<dyn WorkflowExecutionRepository>,
    event_bus: Arc<EventBus>,
}

impl StandardCompleteWorkflowExecutionUseCase {
    pub fn new(
        execution_repository: Arc<dyn WorkflowExecutionRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            execution_repository,
            event_bus,
        }
    }
}

#[async_trait]
impl CompleteWorkflowExecutionUseCase for StandardCompleteWorkflowExecutionUseCase {
    async fn complete_execution(
        &self,
        request: CompleteWorkflowExecutionRequest,
    ) -> Result<CompletedWorkflowExecution> {
        info!(
            execution_id = %request.execution_id,
            status = ?request.status,
            "Completing workflow execution"
        );

        // Step 1: Parse execution ID
        let execution_id = ExecutionId(
            uuid::Uuid::parse_str(&request.execution_id)
                .context("Invalid execution_id format (not a UUID)")?
        );

        // Step 2: Load execution from repository
        let mut execution = self.execution_repository
            .find_by_id(execution_id)
            .await
            .context("Failed to query execution repository")?
            .ok_or_else(|| anyhow::anyhow!(
                "Workflow execution not found: {}",
                request.execution_id
            ))?;

        // Step 3: Update execution status
        let final_status = match request.status {
            CompletionStatus::Success => ExecutionStatus::Completed,
            CompletionStatus::Failed => ExecutionStatus::Failed,
            CompletionStatus::Cancelled => ExecutionStatus::Cancelled,
        };

        let final_status_str = format!("{:?}", final_status).to_lowercase();
        execution.status = final_status;

        // Step 4: Merge final blackboard state if provided
        if let Some(blackboard_json) = request.final_blackboard {
            if let Ok(blackboard_map) =
                serde_json::from_value::<std::collections::HashMap<String, serde_json::Value>>(blackboard_json)
            {
                for (key, value) in blackboard_map {
                    execution.blackboard.set(key, value);
                }
            }
        }

        // Step 5: Persist to repository
        self.execution_repository
            .save(&execution)
            .await
            .context("Failed to persist execution to repository")?;

        // Step 6: Publish domain event
        self.event_bus.publish_workflow_event(
            crate::domain::events::WorkflowEvent::WorkflowExecutionCompleted {
                execution_id,
                final_blackboard: serde_json::to_value(&execution.blackboard.data()).unwrap_or(serde_json::Value::Null),
                artifacts: request.artifacts,
                completed_at: Utc::now(),
            },
        );

        // Step 7: If execution had failures/refinements, trigger Cortex learning
        // This would be handled by event subscribers in a separate service
        // (Pattern learning is event-driven, not synchronous)

        Ok(CompletedWorkflowExecution {
            execution_id: request.execution_id,
            status: final_status_str,
            completed_at: Utc::now(),
        })
    }
}

