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
use crate::domain::tenant::TenantId;
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
    async fn complete_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        request: CompleteWorkflowExecutionRequest,
    ) -> Result<CompletedWorkflowExecution>;

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
    ) -> Result<CompletedWorkflowExecution> {
        self.complete_execution_for_tenant(&TenantId::local_default(), request)
            .await
    }
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
    async fn complete_execution_for_tenant(
        &self,
        tenant_id: &TenantId,
        request: CompleteWorkflowExecutionRequest,
    ) -> Result<CompletedWorkflowExecution> {
        info!(
            tenant_id = %tenant_id,
            execution_id = %request.execution_id,
            status = ?request.status,
            "Completing workflow execution"
        );

        // Step 1: Parse execution ID
        let execution_id = ExecutionId(
            uuid::Uuid::parse_str(&request.execution_id)
                .context("Invalid execution_id format (not a UUID)")?,
        );

        // Step 2: Load execution from repository
        let mut execution = self
            .execution_repository
            .find_by_id_for_tenant(tenant_id, execution_id)
            .await
            .context("Failed to query execution repository")?
            .ok_or_else(|| {
                anyhow::anyhow!("Workflow execution not found: {}", request.execution_id)
            })?;

        // Step 3: Update execution status
        let final_status = match request.status {
            CompletionStatus::Success => ExecutionStatus::Completed,
            CompletionStatus::Failed => ExecutionStatus::Failed,
            CompletionStatus::Cancelled => ExecutionStatus::Cancelled,
        };

        let final_status_str = format!("{final_status:?}").to_lowercase();
        execution.status = final_status;
        let completed_at = Utc::now();

        // Step 4: Merge final blackboard state if provided
        if let Some(blackboard_json) = request.final_blackboard {
            if let Ok(blackboard_map) = serde_json::from_value::<
                std::collections::HashMap<String, serde_json::Value>,
            >(blackboard_json)
            {
                for (key, value) in blackboard_map {
                    execution.blackboard.set(key, value);
                }
            }
        }

        // Step 5: Persist to repository
        self.execution_repository
            .save_for_tenant(tenant_id, &execution)
            .await
            .context("Failed to persist execution to repository")?;

        // Step 6: Publish domain event
        let event = match request.status {
            CompletionStatus::Success => {
                crate::domain::events::WorkflowEvent::WorkflowExecutionCompleted {
                    execution_id,
                    final_blackboard: serde_json::to_value(execution.blackboard.data())
                        .unwrap_or(serde_json::Value::Null),
                    artifacts: request.artifacts,
                    completed_at,
                }
            }
            CompletionStatus::Failed => {
                crate::domain::events::WorkflowEvent::WorkflowExecutionFailed {
                    execution_id,
                    reason: request
                        .error_reason
                        .unwrap_or_else(|| "workflow execution failed".to_string()),
                    failed_at: completed_at,
                }
            }
            CompletionStatus::Cancelled => {
                crate::domain::events::WorkflowEvent::WorkflowExecutionCancelled {
                    execution_id,
                    cancelled_at: completed_at,
                }
            }
        };
        self.event_bus.publish_workflow_event(event);

        // Step 6b: Record Prometheus metrics (ADR-058, BC-3)
        let metrics_status = match request.status {
            CompletionStatus::Success => "completed",
            CompletionStatus::Failed => "failed",
            CompletionStatus::Cancelled => "cancelled",
        };
        metrics::counter!("aegis_workflow_executions_total", "status" => metrics_status)
            .increment(1);
        metrics::gauge!("aegis_workflow_executions_active").decrement(1.0);

        // Step 7: If execution had failures/refinements, trigger Cortex learning
        // This would be handled by event subscribers in a separate service
        // (Pattern learning is event-driven, not synchronous)

        Ok(CompletedWorkflowExecution {
            execution_id: request.execution_id,
            status: final_status_str,
            completed_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::WorkflowEvent;
    use crate::domain::execution::ExecutionId;
    use crate::domain::repository::WorkflowExecutionRepository;
    use crate::domain::tenant::TenantId;
    use crate::domain::workflow::{
        StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
        WorkflowMetadata, WorkflowSpec, WorkflowState,
    };
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
    use serde_json::json;
    use std::collections::HashMap;

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
                volumes: vec![],
                storage: None,
            },
        )
        .unwrap()
    }

    async fn seed_execution(
        repo: &InMemoryWorkflowExecutionRepository,
        tenant_id: &TenantId,
        workflow_name: &str,
    ) -> ExecutionId {
        let workflow = build_test_workflow(workflow_name);
        let execution_id = ExecutionId::new();
        let mut execution = WorkflowExecution::new(&workflow, execution_id, json!({"task":"x"}));
        execution.blackboard.set("existing", json!("keep"));
        repo.save_for_tenant(tenant_id, &execution).await.unwrap();
        execution_id
    }

    #[tokio::test]
    async fn complete_execution_updates_status_merges_blackboard_and_publishes_event() {
        let repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let tenant_id = TenantId::local_default();
        let execution_id = seed_execution(&repo, &tenant_id, "complete-success").await;
        let event_bus = Arc::new(EventBus::new(32));
        let mut receiver = event_bus.subscribe();

        let service = StandardCompleteWorkflowExecutionUseCase::new(repo.clone(), event_bus);
        let response = service
            .complete_execution_for_tenant(
                &tenant_id,
                CompleteWorkflowExecutionRequest {
                    execution_id: execution_id.to_string(),
                    status: CompletionStatus::Success,
                    final_blackboard: Some(json!({
                        "existing": "updated",
                        "new_key": 42
                    })),
                    error_reason: None,
                    artifacts: Some(json!({"report":"ok"})),
                },
            )
            .await
            .unwrap();

        assert_eq!(response.status, "completed");

        let saved = repo
            .find_by_id_for_tenant(&tenant_id, execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, ExecutionStatus::Completed);
        assert_eq!(saved.blackboard.get("existing"), Some(&json!("updated")));
        assert_eq!(saved.blackboard.get("new_key"), Some(&json!(42)));

        match receiver.recv().await.unwrap() {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionCompleted {
                execution_id: eid,
                final_blackboard,
                artifacts,
                ..
            }) => {
                assert_eq!(eid, execution_id);
                assert_eq!(final_blackboard.get("new_key"), Some(&json!(42)));
                assert_eq!(artifacts, Some(json!({"report":"ok"})));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_execution_handles_failed_status_and_non_object_blackboard() {
        let repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let tenant_id = TenantId::local_default();
        let execution_id = seed_execution(&repo, &tenant_id, "complete-failure").await;
        let event_bus = Arc::new(EventBus::new(8));
        let mut receiver = event_bus.subscribe();
        let service = StandardCompleteWorkflowExecutionUseCase::new(repo.clone(), event_bus);

        let response = service
            .complete_execution_for_tenant(
                &tenant_id,
                CompleteWorkflowExecutionRequest {
                    execution_id: execution_id.to_string(),
                    status: CompletionStatus::Failed,
                    final_blackboard: Some(json!(["not", "an", "object"])),
                    error_reason: Some("boom".to_string()),
                    artifacts: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(response.status, "failed");
        let saved = repo
            .find_by_id_for_tenant(&tenant_id, execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, ExecutionStatus::Failed);
        assert_eq!(saved.blackboard.get("existing"), Some(&json!("keep")));

        match receiver.recv().await.unwrap() {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionFailed {
                execution_id: eid,
                reason,
                ..
            }) => {
                assert_eq!(eid, execution_id);
                assert_eq!(reason, "boom");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_execution_rejects_invalid_execution_id_format() {
        let service = StandardCompleteWorkflowExecutionUseCase::new(
            Arc::new(InMemoryWorkflowExecutionRepository::new()),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .complete_execution(CompleteWorkflowExecutionRequest {
                execution_id: "not-a-uuid".to_string(),
                status: CompletionStatus::Cancelled,
                final_blackboard: None,
                error_reason: None,
                artifacts: None,
            })
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("Invalid execution_id format (not a UUID)"));
    }

    #[tokio::test]
    async fn complete_execution_returns_not_found_for_unknown_execution() {
        let service = StandardCompleteWorkflowExecutionUseCase::new(
            Arc::new(InMemoryWorkflowExecutionRepository::new()),
            Arc::new(EventBus::new(8)),
        );

        let missing = ExecutionId::new().to_string();
        let err = service
            .complete_execution(CompleteWorkflowExecutionRequest {
                execution_id: missing.clone(),
                status: CompletionStatus::Cancelled,
                final_blackboard: None,
                error_reason: None,
                artifacts: None,
            })
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains(&format!("Workflow execution not found: {missing}")));
    }

    #[tokio::test]
    async fn complete_execution_respects_explicit_tenant_scope() {
        let repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let tenant_id = TenantId::from_string("tenant-alpha").unwrap();
        let execution_id = seed_execution(&repo, &tenant_id, "complete-tenant").await;
        let service =
            StandardCompleteWorkflowExecutionUseCase::new(repo.clone(), Arc::new(EventBus::new(8)));

        service
            .complete_execution_for_tenant(
                &tenant_id,
                CompleteWorkflowExecutionRequest {
                    execution_id: execution_id.to_string(),
                    status: CompletionStatus::Cancelled,
                    final_blackboard: Some(json!({"tenant": "alpha"})),
                    error_reason: None,
                    artifacts: None,
                },
            )
            .await
            .unwrap();

        let saved = repo
            .find_by_id_for_tenant(&tenant_id, execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved.status, ExecutionStatus::Cancelled);
        assert_eq!(saved.blackboard.get("tenant"), Some(&json!("alpha")));
        assert!(
            repo.find_by_id(execution_id).await.unwrap().is_none(),
            "default tenant lookup should not see non-local executions"
        );
    }
}
