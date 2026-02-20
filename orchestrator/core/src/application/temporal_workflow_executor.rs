// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal Workflow State Executor with 100monkeys Integration
//!
//! This application service bridges workflow FSM states (Agent, System, Human)
//! with the 100monkeys iterative execution loop, enabling:
//!
//! - **Agent States**: Execute agents via Supervisor, coordinate with Temporal worker
//! - **System States**: Execute commands synchronously
//! - **Human States**: Wait for human input via Temporal Activities
//!
//! # ADR-022 Phase 2 Integration
//!
//! This service implements the missing piece between:
//! - Workflow FSM (states and transitions)
//! - 100monkeys Algorithm (iterative refinement)
//! - Temporal Workflow Engine (async coordination)
//!
//! # State Execution Flow
//!
//! ```text
//! WorkflowEngine.tick()
//!   ↓
//! WorkflowStateExecutor.execute_state(state_name, current_state)
//!   ├─ Agent State:
//!   │   ├ Execute agent via Supervisor (100monkeys loop)
//!   │   ├ Publish iteration events to Temporal
//!   │   └ Return final output
//!   │
//!   ├─ System State:
//!   │   ├ Execute command synchronously
//!   │   └ Return command output
//!   │
//!   └─ Human State:
//!       ├ Create Temporal Activity
//!       ├ Wait for human decision
//!       └ Return human input
//! ```
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for temporal workflow executor

use crate::domain::workflow::{Workflow, WorkflowState, StateKind, WorkflowExecution, StateName};
use crate::domain::events::WorkflowEvent;
use crate::domain::repository::WorkflowRepository;
use crate::application::execution::ExecutionService;
use crate::infrastructure::temporal_client::TemporalClient;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::HumanInputService;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, info};

/// State-agnostic workflow state executor
#[async_trait]
pub trait WorkflowStateExecutor: Send + Sync {
    /// Execute a single workflow state and return its output
    async fn execute_state(
        &self,
        workflow: &Workflow,
        state_name: &StateName,
        state: &WorkflowState,
        workflow_execution: &WorkflowExecution,
    ) -> Result<Value>;
}

/// Standard implementation coordinating Temporal + 100monkeys
#[allow(dead_code)] // Fields used in Phase 2 implementation
pub struct StandardWorkflowStateExecutor {
    execution_service: Arc<dyn ExecutionService>,
    workflow_repository: Arc<dyn WorkflowRepository>,
    temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
    human_input_service: Arc<HumanInputService>,
    event_bus: Arc<EventBus>,
}

impl StandardWorkflowStateExecutor {
    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        workflow_repository: Arc<dyn WorkflowRepository>,
        temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
        human_input_service: Arc<HumanInputService>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            execution_service,
            workflow_repository,
            temporal_client,
            human_input_service,
            event_bus,
        }
    }

    /// Execute an Agent state using the 100monkeys loop
    async fn execute_agent_state(
        &self,
        state_name: &StateName,
        state_kind: &StateKind,
        workflow_execution: &WorkflowExecution,
    ) -> Result<Value> {
        // Extract agent name from StateKind
        let agent_name = match state_kind {
            StateKind::Agent { agent, .. } => agent,
            _ => return Err(anyhow!("Invalid StateKind for agent state")),
        };

        info!(
            state = %state_name,
            agent = agent_name,
            execution_id = %workflow_execution.id.0,
            "Executing agent state with 100monkeys loop"
        );

        // For now, Phase 2 stub: return placeholder
        // Phase 2 will integrate with execution_service.start_execution()
        let output = serde_json::json!({
            "status": "not_implemented",
            "message": format!("Agent state '{}' execution deferred to Phase 2", state_name),
            "agent": agent_name
        });

        // Publish StateEntered event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateEntered {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            entered_at: Utc::now(),
        });

        // Publish StateExited event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateExited {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            output: output.clone(),
            exited_at: Utc::now(),
        });

        Ok(output)
    }

    /// Execute a System state (command execution)
    async fn execute_system_state(
        &self,
        state_name: &StateName,
        state_kind: &StateKind,
        workflow_execution: &WorkflowExecution,
    ) -> Result<Value> {
        // Extract command from StateKind
        let command = match state_kind {
            StateKind::System { command, .. } => command,
            _ => return Err(anyhow!("Invalid StateKind for system state")),
        };

        info!(
            state = %state_name,
            command = command,
            "Executing system state"
        );

        // Publish StateEntered event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateEntered {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            entered_at: Utc::now(),
        });

        // System states typically have a command in the spec
        // For now, we just log and return success
        // Phase 2 should implement actual command execution

        let output = serde_json::json!({
            "status": "not_implemented",
            "message": "System state execution deferred to Phase 2",
            "command": command
        });

        // Publish StateExited event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateExited {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            output: output.clone(),
            exited_at: Utc::now(),
        });

        Ok(output)
    }

    /// Execute a Human state (approval gate)
    async fn execute_human_state(
        &self,
        state_name: &StateName,
        state_kind: &StateKind,
        workflow_execution: &WorkflowExecution,
    ) -> Result<Value> {
        // Extract prompt from StateKind
        let (prompt, _default_response) = match state_kind {
            StateKind::Human { prompt, default_response } => (prompt, default_response),
            _ => return Err(anyhow!("Invalid StateKind for human state")),
        };

        info!(
            state = %state_name,
            prompt = prompt,
            "Executing human state (approval gate)"
        );

        // Publish StateEntered event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateEntered {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            entered_at: Utc::now(),
        });

        // Request human input via service (async wait for response with built-in timeout)
        let status = self
            .human_input_service
            .request_input(
                workflow_execution.id,
                prompt.clone(),
                300, // 5 minute timeout
            )
            .await?;

        info!(
            "Human input response received for state: {} with status: {:?}",
            state_name, status
        );

        // Convert status to output value
        let (status_str, response_text) = match status {
            crate::infrastructure::HumanInputStatus::Approved { feedback, .. } => {
                ("approved", feedback.unwrap_or_else(|| "approved".to_string()))
            },
            crate::infrastructure::HumanInputStatus::Rejected { reason, .. } => {
                ("rejected", reason)
            },
            crate::infrastructure::HumanInputStatus::TimedOut { .. } => {
                ("timed_out", "Request timed out, using default response".to_string())
            },
            crate::infrastructure::HumanInputStatus::Pending => {
                ("pending", "Still pending (unexpected)".to_string())
            },
        };

        let output = serde_json::json!({
            "status": status_str,
            "response": response_text,
        });

        // Publish StateExited event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateExited {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            output: output.clone(),
            exited_at: Utc::now(),
        });

        Ok(output)
    }

    /// Execute parallel agents (coordinated execution of multiple agents)
    async fn execute_parallel_agents(
        &self,
        state_name: &StateName,
        _state_kind: &StateKind,
        workflow_execution: &WorkflowExecution,
    ) -> Result<Value> {
        info!(
            state = %state_name,
            "Executing parallel agents state"
        );

        // Publish StateEntered event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateEntered {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            entered_at: Utc::now(),
        });

        // Phase 2: Parallel agent coordination
        // For now, return a stub response
        let output = serde_json::json!({
            "status": "not_implemented",
            "message": "Parallel agent execution deferred to Phase 2",
            "agents": []
        });

        // Publish StateExited event
        self.event_bus.publish_workflow_event(WorkflowEvent::WorkflowStateExited {
            execution_id: workflow_execution.id,
            state_name: state_name.to_string(),
            output: output.clone(),
            exited_at: Utc::now(),
        });

        Ok(output)
    }
}

#[async_trait]
impl WorkflowStateExecutor for StandardWorkflowStateExecutor {
    async fn execute_state(
        &self,
        _workflow: &Workflow,
        state_name: &StateName,
        state: &WorkflowState,
        workflow_execution: &WorkflowExecution,
    ) -> Result<Value> {
        debug!("Executing state: {} (kind: {:?})", state_name, state.kind);

        match &state.kind {
            StateKind::Agent { .. } => {
                self.execute_agent_state(state_name, &state.kind, workflow_execution).await
            },
            StateKind::System { .. } => {
                self.execute_system_state(state_name, &state.kind, workflow_execution).await
            },
            StateKind::Human { .. } => {
                self.execute_human_state(state_name, &state.kind, workflow_execution).await
            },
            StateKind::ParallelAgents { .. } => {
                self.execute_parallel_agents(state_name, &state.kind, workflow_execution).await
            }
        }
    }
}


