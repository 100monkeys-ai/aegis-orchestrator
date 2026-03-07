// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Start Workflow Execution Use Case
//!
//! Application service for starting workflow executions with Temporal.
//!
//! # DDD Pattern: Application Service
//!
//! - **Layer:** Application
//! - **Responsibility:** Orchestrate workflow execution startup
//! - **Collaborators:**
//!   - Domain: Workflow, WorkflowExecution aggregates
//!   - Infrastructure: WorkflowRepository, WorkflowExecutionRepository, TemporalClient, EventBus
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for start workflow execution

use crate::application::ports::WorkflowEnginePort;
use crate::domain::execution::ExecutionId;
use crate::domain::repository::{WorkflowExecutionRepository, WorkflowRepository};
use crate::domain::workflow::{WorkflowExecution, WorkflowId};
use crate::infrastructure::event_bus::EventBus;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;

/// Workflow execution start request
#[derive(Debug, Clone, serde::Deserialize)]
pub struct StartWorkflowExecutionRequest {
    /// Workflow ID to execute
    pub workflow_id: String,

    /// Input context/parameters for workflow
    pub input: serde_json::Value,

    /// Optional blackboard seed: key-value context forwarded to the TypeScript worker at startup.
    /// Values are included in the `StartWorkflowExecution` Temporal payload and accessible
    /// inside the worker via Handlebars templates (e.g. `{{blackboard.judges}}`).
    /// Rust does not mutate the blackboard during workflow execution.
    pub blackboard: Option<serde_json::Value>,
}

/// Started workflow execution response
#[derive(Debug, Clone, serde::Serialize)]
pub struct StartedWorkflowExecution {
    pub execution_id: String,
    pub workflow_id: String,
    pub temporal_run_id: String,
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Start Workflow Execution Use Case
#[async_trait]
pub trait StartWorkflowExecutionUseCase: Send + Sync {
    /// Start a workflow execution
    ///
    /// # Arguments
    ///
    /// * `request` - Start execution request with workflow_id and input
    ///
    /// # Returns
    ///
    /// Started execution metadata with Temporal run_id
    ///
    /// # Errors
    ///
    /// - WorkflowNotFound: Specified workflow_id doesn't exist
    /// - TemporalError: Temporal server unavailable
    /// - PersistenceError: Database save failed
    async fn start_execution(
        &self,
        request: StartWorkflowExecutionRequest,
    ) -> Result<StartedWorkflowExecution>;
}

/// Standard implementation of StartWorkflowExecutionUseCase
pub struct StandardStartWorkflowExecutionUseCase {
    workflow_repository: Arc<dyn WorkflowRepository>,
    execution_repository: Arc<dyn WorkflowExecutionRepository>,
    workflow_engine: Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>>,
    event_bus: Arc<EventBus>,
}

impl StandardStartWorkflowExecutionUseCase {
    pub fn new(
        workflow_repository: Arc<dyn WorkflowRepository>,
        execution_repository: Arc<dyn WorkflowExecutionRepository>,
        workflow_engine: Arc<tokio::sync::RwLock<Option<Arc<dyn WorkflowEnginePort>>>>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            workflow_repository,
            execution_repository,
            workflow_engine,
            event_bus,
        }
    }
}

#[async_trait]
impl StartWorkflowExecutionUseCase for StandardStartWorkflowExecutionUseCase {
    async fn start_execution(
        &self,
        request: StartWorkflowExecutionRequest,
    ) -> Result<StartedWorkflowExecution> {
        // Step 1: Load workflow from repository
        let workflow = if let Ok(uuid) = uuid::Uuid::parse_str(&request.workflow_id) {
            let id = WorkflowId::from_uuid(uuid);
            self.workflow_repository.find_by_id(id).await
        } else {
            self.workflow_repository
                .find_by_name(&request.workflow_id)
                .await
        }
        .context("Failed to query workflow repository")?
        .ok_or_else(|| anyhow::anyhow!("Workflow not found: {}", request.workflow_id))?;

        // Step 2: Create workflow execution aggregate
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let mut workflow_execution =
            WorkflowExecution::new(&workflow, execution_id, request.input.clone());

        // Step 3: Merge initial blackboard if provided
        if let Some(blackboard_json) = request.blackboard {
            if let Ok(blackboard_map) =
                serde_json::from_value::<HashMap<String, serde_json::Value>>(blackboard_json)
            {
                for (key, value) in blackboard_map {
                    workflow_execution.blackboard.set(key, value);
                }
            }
        }

        // Step 4: Persist execution to repository (establishes idempotency key)
        self.execution_repository
            .save(&workflow_execution)
            .await
            .context("Failed to persist workflow execution to repository")?;

        // Step 5: Start execution in Temporal via gRPC
        let engine = {
            let lock = self.workflow_engine.read().await;
            lock.clone()
                .ok_or_else(|| anyhow::anyhow!("Workflow engine not connected yet"))?
        };

        let temporal_run_id = engine
            .start_workflow(
                &workflow.metadata.name,
                execution_id,
                match &request.input {
                    serde_json::Value::Object(map) => {
                        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    }
                    _ => {
                        // Wrap non-object inputs
                        let mut map = HashMap::new();
                        map.insert("input".to_string(), request.input.clone());
                        map
                    }
                },
            )
            .await
            .context("Failed to start workflow execution in Temporal")?;

        // Step 6: Update execution with temporal_run_id (for tracking)
        // Current behavior attaches the run_id to the response payload.

        // Step 7: Publish domain event
        self.event_bus.publish_workflow_event(
            crate::domain::events::WorkflowEvent::WorkflowExecutionStarted {
                execution_id,
                workflow_id: workflow.id,
                started_at: Utc::now(),
            },
        );

        Ok(StartedWorkflowExecution {
            execution_id: execution_id.0.to_string(),
            workflow_id: request.workflow_id,
            temporal_run_id,
            status: "running".to_string(),
            started_at: Utc::now(),
        })
    }
}
