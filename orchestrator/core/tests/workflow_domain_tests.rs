// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Domain-layer unit tests for the **Workflow Orchestration** bounded context (BC-3).
//!
//! Tests cover the Workflow aggregate root, its value objects (StateName, StateKind,
//! TransitionRule, TransitionCondition, ConsensusConfig, ConfidenceWeighting, Blackboard),
//! the WorkflowExecution entity, and all constructor-enforced invariants documented in
//! ADR-015, ADR-050, and ADR-065.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Validates invariants, value object semantics, and serde round-trips

use std::collections::HashMap;

use aegis_orchestrator_core::domain::workflow::{
    Blackboard, ConfidenceWeighting, ConsensusConfig, ConsensusStrategy, ContainerRunConfig,
    JudgeConfig, ParallelAgentConfig, ParallelCompletionStrategy, StateKind, StateName,
    SubworkflowMode, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
    WorkflowMetadata, WorkflowSpec, WorkflowState, WorkflowVolumeSpec,
};

use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::runtime::ContainerVolumeMount;
use aegis_orchestrator_core::domain::shared_kernel::WorkflowId;

// ============================================================================
// Helpers
// ============================================================================

fn minimal_metadata(name: &str) -> WorkflowMetadata {
    WorkflowMetadata {
        name: name.to_string(),
        version: None,
        description: None,
        tags: vec![],
        labels: HashMap::new(),
        annotations: HashMap::new(),
    }
}

/// Build a single-state workflow spec using a System state (simplest kind).
fn single_system_state_spec(state_name: &str) -> WorkflowSpec {
    let sn = StateName::new(state_name).unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo hello".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    }
}

/// Build a two-state spec with a transition from `from` -> `to`.
fn two_state_spec(from: &str, to: &str) -> WorkflowSpec {
    let from_sn = StateName::new(from).unwrap();
    let to_sn = StateName::new(to).unwrap();
    let mut states = HashMap::new();
    states.insert(
        from_sn.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo start".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: to_sn.clone(),
                feedback: None,
            }],
            timeout: None,
        },
    );
    states.insert(
        to_sn,
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
    WorkflowSpec {
        initial_state: from_sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    }
}

fn container_run_state(
    name: &str,
    image: &str,
    command: Vec<&str>,
    volumes: Vec<ContainerVolumeMount>,
) -> StateKind {
    StateKind::ContainerRun {
        name: name.to_string(),
        image: image.to_string(),
        image_pull_policy: None,
        command: command.into_iter().map(String::from).collect(),
        env: HashMap::new(),
        workdir: None,
        volumes,
        resources: None,
        registry_credentials: None,
        retry: None,
        shell: false,
    }
}

// ============================================================================
// 1. Workflow::new() — valid creation
// ============================================================================

#[test]
fn workflow_new_valid_single_state() {
    let wf = Workflow::new(
        minimal_metadata("simple-workflow"),
        single_system_state_spec("START"),
    );
    assert!(wf.is_ok());
    let wf = wf.unwrap();
    assert_eq!(wf.spec.initial_state.as_str(), "START");
}

#[test]
fn workflow_new_valid_two_states_with_transition() {
    let wf = Workflow::new(
        minimal_metadata("two-step"),
        two_state_spec("GENERATE", "VALIDATE"),
    );
    assert!(wf.is_ok());
    let wf = wf.unwrap();
    assert!(!wf.is_terminal_state(&StateName::new("GENERATE").unwrap()));
    assert!(wf.is_terminal_state(&StateName::new("VALIDATE").unwrap()));
}

#[test]
fn workflow_new_assigns_unique_id_and_timestamp() {
    let wf1 = Workflow::new(minimal_metadata("a"), single_system_state_spec("S")).unwrap();
    let wf2 = Workflow::new(minimal_metadata("b"), single_system_state_spec("S")).unwrap();
    assert_ne!(wf1.id, wf2.id);
}

// ============================================================================
// 2. Workflow invariants
// ============================================================================

#[test]
fn workflow_rejects_empty_states() {
    let spec = WorkflowSpec {
        initial_state: StateName::new("S").unwrap(),
        context: HashMap::new(),
        states: HashMap::new(),
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad"), spec).unwrap_err();
    assert!(err.to_string().contains("at least one state"));
}

#[test]
fn workflow_rejects_missing_initial_state() {
    let sn = StateName::new("OTHER").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "true".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: StateName::new("MISSING").unwrap(),
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad"), spec).unwrap_err();
    assert!(err.to_string().contains("not found"));
}

#[test]
fn workflow_rejects_transition_to_nonexistent_state() {
    let start = StateName::new("START").unwrap();
    let mut states = HashMap::new();
    states.insert(
        start.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "true".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("NOWHERE").unwrap(),
                feedback: None,
            }],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: start,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad"), spec).unwrap_err();
    assert!(err.to_string().contains("NOWHERE"));
}

#[test]
fn workflow_allows_duplicate_state_name_inserts_overwrite_in_hashmap() {
    // HashMap semantics: inserting the same key twice keeps the last value.
    // This is not a domain error — it is just HashMap behavior.
    let sn = StateName::new("ONLY").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "first".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "second".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let wf = Workflow::new(
        minimal_metadata("dedup"),
        WorkflowSpec {
            initial_state: sn,
            context: HashMap::new(),
            states,
            volumes: vec![],
        },
    );
    assert!(wf.is_ok());
    assert_eq!(wf.unwrap().spec.states.len(), 1);
}

// ============================================================================
// 3. WorkflowMetadata — name validation
// ============================================================================

#[test]
fn metadata_name_valid_dns_labels() {
    assert!(WorkflowMetadata::validate_name("my-workflow").is_ok());
    assert!(WorkflowMetadata::validate_name("100monkeys-classic").is_ok());
    assert!(WorkflowMetadata::validate_name("a").is_ok());
    assert!(WorkflowMetadata::validate_name("abc123").is_ok());
}

#[test]
fn metadata_name_rejects_uppercase() {
    assert!(WorkflowMetadata::validate_name("My-Workflow").is_err());
}

#[test]
fn metadata_name_rejects_underscores() {
    assert!(WorkflowMetadata::validate_name("my_workflow").is_err());
}

#[test]
fn metadata_name_rejects_leading_hyphen() {
    assert!(WorkflowMetadata::validate_name("-bad").is_err());
}

#[test]
fn metadata_name_rejects_trailing_hyphen() {
    assert!(WorkflowMetadata::validate_name("bad-").is_err());
}

#[test]
fn metadata_name_rejects_empty() {
    assert!(WorkflowMetadata::validate_name("").is_err());
}

#[test]
fn metadata_name_rejects_over_63_chars() {
    let long = "a".repeat(64);
    assert!(WorkflowMetadata::validate_name(&long).is_err());
}

#[test]
fn metadata_name_accepts_exactly_63_chars() {
    let exact = "a".repeat(63);
    assert!(WorkflowMetadata::validate_name(&exact).is_ok());
}

// ============================================================================
// 4. StateName — newtype construction and display
// ============================================================================

#[test]
fn state_name_valid() {
    let sn = StateName::new("GENERATE").unwrap();
    assert_eq!(sn.as_str(), "GENERATE");
    assert_eq!(format!("{sn}"), "GENERATE");
}

#[test]
fn state_name_rejects_empty() {
    assert!(StateName::new("").is_err());
}

#[test]
fn state_name_preserves_arbitrary_characters() {
    // StateName only checks non-empty — it allows any non-empty string.
    let sn = StateName::new("my state with spaces!").unwrap();
    assert_eq!(sn.as_str(), "my state with spaces!");
}

// ============================================================================
// 5. StateKind variants — construction smoke tests
// ============================================================================

#[test]
fn state_kind_agent_variant() {
    let kind = StateKind::Agent {
        agent: "code-gen".to_string(),
        input: "{{workflow.task}}".to_string(),
        isolation: None,
        judges: vec![],
        max_iterations: Some(5),
        pre_execution_validator: None,
    };
    assert!(matches!(kind, StateKind::Agent { .. }));
}

#[test]
fn state_kind_system_variant() {
    let kind = StateKind::System {
        command: "echo ok".to_string(),
        env: HashMap::new(),
        workdir: None,
    };
    assert!(matches!(kind, StateKind::System { .. }));
}

#[test]
fn state_kind_human_variant() {
    let kind = StateKind::Human {
        prompt: "Approve?".to_string(),
        default_response: Some("yes".to_string()),
    };
    assert!(matches!(kind, StateKind::Human { .. }));
}

#[test]
fn state_kind_parallel_agents_variant() {
    let kind = StateKind::ParallelAgents {
        agents: vec![ParallelAgentConfig {
            agent: "worker-a".to_string(),
            input: "{{task}}".to_string(),
            weight: 1.0,
            timeout_seconds: 60,
            poll_interval_ms: 500,
        }],
        consensus: ConsensusConfig {
            strategy: ConsensusStrategy::Majority,
            threshold: Some(0.7),
            min_agreement_confidence: None,
            n: None,
            min_judges_required: 1,
            confidence_weighting: None,
        },
        judges_for_parallel: vec![],
    };
    assert!(matches!(kind, StateKind::ParallelAgents { .. }));
}

#[test]
fn state_kind_container_run_variant() {
    let kind = container_run_state("build", "rust:1.75", vec!["cargo", "build"], vec![]);
    assert!(matches!(kind, StateKind::ContainerRun { .. }));
}

#[test]
fn state_kind_parallel_container_run_variant() {
    let kind = StateKind::ParallelContainerRun {
        steps: vec![ContainerRunConfig {
            name: "lint".to_string(),
            image: "node:20".to_string(),
            command: vec!["npm".to_string(), "run".to_string(), "lint".to_string()],
            env: HashMap::new(),
            workdir: None,
            volumes: vec![],
            resources: None,
            registry_credentials: None,
            shell: false,
        }],
        completion: ParallelCompletionStrategy::AllSucceed,
    };
    assert!(matches!(kind, StateKind::ParallelContainerRun { .. }));
}

#[test]
fn state_kind_subworkflow_variant() {
    let kind = StateKind::Subworkflow {
        workflow_id: "child-wf".to_string(),
        mode: SubworkflowMode::Blocking,
        result_key: Some("child_result".to_string()),
        input: None,
    };
    assert!(matches!(kind, StateKind::Subworkflow { .. }));
}

// ============================================================================
// 6. TransitionRule — condition + target validation
// ============================================================================

#[test]
fn transition_rule_always_with_feedback() {
    let rule = TransitionRule {
        condition: TransitionCondition::Always,
        target: StateName::new("NEXT").unwrap(),
        feedback: Some("proceed".to_string()),
    };
    assert_eq!(rule.target.as_str(), "NEXT");
    assert_eq!(rule.feedback.as_deref(), Some("proceed"));
}

#[test]
fn workflow_validates_all_transition_targets() {
    // Two states, first transitions to a nonexistent third state.
    let start = StateName::new("A").unwrap();
    let end = StateName::new("B").unwrap();
    let ghost = StateName::new("GHOST").unwrap();

    let mut states = HashMap::new();
    states.insert(
        start.clone(),
        WorkflowState {
            kind: StateKind::System {
                command: "true".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![
                TransitionRule {
                    condition: TransitionCondition::OnSuccess,
                    target: end.clone(),
                    feedback: None,
                },
                TransitionRule {
                    condition: TransitionCondition::OnFailure,
                    target: ghost,
                    feedback: None,
                },
            ],
            timeout: None,
        },
    );
    states.insert(
        end,
        WorkflowState {
            kind: StateKind::System {
                command: "true".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );

    let spec = WorkflowSpec {
        initial_state: start,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad-transition"), spec).unwrap_err();
    assert!(err.to_string().contains("GHOST"));
}

// ============================================================================
// 7. TransitionCondition variants — serde round-trips
// ============================================================================

#[test]
fn transition_condition_always_roundtrip() {
    let cond = TransitionCondition::Always;
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, TransitionCondition::Always));
}

#[test]
fn transition_condition_on_success_roundtrip() {
    let cond = TransitionCondition::OnSuccess;
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, TransitionCondition::OnSuccess));
}

#[test]
fn transition_condition_on_failure_roundtrip() {
    let cond = TransitionCondition::OnFailure;
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, TransitionCondition::OnFailure));
}

#[test]
fn transition_condition_exit_code_roundtrip() {
    let cond = TransitionCondition::ExitCode { value: 42 };
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    if let TransitionCondition::ExitCode { value } = back {
        assert_eq!(value, 42);
    } else {
        panic!("Wrong variant");
    }
}

#[test]
fn transition_condition_score_above_roundtrip() {
    let cond = TransitionCondition::ScoreAbove { threshold: 0.9 };
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    if let TransitionCondition::ScoreAbove { threshold } = back {
        assert!((threshold - 0.9).abs() < f64::EPSILON);
    } else {
        panic!("Wrong variant");
    }
}

#[test]
fn transition_condition_score_below_roundtrip() {
    let cond = TransitionCondition::ScoreBelow { threshold: 0.3 };
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, TransitionCondition::ScoreBelow { .. }));
}

#[test]
fn transition_condition_score_between_roundtrip() {
    let cond = TransitionCondition::ScoreBetween { min: 0.4, max: 0.8 };
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    if let TransitionCondition::ScoreBetween { min, max } = back {
        assert!((min - 0.4).abs() < f64::EPSILON);
        assert!((max - 0.8).abs() < f64::EPSILON);
    } else {
        panic!("Wrong variant");
    }
}

#[test]
fn transition_condition_confidence_above_roundtrip() {
    let cond = TransitionCondition::ConfidenceAbove { threshold: 0.85 };
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, TransitionCondition::ConfidenceAbove { .. }));
}

#[test]
fn transition_condition_score_and_confidence_above_roundtrip() {
    let cond = TransitionCondition::ScoreAndConfidenceAbove { threshold: 0.85 };
    let json = serde_json::to_string(&cond).unwrap();
    assert!(json.contains("score_and_confidence_above"));
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    assert!(matches!(
        back,
        TransitionCondition::ScoreAndConfidenceAbove { .. }
    ));
}

#[test]
fn transition_condition_custom_roundtrip() {
    let cond = TransitionCondition::Custom {
        expression: "{{blackboard.approved}}".to_string(),
    };
    let json = serde_json::to_string(&cond).unwrap();
    let back: TransitionCondition = serde_json::from_str(&json).unwrap();
    if let TransitionCondition::Custom { expression } = back {
        assert_eq!(expression, "{{blackboard.approved}}");
    } else {
        panic!("Wrong variant");
    }
}

// ============================================================================
// 8. ConsensusConfig — strategies, threshold, min_judges
// ============================================================================

#[test]
fn consensus_config_weighted_average() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::WeightedAverage,
        threshold: None,
        min_agreement_confidence: Some(0.7),
        n: None,
        min_judges_required: 3,
        confidence_weighting: None,
    };
    assert!(matches!(
        config.strategy,
        ConsensusStrategy::WeightedAverage
    ));
    assert_eq!(config.min_judges_required, 3);
}

#[test]
fn consensus_config_majority() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::Majority,
        threshold: Some(0.7),
        min_agreement_confidence: None,
        n: None,
        min_judges_required: 3,
        confidence_weighting: None,
    };
    assert!(matches!(config.strategy, ConsensusStrategy::Majority));
    assert_eq!(config.threshold, Some(0.7));
}

#[test]
fn consensus_config_unanimous() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::Unanimous,
        threshold: Some(0.95),
        min_agreement_confidence: None,
        n: None,
        min_judges_required: 5,
        confidence_weighting: None,
    };
    assert!(matches!(config.strategy, ConsensusStrategy::Unanimous));
    assert_eq!(config.min_judges_required, 5);
}

#[test]
fn consensus_config_best_of_n() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::BestOfN,
        threshold: None,
        min_agreement_confidence: None,
        n: Some(3),
        min_judges_required: 5,
        confidence_weighting: None,
    };
    assert!(matches!(config.strategy, ConsensusStrategy::BestOfN));
    assert_eq!(config.n, Some(3));
}

#[test]
fn consensus_strategy_serde_roundtrip() {
    let strategies = vec![
        ConsensusStrategy::WeightedAverage,
        ConsensusStrategy::Majority,
        ConsensusStrategy::Unanimous,
        ConsensusStrategy::BestOfN,
    ];
    for strategy in strategies {
        let json = serde_json::to_string(&strategy).unwrap();
        let back: ConsensusStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(format!("{strategy:?}"), format!("{back:?}"));
    }
}

// ============================================================================
// 9. ConfidenceWeighting — validate() constraints
// ============================================================================

#[test]
fn confidence_weighting_default_sums_to_one() {
    let w = ConfidenceWeighting::default();
    assert_eq!(w.agreement_factor, 0.7);
    assert_eq!(w.self_confidence_factor, 0.3);
    assert!(w.validate().is_ok());
}

#[test]
fn confidence_weighting_valid_custom() {
    let w = ConfidenceWeighting {
        agreement_factor: 0.5,
        self_confidence_factor: 0.5,
    };
    assert!(w.validate().is_ok());
}

#[test]
fn confidence_weighting_rejects_sum_not_one() {
    let w = ConfidenceWeighting {
        agreement_factor: 0.5,
        self_confidence_factor: 0.3,
    };
    let err = w.validate().unwrap_err();
    assert!(err.contains("must sum to 1.0"));
}

#[test]
fn confidence_weighting_rejects_negative_agreement() {
    let w = ConfidenceWeighting {
        agreement_factor: -0.5,
        self_confidence_factor: 1.5,
    };
    assert!(w.validate().is_err());
}

#[test]
fn confidence_weighting_rejects_agreement_over_one() {
    let w = ConfidenceWeighting {
        agreement_factor: 1.5,
        self_confidence_factor: -0.5,
    };
    assert!(w.validate().is_err());
}

#[test]
fn confidence_weighting_rejects_negative_self_confidence() {
    let w = ConfidenceWeighting {
        agreement_factor: 1.3,
        self_confidence_factor: -0.3,
    };
    assert!(w.validate().is_err());
}

#[test]
fn confidence_weighting_boundary_zero_and_one() {
    let w = ConfidenceWeighting {
        agreement_factor: 1.0,
        self_confidence_factor: 0.0,
    };
    assert!(w.validate().is_ok());

    let w2 = ConfidenceWeighting {
        agreement_factor: 0.0,
        self_confidence_factor: 1.0,
    };
    assert!(w2.validate().is_ok());
}

// ============================================================================
// 10. JudgeConfig — construction
// ============================================================================

#[test]
fn judge_config_with_defaults() {
    let judge = JudgeConfig {
        agent_id: "security-judge".to_string(),
        input_template: None,
        weight: 1.0,
    };
    assert_eq!(judge.agent_id, "security-judge");
    assert!(judge.input_template.is_none());
    assert_eq!(judge.weight, 1.0);
}

#[test]
fn judge_config_with_template_and_custom_weight() {
    let judge = JudgeConfig {
        agent_id: "quality-judge".to_string(),
        input_template: Some("Review: {{output}}".to_string()),
        weight: 0.8,
    };
    assert_eq!(judge.input_template.as_deref(), Some("Review: {{output}}"));
    assert_eq!(judge.weight, 0.8);
}

// ============================================================================
// 11. Blackboard — get/set/remove, contains_key, merge, from_json/to_json
// ============================================================================

#[test]
fn blackboard_get_set_remove() {
    let mut bb = Blackboard::new();
    assert!(bb.get("key").is_none());

    bb.set("key", serde_json::json!("value"));
    assert_eq!(bb.get("key"), Some(&serde_json::json!("value")));

    let removed = bb.remove("key");
    assert_eq!(removed, Some(serde_json::json!("value")));
    assert!(bb.get("key").is_none());
}

#[test]
fn blackboard_contains_key() {
    let mut bb = Blackboard::new();
    assert!(!bb.contains_key("x"));
    bb.set("x", serde_json::json!(42));
    assert!(bb.contains_key("x"));
}

#[test]
fn blackboard_set_overwrites() {
    let mut bb = Blackboard::new();
    bb.set("v", serde_json::json!(1));
    bb.set("v", serde_json::json!(2));
    assert_eq!(bb.get("v"), Some(&serde_json::json!(2)));
}

#[test]
fn blackboard_merge() {
    let mut bb1 = Blackboard::new();
    bb1.set("a", serde_json::json!(1));
    bb1.set("b", serde_json::json!(2));

    let mut bb2 = Blackboard::new();
    bb2.set("b", serde_json::json!(99));
    bb2.set("c", serde_json::json!(3));

    bb1.merge(&bb2);
    assert_eq!(bb1.get("a"), Some(&serde_json::json!(1)));
    assert_eq!(bb1.get("b"), Some(&serde_json::json!(99))); // overwritten
    assert_eq!(bb1.get("c"), Some(&serde_json::json!(3)));
}

#[test]
fn blackboard_clear() {
    let mut bb = Blackboard::new();
    bb.set("x", serde_json::json!(1));
    bb.clear();
    assert!(!bb.contains_key("x"));
    assert!(bb.data().is_empty());
}

#[test]
fn blackboard_from_json_object() {
    let json = serde_json::json!({"alpha": 1, "beta": "two"});
    let bb = Blackboard::from_json(&json).unwrap();
    assert_eq!(bb.get("alpha"), Some(&serde_json::json!(1)));
    assert_eq!(bb.get("beta"), Some(&serde_json::json!("two")));
}

#[test]
fn blackboard_from_json_non_object_returns_empty() {
    let json = serde_json::json!(42);
    let bb = Blackboard::from_json(&json).unwrap();
    assert!(bb.data().is_empty());
}

#[test]
fn blackboard_to_json_roundtrip() {
    let mut bb = Blackboard::new();
    bb.set("score", serde_json::json!(0.95));
    bb.set("tags", serde_json::json!(["rust", "wasm"]));

    let json = bb.to_json();
    let bb2 = Blackboard::from_json(&json).unwrap();
    assert_eq!(bb2.get("score"), Some(&serde_json::json!(0.95)));
    assert_eq!(bb2.get("tags"), Some(&serde_json::json!(["rust", "wasm"])));
}

// ============================================================================
// 12. WorkflowExecution — new, transition_to, record/get_state_output
// ============================================================================

#[test]
fn workflow_execution_new_starts_at_initial_state() {
    let wf = Workflow::new(
        minimal_metadata("exec-test"),
        two_state_spec("INIT", "DONE"),
    )
    .unwrap();

    let exec = WorkflowExecution::new(&wf, ExecutionId::new(), serde_json::json!({"task": "go"}));
    assert_eq!(exec.current_state.as_str(), "INIT");
    assert_eq!(exec.workflow_id, wf.id);
}

#[test]
fn workflow_execution_transition_to() {
    let wf = Workflow::new(minimal_metadata("trans-test"), two_state_spec("A", "B")).unwrap();

    let mut exec = WorkflowExecution::new(&wf, ExecutionId::new(), serde_json::json!(null));
    assert_eq!(exec.current_state.as_str(), "A");

    let before = exec.last_transition_at;
    exec.transition_to(StateName::new("B").unwrap());
    assert_eq!(exec.current_state.as_str(), "B");
    assert!(exec.last_transition_at >= before);
}

#[test]
fn workflow_execution_record_and_get_state_output() {
    let wf = Workflow::new(
        minimal_metadata("output-test"),
        single_system_state_spec("RUN"),
    )
    .unwrap();

    let mut exec = WorkflowExecution::new(&wf, ExecutionId::new(), serde_json::json!(null));
    let state = StateName::new("RUN").unwrap();
    assert!(exec.get_state_output(&state).is_none());

    exec.record_state_output(state.clone(), serde_json::json!({"exit_code": 0}));
    assert_eq!(
        exec.get_state_output(&state),
        Some(&serde_json::json!({"exit_code": 0}))
    );
}

// ============================================================================
// 13. ContainerRun validation
// ============================================================================

#[test]
fn container_run_rejects_empty_name() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state("", "rust:1.75", vec!["cargo", "build"], vec![]),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad-name"), spec).unwrap_err();
    assert!(err.to_string().contains("name cannot be empty"));
}

#[test]
fn container_run_rejects_empty_image() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state("build", "", vec!["cargo", "build"], vec![]),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad-image"), spec).unwrap_err();
    assert!(err.to_string().contains("image cannot be empty"));
}

#[test]
fn container_run_rejects_empty_command() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state("build", "rust:1.75", vec![], vec![]),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad-cmd"), spec).unwrap_err();
    assert!(err.to_string().contains("at least one token"));
}

#[test]
fn container_run_rejects_non_absolute_mount_path() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state(
                "build",
                "rust:1.75",
                vec!["cargo", "build"],
                vec![ContainerVolumeMount {
                    name: "src".to_string(),
                    mount_path: "relative/path".to_string(),
                    read_only: false,
                }],
            ),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad-mount"), spec).unwrap_err();
    assert!(err.to_string().contains("absolute path"));
}

#[test]
fn container_run_accepts_valid_mount() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state(
                "build",
                "rust:1.75",
                vec!["cargo", "build"],
                vec![ContainerVolumeMount {
                    name: "src".to_string(),
                    mount_path: "/workspace/src".to_string(),
                    read_only: false,
                }],
            ),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    assert!(Workflow::new(minimal_metadata("good-mount"), spec).is_ok());
}

// ============================================================================
// 14. ParallelContainerRun validation
// ============================================================================

#[test]
fn parallel_container_run_rejects_empty_steps() {
    let sn = StateName::new("PAR").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::ParallelContainerRun {
                steps: vec![],
                completion: ParallelCompletionStrategy::AllSucceed,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("empty-par"), spec).unwrap_err();
    assert!(err.to_string().contains("at least one step"));
}

#[test]
fn parallel_container_run_rejects_duplicate_step_names() {
    let sn = StateName::new("PAR").unwrap();
    let step = |name: &str| ContainerRunConfig {
        name: name.to_string(),
        image: "alpine:3".to_string(),
        command: vec!["echo".to_string()],
        env: HashMap::new(),
        workdir: None,
        volumes: vec![],
        resources: None,
        registry_credentials: None,
        shell: false,
    };
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::ParallelContainerRun {
                steps: vec![step("lint"), step("lint")],
                completion: ParallelCompletionStrategy::AllSucceed,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("dup-par"), spec).unwrap_err();
    assert!(err.to_string().contains("must be unique"));
}

#[test]
fn parallel_container_run_rejects_non_absolute_step_mount() {
    let sn = StateName::new("PAR").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::ParallelContainerRun {
                steps: vec![ContainerRunConfig {
                    name: "test".to_string(),
                    image: "node:20".to_string(),
                    command: vec!["npm".to_string(), "test".to_string()],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![ContainerVolumeMount {
                        name: "data".to_string(),
                        mount_path: "data".to_string(),
                        read_only: false,
                    }],
                    resources: None,
                    registry_credentials: None,
                    shell: false,
                }],
                completion: ParallelCompletionStrategy::AnySucceed,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("bad-par-mount"), spec).unwrap_err();
    assert!(err.to_string().contains("non-absolute mount_path"));
}

#[test]
fn parallel_container_run_rejects_empty_step_image() {
    let sn = StateName::new("PAR").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::ParallelContainerRun {
                steps: vec![ContainerRunConfig {
                    name: "test".to_string(),
                    image: "".to_string(),
                    command: vec!["echo".to_string()],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![],
                    resources: None,
                    registry_credentials: None,
                    shell: false,
                }],
                completion: ParallelCompletionStrategy::AllSucceed,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("empty-img"), spec).unwrap_err();
    assert!(err.to_string().contains("empty image"));
}

#[test]
fn parallel_container_run_rejects_empty_step_command() {
    let sn = StateName::new("PAR").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::ParallelContainerRun {
                steps: vec![ContainerRunConfig {
                    name: "test".to_string(),
                    image: "node:20".to_string(),
                    command: vec![],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![],
                    resources: None,
                    registry_credentials: None,
                    shell: false,
                }],
                completion: ParallelCompletionStrategy::AllSucceed,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("empty-cmd"), spec).unwrap_err();
    assert!(err.to_string().contains("at least one token"));
}

// ============================================================================
// 15. Subworkflow validation
// ============================================================================

#[test]
fn subworkflow_blocking_requires_result_key() {
    let sn = StateName::new("SUB").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::Subworkflow {
                workflow_id: "child".to_string(),
                mode: SubworkflowMode::Blocking,
                result_key: None, // missing!
                input: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("sub-bad"), spec).unwrap_err();
    assert!(err.to_string().contains("result_key"));
}

#[test]
fn subworkflow_fire_and_forget_forbids_result_key() {
    let sn = StateName::new("SUB").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::Subworkflow {
                workflow_id: "child".to_string(),
                mode: SubworkflowMode::FireAndForget,
                result_key: Some("oops".to_string()),
                input: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("sub-bad2"), spec).unwrap_err();
    assert!(err.to_string().contains("must not specify result_key"));
}

#[test]
fn subworkflow_rejects_empty_workflow_id() {
    let sn = StateName::new("SUB").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::Subworkflow {
                workflow_id: "".to_string(),
                mode: SubworkflowMode::FireAndForget,
                result_key: None,
                input: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    let err = Workflow::new(minimal_metadata("sub-empty"), spec).unwrap_err();
    assert!(err.to_string().contains("workflow_id cannot be empty"));
}

#[test]
fn subworkflow_blocking_with_result_key_accepted() {
    let sn = StateName::new("SUB").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::Subworkflow {
                workflow_id: "child-wf".to_string(),
                mode: SubworkflowMode::Blocking,
                result_key: Some("child_output".to_string()),
                input: Some("{{blackboard.task}}".to_string()),
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    assert!(Workflow::new(minimal_metadata("sub-ok"), spec).is_ok());
}

#[test]
fn subworkflow_fire_and_forget_without_result_key_accepted() {
    let sn = StateName::new("SUB").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::Subworkflow {
                workflow_id: "child-wf".to_string(),
                mode: SubworkflowMode::FireAndForget,
                result_key: None,
                input: None,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![],
    };
    assert!(Workflow::new(minimal_metadata("sub-ok2"), spec).is_ok());
}

// ============================================================================
// 16. Volume mount resolution — mounts must reference declared spec.volumes
// ============================================================================

#[test]
fn volume_mount_rejects_undeclared_volume() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state(
                "build",
                "rust:1.75",
                vec!["cargo", "build"],
                vec![ContainerVolumeMount {
                    name: "ghost-vol".to_string(),
                    mount_path: "/data".to_string(),
                    read_only: false,
                }],
            ),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![WorkflowVolumeSpec {
            name: "real-vol".to_string(),
            storage_class: Default::default(),
            size_limit_bytes: None,
        }],
    };
    let err = Workflow::new(minimal_metadata("bad-vol"), spec).unwrap_err();
    assert!(err.to_string().contains("ghost-vol"));
    assert!(err.to_string().contains("not declared"));
}

#[test]
fn volume_mount_accepts_declared_volume() {
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state(
                "build",
                "rust:1.75",
                vec!["cargo", "build"],
                vec![ContainerVolumeMount {
                    name: "workspace".to_string(),
                    mount_path: "/workspace".to_string(),
                    read_only: false,
                }],
            ),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![WorkflowVolumeSpec {
            name: "workspace".to_string(),
            storage_class: Default::default(),
            size_limit_bytes: Some(1_073_741_824),
        }],
    };
    assert!(Workflow::new(minimal_metadata("good-vol"), spec).is_ok());
}

#[test]
fn volume_mount_skips_validation_when_no_volumes_declared() {
    // When spec.volumes is empty, mount names are not validated (opt-in per ADR-050).
    let sn = StateName::new("BUILD").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: container_run_state(
                "build",
                "rust:1.75",
                vec!["cargo", "build"],
                vec![ContainerVolumeMount {
                    name: "anything".to_string(),
                    mount_path: "/data".to_string(),
                    read_only: false,
                }],
            ),
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![], // empty => skip resolution check
    };
    assert!(Workflow::new(minimal_metadata("no-vol-decl"), spec).is_ok());
}

#[test]
fn parallel_container_run_mount_rejects_undeclared_volume() {
    let sn = StateName::new("PAR").unwrap();
    let mut states = HashMap::new();
    states.insert(
        sn.clone(),
        WorkflowState {
            kind: StateKind::ParallelContainerRun {
                steps: vec![ContainerRunConfig {
                    name: "lint".to_string(),
                    image: "node:20".to_string(),
                    command: vec!["npm".to_string(), "run".to_string(), "lint".to_string()],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![ContainerVolumeMount {
                        name: "missing-vol".to_string(),
                        mount_path: "/app".to_string(),
                        read_only: true,
                    }],
                    resources: None,
                    registry_credentials: None,
                    shell: false,
                }],
                completion: ParallelCompletionStrategy::AllSucceed,
            },
            transitions: vec![],
            timeout: None,
        },
    );
    let spec = WorkflowSpec {
        initial_state: sn,
        context: HashMap::new(),
        states,
        volumes: vec![WorkflowVolumeSpec {
            name: "declared-vol".to_string(),
            storage_class: Default::default(),
            size_limit_bytes: None,
        }],
    };
    let err = Workflow::new(minimal_metadata("bad-par-vol"), spec).unwrap_err();
    assert!(err.to_string().contains("missing-vol"));
}

// ============================================================================
// 17. Serialization round-trips
// ============================================================================

#[test]
fn workflow_serde_roundtrip() {
    let wf = Workflow::new(
        WorkflowMetadata {
            name: "roundtrip-test".to_string(),
            version: Some("1.2.3".to_string()),
            description: Some("A test workflow".to_string()),
            tags: vec![],
            labels: {
                let mut m = HashMap::new();
                m.insert("env".to_string(), "test".to_string());
                m
            },
            annotations: HashMap::new(),
        },
        two_state_spec("GENERATE", "VALIDATE"),
    )
    .unwrap();

    let json = serde_json::to_string(&wf).unwrap();
    let back: Workflow = serde_json::from_str(&json).unwrap();

    assert_eq!(back.id, wf.id);
    assert_eq!(back.metadata.name, "roundtrip-test");
    assert_eq!(back.metadata.version, Some("1.2.3".to_string()));
    assert_eq!(
        back.metadata.description,
        Some("A test workflow".to_string())
    );
    assert_eq!(back.spec.initial_state.as_str(), "GENERATE");
    assert_eq!(back.spec.states.len(), 2);
}

#[test]
fn workflow_spec_serde_roundtrip() {
    let spec = two_state_spec("A", "B");
    let json = serde_json::to_string(&spec).unwrap();
    let back: WorkflowSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(back.initial_state.as_str(), "A");
    assert_eq!(back.states.len(), 2);
}

#[test]
fn consensus_config_serde_roundtrip() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::BestOfN,
        threshold: Some(0.8),
        min_agreement_confidence: Some(0.6),
        n: Some(3),
        min_judges_required: 5,
        confidence_weighting: Some(ConfidenceWeighting {
            agreement_factor: 0.6,
            self_confidence_factor: 0.4,
        }),
    };
    let json = serde_json::to_string(&config).unwrap();
    let back: ConsensusConfig = serde_json::from_str(&json).unwrap();

    assert!(matches!(back.strategy, ConsensusStrategy::BestOfN));
    assert_eq!(back.threshold, Some(0.8));
    assert_eq!(back.min_agreement_confidence, Some(0.6));
    assert_eq!(back.n, Some(3));
    assert_eq!(back.min_judges_required, 5);
    let w = back.confidence_weighting.unwrap();
    assert_eq!(w.agreement_factor, 0.6);
    assert_eq!(w.self_confidence_factor, 0.4);
}

#[test]
fn blackboard_serde_roundtrip() {
    let mut bb = Blackboard::new();
    bb.set("count", serde_json::json!(42));
    bb.set("nested", serde_json::json!({"a": [1, 2, 3]}));

    let json = serde_json::to_string(&bb).unwrap();
    let back: Blackboard = serde_json::from_str(&json).unwrap();

    assert_eq!(back.get("count"), Some(&serde_json::json!(42)));
    assert_eq!(
        back.get("nested"),
        Some(&serde_json::json!({"a": [1, 2, 3]}))
    );
}

#[test]
fn workflow_execution_serde_roundtrip() {
    let wf = Workflow::new(minimal_metadata("exec-rt"), single_system_state_spec("RUN")).unwrap();
    let mut exec = WorkflowExecution::new(&wf, ExecutionId::new(), serde_json::json!({"x": 1}));
    exec.record_state_output(StateName::new("RUN").unwrap(), serde_json::json!("ok"));

    let json = serde_json::to_string(&exec).unwrap();
    let back: WorkflowExecution = serde_json::from_str(&json).unwrap();

    assert_eq!(back.id, exec.id);
    assert_eq!(back.workflow_id, exec.workflow_id);
    assert_eq!(back.current_state.as_str(), "RUN");
    assert_eq!(
        back.get_state_output(&StateName::new("RUN").unwrap()),
        Some(&serde_json::json!("ok"))
    );
}

// ============================================================================
// Additional edge cases
// ============================================================================

#[test]
fn workflow_initial_state_accessor() {
    let wf = Workflow::new(
        minimal_metadata("accessor"),
        single_system_state_spec("MAIN"),
    )
    .unwrap();
    let state = wf.initial_state();
    assert!(matches!(state.kind, StateKind::System { .. }));
}

#[test]
fn workflow_get_state_returns_none_for_unknown() {
    let wf = Workflow::new(
        minimal_metadata("get-state"),
        single_system_state_spec("ONLY"),
    )
    .unwrap();
    assert!(wf.get_state(&StateName::new("NOPE").unwrap()).is_none());
    assert!(wf.get_state(&StateName::new("ONLY").unwrap()).is_some());
}

#[test]
fn workflow_is_terminal_state_false_for_unknown() {
    let wf = Workflow::new(
        minimal_metadata("terminal"),
        single_system_state_spec("END"),
    )
    .unwrap();
    // Unknown state returns false (not found => unwrap_or(false))
    assert!(!wf.is_terminal_state(&StateName::new("GHOST").unwrap()));
}

#[test]
fn workflow_referenced_judge_agents_empty_when_no_judges() {
    let wf = Workflow::new(
        minimal_metadata("no-judges"),
        single_system_state_spec("RUN"),
    )
    .unwrap();
    assert!(wf.referenced_judge_agents().is_empty());
}

#[test]
fn workflow_id_display() {
    let id = WorkflowId::new();
    let display = format!("{id}");
    // Should be a valid UUID string
    assert!(uuid::Uuid::parse_str(&display).is_ok());
}
