// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal Event Listener
//!
//! Infrastructure component that receives workflow events from the Temporal TypeScript worker
//! and publishes them to the domain event bus.
//!
//! # DDD Pattern: Anti-Corruption Layer + Infrastructure Service
//!
//! - **Layer:** Infrastructure
//! - **Responsibility:** Translate external Temporal events to domain events
//! - **Collaborators:**
//!   - EventBus: Publishes domain events to subscribers
//!   - Domain: WorkflowEvent types
//!
//! # Event Flow
//!
//! ```text
//! Temporal Worker (TypeScript)
//!   |
//!   | HTTP POST /temporal-events
//!   v
//! TemporalEventListener
//!   |
//!   | Parse & validate payload
//!   v
//! TemporalEventMapper (ACL)
//!   |
//!   | Map to domain WorkflowEvent
//!   v
//! EventBus::publish_workflow_event()
//!   |
//!   v
//! All Subscribers (Cortex, Audit Trail, Metrics, etc.)
//! ```
//!
//! # Event Payload Contract
//!
//! The Temporal worker sends JSON POST requests with this structure:
//!
//! ```json
//! {
//!   "event_type": "WorkflowExecutionStarted|WorkflowStateEntered|WorkflowStateExited|...",
//!   "execution_id": "uuid",
//!   "workflow_id": "uuid (optional)",
//!   "state_name": "string (optional)",
//!   "output": "string (optional)",
//!   "error": "string (optional)",
//!   "timestamp": "RFC3339"
//! }
//! ```
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for temporal event listener

use crate::domain::events::WorkflowEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::repository::WorkflowExecutionRepository;
use crate::domain::workflow::WorkflowId;
use crate::infrastructure::event_bus::EventBus;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

const VALIDATION_FEEDBACK_FILE_NAME: &str = "validation_feedback";

/// External event payload from Temporal worker
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemporalEventPayload {
    /// Event type (WorkflowExecutionStarted, StateEntered, etc.)
    pub event_type: String,

    /// Execution ID (always present)
    pub execution_id: String,

    /// Temporal correlator / sequence number
    pub temporal_sequence_number: i64,

    /// Workflow ID (optional)
    pub workflow_id: Option<String>,

    /// State name (optional, for StateEntered/StateExited events)
    pub state_name: Option<String>,

    /// State output or final blackboard snapshot (optional).
    /// Populated from `WorkflowExecutionCompleted` events; stored on `WorkflowExecution` for audit.
    pub output: Option<serde_json::Value>,

    /// Error message (optional, for failure events)
    pub error: Option<String>,

    /// Iteration number (optional, for iteration events)
    pub iteration_number: Option<u8>,

    /// Final blackboard state (optional, for completion events)
    pub final_blackboard: Option<serde_json::Value>,

    /// Artifacts produced (optional)
    pub artifacts: Option<Vec<String>>,

    /// Agent ID (optional)
    pub agent_id: Option<String>,

    /// Code diff / reasoning (optional, for refinement)
    pub code_diff: Option<serde_json::Value>,

    /// Event timestamp
    pub timestamp: String,
}

/// Temporal Event Mapper (Anti-Corruption Layer)
///
/// Translates external Temporal event payloads to domain WorkflowEvent objects.
/// Ensures domain logic is not polluted by external event structure.
pub struct TemporalEventMapper;

impl TemporalEventMapper {
    /// Map external Temporal event to domain WorkflowEvent
    ///
    /// # Errors
    ///
    /// - Invalid execution_id (not valid UUID)
    /// - Unknown event_type
    /// - Missing required fields for event type
    /// - Invalid timestamp format
    pub fn to_domain_event(payload: &TemporalEventPayload) -> Result<WorkflowEvent> {
        // Parse execution ID
        let execution_id = ExecutionId(
            Uuid::parse_str(&payload.execution_id)
                .context("Invalid execution_id format (expected UUID)")?,
        );

        // Parse timestamp
        let timestamp = DateTime::parse_from_rfc3339(&payload.timestamp)
            .context("Invalid timestamp format (expected RFC3339)")?
            .with_timezone(&Utc);

        // Parse workflow ID (optional)
        let workflow_id = match &payload.workflow_id {
            Some(id_str) => match Uuid::parse_str(id_str) {
                Ok(uuid) => WorkflowId(uuid),
                Err(err) => {
                    tracing::warn!(
                        workflow_id = %id_str,
                        error = %err,
                        "TemporalEventMapper: invalid workflow_id encountered; falling back to nil UUID"
                    );
                    WorkflowId(Uuid::nil())
                }
            },
            None => WorkflowId(Uuid::nil()),
        };

        // Map event type to domain event
        match payload.event_type.as_str() {
            "WorkflowExecutionStarted" => Ok(WorkflowEvent::WorkflowExecutionStarted {
                execution_id,
                workflow_id,
                started_at: timestamp,
            }),

            "WorkflowStateEntered" => {
                let state_name = payload
                    .state_name
                    .clone()
                    .ok_or_else(|| anyhow!("state_name required for WorkflowStateEntered event"))?;

                Ok(WorkflowEvent::WorkflowStateEntered {
                    execution_id,
                    state_name,
                    entered_at: timestamp,
                })
            }

            "WorkflowStateExited" => {
                let state_name = payload
                    .state_name
                    .clone()
                    .ok_or_else(|| anyhow!("state_name required for WorkflowStateExited event"))?;

                let output = payload
                    .output
                    .clone()
                    .ok_or_else(|| anyhow!("output required for WorkflowStateExited event"))?;

                Ok(WorkflowEvent::WorkflowStateExited {
                    execution_id,
                    state_name,
                    output,
                    exited_at: timestamp,
                })
            }

            "WorkflowIterationStarted" => {
                let iteration_number = payload.iteration_number.ok_or_else(|| {
                    anyhow!("iteration_number required for WorkflowIterationStarted event")
                })?;

                Ok(WorkflowEvent::WorkflowIterationStarted {
                    execution_id,
                    iteration_number,
                    started_at: timestamp,
                })
            }

            "WorkflowIterationCompleted" => {
                let iteration_number = payload.iteration_number.ok_or_else(|| {
                    anyhow!("iteration_number required for WorkflowIterationCompleted event")
                })?;

                let output = payload.output.clone().ok_or_else(|| {
                    anyhow!("output required for WorkflowIterationCompleted event")
                })?;

                Ok(WorkflowEvent::WorkflowIterationCompleted {
                    execution_id,
                    iteration_number,
                    output,
                    completed_at: timestamp,
                })
            }

            "WorkflowIterationFailed" => {
                let iteration_number = payload.iteration_number.ok_or_else(|| {
                    anyhow!("iteration_number required for WorkflowIterationFailed event")
                })?;

                let error = payload
                    .error
                    .clone()
                    .ok_or_else(|| anyhow!("error required for WorkflowIterationFailed event"))?;

                Ok(WorkflowEvent::WorkflowIterationFailed {
                    execution_id,
                    iteration_number,
                    error,
                    failed_at: timestamp,
                })
            }

            "WorkflowExecutionCompleted" => Ok(WorkflowEvent::WorkflowExecutionCompleted {
                execution_id,
                final_blackboard: payload
                    .final_blackboard
                    .clone()
                    .unwrap_or(serde_json::json!({})),
                artifacts: payload.artifacts.as_ref().map(|v| serde_json::json!(v)),
                completed_at: timestamp,
            }),

            "WorkflowExecutionFailed" => {
                let reason = payload
                    .error
                    .clone()
                    .ok_or_else(|| anyhow!("error required for WorkflowExecutionFailed event"))?;

                Ok(WorkflowEvent::WorkflowExecutionFailed {
                    execution_id,
                    reason,
                    failed_at: timestamp,
                })
            }

            "WorkflowExecutionCancelled" => Ok(WorkflowEvent::WorkflowExecutionCancelled {
                execution_id,
                cancelled_at: timestamp,
            }),

            _ => Err(anyhow!("Unknown event_type: {}", payload.event_type)),
        }
    }
}

/// Temporal Event Listener Service
///
/// Application service that receives Temporal events and publishes to event bus.
pub struct TemporalEventListener {
    event_bus: Arc<EventBus>,
    execution_repository: Arc<dyn WorkflowExecutionRepository>,
}

impl TemporalEventListener {
    pub fn new(
        event_bus: Arc<EventBus>,
        execution_repository: Arc<dyn WorkflowExecutionRepository>,
    ) -> Self {
        Self {
            event_bus,
            execution_repository,
        }
    }

    /// Process incoming event from Temporal worker
    ///
    /// # Arguments
    ///
    /// * `payload` - Raw event payload from Temporal worker
    ///
    /// # Returns
    ///
    /// Parsed domain event ID (execution_id) on success
    ///
    /// # Errors
    ///
    /// - Invalid event format
    /// - Unrecognized event type
    /// - Missing required fields
    pub async fn handle_event(&self, payload: TemporalEventPayload) -> Result<String> {
        // Special case for RefinementApplied execution event
        if payload.event_type == "RefinementApplied" {
            let execution_id = ExecutionId(uuid::Uuid::parse_str(&payload.execution_id)?);
            let agent_id_str = payload
                .agent_id
                .clone()
                .ok_or_else(|| anyhow!("RefinementApplied event requires agent_id"))?;
            let agent_id = crate::domain::agent::AgentId(
                uuid::Uuid::parse_str(&agent_id_str)
                    .context("Failed to parse agent_id as UUID for RefinementApplied event")?,
            );
            let iteration_number = payload
                .iteration_number
                .ok_or_else(|| anyhow!("Missing iteration_number for RefinementApplied event"))?;

            let diff_val = payload.code_diff.clone().unwrap_or_default();
            let diff_str = match diff_val {
                serde_json::Value::String(s) => s,
                _ => diff_val.to_string(),
            };

            let code_diff = crate::domain::execution::CodeDiff {
                file_path: VALIDATION_FEEDBACK_FILE_NAME.to_string(),
                diff: diff_str,
            };

            let domain_event = crate::domain::events::ExecutionEvent::RefinementApplied {
                execution_id,
                agent_id,
                iteration_number,
                code_diff,
                applied_at: chrono::Utc::now(),
            };

            self.execution_repository
                .append_event(
                    execution_id,
                    payload.temporal_sequence_number,
                    payload.event_type.clone(),
                    serde_json::to_value(&payload)?,
                    Some(iteration_number),
                )
                .await
                .context("Failed to persist execution event")?;

            self.event_bus.publish_execution_event(domain_event);
            return Ok(payload.execution_id.clone());
        }

        // Step 1: Map external event to domain event (ACL)
        let domain_event = TemporalEventMapper::to_domain_event(&payload)
            .context("Failed to map Temporal event to domain event")?;

        let execution_id_obj = match &domain_event {
            WorkflowEvent::WorkflowRegistered { .. } => {
                // Definition-time event — no execution_id exists.
                // Publish to the event bus so subscribers are notified, then return early.
                self.event_bus.publish_workflow_event(domain_event.clone());
                return Ok(String::new());
            }
            WorkflowEvent::WorkflowExecutionStarted { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowStateEntered { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowStateExited { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowIterationStarted { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowIterationCompleted { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowIterationFailed { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowExecutionCompleted { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowExecutionFailed { execution_id, .. } => *execution_id,
            WorkflowEvent::WorkflowExecutionCancelled { execution_id, .. } => *execution_id,
        };

        let execution_id_str = execution_id_obj.0.to_string();

        // Step 2: Persist event to the repository for event sourcing
        self.execution_repository
            .append_event(
                execution_id_obj,
                payload.temporal_sequence_number,
                payload.event_type.clone(),
                serde_json::to_value(&payload)?,
                payload.iteration_number,
            )
            .await
            .context("Failed to persist execution event")?;

        // Step 3: Publish to event bus
        self.event_bus.publish_workflow_event(domain_event.clone());

        // Step 4: Return execution ID for response
        Ok(execution_id_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::{ExecutionEvent, WorkflowEvent};
    use crate::domain::execution::ExecutionId;
    use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
    use async_trait::async_trait;

    #[test]
    fn test_map_workflow_execution_started() {
        let payload = TemporalEventPayload {
            event_type: "WorkflowExecutionStarted".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 1,
            workflow_id: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let event = TemporalEventMapper::to_domain_event(&payload).unwrap();
        assert!(
            matches!(event, WorkflowEvent::WorkflowExecutionStarted { .. }),
            "Expected WorkflowExecutionStarted, got {:?}",
            event
        );
        let WorkflowEvent::WorkflowExecutionStarted { execution_id, .. } = event else {
            return;
        };
        assert_eq!(
            execution_id.0.to_string(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_map_invalid_execution_id() {
        let payload = TemporalEventPayload {
            event_type: "WorkflowExecutionStarted".to_string(),
            execution_id: "not-a-uuid".to_string(),
            temporal_sequence_number: 2,
            workflow_id: None,
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let result = TemporalEventMapper::to_domain_event(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_map_unknown_event_type() {
        let payload = TemporalEventPayload {
            event_type: "UnknownEvent".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 3,
            workflow_id: None,
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let result = TemporalEventMapper::to_domain_event(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_map_state_entered_requires_state_name() {
        let payload = TemporalEventPayload {
            event_type: "WorkflowStateEntered".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 10,
            workflow_id: None,
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let err = TemporalEventMapper::to_domain_event(&payload).unwrap_err();
        assert!(err.to_string().contains("state_name required"));
    }

    #[test]
    fn test_map_state_exited_requires_output() {
        let payload = TemporalEventPayload {
            event_type: "WorkflowStateExited".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 11,
            workflow_id: None,
            state_name: Some("BUILD".to_string()),
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let err = TemporalEventMapper::to_domain_event(&payload).unwrap_err();
        assert!(err.to_string().contains("output required"));
    }

    struct FailingAppendRepo;

    #[async_trait]
    impl WorkflowExecutionRepository for FailingAppendRepo {
        async fn save(
            &self,
            _execution: &crate::domain::workflow::WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn find_by_id(
            &self,
            _id: ExecutionId,
        ) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            Ok(None)
        }

        async fn find_active(
            &self,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn append_event(
            &self,
            _execution_id: ExecutionId,
            _temporal_sequence_number: i64,
            _event_type: String,
            _payload: serde_json::Value,
            _iteration_number: Option<u8>,
        ) -> Result<(), RepositoryError> {
            Err(RepositoryError::Database("append failed".to_string()))
        }
    }

    #[tokio::test]
    async fn test_handle_event_persists_and_publishes_workflow_event() {
        let event_bus = Arc::new(EventBus::new(16));
        let repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let listener = TemporalEventListener::new(event_bus.clone(), repo);
        let mut receiver = event_bus.subscribe();

        let payload = TemporalEventPayload {
            event_type: "WorkflowExecutionStarted".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 99,
            workflow_id: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let result_id = listener.handle_event(payload).await.unwrap();
        assert_eq!(result_id, "550e8400-e29b-41d4-a716-446655440000");

        match receiver.recv().await.unwrap() {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionStarted {
                execution_id, ..
            }) => {
                assert_eq!(execution_id.0.to_string(), result_id);
            }
            other => panic!("expected workflow event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_event_refinement_applied_publishes_execution_event() {
        let event_bus = Arc::new(EventBus::new(16));
        let repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let listener = TemporalEventListener::new(event_bus.clone(), repo);
        let mut receiver = event_bus.subscribe();

        let payload = TemporalEventPayload {
            event_type: "RefinementApplied".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 5,
            workflow_id: None,
            state_name: None,
            output: None,
            error: None,
            iteration_number: Some(2),
            final_blackboard: None,
            artifacts: None,
            agent_id: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
            code_diff: Some(serde_json::json!({"summary": "updated prompt"})),
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let result_id = listener.handle_event(payload).await.unwrap();
        assert_eq!(result_id, "550e8400-e29b-41d4-a716-446655440000");

        match receiver.recv().await.unwrap() {
            DomainEvent::Execution(ExecutionEvent::RefinementApplied {
                execution_id,
                iteration_number,
                ..
            }) => {
                assert_eq!(execution_id.0.to_string(), result_id);
                assert_eq!(iteration_number, 2);
            }
            other => panic!("expected execution refinement event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_event_does_not_publish_when_append_fails() {
        let event_bus = Arc::new(EventBus::new(16));
        let listener = TemporalEventListener::new(event_bus.clone(), Arc::new(FailingAppendRepo));
        let mut receiver = event_bus.subscribe();

        let payload = TemporalEventPayload {
            event_type: "WorkflowExecutionStarted".to_string(),
            execution_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            temporal_sequence_number: 12,
            workflow_id: Some("123e4567-e89b-12d3-a456-426614174000".to_string()),
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: None,
            artifacts: None,
            agent_id: None,
            code_diff: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let err = listener.handle_event(payload).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to persist execution event"));
        assert!(receiver.try_recv().is_err());
    }
}
