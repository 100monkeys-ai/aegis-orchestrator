//! Integration tests for recursive agent execution (Agent-as-Judge pattern)
//!
//! Tests the ability of agents to call other agents recursively,
//! creating hierarchical execution trees with depth tracking.

use aegis_core::application::workflow_engine::WorkflowEngine;
use aegis_core::domain::agent::AgentId;
use aegis_core::domain::execution::{Execution, ExecutionId, ExecutionInput, ExecutionStatus, MAX_RECURSIVE_DEPTH, Iteration, LlmInteraction};
use aegis_core::infrastructure::event_bus::{EventBus, DomainEvent};
use aegis_core::domain::events::ExecutionEvent;
use aegis_core::application::execution::ExecutionService;
use aegis_core::application::validation_service::ValidationService;
use std::sync::Arc;
use async_trait::async_trait;

struct MockExecutionService;

#[async_trait]
impl ExecutionService for MockExecutionService {
    async fn start_execution(&self, _agent_id: AgentId, _input: ExecutionInput) -> anyhow::Result<ExecutionId> {
        Ok(ExecutionId::new())
    }
    async fn get_execution(&self, _id: ExecutionId) -> anyhow::Result<Execution> {
        Ok(Execution::new(AgentId::new(), ExecutionInput { intent: None, payload: serde_json::Value::Null }, 3))
    }
    async fn get_iterations(&self, _exec_id: ExecutionId) -> anyhow::Result<Vec<Iteration>> { Ok(vec![]) }
    async fn cancel_execution(&self, _id: ExecutionId) -> anyhow::Result<()> { Ok(()) }
    async fn stream_execution(&self, id: ExecutionId) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>> {
             let event = ExecutionEvent::ExecutionCompleted {
                 execution_id: id,
                 agent_id: AgentId::new(),
                 final_output: "mock output".to_string(),
                 total_iterations: 1,
                 completed_at: chrono::Utc::now(),
             };
             Ok(Box::pin(futures::stream::iter(vec![Ok(event)])))
    }
    async fn stream_agent_events(&self, _id: AgentId) -> anyhow::Result<std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<DomainEvent>> + Send>>> {
            Ok(Box::pin(futures::stream::empty()))
    }
    async fn list_executions(&self, _agent_id: Option<AgentId>, _limit: usize) -> anyhow::Result<Vec<Execution>> { Ok(vec![]) }
    async fn delete_execution(&self, _id: ExecutionId) -> anyhow::Result<()> { Ok(()) }
    async fn record_llm_interaction(&self, _execution_id: ExecutionId, _iteration: u8, _interaction: LlmInteraction) -> anyhow::Result<()> { Ok(()) }
}

/// Helper to create a basic workflow engine
fn create_test_engine() -> WorkflowEngine {
    let event_bus = Arc::new(EventBus::new(100));
    let exec_service = Arc::new(MockExecutionService);
    let val_service = Arc::new(ValidationService::new(event_bus.clone(), exec_service.clone()));
    
    // Create in-memory repository for testing
    let repository = Arc::new(aegis_core::infrastructure::repositories::InMemoryWorkflowRepository::new());
    
    WorkflowEngine::new(repository, event_bus, val_service, exec_service, Arc::new(tokio::sync::RwLock::new(None)))
}

/// Helper to create test ExecutionInput
fn test_input(task: &str) -> ExecutionInput {
    ExecutionInput {
        intent: Some(task.to_string()),
        payload: serde_json::json!({}),
    }
}

/// Test 1: Root execution creation (depth 0)
/// Verify root execution has correct hierarchy
#[test]
fn test_root_execution_depth_0() {
    let agent_id = AgentId::new();
    let input = test_input("Test task");
    
    let execution = Execution::new(agent_id, input, 10);

    // Verify depth is 0 (root)
    assert_eq!(execution.depth(), 0, "Root execution should have depth 0");
    assert_eq!(execution.parent_id(), None, "Root execution should have no parent");
    
    // Verify can spawn child
    assert!(
        execution.can_spawn_child(),
        "Root execution should be able to spawn child"
    );
    
    // Verify status
    assert_eq!(execution.status, ExecutionStatus::Pending);
}

/// Test 2: Single child execution (depth 1)
/// Verify child execution has correct parent reference
#[test]
fn test_child_execution_depth_1() {
    let agent_id = AgentId::new();
    let input = test_input("Root task");
    
    let root = Execution::new(agent_id, input, 10);

    let child_agent_id = AgentId::new();
    let child_input = test_input("Child task");
    
    // Create child execution
    let child = Execution::new_child(child_agent_id, child_input, 10, &root)
        .expect("Failed to create child execution");

    // Verify child depth is 1
    assert_eq!(child.depth(), 1, "Child execution should have depth 1");

    // Verify parent reference
    assert_eq!(
        child.parent_id(),
        Some(root.id),
        "Child should reference parent"
    );

    // Verify child can spawn grandchild
    assert!(
        child.can_spawn_child(),
        "Depth 1 execution should be able to spawn child"
    );
}

/// Test 3: Depth limit enforcement (depth 3)
/// Verify MaxDepthExceeded error at depth 4
#[test]
fn test_depth_limit_enforcement() {
    let agent_id = AgentId::new();
    let input = test_input("Root task");
    
    let root = Execution::new(agent_id, input, 10);

    // Create depth 1 (child)
    let depth_1 = Execution::new_child(
        AgentId::new(),
        test_input("Depth 1 task"),
        10,
        &root,
    ).expect("Failed to create depth 1");
    assert_eq!(depth_1.depth(), 1);
    assert!(depth_1.can_spawn_child());

    // Create depth 2 (grandchild)
    let depth_2 = Execution::new_child(
        AgentId::new(),
        test_input("Depth 2 task"),
        10,
        &depth_1,
    ).expect("Failed to create depth 2");
    assert_eq!(depth_2.depth(), 2);
    assert!(depth_2.can_spawn_child());

    // Create depth 3 (great-grandchild) - this is MAX_RECURSIVE_DEPTH
    let depth_3 = Execution::new_child(
        AgentId::new(),
        test_input("Depth 3 task"),
        10,
        &depth_2,
    ).expect("Failed to create depth 3");
    assert_eq!(depth_3.depth(), 3);
    assert_eq!(depth_3.depth(), MAX_RECURSIVE_DEPTH);
    assert!(
        !depth_3.can_spawn_child(),
        "Depth 3 should NOT be able to spawn child"
    );

    // Attempt to create depth 4 - should fail
    let result = Execution::new_child(
        AgentId::new(),
        test_input("Depth 4 task"),
        10,
        &depth_3,
    );
    assert!(
        result.is_err(),
        "Creating depth 4 execution should fail (exceeds MAX_RECURSIVE_DEPTH)"
    );

    // Verify error is MaxDepthExceeded
    if let Err(e) = result {
        assert!(
            format!("{:?}", e).contains("MaxDepthExceeded"),
            "Error should be MaxDepthExceeded, got: {:?}",
            e
        );
    }
}

/// Test 4: Execution tree validation
/// Verify parent_id, depth, path fields correct throughout hierarchy
#[test]
fn test_execution_tree_validation() {
    let agent_id = AgentId::new();
    let input = test_input("Root task");
    
    let root = Execution::new(agent_id, input, 10);
    let root_id = root.id;

    // Build execution tree: root → child → grandchild
    let child = Execution::new_child(
        AgentId::new(),
        test_input("Child task"),
        10,
        &root,
    ).expect("Failed to create child");
    let child_id = child.id;

    let grandchild = Execution::new_child(
        AgentId::new(),
        test_input("Grandchild task"),
        10,
        &child,
    ).expect("Failed to create grandchild");

    // Validate root
    assert_eq!(root.depth(), 0);
    assert_eq!(root.parent_id(), None);
    assert_eq!(root.hierarchy.root_id(), root_id);

    // Validate child
    assert_eq!(child.depth(), 1);
    assert_eq!(child.parent_id(), Some(root_id));
    assert_eq!(child.hierarchy.root_id(), root_id);

    // Validate grandchild
    assert_eq!(grandchild.depth(), 2);
    assert_eq!(grandchild.parent_id(), Some(child_id));
    assert_eq!(grandchild.hierarchy.root_id(), root_id);

    // All should share same root_id
    assert_eq!(
        root.hierarchy.root_id(),
        child.hierarchy.root_id(),
        "Child should have same root_id as parent"
    );
    assert_eq!(
        root.hierarchy.root_id(),
        grandchild.hierarchy.root_id(),
        "Grandchild should have same root_id as root"
    );
}

/// Test 5: WorkflowEngine helper methods
/// Verify get_execution_context, can_spawn_child, get_execution_depth work correctly
#[tokio::test]
async fn test_workflow_engine_helper_methods() {
    let engine = create_test_engine();

    // Register a simple workflow
    let workflow_yaml = r#"
apiVersion: "100monkeys.ai/v1"
kind: Workflow
metadata:
  name: "helper-test"
  version: "1.0.0"
spec:
  initial_state: START
  states:
    START:
      kind: Agent
      agent: "test-judge"
      input: "Test"
      transitions:
        - target: END
          condition: always
    END:
      kind: System
      command: complete
      transitions: []
"#;

    let _workflow_id = engine
        .load_workflow_from_yaml(workflow_yaml)
        .await
        .expect("Failed to load workflow");

    // Create an execution ID and manually add it to execution_contexts
    let execution_id = ExecutionId::new();
    let agent_id = AgentId::new();
    let execution = Execution::new(
        agent_id,
        test_input("Test task"),
        10,
    );

    // Store in execution_contexts (this is normally done by start_execution)
    {
        let mut contexts = engine.execution_contexts.write().await;
        contexts.insert(execution_id, execution);
    }

    // Test get_execution_context
    let context = engine.get_execution_context(&execution_id).await;
    assert!(context.is_some(), "Execution context should exist");

    // Test get_execution_depth
    let depth = engine.get_execution_depth(&execution_id).await;
    assert_eq!(depth, Some(0), "Root execution should have depth 0");

    // Test can_spawn_child
    let can_spawn = engine.can_spawn_child(&execution_id).await;
    assert!(can_spawn, "Root execution should be able to spawn child");

    // Test with non-existent execution
    let fake_id = ExecutionId::new();
    let fake_context = engine.get_execution_context(&fake_id).await;
    assert!(fake_context.is_none(), "Non-existent execution should return None");

    let fake_depth = engine.get_execution_depth(&fake_id).await;
    assert!(fake_depth.is_none(), "Non-existent execution should return None depth");

    let fake_can_spawn = engine.can_spawn_child(&fake_id).await;
    assert!(!fake_can_spawn, "Non-existent execution cannot spawn child");
}
