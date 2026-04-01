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

use crate::application::complete_workflow_execution::{
    CompleteWorkflowExecutionRequest, CompleteWorkflowExecutionUseCase, CompletionStatus,
    StandardCompleteWorkflowExecutionUseCase,
};
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

const EVENT_TYPE_REFINEMENT_APPLIED: &str = "RefinementApplied";

/// Canonical file name used to store validation feedback artifacts produced by refinement
/// workflows.
///
/// This is consumed by the `RefinementApplied` event handler, which expects any validation
/// feedback generated during a refinement iteration to be written under this name within the
/// associated artifact set or storage location.
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

    /// Parent execution ID for subworkflow events (ADR-065)
    #[serde(default)]
    pub parent_execution_id: Option<String>,

    /// Child execution ID for subworkflow events (ADR-065)
    #[serde(default)]
    pub child_execution_id: Option<String>,

    /// Child workflow ID for subworkflow events (ADR-065)
    #[serde(default)]
    pub child_workflow_id: Option<String>,

    /// Subworkflow mode: "blocking" or "fire_and_forget" (ADR-065)
    #[serde(default)]
    pub mode: Option<String>,

    /// Result key for SubworkflowCompleted events (ADR-065)
    #[serde(default)]
    pub result_key: Option<String>,

    /// Parent state name for SubworkflowTriggered events (ADR-065)
    #[serde(default)]
    pub parent_state_name: Option<String>,

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

                // Record state transition metric (ADR-058, BC-3).
                // The entered event does not carry the previous state, so we record
                // "unknown" as `from_kind` and the entered state as `to_kind`.
                metrics::counter!(
                    "aegis_workflow_state_transitions_total",
                    "from_kind" => "unknown",
                    "to_kind" => state_name.clone(),
                )
                .increment(1);

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

            "SubworkflowTriggered" => {
                let parent_execution_id = ExecutionId(Uuid::parse_str(&payload.execution_id)?);
                let child_execution_id = ExecutionId(Uuid::parse_str(
                    payload.child_execution_id.as_deref().ok_or_else(|| {
                        anyhow!("SubworkflowTriggered missing child_execution_id")
                    })?,
                )?);
                let child_workflow_id = WorkflowId(Uuid::parse_str(
                    payload
                        .child_workflow_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("SubworkflowTriggered missing child_workflow_id"))?,
                )?);
                let mode = payload
                    .mode
                    .clone()
                    .unwrap_or_else(|| "blocking".to_string());
                let parent_state_name = payload
                    .parent_state_name
                    .clone()
                    .or_else(|| payload.state_name.clone())
                    .unwrap_or_default();

                Ok(WorkflowEvent::SubworkflowTriggered {
                    parent_execution_id,
                    child_execution_id,
                    child_workflow_id,
                    mode,
                    parent_state_name,
                    triggered_at: timestamp,
                })
            }

            "SubworkflowCompleted" => {
                let parent_execution_id = ExecutionId(Uuid::parse_str(&payload.execution_id)?);
                let child_execution_id = ExecutionId(Uuid::parse_str(
                    payload.child_execution_id.as_deref().ok_or_else(|| {
                        anyhow!("SubworkflowCompleted missing child_execution_id")
                    })?,
                )?);
                let result_key = payload.result_key.clone().unwrap_or_default();

                Ok(WorkflowEvent::SubworkflowCompleted {
                    parent_execution_id,
                    child_execution_id,
                    result_key,
                    completed_at: timestamp,
                })
            }

            "SubworkflowFailed" => {
                let parent_execution_id = ExecutionId(Uuid::parse_str(&payload.execution_id)?);
                let child_execution_id = ExecutionId(Uuid::parse_str(
                    payload
                        .child_execution_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("SubworkflowFailed missing child_execution_id"))?,
                )?);
                let reason = payload
                    .error
                    .clone()
                    .unwrap_or_else(|| "Unknown child workflow failure".to_string());

                Ok(WorkflowEvent::SubworkflowFailed {
                    parent_execution_id,
                    child_execution_id,
                    reason,
                    failed_at: timestamp,
                })
            }

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

    /// Persist an execution-scoped event to the repository and publish it to the event bus.
    ///
    /// This helper encapsulates the two-step pattern used for all execution events:
    /// 1. Serialise the raw payload and append it to the event log via the repository.
    /// 2. Publish the mapped domain event to the in-process event bus for subscribers.
    ///
    /// # Arguments
    ///
    /// * `execution_id` - The execution this event belongs to.
    /// * `temporal_sequence_number` - The Temporal sequence number for ordering.
    /// * `event_type` - The string event type name.
    /// * `raw_payload` - The original Temporal payload to persist as JSON.
    /// * `iteration_number` - Optional iteration number associated with this event.
    /// * `domain_event` - The mapped domain event to publish after persistence.
    async fn persist_and_publish_execution_event(
        &self,
        execution_id: ExecutionId,
        temporal_sequence_number: i64,
        event_type: String,
        raw_payload: &TemporalEventPayload,
        iteration_number: Option<u8>,
        domain_event: crate::domain::events::ExecutionEvent,
    ) -> Result<()> {
        let serialized_payload = serde_json::to_value(raw_payload)
            .context("Failed to serialize TemporalEventPayload for persistence")?;

        self.execution_repository
            .append_event(
                execution_id,
                temporal_sequence_number,
                event_type,
                serialized_payload,
                iteration_number,
            )
            .await
            .context("Failed to persist execution event")?;

        self.event_bus.publish_execution_event(domain_event);
        Ok(())
    }

    async fn persist_workflow_event(
        &self,
        execution_id: ExecutionId,
        temporal_sequence_number: i64,
        event_type: String,
        raw_payload: &TemporalEventPayload,
        iteration_number: Option<u8>,
    ) -> Result<()> {
        self.execution_repository
            .append_event(
                execution_id,
                temporal_sequence_number,
                event_type,
                serde_json::to_value(raw_payload)?,
                iteration_number,
            )
            .await
            .context("Failed to persist execution event")?;

        Ok(())
    }

    async fn reconcile_terminal_workflow_event(
        &self,
        execution_id: ExecutionId,
        payload: &TemporalEventPayload,
        domain_event: &WorkflowEvent,
    ) -> Result<()> {
        let tenant_id = self
            .execution_repository
            .find_tenant_id_by_execution(execution_id)
            .await
            .context("Failed to resolve workflow execution tenant")?
            .ok_or_else(|| anyhow!("Workflow execution not found: {}", execution_id.0))?;

        let completion_request = match domain_event {
            WorkflowEvent::WorkflowExecutionCompleted { .. } => CompleteWorkflowExecutionRequest {
                execution_id: execution_id.to_string(),
                status: CompletionStatus::Success,
                final_blackboard: payload.final_blackboard.clone(),
                error_reason: None,
                artifacts: payload
                    .artifacts
                    .as_ref()
                    .map(|artifacts| serde_json::json!(artifacts)),
            },
            WorkflowEvent::WorkflowExecutionFailed { reason, .. } => {
                CompleteWorkflowExecutionRequest {
                    execution_id: execution_id.to_string(),
                    status: CompletionStatus::Failed,
                    final_blackboard: payload.final_blackboard.clone(),
                    error_reason: Some(reason.clone()),
                    artifacts: None,
                }
            }
            WorkflowEvent::WorkflowExecutionCancelled { .. } => CompleteWorkflowExecutionRequest {
                execution_id: execution_id.to_string(),
                status: CompletionStatus::Cancelled,
                final_blackboard: payload.final_blackboard.clone(),
                error_reason: None,
                artifacts: None,
            },
            _ => unreachable!("non-terminal workflow event passed to terminal reconciler"),
        };

        StandardCompleteWorkflowExecutionUseCase::new(
            self.execution_repository.clone(),
            self.event_bus.clone(),
        )
        .complete_execution_for_tenant(&tenant_id, completion_request)
        .await?;

        Ok(())
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
        if payload.event_type == EVENT_TYPE_REFINEMENT_APPLIED {
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

            let diff_val = payload
                .code_diff
                .clone()
                .ok_or_else(|| anyhow!("Missing code_diff for RefinementApplied event"))?;
            let diff_str = match diff_val {
                serde_json::Value::String(s) => s,
                other => {
                    return Err(anyhow!(
                        "Invalid code_diff format for RefinementApplied event: expected string, got {other}"
                    ));
                }
            };

            let code_diff = crate::domain::execution::CodeDiff {
                // NOTE: RefinementApplied code diffs are persisted under the canonical
                // validation feedback artifact path. The file_path here is intentionally
                // set to VALIDATION_FEEDBACK_FILE_NAME to indicate where in the workspace
                // this diff content should be stored/loaded.
                file_path: VALIDATION_FEEDBACK_FILE_NAME.to_string(),
                diff: diff_str,
            };

            let applied_at = DateTime::parse_from_rfc3339(&payload.timestamp)
                .context("Failed to parse timestamp as RFC3339 for RefinementApplied event")?
                .with_timezone(&Utc);

            let domain_event = crate::domain::events::ExecutionEvent::RefinementApplied {
                execution_id,
                agent_id,
                iteration_number,
                code_diff,
                applied_at,
                cortex_pattern_id: None,
                cortex_pattern_category: None,
                cortex_success_score: None,
                cortex_solution_approach: None,
            };

            self.persist_and_publish_execution_event(
                execution_id,
                payload.temporal_sequence_number,
                payload.event_type.clone(),
                &payload,
                Some(iteration_number),
                domain_event,
            )
            .await?;

            return Ok(payload.execution_id.clone());
        }

        // Step 1: Map external event to domain event (ACL)
        let domain_event = TemporalEventMapper::to_domain_event(&payload)
            .context("Failed to map Temporal event to domain event")?;

        // All remaining workflow events are execution-scoped and carry an execution_id.
        let execution_id_obj = match &domain_event {
            WorkflowEvent::WorkflowExecutionStarted { execution_id, .. }
            | WorkflowEvent::WorkflowStateEntered { execution_id, .. }
            | WorkflowEvent::WorkflowStateExited { execution_id, .. }
            | WorkflowEvent::WorkflowIterationStarted { execution_id, .. }
            | WorkflowEvent::WorkflowIterationCompleted { execution_id, .. }
            | WorkflowEvent::WorkflowIterationFailed { execution_id, .. }
            | WorkflowEvent::WorkflowExecutionCompleted { execution_id, .. }
            | WorkflowEvent::WorkflowExecutionFailed { execution_id, .. }
            | WorkflowEvent::WorkflowExecutionCancelled { execution_id, .. } => *execution_id,
            WorkflowEvent::SubworkflowTriggered {
                parent_execution_id,
                ..
            }
            | WorkflowEvent::SubworkflowCompleted {
                parent_execution_id,
                ..
            }
            | WorkflowEvent::SubworkflowFailed {
                parent_execution_id,
                ..
            } => *parent_execution_id,
            WorkflowEvent::IntentExecutionPipelineStarted {
                pipeline_execution_id,
                ..
            }
            | WorkflowEvent::IntentExecutionPipelineCompleted {
                pipeline_execution_id,
                ..
            }
            | WorkflowEvent::IntentExecutionPipelineFailed {
                pipeline_execution_id,
                ..
            } => *pipeline_execution_id,
            WorkflowEvent::WorkflowRegistered { .. }
            | WorkflowEvent::WorkflowScopeChanged { .. } => {
                return Err(anyhow!(
                    "WorkflowRegistered/WorkflowScopeChanged event unexpectedly reached \
                     execution-scoped handling; this variant should be handled by \
                     TemporalEventMapper::to_domain_event"
                ));
            }
        };

        // Step 2: Persist event to the repository for event sourcing
        self.persist_workflow_event(
            execution_id_obj,
            payload.temporal_sequence_number,
            payload.event_type.clone(),
            &payload,
            payload.iteration_number,
        )
        .await?;

        // Step 3: Reconcile terminal state before publishing, otherwise publish directly.
        match &domain_event {
            WorkflowEvent::WorkflowExecutionCompleted { .. }
            | WorkflowEvent::WorkflowExecutionFailed { .. }
            | WorkflowEvent::WorkflowExecutionCancelled { .. } => {
                self.reconcile_terminal_workflow_event(execution_id_obj, &payload, &domain_event)
                    .await?;
            }
            _ => self.event_bus.publish_workflow_event(domain_event.clone()),
        }

        // Step 4: Return execution ID for response
        Ok(execution_id_obj.0.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::{ExecutionEvent, WorkflowEvent};
    use crate::domain::execution::ExecutionId;
    use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
    use crate::domain::tenant::TenantId;
    use crate::domain::workflow::{
        StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
        WorkflowMetadata, WorkflowSpec, WorkflowState,
    };
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;

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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let event = TemporalEventMapper::to_domain_event(&payload).unwrap();
        assert!(
            matches!(event, WorkflowEvent::WorkflowExecutionStarted { .. }),
            "Expected WorkflowExecutionStarted, got {event:?}"
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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let err = TemporalEventMapper::to_domain_event(&payload).unwrap_err();
        assert!(err.to_string().contains("output required"));
    }

    struct FailingAppendRepo;

    #[async_trait]
    impl WorkflowExecutionRepository for FailingAppendRepo {
        async fn save_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            execution: &crate::domain::workflow::WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            self.save(execution).await
        }

        async fn save(
            &self,
            _execution: &crate::domain::workflow::WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn find_by_id_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            id: ExecutionId,
        ) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            self.find_by_id(id).await
        }

        async fn find_by_id(
            &self,
            _id: ExecutionId,
        ) -> Result<Option<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            Ok(None)
        }

        async fn find_tenant_id_by_execution(
            &self,
            _id: ExecutionId,
        ) -> Result<Option<TenantId>, RepositoryError> {
            Ok(None)
        }

        async fn find_active_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            self.find_active().await
        }

        async fn find_active(
            &self,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn find_by_workflow_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _workflow_id: crate::domain::workflow::WorkflowId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn update_temporal_linkage_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _execution_id: ExecutionId,
            _temporal_workflow_id: &str,
            _temporal_run_id: &str,
        ) -> Result<(), RepositoryError> {
            Ok(())
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

        async fn find_events_by_execution(
            &self,
            _id: ExecutionId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecutionEventRecord>, RepositoryError>
        {
            Ok(vec![])
        }

        async fn list_paginated(
            &self,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn list_paginated_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<crate::domain::workflow::WorkflowExecution>, RepositoryError> {
            self.list_paginated(limit, offset).await
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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
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
            code_diff: Some(serde_json::json!("updated prompt")),
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
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
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        let err = listener.handle_event(payload).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Failed to persist execution event"));
        assert!(receiver.try_recv().is_err());
    }

    fn build_test_workflow(name: &str) -> Workflow {
        let mut states = HashMap::new();
        states.insert(
            StateName::new("START").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo start".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![TransitionRule {
                    condition: TransitionCondition::Always,
                    target: StateName::new("END").unwrap(),
                    feedback: None,
                }],
                timeout: None,
            },
        );
        states.insert(
            StateName::new("END").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo end".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );

        Workflow::new(
            WorkflowMetadata {
                name: name.to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                tags: vec![],
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
            WorkflowSpec {
                initial_state: StateName::new("START").unwrap(),
                context: HashMap::new(),
                states,
                storage: Default::default(),
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_handle_terminal_event_updates_persisted_status_for_non_local_tenant() {
        let tenant_id = TenantId::from_string("tenant-blue").unwrap();
        let workflow = build_test_workflow("listener-complete");
        let execution_id = ExecutionId::new();
        let execution = WorkflowExecution::new(&workflow, execution_id, json!({"task": "ship"}));
        let repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        repo.save_for_tenant(&tenant_id, &execution).await.unwrap();

        let event_bus = Arc::new(EventBus::new(16));
        let listener = TemporalEventListener::new(event_bus.clone(), repo.clone());
        let mut receiver = event_bus.subscribe();

        let payload = TemporalEventPayload {
            event_type: "WorkflowExecutionCompleted".to_string(),
            execution_id: execution_id.to_string(),
            temporal_sequence_number: 100,
            workflow_id: Some(workflow.id.to_string()),
            state_name: None,
            output: None,
            error: None,
            iteration_number: None,
            final_blackboard: Some(json!({"result": "done"})),
            artifacts: Some(vec!["report.md".to_string()]),
            agent_id: None,
            code_diff: None,
            parent_execution_id: None,
            child_execution_id: None,
            child_workflow_id: None,
            mode: None,
            result_key: None,
            parent_state_name: None,
            timestamp: "2026-02-19T12:00:00Z".to_string(),
        };

        listener.handle_event(payload).await.unwrap();

        let saved = repo
            .find_by_id_for_tenant(&tenant_id, execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.status,
            crate::domain::execution::ExecutionStatus::Completed
        );
        assert_eq!(saved.blackboard.get("result"), Some(&json!("done")));
        assert!(
            repo.find_by_id(execution_id).await.unwrap().is_none(),
            "default tenant lookup should not be used for non-local executions"
        );

        match receiver.recv().await.unwrap() {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionCompleted {
                execution_id: published_id,
                final_blackboard,
                artifacts,
                ..
            }) => {
                assert_eq!(published_id, execution_id);
                assert_eq!(final_blackboard.get("result"), Some(&json!("done")));
                assert_eq!(artifacts, Some(json!(["report.md"])));
            }
            other => panic!("expected workflow completion event, got {other:?}"),
        }
    }
}
