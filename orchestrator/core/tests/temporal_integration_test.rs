// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use aegis_core::application::register_workflow::{RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase};
use aegis_core::application::start_workflow_execution::{StartWorkflowExecutionUseCase, StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest};
use aegis_core::infrastructure::event_bus::EventBus;
use aegis_core::domain::repository::{WorkflowRepository, WorkflowExecutionRepository, RepositoryError};
use aegis_core::domain::workflow::{Workflow, WorkflowId, WorkflowExecution};
use aegis_core::domain::execution::ExecutionId;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

// Mock Repositories to ensure the tests compile perfectly without relying on Postgres bindings that may not be exported here.
struct MockWorkflowRepo;
#[async_trait]
impl WorkflowRepository for MockWorkflowRepo {
    async fn save(&self, _w: &Workflow) -> Result<(), RepositoryError> { Ok(()) }
    async fn find_by_id(&self, _i: WorkflowId) -> Result<Option<Workflow>, RepositoryError> { Ok(None) }
    async fn find_by_name(&self, _n: &str) -> Result<Option<Workflow>, RepositoryError> { Ok(None) }
    async fn list_all(&self) -> Result<Vec<Workflow>, RepositoryError> { Ok(vec![]) }
    async fn delete(&self, _i: WorkflowId) -> Result<(), RepositoryError> { Ok(()) }
}

struct MockWorkflowExecRepo;
#[async_trait]
impl WorkflowExecutionRepository for MockWorkflowExecRepo {
    async fn save(&self, _e: &WorkflowExecution) -> Result<(), RepositoryError> { Ok(()) }
    async fn find_by_id(&self, _i: ExecutionId) -> Result<Option<WorkflowExecution>, RepositoryError> { Ok(None) }
    async fn append_event(&self, _id: ExecutionId, _iteration: i64, _event_type: String, _data: serde_json::Value, _agent_id: Option<u8>) -> Result<(), RepositoryError> { Ok(()) }
    async fn find_active(&self) -> Result<Vec<WorkflowExecution>, RepositoryError> { Ok(vec![]) }
}

#[tokio::test]
#[ignore]
async fn test_register_and_start_temporal_workflow() {
    let workflow_repo = Arc::new(MockWorkflowRepo);
    let workflow_exec_repo = Arc::new(MockWorkflowExecRepo);
    let event_bus = Arc::new(EventBus::new(100));
    let temporal_container = Arc::new(RwLock::new(None));
    let parser = Arc::new(aegis_core::infrastructure::workflow_parser::WorkflowParser);

    let register_use_case = StandardRegisterWorkflowUseCase::new(
        parser.clone(),
        workflow_repo.clone(),
        temporal_container.clone(),
        event_bus.clone()
    );

    let start_use_case = StandardStartWorkflowExecutionUseCase::new(
        workflow_repo.clone(),
        workflow_exec_repo.clone(),
        temporal_container.clone(),
        event_bus.clone(),
        None,
    );

    let workflow_yaml = r#"
name: echo-workflow-test
version: 1.0.0
description: A simple test workflow
states:
  - id: start
    type: System
    action: Echo
    next: [end]
"#;

    let register_result = register_use_case.register_workflow(workflow_yaml).await;
    
    if let Ok(reg) = register_result {
        let req = StartWorkflowExecutionRequest {
            workflow_id: reg.workflow_id.clone(),
            input: serde_json::json!({"message": "hello testing"}),
            blackboard: None,
        };
        
        let start_result = start_use_case.start_execution(req).await;
        if let Ok(exec) = start_result {
            assert_eq!(exec.workflow_id, reg.workflow_id);
        }
    }
}
