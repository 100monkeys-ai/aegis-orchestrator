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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::WorkflowEvent;
    use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
    use crate::domain::workflow::{
        StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowId,
        WorkflowMetadata, WorkflowSpec, WorkflowState,
    };
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::{
        InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    struct StartCall {
        workflow_name: String,
        execution_id: ExecutionId,
        input: HashMap<String, serde_json::Value>,
    }

    struct RecordingWorkflowEngine {
        calls: Mutex<Vec<StartCall>>,
        run_id: String,
    }

    impl RecordingWorkflowEngine {
        fn new(run_id: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                run_id: run_id.to_string(),
            }
        }

        fn calls(&self) -> Vec<StartCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WorkflowEnginePort for RecordingWorkflowEngine {
        async fn register_workflow(
            &self,
            _definition: &crate::application::temporal_mapper::TemporalWorkflowDefinition,
        ) -> Result<()> {
            Ok(())
        }

        async fn start_workflow(
            &self,
            workflow_name: &str,
            execution_id: ExecutionId,
            input: HashMap<String, serde_json::Value>,
        ) -> Result<String> {
            self.calls.lock().unwrap().push(StartCall {
                workflow_name: workflow_name.to_string(),
                execution_id,
                input,
            });
            Ok(self.run_id.clone())
        }
    }

    fn build_test_workflow(name: &str) -> Workflow {
        let mut states = HashMap::new();
        states.insert(
            StateName::new("START").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo ready".to_string(),
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
                    command: "echo done".to_string(),
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
                labels: HashMap::new(),
                annotations: HashMap::new(),
            },
            WorkflowSpec {
                initial_state: StateName::new("START").unwrap(),
                context: HashMap::new(),
                states,
                volumes: vec![],
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn start_execution_wraps_scalar_input_seeds_blackboard_and_publishes_event() {
        let workflow = build_test_workflow("build-and-test");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-123"));
        let event_bus = Arc::new(EventBus::new(32));
        let mut receiver = event_bus.subscribe();

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo.clone(),
            execution_repo.clone(),
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            event_bus,
        );

        let result = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!("run-ci"),
                blackboard: Some(json!({
                    "judges": ["lint-judge", "security-judge"],
                    "validation_threshold": 0.85
                })),
            })
            .await
            .unwrap();

        assert_eq!(result.status, "running");
        assert_eq!(result.temporal_run_id, "temporal-run-123");

        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_name, "build-and-test");
        assert_eq!(calls[0].input.get("input"), Some(&json!("run-ci")));
        assert_eq!(calls[0].execution_id.to_string(), result.execution_id);

        let execution_id = ExecutionId::from_string(&result.execution_id).unwrap();
        let persisted = execution_repo
            .find_by_id(execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.blackboard.get("validation_threshold"),
            Some(&json!(0.85))
        );
        assert_eq!(
            persisted.blackboard.get("judges"),
            Some(&json!(["lint-judge", "security-judge"]))
        );

        let event = receiver.recv().await.unwrap();
        match event {
            DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionStarted {
                execution_id,
                workflow_id,
                ..
            }) => {
                assert_eq!(execution_id.to_string(), result.execution_id);
                assert_eq!(workflow_id, workflow.id);
            }
            other => panic!("unexpected event type: {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_execution_saves_before_engine_connection_check() {
        let workflow = build_test_workflow("save-first-workflow");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo.clone(),
            Arc::new(tokio::sync::RwLock::new(None)),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.metadata.name.clone(),
                input: json!({ "branch": "main" }),
                blackboard: None,
            })
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains("Workflow engine not connected yet"));

        let active = execution_repo.find_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].workflow_id, workflow.id);
    }

    #[tokio::test]
    async fn start_execution_resolves_workflow_by_uuid_identifier() {
        let workflow = build_test_workflow("uuid-resolve-workflow");
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        workflow_repo.save(&workflow).await.unwrap();
        let execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let engine = Arc::new(RecordingWorkflowEngine::new("temporal-run-xyz"));

        let service = StandardStartWorkflowExecutionUseCase::new(
            workflow_repo,
            execution_repo,
            Arc::new(tokio::sync::RwLock::new(Some(engine.clone()))),
            Arc::new(EventBus::new(8)),
        );

        let response = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: workflow.id.to_string(),
                input: json!({ "target": "release" }),
                blackboard: None,
            })
            .await
            .unwrap();

        let calls = engine.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workflow_name, "uuid-resolve-workflow");
        assert_eq!(calls[0].input.get("target"), Some(&json!("release")));
        assert_eq!(response.temporal_run_id, "temporal-run-xyz");
    }

    #[tokio::test]
    async fn start_execution_returns_not_found_for_unknown_workflow() {
        struct EmptyWorkflowRepo;

        #[async_trait]
        impl WorkflowRepository for EmptyWorkflowRepo {
            async fn save(&self, _workflow: &Workflow) -> Result<(), RepositoryError> {
                Ok(())
            }

            async fn find_by_id(
                &self,
                _id: WorkflowId,
            ) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }

            async fn find_by_name(&self, _name: &str) -> Result<Option<Workflow>, RepositoryError> {
                Ok(None)
            }

            async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> {
                Ok(vec![])
            }

            async fn delete(&self, _id: WorkflowId) -> Result<(), RepositoryError> {
                Ok(())
            }
        }

        let service = StandardStartWorkflowExecutionUseCase::new(
            Arc::new(EmptyWorkflowRepo),
            Arc::new(InMemoryWorkflowExecutionRepository::new()),
            Arc::new(tokio::sync::RwLock::new(None)),
            Arc::new(EventBus::new(8)),
        );

        let err = service
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: "does-not-exist".to_string(),
                input: json!({}),
                blackboard: None,
            })
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Workflow not found"));
    }
}
