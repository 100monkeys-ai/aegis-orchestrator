// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Integration tests for workflow engine
//!
//! These tests verify the end-to-end workflow execution pipeline:
//! 1. Parse workflow YAML
//! 2. Load into WorkflowEngine
//! 3. Execute workflow states
//! 4. Validate state transitions
//! 5. Verify blackboard context passing

use aegis_core::application::workflow_engine::WorkflowEngine;
use aegis_core::infrastructure::event_bus::EventBus;
use aegis_core::infrastructure::workflow_parser::WorkflowParser;
use std::sync::Arc;

#[tokio::test]
async fn test_parse_100monkeys_classic_workflow() {
    // Test that the default workflow parses successfully
    let yaml_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../demo-agents/workflows/100monkeys-classic.yaml"
    );

    let workflow = WorkflowParser::parse_file(yaml_path)
        .expect("Failed to parse 100monkeys-classic.yaml");

    // Verify metadata
    assert_eq!(workflow.metadata.name, "100monkeys-classic");
    assert_eq!(workflow.metadata.version.as_deref(), Some("1.0.0"));

    // Verify states
    assert_eq!(workflow.spec.states.len(), 6); // GENERATE, EXECUTE, VALIDATE, REFINE, COMPLETE, FAILED = 6
    assert!(workflow.spec.states.contains_key(&aegis_core::domain::workflow::StateName::new("GENERATE").unwrap()));
    assert!(workflow.spec.states.contains_key(&aegis_core::domain::workflow::StateName::new("EXECUTE").unwrap()));
    assert!(workflow.spec.states.contains_key(&aegis_core::domain::workflow::StateName::new("VALIDATE").unwrap()));
    assert!(workflow.spec.states.contains_key(&aegis_core::domain::workflow::StateName::new("REFINE").unwrap()));
    assert!(workflow.spec.states.contains_key(&aegis_core::domain::workflow::StateName::new("COMPLETE").unwrap()));
    assert!(workflow.spec.states.contains_key(&aegis_core::domain::workflow::StateName::new("FAILED").unwrap()));

    // Verify initial state
    assert_eq!(
        workflow.spec.initial_state.as_str(),
        "GENERATE"
    );

    // Verify context
    assert_eq!(
        workflow.spec.context.get("max_iterations").and_then(|v| v.as_u64()),
        Some(10)
    );
}

#[tokio::test]
async fn test_load_workflow_into_engine() {
    // Create workflow engine
    let event_bus = EventBus::with_default_capacity();
    let engine = WorkflowEngine::new(Arc::new(event_bus));

    // Load workflow
    let yaml_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../demo-agents/workflows/100monkeys-classic.yaml"
    );

    let workflow_id = engine
        .load_workflow_from_file(yaml_path)
        .await
        .expect("Failed to load workflow");

    // Verify workflow is registered
    let workflows = engine.list_workflows().await;
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0], "100monkeys-classic");

    // Retrieve workflow
    let workflow = engine
        .get_workflow("100monkeys-classic")
        .await
        .expect("Workflow not found");

    assert_eq!(workflow.id, workflow_id);
}

#[tokio::test]
async fn test_workflow_validation() {
    use aegis_core::domain::workflow::{WorkflowValidator, Workflow, WorkflowMetadata, WorkflowSpec, WorkflowState, StateKind, TransitionRule, TransitionCondition, StateName};
    use std::collections::HashMap;

    // Create a simple workflow
    let metadata = WorkflowMetadata {
        name: "test-workflow".to_string(),
        version: Some("1.0.0".to_string()),
        description: None,
        labels: HashMap::new(),
        annotations: HashMap::new(),
    };

    let mut states = HashMap::new();
    
    // START state
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

    // END state (terminal)
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

    let spec = WorkflowSpec {
        initial_state: StateName::new("START").unwrap(),
        context: HashMap::new(),
        states,
    };

    let workflow = Workflow::new(metadata, spec).expect("Failed to create workflow");

    // Validate for cycles
    WorkflowValidator::check_for_cycles(&workflow).expect("Cycle detection failed");
}

#[tokio::test]
async fn test_workflow_execution_initialization() {
    use aegis_core::domain::workflow::WorkflowExecution;
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;

    // Parse workflow
    let yaml_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../demo-agents/workflows/100monkeys-classic.yaml"
    );

    let workflow = WorkflowParser::parse_file(yaml_path)
        .expect("Failed to parse workflow");

    // Initialize execution
    let workflow_execution = WorkflowExecution::new(&workflow);

    // Verify initial state
    assert_eq!(workflow_execution.current_state.as_str(), "GENERATE");

    // Verify blackboard is empty
    assert!(workflow_execution.blackboard.data().is_empty());

    // Verify no state outputs yet
    assert!(workflow_execution.state_outputs.is_empty());
}

#[tokio::test]
async fn test_blackboard_operations() {
    use aegis_core::domain::workflow::Blackboard;

    let mut blackboard = Blackboard::new();

    // Set values
    blackboard.set("iteration", serde_json::json!(1));
    blackboard.set("previous_code", serde_json::json!("print('hello')"));
    blackboard.set("validation_errors", serde_json::json!("Missing docstring"));

    // Get values
    assert_eq!(blackboard.get("iteration"), Some(&serde_json::json!(1)));
    assert_eq!(
        blackboard.get("previous_code"),
        Some(&serde_json::json!("print('hello')"))
    );

    // Check contains
    assert!(blackboard.contains_key("iteration"));
    assert!(!blackboard.contains_key("nonexistent"));

    // Remove value
    let removed = blackboard.remove("iteration");
    assert_eq!(removed, Some(serde_json::json!(1)));
    assert!(!blackboard.contains_key("iteration"));

    // Clear
    blackboard.clear();
    assert!(blackboard.data().is_empty());
}

#[tokio::test]
async fn test_workflow_state_transitions() {
    use aegis_core::domain::workflow::{StateName, WorkflowExecution};
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;

    // Parse workflow
    let yaml_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../demo-agents/workflows/100monkeys-classic.yaml"
    );

    let workflow = WorkflowParser::parse_file(yaml_path)
        .expect("Failed to parse workflow");

    let mut workflow_execution = WorkflowExecution::new(&workflow);

    // Initial state
    assert_eq!(workflow_execution.current_state.as_str(), "GENERATE");

    // Transition to EXECUTE
    workflow_execution.transition_to(StateName::new("EXECUTE").unwrap());
    assert_eq!(workflow_execution.current_state.as_str(), "EXECUTE");

    // Transition to VALIDATE
    workflow_execution.transition_to(StateName::new("VALIDATE").unwrap());
    assert_eq!(workflow_execution.current_state.as_str(), "VALIDATE");

    // Record state output
    workflow_execution.record_state_output(
        StateName::new("VALIDATE").unwrap(),
        serde_json::json!({
            "valid": false,
            "score": 0.6,
            "reasoning": "Missing error handling"
        }),
    );

    // Verify output recorded
    let output = workflow_execution
        .get_state_output(&StateName::new("VALIDATE").unwrap())
        .expect("Output not found");

    assert_eq!(output["valid"], false);
    assert_eq!(output["score"], 0.6);
}

#[tokio::test]
async fn test_workflow_round_trip() {
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;

    // Parse original workflow
    let yaml_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../demo-agents/workflows/100monkeys-classic.yaml"
    );

    let workflow1 = WorkflowParser::parse_file(yaml_path)
        .expect("Failed to parse workflow");

    // Serialize back to YAML
    let yaml_string = WorkflowParser::to_yaml(&workflow1)
        .expect("Failed to serialize workflow");

    // Parse again
    let workflow2 = WorkflowParser::parse_yaml(&yaml_string)
        .expect("Failed to parse serialized workflow");

    // Verify metadata matches
    assert_eq!(workflow1.metadata.name, workflow2.metadata.name);
    assert_eq!(workflow1.metadata.version, workflow2.metadata.version);

    // Verify state count matches
    assert_eq!(
        workflow1.spec.states.len(),
        workflow2.spec.states.len()
    );

    // Verify initial state matches
    assert_eq!(
        workflow1.spec.initial_state.as_str(),
        workflow2.spec.initial_state.as_str()
    );
}
