use aegis_core::application::temporal_mapper::TemporalWorkflowMapper;
use aegis_core::domain::workflow::*;
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
            labels: HashMap::new(),
            annotations: HashMap::new(),
        },
        WorkflowSpec {
            initial_state: StateName::new("GENERATE").unwrap(),
            context: HashMap::from([("task".to_string(), serde_json::json!("Write fibonacci"))]),
            states,
        },
    )
    .unwrap();

    let def = TemporalWorkflowMapper::to_temporal_definition(&workflow).expect("Mapping failed");

    // Assertions
    assert_eq!(def.name, "100monkeys-test");
    assert_eq!(def.initial_state, "GENERATE");
    
    // Verify GENERATE state
    let generate_state = def.states.get("GENERATE").expect("GENERATE state missing");
    assert_eq!(generate_state.kind, "Agent");
    assert_eq!(generate_state.agent, Some("coder".to_string()));
    assert_eq!(generate_state.isolation, Some("docker".to_string()));
    
    // Verify transitions
    assert_eq!(generate_state.transitions.len(), 1);
    assert_eq!(generate_state.transitions[0].condition, "on_success");
    assert_eq!(generate_state.transitions[0].target, Some("EXECUTE".to_string()));

    // Verify EXECUTE state
    let execute_state = def.states.get("EXECUTE").expect("EXECUTE state missing");
    assert_eq!(execute_state.kind, "System");
    assert_eq!(execute_state.command, Some("python script.py".to_string()));
}
