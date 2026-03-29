// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Execution Domain Aggregate Tests (BC-2)
//!
//! Comprehensive unit tests for the Execution bounded context domain layer.
//! Covers the `Execution` aggregate root, `Iteration` entity, `ExecutionHierarchy`
//! value object, and all related domain types.
//!
//! References: ADR-005 (Iterative Execution Strategy), ADR-016 (Agent-as-Judge),
//! ADR-036 (NFS Server Gateway / container UID/GID).

use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::{
    CodeDiff, Execution, ExecutionError, ExecutionHierarchy, ExecutionId, ExecutionInfo,
    ExecutionInput, ExecutionStatus, IterationError, IterationStatus, LlmInteraction,
    TrajectoryStep, MAX_RECURSIVE_DEPTH,
};
use aegis_orchestrator_core::domain::validation::ValidationResults;
use chrono::Utc;

// ============================================================================
// Test Helpers
// ============================================================================

fn make_input(intent: &str) -> ExecutionInput {
    ExecutionInput {
        intent: Some(intent.to_string()),
        payload: serde_json::json!({}),
    }
}

fn make_input_with_payload(intent: &str, payload: serde_json::Value) -> ExecutionInput {
    ExecutionInput {
        intent: Some(intent.to_string()),
        payload,
    }
}

fn make_execution(max_iterations: u8) -> Execution {
    Execution::new(
        AgentId::new(),
        make_input("test task"),
        max_iterations,
        "aegis-system-operator".to_string(),
    )
}

fn make_running_execution(max_iterations: u8) -> Execution {
    let mut exec = make_execution(max_iterations);
    exec.start();
    exec
}

fn empty_validation_results() -> ValidationResults {
    ValidationResults {
        system: None,
        output: None,
        semantic: None,
        gradient: None,
        consensus: None,
    }
}

fn make_llm_interaction(provider: &str, model: &str) -> LlmInteraction {
    LlmInteraction {
        provider: provider.to_string(),
        model: model.to_string(),
        prompt: "test prompt".to_string(),
        response: "test response".to_string(),
        timestamp: Utc::now(),
    }
}

// ============================================================================
// 1. Execution::new() — valid creation, default status, initial state
// ============================================================================

#[test]
fn new_execution_has_pending_status() {
    let exec = make_execution(10);
    assert_eq!(exec.status, ExecutionStatus::Pending);
}

#[test]
fn new_execution_has_zero_iterations() {
    let exec = make_execution(10);
    assert!(exec.iterations.is_empty());
    assert_eq!(exec.total_attempts(), 0);
}

#[test]
fn new_execution_preserves_max_iterations() {
    let exec = make_execution(7);
    assert_eq!(exec.max_iterations, 7);
}

#[test]
fn new_execution_has_unique_id() {
    let a = make_execution(5);
    let b = make_execution(5);
    assert_ne!(a.id, b.id);
}

#[test]
fn new_execution_stores_agent_id() {
    let agent_id = AgentId::new();
    let exec = Execution::new(
        agent_id,
        make_input("task"),
        5,
        "aegis-system-operator".to_string(),
    );
    assert_eq!(exec.agent_id, agent_id);
}

#[test]
fn new_execution_has_no_end_time() {
    let exec = make_execution(5);
    assert!(exec.ended_at.is_none());
}

#[test]
fn new_execution_has_no_error() {
    let exec = make_execution(5);
    assert!(exec.error.is_none());
}

#[test]
fn new_execution_is_not_completed() {
    let exec = make_execution(5);
    assert!(!exec.is_completed());
}

#[test]
fn new_execution_has_no_current_iteration() {
    let exec = make_execution(5);
    assert!(exec.current_iteration().is_none());
}

// ============================================================================
// 2. Execution::new_child() — hierarchy, depth tracking, tenant inheritance
// ============================================================================

#[test]
fn new_child_has_depth_one() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child task"), 3, &parent).unwrap();
    assert_eq!(child.depth(), 1);
}

#[test]
fn new_child_references_parent_id() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child task"), 3, &parent).unwrap();
    assert_eq!(child.parent_id(), Some(parent.id));
}

#[test]
fn new_child_inherits_tenant_id() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child task"), 3, &parent).unwrap();
    assert_eq!(child.tenant_id, parent.tenant_id);
}

#[test]
fn new_child_starts_pending() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child task"), 3, &parent).unwrap();
    assert_eq!(child.status, ExecutionStatus::Pending);
}

#[test]
fn new_child_has_own_unique_id() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child task"), 3, &parent).unwrap();
    assert_ne!(child.id, parent.id);
}

#[test]
fn new_child_hierarchy_path_includes_parent_and_child() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child task"), 3, &parent).unwrap();
    assert_eq!(child.hierarchy.path.len(), 2);
    assert_eq!(child.hierarchy.path[0], parent.id);
    assert_eq!(child.hierarchy.path[1], child.id);
}

#[test]
fn new_child_root_id_is_original_root() {
    let root = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("c1"), 3, &root).unwrap();
    let grandchild = Execution::new_child(AgentId::new(), make_input("c2"), 3, &child).unwrap();
    assert_eq!(grandchild.hierarchy.root_id(), root.id);
}

// ============================================================================
// 3. ExecutionHierarchy — root(), child(), can_spawn_child(), depth limits
// ============================================================================

#[test]
fn hierarchy_root_has_depth_zero() {
    let id = ExecutionId::new();
    let h = ExecutionHierarchy::root(id);
    assert_eq!(h.depth, 0);
}

#[test]
fn hierarchy_root_has_no_parent() {
    let id = ExecutionId::new();
    let h = ExecutionHierarchy::root(id);
    assert!(h.parent_id().is_none());
}

#[test]
fn hierarchy_root_path_contains_only_self() {
    let id = ExecutionId::new();
    let h = ExecutionHierarchy::root(id);
    assert_eq!(h.path, vec![id]);
}

#[test]
fn hierarchy_root_can_spawn_child() {
    let id = ExecutionId::new();
    let h = ExecutionHierarchy::root(id);
    assert!(h.can_spawn_child());
}

#[test]
fn hierarchy_root_id_returns_first_path_element() {
    let id = ExecutionId::new();
    let h = ExecutionHierarchy::root(id);
    assert_eq!(h.root_id(), id);
}

#[test]
fn hierarchy_child_increments_depth() {
    let root = ExecutionHierarchy::root(ExecutionId::new());
    let child_id = ExecutionId::new();
    let child = ExecutionHierarchy::child(&root, child_id).unwrap();
    assert_eq!(child.depth, 1);
}

#[test]
fn hierarchy_child_sets_parent_id() {
    let root_id = ExecutionId::new();
    let root = ExecutionHierarchy::root(root_id);
    let child_id = ExecutionId::new();
    let child = ExecutionHierarchy::child(&root, child_id).unwrap();
    assert_eq!(child.parent_id(), Some(root_id));
}

#[test]
fn hierarchy_child_appends_to_path() {
    let root_id = ExecutionId::new();
    let root = ExecutionHierarchy::root(root_id);
    let child_id = ExecutionId::new();
    let child = ExecutionHierarchy::child(&root, child_id).unwrap();
    assert_eq!(child.path, vec![root_id, child_id]);
}

#[test]
fn hierarchy_max_depth_exactly_reachable() {
    let mut h = ExecutionHierarchy::root(ExecutionId::new());
    for _ in 0..MAX_RECURSIVE_DEPTH {
        h = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap();
    }
    assert_eq!(h.depth, MAX_RECURSIVE_DEPTH);
}

#[test]
fn hierarchy_beyond_max_depth_fails() {
    let mut h = ExecutionHierarchy::root(ExecutionId::new());
    for _ in 0..MAX_RECURSIVE_DEPTH {
        h = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap();
    }
    let result = ExecutionHierarchy::child(&h, ExecutionId::new());
    assert!(result.is_err());
}

#[test]
fn hierarchy_error_message_contains_depth_limit() {
    let mut h = ExecutionHierarchy::root(ExecutionId::new());
    for _ in 0..MAX_RECURSIVE_DEPTH {
        h = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap();
    }
    let err = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap_err();
    assert!(
        err.contains(&MAX_RECURSIVE_DEPTH.to_string()),
        "error should reference depth limit; got: {err}"
    );
}

#[test]
fn hierarchy_can_spawn_child_false_at_max_depth() {
    let mut h = ExecutionHierarchy::root(ExecutionId::new());
    for _ in 0..MAX_RECURSIVE_DEPTH {
        h = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap();
    }
    assert!(!h.can_spawn_child());
}

#[test]
fn hierarchy_can_spawn_child_true_below_max_depth() {
    let mut h = ExecutionHierarchy::root(ExecutionId::new());
    for _ in 0..(MAX_RECURSIVE_DEPTH - 1) {
        h = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap();
    }
    assert!(h.can_spawn_child());
}

#[test]
fn hierarchy_path_length_equals_depth_plus_one() {
    let mut h = ExecutionHierarchy::root(ExecutionId::new());
    assert_eq!(h.path.len(), 1);
    for i in 0..MAX_RECURSIVE_DEPTH {
        h = ExecutionHierarchy::child(&h, ExecutionId::new()).unwrap();
        assert_eq!(h.path.len(), (i + 2) as usize);
    }
}

// ============================================================================
// 4. Execution state machine — start(), complete(), fail() transitions
// ============================================================================

#[test]
fn start_transitions_to_running() {
    let mut exec = make_execution(5);
    exec.start();
    assert_eq!(exec.status, ExecutionStatus::Running);
}

#[test]
fn complete_transitions_to_completed() {
    let mut exec = make_running_execution(5);
    exec.complete();
    assert_eq!(exec.status, ExecutionStatus::Completed);
}

#[test]
fn complete_sets_ended_at() {
    let mut exec = make_running_execution(5);
    exec.complete();
    assert!(exec.ended_at.is_some());
}

#[test]
fn complete_marks_is_completed_true() {
    let mut exec = make_running_execution(5);
    exec.complete();
    assert!(exec.is_completed());
}

#[test]
fn fail_transitions_to_failed() {
    let mut exec = make_running_execution(5);
    exec.fail("broken".to_string());
    assert_eq!(exec.status, ExecutionStatus::Failed);
}

#[test]
fn fail_sets_error_message() {
    let mut exec = make_running_execution(5);
    exec.fail("something broke".to_string());
    assert_eq!(exec.error.as_deref(), Some("something broke"));
}

#[test]
fn fail_sets_ended_at() {
    let mut exec = make_running_execution(5);
    exec.fail("broken".to_string());
    assert!(exec.ended_at.is_some());
}

#[test]
fn fail_marks_is_completed_true() {
    let mut exec = make_running_execution(5);
    exec.fail("broken".to_string());
    assert!(exec.is_completed());
}

#[test]
fn cancelled_status_is_completed() {
    // ExecutionStatus::Cancelled should satisfy is_completed via match arm.
    let mut exec = make_execution(5);
    exec.status = ExecutionStatus::Cancelled;
    assert!(exec.is_completed());
}

#[test]
fn running_status_is_not_completed() {
    let mut exec = make_execution(5);
    exec.start();
    assert!(!exec.is_completed());
}

// ============================================================================
// 5. Iteration lifecycle — start, complete, fail, sequential numbering
// ============================================================================

#[test]
fn start_iteration_returns_running_iteration() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("generate".to_string()).unwrap();
    assert_eq!(iter.status, IterationStatus::Running);
}

#[test]
fn first_iteration_is_number_one() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("generate".to_string()).unwrap();
    assert_eq!(iter.number, 1);
}

#[test]
fn iteration_numbers_are_sequential() {
    let mut exec = make_running_execution(5);

    exec.start_iteration("iter1".to_string()).unwrap();
    exec.complete_iteration("out1".to_string());

    exec.start_iteration("iter2".to_string()).unwrap();
    exec.complete_iteration("out2".to_string());

    exec.start_iteration("iter3".to_string()).unwrap();

    assert_eq!(exec.iterations()[0].number, 1);
    assert_eq!(exec.iterations()[1].number, 2);
    assert_eq!(exec.iterations()[2].number, 3);
}

#[test]
fn start_iteration_stores_action() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("validate".to_string()).unwrap();
    assert_eq!(iter.action, "validate");
}

#[test]
fn start_iteration_has_no_output() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("generate".to_string()).unwrap();
    assert!(iter.output.is_none());
}

#[test]
fn start_iteration_has_no_validation_results() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("generate".to_string()).unwrap();
    assert!(iter.validation_results.is_none());
}

#[test]
fn start_iteration_has_empty_llm_interactions() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("generate".to_string()).unwrap();
    assert!(iter.llm_interactions.is_empty());
}

#[test]
fn start_iteration_has_no_trajectory() {
    let mut exec = make_running_execution(5);
    let iter = exec.start_iteration("generate".to_string()).unwrap();
    assert!(iter.trajectory.is_none());
}

#[test]
fn complete_iteration_sets_success_status() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("generate".to_string()).unwrap();
    exec.complete_iteration("result".to_string());
    assert_eq!(exec.iterations()[0].status, IterationStatus::Success);
}

#[test]
fn complete_iteration_stores_output() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("generate".to_string()).unwrap();
    exec.complete_iteration("hello world".to_string());
    assert_eq!(exec.iterations()[0].output.as_deref(), Some("hello world"));
}

#[test]
fn complete_iteration_sets_ended_at() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("generate".to_string()).unwrap();
    exec.complete_iteration("result".to_string());
    assert!(exec.iterations()[0].ended_at.is_some());
}

#[test]
fn fail_iteration_sets_failed_status() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("generate".to_string()).unwrap();
    exec.fail_iteration(IterationError {
        message: "compile error".to_string(),
        details: None,
    });
    assert_eq!(exec.iterations()[0].status, IterationStatus::Failed);
}

#[test]
fn fail_iteration_stores_error() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("generate".to_string()).unwrap();
    exec.fail_iteration(IterationError {
        message: "compile error".to_string(),
        details: Some("line 42".to_string()),
    });
    let err = exec.iterations()[0].error.as_ref().unwrap();
    assert_eq!(err.message, "compile error");
    assert_eq!(err.details.as_deref(), Some("line 42"));
}

#[test]
fn fail_iteration_sets_ended_at() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("generate".to_string()).unwrap();
    exec.fail_iteration(IterationError {
        message: "error".to_string(),
        details: None,
    });
    assert!(exec.iterations()[0].ended_at.is_some());
}

#[test]
fn total_attempts_tracks_iteration_count() {
    let mut exec = make_running_execution(5);
    assert_eq!(exec.total_attempts(), 0);

    exec.start_iteration("i1".to_string()).unwrap();
    assert_eq!(exec.total_attempts(), 1);

    exec.complete_iteration("o1".to_string());
    exec.start_iteration("i2".to_string()).unwrap();
    assert_eq!(exec.total_attempts(), 2);
}

#[test]
fn current_iteration_returns_most_recent() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("first".to_string()).unwrap();
    exec.complete_iteration("o1".to_string());
    exec.start_iteration("second".to_string()).unwrap();

    let current = exec.current_iteration().unwrap();
    assert_eq!(current.number, 2);
    assert_eq!(current.action, "second");
}

#[test]
fn iterations_accessor_returns_all() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("a".to_string()).unwrap();
    exec.complete_iteration("o".to_string());
    exec.start_iteration("b".to_string()).unwrap();
    exec.complete_iteration("o2".to_string());

    let iters = exec.iterations();
    assert_eq!(iters.len(), 2);
}

// ============================================================================
// 6. IterationStatus — variant coverage
// ============================================================================

#[test]
fn iteration_status_running_on_start() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();
    assert_eq!(exec.iterations()[0].status, IterationStatus::Running);
}

#[test]
fn iteration_status_success_after_complete() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();
    exec.complete_iteration("ok".to_string());
    assert_eq!(exec.iterations()[0].status, IterationStatus::Success);
}

#[test]
fn iteration_status_failed_after_fail() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();
    exec.fail_iteration(IterationError {
        message: "err".to_string(),
        details: None,
    });
    assert_eq!(exec.iterations()[0].status, IterationStatus::Failed);
}

#[test]
fn iteration_status_refining_is_distinct_variant() {
    // IterationStatus::Refining exists as a variant and is distinct from others.
    assert_ne!(IterationStatus::Refining, IterationStatus::Running);
    assert_ne!(IterationStatus::Refining, IterationStatus::Success);
    assert_ne!(IterationStatus::Refining, IterationStatus::Failed);
}

// ============================================================================
// 7. Max iterations enforcement
// ============================================================================

#[test]
fn max_iterations_default_is_10_by_convention() {
    let exec = make_execution(10);
    assert_eq!(exec.max_iterations, 10);
}

#[test]
fn start_iteration_fails_when_max_reached() {
    let mut exec = make_running_execution(2);
    exec.start_iteration("i1".to_string()).unwrap();
    exec.complete_iteration("o1".to_string());
    exec.start_iteration("i2".to_string()).unwrap();
    exec.complete_iteration("o2".to_string());

    let err = exec.start_iteration("i3".to_string()).unwrap_err();
    assert!(matches!(err, ExecutionError::MaxIterationsReached));
}

#[test]
fn max_iterations_of_one_allows_exactly_one() {
    let mut exec = make_running_execution(1);
    exec.start_iteration("only".to_string()).unwrap();
    exec.complete_iteration("done".to_string());

    let err = exec.start_iteration("too many".to_string()).unwrap_err();
    assert!(matches!(err, ExecutionError::MaxIterationsReached));
}

#[test]
fn max_iterations_boundary_exact() {
    let max = 3u8;
    let mut exec = make_running_execution(max);

    for i in 0..max {
        exec.start_iteration(format!("iter{}", i + 1)).unwrap();
        exec.complete_iteration(format!("out{}", i + 1));
    }

    // The (max+1)th should fail.
    let result = exec.start_iteration("overflow".to_string());
    assert!(result.is_err());
}

// ============================================================================
// 8. Only one Running iteration invariant (observed behavior)
// ============================================================================

#[test]
fn starting_iteration_while_previous_running_still_adds() {
    // The current implementation does not explicitly prevent starting a new
    // iteration while a previous one is Running — it simply appends. This test
    // documents that behavior. If the invariant is enforced in the future,
    // this test should be updated to expect an error.
    let mut exec = make_running_execution(5);
    exec.start_iteration("first".to_string()).unwrap();
    // Don't complete the first iteration — start a second immediately.
    let result = exec.start_iteration("second".to_string());
    // Currently succeeds; both iterations exist.
    assert!(result.is_ok());
    assert_eq!(exec.iterations().len(), 2);
}

// ============================================================================
// 9. store_validation_results()
// ============================================================================

#[test]
fn store_validation_results_attaches_to_correct_iteration() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();
    exec.complete_iteration("out".to_string());
    exec.start_iteration("validate".to_string()).unwrap();

    let results = empty_validation_results();
    exec.store_validation_results(2, results).unwrap();

    assert!(exec.iterations()[0].validation_results.is_none());
    assert!(exec.iterations()[1].validation_results.is_some());
}

#[test]
fn store_validation_results_nonexistent_iteration_returns_error() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    let results = empty_validation_results();
    let err = exec.store_validation_results(99, results).unwrap_err();
    assert!(matches!(err, ExecutionError::IterationNotFound(99)));
}

#[test]
fn store_validation_results_overwrites_previous() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    exec.store_validation_results(1, empty_validation_results())
        .unwrap();
    // Store again — should overwrite without error.
    exec.store_validation_results(1, empty_validation_results())
        .unwrap();
    assert!(exec.iterations()[0].validation_results.is_some());
}

// ============================================================================
// 10. store_iteration_trajectory()
// ============================================================================

#[test]
fn store_trajectory_attaches_to_correct_iteration() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    let steps = vec![
        TrajectoryStep {
            tool_name: "write_file".to_string(),
            arguments_json: r#"{"path":"main.py"}"#.to_string(),
            status: "success".to_string(),
            result_json: Some(r#"{"ok":true}"#.to_string()),
            error: None,
        },
        TrajectoryStep {
            tool_name: "run_tests".to_string(),
            arguments_json: "{}".to_string(),
            status: "failed".to_string(),
            result_json: None,
            error: Some("test failure".to_string()),
        },
    ];

    exec.store_iteration_trajectory(1, steps).unwrap();
    let traj = exec.iterations()[0].trajectory.as_ref().unwrap();
    assert_eq!(traj.len(), 2);
    assert_eq!(traj[0].tool_name, "write_file");
    assert_eq!(traj[1].tool_name, "run_tests");
}

#[test]
fn store_trajectory_nonexistent_iteration_returns_error() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    let err = exec.store_iteration_trajectory(42, vec![]).unwrap_err();
    assert!(matches!(err, ExecutionError::IterationNotFound(42)));
}

// ============================================================================
// 11. add_llm_interaction()
// ============================================================================

#[test]
fn add_llm_interaction_appends_to_iteration() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    exec.add_llm_interaction(1, make_llm_interaction("openai", "gpt-4o"))
        .unwrap();
    exec.add_llm_interaction(1, make_llm_interaction("anthropic", "claude-4"))
        .unwrap();

    assert_eq!(exec.iterations()[0].llm_interactions.len(), 2);
    assert_eq!(exec.iterations()[0].llm_interactions[0].provider, "openai");
    assert_eq!(
        exec.iterations()[0].llm_interactions[1].provider,
        "anthropic"
    );
}

#[test]
fn add_llm_interaction_wrong_iteration_returns_error() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    let err = exec
        .add_llm_interaction(77, make_llm_interaction("openai", "gpt-4o"))
        .unwrap_err();
    assert!(matches!(err, ExecutionError::IterationNotFound(77)));
}

#[test]
fn llm_interaction_preserves_all_fields() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();

    let ts = Utc::now();
    let interaction = LlmInteraction {
        provider: "anthropic".to_string(),
        model: "claude-4-opus".to_string(),
        prompt: "solve P=NP".to_string(),
        response: "here is a proof...".to_string(),
        timestamp: ts,
    };
    exec.add_llm_interaction(1, interaction).unwrap();

    let stored = &exec.iterations()[0].llm_interactions[0];
    assert_eq!(stored.provider, "anthropic");
    assert_eq!(stored.model, "claude-4-opus");
    assert_eq!(stored.prompt, "solve P=NP");
    assert_eq!(stored.response, "here is a proof...");
    assert_eq!(stored.timestamp, ts);
}

// ============================================================================
// 12. ExecutionInput — construction
// ============================================================================

#[test]
fn execution_input_with_intent_and_empty_payload() {
    let input = make_input("deploy to prod");
    assert_eq!(input.intent.as_deref(), Some("deploy to prod"));
    assert_eq!(input.payload, serde_json::json!({}));
}

#[test]
fn execution_input_with_rich_payload() {
    let payload = serde_json::json!({
        "language": "python",
        "files": ["main.py", "test_main.py"]
    });
    let input = make_input_with_payload("generate code", payload.clone());
    assert_eq!(input.intent.as_deref(), Some("generate code"));
    assert_eq!(input.payload, payload);
}

#[test]
fn execution_input_without_intent() {
    let input = ExecutionInput {
        intent: None,
        payload: serde_json::json!({"raw": true}),
    };
    assert!(input.intent.is_none());
}

// ============================================================================
// 13. ExecutionInfo — summary extraction
// ============================================================================

#[test]
fn execution_info_from_completed_execution() {
    let agent_id = AgentId::new();
    let mut exec = Execution::new(
        agent_id,
        make_input("task"),
        5,
        "aegis-system-operator".to_string(),
    );
    exec.start();
    exec.complete();

    let info = ExecutionInfo::from(exec.clone());
    assert_eq!(info.id, exec.id);
    assert_eq!(info.agent_id, agent_id);
    assert_eq!(info.status, ExecutionStatus::Completed);
    assert!(info.ended_at.is_some());
    assert!(info.error.is_none());
}

#[test]
fn execution_info_from_failed_execution() {
    let mut exec = make_execution(5);
    exec.start();
    exec.fail("kaboom".to_string());

    let info = ExecutionInfo::from(exec.clone());
    assert_eq!(info.status, ExecutionStatus::Failed);
    assert_eq!(info.error.as_deref(), Some("kaboom"));
    assert!(info.ended_at.is_some());
}

#[test]
fn execution_info_from_pending_execution() {
    let exec = make_execution(5);
    let info = ExecutionInfo::from(exec.clone());
    assert_eq!(info.status, ExecutionStatus::Pending);
    assert!(info.ended_at.is_none());
    assert!(info.error.is_none());
}

#[test]
fn execution_info_preserves_tenant_id() {
    let exec = make_execution(5);
    let info = ExecutionInfo::from(exec.clone());
    assert_eq!(info.tenant_id, exec.tenant_id);
}

// ============================================================================
// 14. container_uid/gid defaults
// ============================================================================

#[test]
fn default_container_uid_is_1000() {
    let exec = make_execution(5);
    assert_eq!(exec.container_uid, 1000);
}

#[test]
fn default_container_gid_is_1000() {
    let exec = make_execution(5);
    assert_eq!(exec.container_gid, 1000);
}

#[test]
fn child_execution_has_default_uid_gid() {
    let parent = make_execution(5);
    let child = Execution::new_child(AgentId::new(), make_input("child"), 3, &parent).unwrap();
    assert_eq!(child.container_uid, 1000);
    assert_eq!(child.container_gid, 1000);
}

// ============================================================================
// 15. Value object construction and serialization
// ============================================================================

#[test]
fn llm_interaction_serializes_roundtrip() {
    let interaction = LlmInteraction {
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        prompt: "hello".to_string(),
        response: "world".to_string(),
        timestamp: Utc::now(),
    };
    let json = serde_json::to_string(&interaction).unwrap();
    let deserialized: LlmInteraction = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.provider, "openai");
    assert_eq!(deserialized.model, "gpt-4o");
    assert_eq!(deserialized.prompt, "hello");
    assert_eq!(deserialized.response, "world");
}

#[test]
fn trajectory_step_serializes_roundtrip() {
    let step = TrajectoryStep {
        tool_name: "exec_command".to_string(),
        arguments_json: r#"{"cmd":"ls"}"#.to_string(),
        status: "success".to_string(),
        result_json: Some(r#"{"files":["a.txt"]}"#.to_string()),
        error: None,
    };
    let json = serde_json::to_string(&step).unwrap();
    let deserialized: TrajectoryStep = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.tool_name, "exec_command");
    assert_eq!(deserialized.status, "success");
    assert!(deserialized.result_json.is_some());
    assert!(deserialized.error.is_none());
}

#[test]
fn trajectory_step_with_error_serializes() {
    let step = TrajectoryStep {
        tool_name: "run_tests".to_string(),
        arguments_json: "{}".to_string(),
        status: "failed".to_string(),
        result_json: None,
        error: Some("exit code 1".to_string()),
    };
    let json = serde_json::to_string(&step).unwrap();
    // result_json is skip_serializing_if None
    assert!(!json.contains("result_json"));
    assert!(json.contains("exit code 1"));
}

#[test]
fn code_diff_construction_and_serialization() {
    let diff = CodeDiff {
        file_path: "src/main.rs".to_string(),
        diff: "+fn main() { println!(\"hello\"); }".to_string(),
    };
    let json = serde_json::to_string(&diff).unwrap();
    let deserialized: CodeDiff = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.file_path, "src/main.rs");
    assert!(deserialized.diff.contains("fn main"));
}

#[test]
fn iteration_error_construction() {
    let err = IterationError {
        message: "segfault".to_string(),
        details: Some("address 0x0".to_string()),
    };
    assert_eq!(err.message, "segfault");
    assert_eq!(err.details.as_deref(), Some("address 0x0"));
}

#[test]
fn iteration_error_without_details() {
    let err = IterationError {
        message: "unknown error".to_string(),
        details: None,
    };
    assert_eq!(err.message, "unknown error");
    assert!(err.details.is_none());
}

#[test]
fn execution_status_serializes_roundtrip() {
    for status in [
        ExecutionStatus::Pending,
        ExecutionStatus::Running,
        ExecutionStatus::Completed,
        ExecutionStatus::Failed,
        ExecutionStatus::Cancelled,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: ExecutionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, status);
    }
}

#[test]
fn iteration_status_serializes_roundtrip() {
    for status in [
        IterationStatus::Running,
        IterationStatus::Success,
        IterationStatus::Failed,
        IterationStatus::Refining,
    ] {
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: IterationStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, status);
    }
}

#[test]
fn execution_full_serialization_roundtrip() {
    let mut exec = make_running_execution(5);
    exec.start_iteration("gen".to_string()).unwrap();
    exec.complete_iteration("output".to_string());
    exec.complete();

    let json = serde_json::to_string(&exec).unwrap();
    let deserialized: Execution = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.id, exec.id);
    assert_eq!(deserialized.status, ExecutionStatus::Completed);
    assert_eq!(deserialized.iterations.len(), 1);
    assert_eq!(deserialized.container_uid, 1000);
    assert_eq!(deserialized.container_gid, 1000);
}

#[test]
fn execution_hierarchy_serializes_roundtrip() {
    let root_id = ExecutionId::new();
    let root_h = ExecutionHierarchy::root(root_id);
    let child_id = ExecutionId::new();
    let child_h = ExecutionHierarchy::child(&root_h, child_id).unwrap();

    let json = serde_json::to_string(&child_h).unwrap();
    let deserialized: ExecutionHierarchy = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.depth, 1);
    assert_eq!(deserialized.parent_execution_id, Some(root_id));
    assert_eq!(deserialized.path.len(), 2);
}

// ============================================================================
// ExecutionId — identity type tests
// ============================================================================

#[test]
fn execution_id_new_generates_unique_ids() {
    let ids: Vec<ExecutionId> = (0..100).map(|_| ExecutionId::new()).collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 100);
}

#[test]
fn execution_id_display_matches_inner_uuid() {
    let id = ExecutionId::new();
    assert_eq!(format!("{id}"), id.0.to_string());
}

#[test]
fn execution_id_from_string_roundtrip() {
    let original = ExecutionId::new();
    let s = original.0.to_string();
    let parsed = ExecutionId::from_string(&s).unwrap();
    assert_eq!(parsed, original);
}

#[test]
fn execution_id_from_string_invalid_returns_error() {
    let result = ExecutionId::from_string("not-a-uuid");
    assert!(result.is_err());
}

#[test]
fn execution_id_clone_equality() {
    let id = ExecutionId::new();
    let cloned = id;
    assert_eq!(id, cloned);
}
