// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # ADR-016: Agent-as-Judge Recursion Tests
//!
//! Validates the Meta-Monkey recursion depth enforcement specified in ADR-016.
//! These tests ensure that:
//! 1. The `MAX_RECURSIVE_DEPTH` boundary is enforced (judges cannot nest beyond depth 3)
//! 2. The full judge-of-judges hierarchy behaves correctly up to the maximum depth

use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionHierarchy, ExecutionId, ExecutionInput, MAX_RECURSIVE_DEPTH,
};

fn make_input(intent: &str) -> ExecutionInput {
    ExecutionInput {
        intent: Some(intent.to_string()),
        payload: serde_json::json!({}),
    }
}

/// Verifies that `ExecutionHierarchy::child` rejects a depth exceeding `MAX_RECURSIVE_DEPTH`.
#[test]
fn test_max_recursive_depth_boundary_enforced() {
    let root_id = ExecutionId::new();
    let root = ExecutionHierarchy::root(root_id);

    let id1 = ExecutionId::new();
    let depth1 = ExecutionHierarchy::child(&root, id1).expect("depth 1 should be allowed");
    assert_eq!(depth1.depth, 1);
    assert!(depth1.can_spawn_child());

    let id2 = ExecutionId::new();
    let depth2 = ExecutionHierarchy::child(&depth1, id2).expect("depth 2 should be allowed");
    assert_eq!(depth2.depth, 2);
    assert!(depth2.can_spawn_child());

    let id3 = ExecutionId::new();
    let depth3 = ExecutionHierarchy::child(&depth2, id3).expect("depth 3 should be allowed");
    assert_eq!(depth3.depth, 3);
    assert_eq!(depth3.depth, MAX_RECURSIVE_DEPTH);
    // At max depth no further children may be spawned.
    assert!(!depth3.can_spawn_child());

    // Attempting to create a depth-4 hierarchy must return an error.
    let id4 = ExecutionId::new();
    let result = ExecutionHierarchy::child(&depth3, id4);
    assert!(result.is_err(), "depth 4 must be rejected");
    let msg = result.unwrap_err();
    assert!(
        msg.contains(&MAX_RECURSIVE_DEPTH.to_string()),
        "error message should reference the depth limit; got: {msg}"
    );
}

/// Verifies `can_spawn_child` returns the correct boolean at every depth level.
#[test]
fn test_can_spawn_child_at_each_depth() {
    let root_id = ExecutionId::new();
    let root = ExecutionHierarchy::root(root_id);
    assert!(root.can_spawn_child(), "depth 0 should be able to spawn");

    let id1 = ExecutionId::new();
    let depth1 = ExecutionHierarchy::child(&root, id1).unwrap();
    assert!(depth1.can_spawn_child(), "depth 1 should be able to spawn");

    let id2 = ExecutionId::new();
    let depth2 = ExecutionHierarchy::child(&depth1, id2).unwrap();
    assert!(depth2.can_spawn_child(), "depth 2 should be able to spawn");

    let id3 = ExecutionId::new();
    let depth3 = ExecutionHierarchy::child(&depth2, id3).unwrap();
    assert!(
        !depth3.can_spawn_child(),
        "depth 3 (= MAX_RECURSIVE_DEPTH) must not be able to spawn"
    );
}

/// Simulates the Meta-Monkey judge-of-judges hierarchy described in ADR-016:
///
/// ```text
/// primary execution (depth 0)
///   └─ L1 judge         (depth 1) – evaluates primary output
///       └─ L2 meta-judge (depth 2) – evaluates the L1 judge
///           └─ L3 meta-meta-judge (depth 3) – evaluates the L2 meta-judge
/// ```
///
/// Verifies that parent linkage, root tracking, and path construction are
/// correct at every level of the hierarchy.
#[test]
fn test_meta_monkey_judge_hierarchy_chain() {
    // Primary execution (root, depth 0).
    let root_exec = Execution::new(AgentId::new(), make_input("primary task"), 5);
    let root_id = root_exec.id;
    assert_eq!(root_exec.depth(), 0);
    assert!(root_exec.parent_id().is_none());
    assert!(root_exec.can_spawn_child());

    // L1 judge spawned by the primary execution.
    let l1_exec = Execution::new_child(AgentId::new(), make_input("judge L1"), 1, &root_exec)
        .expect("L1 judge should be spawnable from depth 0");
    let l1_id = l1_exec.id;
    assert_eq!(l1_exec.depth(), 1);
    assert_eq!(l1_exec.parent_id(), Some(root_id));
    assert_eq!(l1_exec.hierarchy.root_id(), root_id);
    assert!(l1_exec.can_spawn_child());

    // L2 meta-judge spawned by the L1 judge.
    let l2_exec = Execution::new_child(AgentId::new(), make_input("meta-judge L2"), 1, &l1_exec)
        .expect("L2 meta-judge should be spawnable from depth 1");
    let l2_id = l2_exec.id;
    assert_eq!(l2_exec.depth(), 2);
    assert_eq!(l2_exec.parent_id(), Some(l1_id));
    assert_eq!(l2_exec.hierarchy.root_id(), root_id);
    assert!(l2_exec.can_spawn_child());

    // L3 meta-meta-judge spawned by the L2 judge (maximum allowed depth).
    let l3_exec = Execution::new_child(
        AgentId::new(),
        make_input("meta-meta-judge L3"),
        1,
        &l2_exec,
    )
    .expect("L3 meta-meta-judge should be spawnable from depth 2");
    let l3_id = l3_exec.id;
    assert_eq!(l3_exec.depth(), 3);
    assert_eq!(l3_exec.depth(), MAX_RECURSIVE_DEPTH);
    assert_eq!(l3_exec.parent_id(), Some(l2_id));
    assert_eq!(l3_exec.hierarchy.root_id(), root_id);
    // At max depth a judge cannot recursively spawn further judges.
    assert!(!l3_exec.can_spawn_child());

    // The full ancestry path is preserved in order.
    assert_eq!(l3_exec.hierarchy.path.len(), 4);
    assert_eq!(l3_exec.hierarchy.path[0], root_id);
    assert_eq!(l3_exec.hierarchy.path[1], l1_id);
    assert_eq!(l3_exec.hierarchy.path[2], l2_id);
    assert_eq!(l3_exec.hierarchy.path[3], l3_id);
}

/// Verifies that `Execution::new_child` returns an error when called on an execution
/// that has already reached `MAX_RECURSIVE_DEPTH`, ensuring the domain-level guard
/// propagates the hierarchy enforcement correctly.
#[test]
fn test_new_child_at_max_depth_returns_error() {
    let root = Execution::new(AgentId::new(), make_input("root"), 5);
    let l1 = Execution::new_child(AgentId::new(), make_input("l1"), 1, &root).unwrap();
    let l2 = Execution::new_child(AgentId::new(), make_input("l2"), 1, &l1).unwrap();
    let l3 = Execution::new_child(AgentId::new(), make_input("l3"), 1, &l2).unwrap();

    let result = Execution::new_child(AgentId::new(), make_input("l4-too-deep"), 1, &l3);
    assert!(result.is_err(), "spawning a child at depth 4 must fail");
}
