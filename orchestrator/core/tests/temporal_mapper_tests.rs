// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal Mapper Tests
//!
//! Provides temporal mapper tests functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Core System
//! - **Purpose:** Implements temporal mapper tests

use aegis_orchestrator_core::application::temporal_mapper::{
    TemporalWorkflowMapper, DEFAULT_WORKFLOW_VERSION,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::*;
use std::collections::HashMap;

#[test]
fn test_map_100monkeys_workflow() {
    // Create a mock of the 100monkeys workflow
    let mut states = HashMap::new();

    // GENERATE State
    states.insert(
        StateName::new("GENERATE").unwrap(),
        WorkflowState {
            kind: StateKind::Agent {
                agent: "coder".to_string(),
                input: "Task: {{task}}".to_string(),
                isolation: Some(IsolationMode::Docker),
                judges: vec![],
                max_iterations: None,
                pre_execution_validator: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::OnSuccess,
                target: StateName::new("EXECUTE").unwrap(),
                feedback: None,
            }],
            timeout: Some(std::time::Duration::from_secs(60)),
        },
    );

    // EXECUTE State
    states.insert(
        StateName::new("EXECUTE").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "python script.py".to_string(),
                env: HashMap::from([("PYTHONPATH".to_string(), ".".to_string())]),
                workdir: Some("/workspace".to_string()),
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::ExitCodeZero,
                target: StateName::new("VALIDATE").unwrap(),
                feedback: None,
            }],
            timeout: None,
        },
    );

    // VALIDATE State (Terminal for test)
    states.insert(
        StateName::new("VALIDATE").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo validated".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );

    let workflow = Workflow::new(
        WorkflowMetadata {
            name: "100monkeys-test".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
        },
        WorkflowSpec {
            initial_state: StateName::new("GENERATE").unwrap(),
            context: HashMap::from([("task".to_string(), serde_json::json!("Write fibonacci"))]),
            states,
            volumes: vec![],
            workspace: None,
        },
    )
    .unwrap();

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::local_default())
        .expect("Mapping failed");

    // Assertions
    assert_eq!(def.name, "100monkeys-test");
    assert_eq!(def.tenant_id, TenantId::local_default().to_string());
    assert_eq!(def.initial_state, "GENERATE");

    // Verify GENERATE state
    let generate_state = def.states.get("GENERATE").expect("GENERATE state missing");
    assert_eq!(generate_state.kind, "Agent");
    assert_eq!(generate_state.agent, Some("coder".to_string()));
    assert_eq!(generate_state.isolation, Some("docker".to_string()));

    // Verify transitions
    assert_eq!(generate_state.transitions.len(), 1);
    assert_eq!(generate_state.transitions[0].condition, "on_success");
    assert_eq!(
        generate_state.transitions[0].target,
        Some("EXECUTE".to_string())
    );

    // Verify EXECUTE state
    let execute_state = def.states.get("EXECUTE").expect("EXECUTE state missing");
    assert_eq!(execute_state.kind, "System");
    assert_eq!(execute_state.command, Some("python script.py".to_string()));
}

#[test]
fn test_map_workflow_defaults_missing_version_to_one_zero_zero() {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("START").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo start".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );

    let workflow = Workflow::new(
        WorkflowMetadata {
            name: "default-version-workflow".to_string(),
            version: None,
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
            workspace: None,
        },
    )
    .unwrap();

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::local_default())
        .expect("Mapping failed");

    assert_eq!(def.version, DEFAULT_WORKFLOW_VERSION);
}
