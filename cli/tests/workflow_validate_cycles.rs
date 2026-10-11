// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis workflow validate` accepts a manifest whose states loop.
//!
//! The engine bounds every loop: each state is entered at most its
//! `max_state_visits` times (5 when the manifest sets none, never over 20),
//! and a run takes at most its `max_total_transitions` (50 by default). A
//! cycle is therefore not a validation error, as the CLI's deploy of the
//! built-ins already holds.

use std::path::Path;
use std::process::Command;

/// A loop whose refining state carries its own visit limit, the shape of
/// `aegis-examples` `100monkeys-classic.yaml` (GENERATE and REFINE each
/// `max_state_visits: 10`).
const BOUNDED_BY_STATE: &str = r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: bounded-by-state
  version: "1.0.0"
spec:
  initial_state: GENERATE
  states:
    GENERATE:
      kind: Agent
      agent: coder-v1
      input: "{{input}}"
      transitions:
        - condition: always
          target: VALIDATE
    VALIDATE:
      kind: System
      command: check
      transitions:
        - condition: exit_code_zero
          target: COMPLETE
        - condition: exit_code_non_zero
          target: REFINE
    REFINE:
      kind: Agent
      agent: coder-v1
      input: "{{input}}"
      max_state_visits: 10
      transitions:
        - condition: always
          target: GENERATE
    COMPLETE:
      kind: System
      command: finalize
      transitions: []
"#;

/// A loop with no `max_state_visits` on any of its states: the engine's
/// default visit limit bounds it.
const BOUNDED_BY_DEFAULT: &str = r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: bounded-by-default
  version: "1.0.0"
spec:
  initial_state: CODE
  states:
    CODE:
      kind: Agent
      agent: coder-v1
      input: "{{input}}"
      transitions:
        - condition: always
          target: TEST
    TEST:
      kind: System
      command: check
      transitions:
        - condition: exit_code_zero
          target: COMPLETE
        - condition: exit_code_non_zero
          target: CODE
    COMPLETE:
      kind: System
      command: finalize
      transitions: []
"#;

/// Run `aegis workflow validate <path>` from this build and return its exit
/// status with stdout and stderr together.
fn validate(path: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_aegis"))
        .args(["workflow", "validate"])
        .arg(path)
        .env("NO_COLOR", "1")
        .output()
        .expect("run aegis workflow validate");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn validate_manifest(name: &str, yaml: &str) -> (bool, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    std::fs::write(&path, yaml).expect("write manifest");
    validate(&path)
}

#[test]
fn validate_accepts_a_loop_bounded_by_a_states_visit_limit() {
    let (ok, text) = validate_manifest("bounded-by-state.yaml", BOUNDED_BY_STATE);
    assert!(ok, "validate refused a bounded loop:\n{text}");
    assert!(text.contains("Workflow is valid"), "{text}");
}

#[test]
fn validate_accepts_a_loop_bounded_by_the_engines_default_visit_limit() {
    let (ok, text) = validate_manifest("bounded-by-default.yaml", BOUNDED_BY_DEFAULT);
    assert!(ok, "validate refused a loop the default bounds:\n{text}");
    assert!(text.contains("Workflow is valid"), "{text}");
}

#[test]
fn validate_accepts_the_forge_built_in() {
    let forge = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/workflows/the-forge.yaml");
    let (ok, text) = validate(&forge);
    assert!(ok, "validate refused the Forge:\n{text}");
    assert!(text.contains("Workflow is valid"), "{text}");
}
