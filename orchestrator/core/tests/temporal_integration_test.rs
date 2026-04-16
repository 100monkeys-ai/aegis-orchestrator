// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal Integration Test
//!
//! Provides temporal integration test functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements temporal integration test

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::ports::WorkflowEnginePort;
use aegis_orchestrator_core::application::register_workflow::{
    RegisterWorkflowUseCase, StandardRegisterWorkflowUseCase,
};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest};
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::repository::{
    AgentVersion, RepositoryError, WorkflowExecutionRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
    WorkflowId, WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::RwLock;

struct MockAgentServiceInt;

#[async_trait]
impl AgentLifecycleService for MockAgentServiceInt {
    async fn deploy_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        manifest: AgentManifest,
        force: bool,
        _scope: aegis_orchestrator_core::domain::agent::AgentScope,
        _caller_identity: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> anyhow::Result<AgentId> {
        self.deploy_agent(manifest, force).await
    }

    async fn deploy_agent(
        &self,
        _manifest: AgentManifest,
        _force: bool,
    ) -> anyhow::Result<AgentId> {
        panic!("test-only mock: deploy_agent is not exercised in this integration fixture")
    }

    async fn get_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
    ) -> anyhow::Result<Agent> {
        self.get_agent(id).await
    }

    async fn get_agent(&self, _id: AgentId) -> anyhow::Result<Agent> {
        panic!("test-only mock: get_agent is not exercised in this integration fixture")
    }

    async fn update_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
        manifest: AgentManifest,
    ) -> anyhow::Result<()> {
        self.update_agent(id, manifest).await
    }

    async fn update_agent(&self, _id: AgentId, _manifest: AgentManifest) -> anyhow::Result<()> {
        panic!("test-only mock: update_agent is not exercised in this integration fixture")
    }

    async fn delete_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        id: AgentId,
    ) -> anyhow::Result<()> {
        self.delete_agent(id).await
    }

    async fn delete_agent(&self, _id: AgentId) -> anyhow::Result<()> {
        panic!("test-only mock: delete_agent is not exercised in this integration fixture")
    }

    async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> anyhow::Result<Vec<Agent>> {
        self.list_agents().await
    }

    async fn list_agents(&self) -> anyhow::Result<Vec<Agent>> {
        panic!("test-only mock: list_agents is not exercised in this integration fixture")
    }

    async fn lookup_agent_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        self.lookup_agent(name).await
    }

    async fn lookup_agent(&self, _name: &str) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(AgentId(
            uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        )))
    }

    async fn lookup_agent_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(AgentId(
            uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        )))
    }

    async fn lookup_agent_for_tenant_with_version(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
        _version: &str,
    ) -> anyhow::Result<Option<AgentId>> {
        Ok(Some(AgentId(
            uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        )))
    }

    async fn list_agents_visible_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> anyhow::Result<Vec<Agent>> {
        Ok(vec![])
    }

    async fn list_versions_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _agent_id: AgentId,
    ) -> anyhow::Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

// Mock Repositories to ensure the tests compile perfectly without relying on Postgres bindings that may not be exported here.
struct MockWorkflowRepo;
#[async_trait]
impl WorkflowRepository for MockWorkflowRepo {
    async fn save_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow: &Workflow,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: WorkflowId,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok(None)
    }

    async fn find_by_name_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok(None)
    }

    async fn find_by_name_and_version_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
        _version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok(None)
    }

    async fn list_all_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![])
    }

    async fn delete_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: WorkflowId,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn resolve_by_name(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok(None)
    }
    async fn resolve_by_name_and_version(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
        _version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok(None)
    }
    async fn list_visible(&self, _tenant_id: &TenantId) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![])
    }
    async fn list_global(&self) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![])
    }
    async fn update_scope(
        &self,
        _id: WorkflowId,
        _new_scope: aegis_orchestrator_core::domain::workflow::WorkflowScope,
        _new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn list_by_name_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_by_name_visible(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok(None)
    }
}

struct StaticWorkflowRepo {
    workflow: Workflow,
}

#[async_trait]
impl WorkflowRepository for StaticWorkflowRepo {
    async fn save_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow: &Workflow,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        _tenant_id: &TenantId,
        workflow_id: WorkflowId,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok((self.workflow.id == workflow_id).then(|| self.workflow.clone()))
    }

    async fn find_by_name_for_tenant(
        &self,
        _tenant_id: &TenantId,
        workflow_name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok((self.workflow.metadata.name == workflow_name).then(|| self.workflow.clone()))
    }

    async fn find_by_name_and_version_for_tenant(
        &self,
        _tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok((self.workflow.metadata.name == name
            && self.workflow.metadata.version.as_deref() == Some(version))
        .then(|| self.workflow.clone()))
    }

    async fn list_all_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![self.workflow.clone()])
    }

    async fn delete_for_tenant(
        &self,
        _tenant_id: &TenantId,
        workflow_id: WorkflowId,
    ) -> Result<(), RepositoryError> {
        if self.workflow.id == workflow_id {
            Ok(())
        } else {
            Err(RepositoryError::NotFound(workflow_id.to_string()))
        }
    }

    async fn resolve_by_name(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        self.find_by_name_for_tenant(tenant_id, name).await
    }
    async fn resolve_by_name_and_version(
        &self,
        _tenant_id: &TenantId,
        name: &str,
        version: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        Ok((self.workflow.metadata.name == name
            && self.workflow.metadata.version.as_deref() == Some(version))
        .then(|| self.workflow.clone()))
    }
    async fn list_visible(&self, tenant_id: &TenantId) -> Result<Vec<Workflow>, RepositoryError> {
        self.list_all_for_tenant(tenant_id).await
    }
    async fn list_global(&self) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![])
    }
    async fn update_scope(
        &self,
        _id: WorkflowId,
        _new_scope: aegis_orchestrator_core::domain::workflow::WorkflowScope,
        _new_tenant_id: &TenantId,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn list_by_name_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _name: &str,
    ) -> Result<Vec<Workflow>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_by_name_visible(
        &self,
        tenant_id: &TenantId,
        name: &str,
    ) -> Result<Option<Workflow>, RepositoryError> {
        self.find_by_name_for_tenant(tenant_id, name).await
    }
}

struct MockWorkflowExecRepo;
#[async_trait]
impl WorkflowExecutionRepository for MockWorkflowExecRepo {
    async fn find_tenant_id_by_execution(
        &self,
        _id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        Ok(None)
    }

    async fn save_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution: &WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn find_by_id_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
    ) -> Result<Option<WorkflowExecution>, RepositoryError> {
        Ok(None)
    }

    async fn append_event(
        &self,
        _execution_id: ExecutionId,
        _temporal_sequence_number: i64,
        _event_type: String,
        _payload: serde_json::Value,
        _iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn update_temporal_linkage_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
        _temporal_workflow_id: &str,
        _temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn find_active_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: WorkflowId,
        _limit: usize,
        _offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_events_by_execution(
        &self,
        _id: ExecutionId,
        _limit: usize,
        _offset: usize,
    ) -> Result<
        Vec<aegis_orchestrator_core::domain::workflow::WorkflowExecutionEventRecord>,
        RepositoryError,
    > {
        Ok(vec![])
    }

    async fn count_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: WorkflowId,
    ) -> Result<i64, RepositoryError> {
        Ok(0)
    }

    async fn list_paginated_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _limit: usize,
        _offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        Ok(vec![])
    }
}

struct FailingWorkflowExecRepo;

#[async_trait]
impl WorkflowExecutionRepository for FailingWorkflowExecRepo {
    async fn find_tenant_id_by_execution(
        &self,
        _id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        Ok(None)
    }

    async fn save_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution: &WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        Err(RepositoryError::Database(
            "simulated workflow execution persistence failure".to_string(),
        ))
    }

    async fn find_by_id_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
    ) -> Result<Option<WorkflowExecution>, RepositoryError> {
        Ok(None)
    }

    async fn append_event(
        &self,
        _execution_id: ExecutionId,
        _temporal_sequence_number: i64,
        _event_type: String,
        _payload: serde_json::Value,
        _iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn update_temporal_linkage_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _execution_id: ExecutionId,
        _temporal_workflow_id: &str,
        _temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        Err(RepositoryError::Database(
            "simulated workflow execution persistence failure".to_string(),
        ))
    }

    async fn find_active_for_tenant(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: WorkflowId,
        _limit: usize,
        _offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        Ok(vec![])
    }

    async fn find_events_by_execution(
        &self,
        _id: ExecutionId,
        _limit: usize,
        _offset: usize,
    ) -> Result<
        Vec<aegis_orchestrator_core::domain::workflow::WorkflowExecutionEventRecord>,
        RepositoryError,
    > {
        Ok(vec![])
    }

    async fn count_by_workflow_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _workflow_id: WorkflowId,
    ) -> Result<i64, RepositoryError> {
        Ok(0)
    }

    async fn list_paginated_for_tenant(
        &self,
        _tenant_id: &TenantId,
        _limit: usize,
        _offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        Ok(vec![])
    }
}

#[derive(Debug, Clone)]
struct StartCall {
    workflow_id: String,
    execution_id: ExecutionId,
    input: HashMap<String, serde_json::Value>,
    blackboard: Option<HashMap<String, serde_json::Value>>,
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
impl aegis_orchestrator_core::application::ports::WorkflowEnginePort for RecordingWorkflowEngine {
    async fn register_workflow(
        &self,
        _definition: &aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(
        &self,
        params: aegis_orchestrator_core::application::ports::StartWorkflowParams<'_>,
    ) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push(StartCall {
            workflow_id: params.workflow_id.to_string(),
            execution_id: params.execution_id,
            input: params.input,
            blackboard: params.blackboard,
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
            max_state_visits: None,
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
            max_state_visits: None,
        },
    );

    Workflow::new(
        WorkflowMetadata {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
            output_schema: None,
            output_template: None,
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
        },
    )
    .unwrap()
}

// NOTE: This is a full Temporal integration test that may require external services
// (e.g., a running Temporal server) and can be slow or environment-dependent.
// It is therefore marked `#[ignore]` and must be run explicitly, for example with:
// `cargo test --test temporal_integration_test -- --ignored`.
#[tokio::test]
#[ignore]
async fn test_register_and_start_temporal_workflow() {
    let workflow_repo = Arc::new(MockWorkflowRepo);
    let workflow_exec_repo = Arc::new(MockWorkflowExecRepo);
    let event_bus = Arc::new(EventBus::new(100));
    let temporal_container = Arc::new(RwLock::new(None));

    let register_use_case = StandardRegisterWorkflowUseCase::new(
        workflow_repo.clone(),
        temporal_container.clone(),
        event_bus.clone(),
        Arc::new(MockAgentServiceInt),
    );

    let start_use_case = StandardStartWorkflowExecutionUseCase::new(
        workflow_repo.clone(),
        workflow_exec_repo.clone(),
        temporal_container.clone(),
        event_bus.clone(),
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

    let register_result = register_use_case
        .register_workflow(workflow_yaml, false)
        .await;

    let reg = register_result.expect("Failed to register workflow in temporal integration test");
    let req = StartWorkflowExecutionRequest {
        workflow_id: reg.workflow_id.clone(),
        input: serde_json::json!({"message": "hello testing"}),
        blackboard: None,
        version: None,
        tenant_id: Some(TenantId::consumer()),
        security_context_name: None,
        intent: None,
    };

    let start_result = start_use_case.start_execution(req).await;
    let exec =
        start_result.expect("Failed to start workflow execution in temporal integration test");
    assert_eq!(exec.workflow_id, reg.workflow_id);
}

#[tokio::test]
async fn start_execution_surfaces_repository_save_failure_before_temporal_start() {
    let workflow = build_test_workflow("save-failure-workflow");
    let workflow_repo = Arc::new(StaticWorkflowRepo {
        workflow: workflow.clone(),
    });
    let failing_exec_repo = Arc::new(FailingWorkflowExecRepo);
    let engine = Arc::new(RecordingWorkflowEngine::new(
        "temporal-run-should-not-start",
    ));
    let event_bus = Arc::new(EventBus::new(8));

    let service = StandardStartWorkflowExecutionUseCase::new(
        workflow_repo,
        failing_exec_repo,
        Arc::new(RwLock::new(Some(engine.clone()))),
        event_bus,
    );

    let err = service
        .start_execution(StartWorkflowExecutionRequest {
            workflow_id: workflow.metadata.name.clone(),
            input: json!({"topic": "copy"}),
            blackboard: None,
            version: None,
            tenant_id: Some(TenantId::consumer()),
            security_context_name: None,
            intent: None,
        })
        .await
        .unwrap_err();

    let err_text = err.to_string();
    assert!(err_text.contains("Failed to persist workflow execution to repository"));
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn recording_workflow_engine_captures_start_call_arguments() {
    let engine = RecordingWorkflowEngine::new("recorded-run-id");
    let execution_id = ExecutionId::new();
    let input = HashMap::from([
        (
            "topic".to_string(),
            serde_json::Value::String("copy".to_string()),
        ),
        (
            "priority".to_string(),
            serde_json::Value::Number(serde_json::Number::from(3)),
        ),
    ]);

    let run_id = engine
        .start_workflow(
            aegis_orchestrator_core::application::ports::StartWorkflowParams {
                workflow_id: "workflow-alpha",
                execution_id: execution_id.clone(),
                tenant_id: "aegis-system",
                input: input.clone(),
                blackboard: None,
                security_context_name: None,
                intent: None,
            },
        )
        .await
        .expect("recording engine should return its configured run id");

    let calls = engine.calls();
    assert_eq!(run_id, "recorded-run-id");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workflow_id, "workflow-alpha");
    assert_eq!(calls[0].execution_id, execution_id);
    assert_eq!(calls[0].input, input);
    assert_eq!(calls[0].blackboard, None);
}
