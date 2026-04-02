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
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
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
            storage: Default::default(),
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
            storage: Default::default(),
        },
    )
    .unwrap();

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::local_default())
        .expect("Mapping failed");

    assert_eq!(def.version, DEFAULT_WORKFLOW_VERSION);
}

#[test]
fn test_spec_storage_is_mapped_to_temporal_definition() {
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
            name: "storage-test-workflow".to_string(),
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
            storage: WorkflowStorageSpec {
                workspace: Some(WorkflowWorkspaceSpec {
                    storage_class: WorkflowStorageClass::Ephemeral,
                    ttl_hours: 2,
                    size_limit_mb: 512,
                    volume_id: None,
                    blackboard_key: "workspace_volume_id".to_string(),
                }),
                shared_volumes: vec![],
            },
        },
    )
    .unwrap();

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::local_default())
        .expect("Mapping failed");

    let spec_storage = def.spec_storage.expect("spec_storage must be Some");
    let workspace = spec_storage.workspace.expect("workspace must be Some");
    assert_eq!(workspace.storage_class, WorkflowStorageClass::Ephemeral);
    assert_eq!(workspace.ttl_hours, 2);
}

#[test]
fn test_builtin_intent_to_execution_yaml_maps_spec_storage() {
    let yaml_content = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../cli/templates/workflows/builtin-intent-to-execution.yaml"
    ))
    .expect("builtin YAML must be readable");

    let workflow = WorkflowParser::parse_yaml(&yaml_content).expect("builtin YAML must parse");

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::local_default())
        .expect("Mapping failed");

    let spec_storage = def
        .spec_storage
        .as_ref()
        .expect("spec_storage must be Some for builtin workflow");
    let workspace = spec_storage
        .workspace
        .as_ref()
        .expect("workspace must be Some for builtin workflow");
    assert_eq!(workspace.storage_class, WorkflowStorageClass::Ephemeral);

    // Round-trip through JSON
    let json = serde_json::to_string(&def).expect("must serialize to JSON");
    let def2: aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition =
        serde_json::from_str(&json).expect("must deserialize from JSON");
    assert!(
        def2.spec_storage.is_some(),
        "spec_storage must survive JSON round-trip"
    );
}

#[test]
fn test_scope_and_owner_user_id_mapped_to_temporal_definition() {
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

    // ── User scope: both scope and owner_user_id must be forwarded ──
    let mut workflow = Workflow::new(
        WorkflowMetadata {
            name: "user-scoped-workflow".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
        },
        WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states: states.clone(),
            storage: Default::default(),
        },
    )
    .unwrap();
    workflow.scope = WorkflowScope::User {
        owner_user_id: "user-123".to_string(),
    };

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::local_default())
        .expect("Mapping failed");

    assert_eq!(def.scope, Some("user".to_string()));
    assert_eq!(def.owner_user_id, Some("user-123".to_string()));

    // ── Tenant scope: scope is "tenant", owner_user_id is None ──
    let tenant_workflow = Workflow::new(
        WorkflowMetadata {
            name: "tenant-scoped-workflow".to_string(),
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
    .unwrap();
    // scope defaults to Tenant

    let def = TemporalWorkflowMapper::to_temporal_definition(
        &tenant_workflow,
        &TenantId::local_default(),
    )
    .expect("Mapping failed");

    assert_eq!(def.scope, Some("tenant".to_string()));
    assert_eq!(def.owner_user_id, None);
}
