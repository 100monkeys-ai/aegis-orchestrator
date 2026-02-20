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

use crate::domain::workflow::{WorkflowExecution, WorkflowId};
use crate::domain::repository::{WorkflowRepository, WorkflowExecutionRepository};
use crate::domain::execution::ExecutionId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::temporal_client::TemporalClient;
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
    
    /// Optional blackboard initial state
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
    async fn start_execution(&self, request: StartWorkflowExecutionRequest) -> Result<StartedWorkflowExecution>;
}

/// Standard implementation of StartWorkflowExecutionUseCase
pub struct StandardStartWorkflowExecutionUseCase {
    workflow_repository: Arc<dyn WorkflowRepository>,
    execution_repository: Arc<dyn WorkflowExecutionRepository>,
    temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
    event_bus: Arc<EventBus>,
    cortex_service: Option<Arc<dyn aegis_cortex::application::CortexService>>,
}

impl StandardStartWorkflowExecutionUseCase {
    pub fn new(
        workflow_repository: Arc<dyn WorkflowRepository>,
        execution_repository: Arc<dyn WorkflowExecutionRepository>,
        temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
        event_bus: Arc<EventBus>,
        cortex_service: Option<Arc<dyn aegis_cortex::application::CortexService>>,
    ) -> Self {
        Self {
            workflow_repository,
            execution_repository,
            temporal_client,
            event_bus,
            cortex_service,
        }
    }
}

#[async_trait]
impl StartWorkflowExecutionUseCase for StandardStartWorkflowExecutionUseCase {
    async fn start_execution(&self, request: StartWorkflowExecutionRequest) -> Result<StartedWorkflowExecution> {
        // Step 1: Load workflow from repository
        let workflow = if let Ok(uuid) = uuid::Uuid::parse_str(&request.workflow_id) {
            let id = WorkflowId::from_uuid(uuid);
            self.workflow_repository.find_by_id(id).await
        } else {
            self.workflow_repository.find_by_name(&request.workflow_id).await
        }
        .context("Failed to query workflow repository")?
        .ok_or_else(|| anyhow::anyhow!(
            "Workflow not found: {}",
            request.workflow_id
        ))?;

        // Step 2: Create workflow execution aggregate
        let execution_id = ExecutionId(uuid::Uuid::new_v4());
        let mut workflow_execution = WorkflowExecution::new(&workflow, execution_id, request.input.clone());

        // Step 3: Merge initial blackboard if provided
        if let Some(blackboard_json) = request.blackboard {
            if let Ok(blackboard_map) = serde_json::from_value::<HashMap<String, serde_json::Value>>(blackboard_json) {
                for (key, value) in blackboard_map {
                    workflow_execution.blackboard.set(key, value);
                }
            }
        }

        // Optional Step 3.5: Cortex pattern pre-population
        if let Some(cortex) = &self.cortex_service {
            // For MVP, if we don't have an embedding client we can query with zeros to get recent patterns
            if let Ok(patterns) = cortex.search_patterns(vec![0.0; 384], 3).await {
                if !patterns.is_empty() {
                    let patterns_json = serde_json::to_value(patterns).unwrap_or(serde_json::json!([]));
                    workflow_execution.blackboard.set("cortex_prepopulated_patterns".to_string(), patterns_json);
                }
            }
        }

        // Step 4: Persist execution to repository (establishes idempotency key)
        self.execution_repository
            .save(&workflow_execution)
            .await
            .context("Failed to persist workflow execution to repository")?;

        // Step 5: Start execution in Temporal via gRPC
        let client = {
            let lock = self.temporal_client.read().await;
            lock.clone().ok_or_else(|| anyhow::anyhow!("Temporal client not connected yet"))?
        };

        let temporal_run_id = client
            .start_workflow(
                &workflow.metadata.name,
                execution_id,
                match &request.input {
                    serde_json::Value::Object(map) => {
                        map.iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    },
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
        // Note: In a full implementation, we would update the repository with the run_id
        // For now, we attach it only in the response

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

