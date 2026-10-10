// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The `Action` state kind (AEGIS ADR-142 A1, A2, A6 and A7's mapping).
//!
//! A workflow's own deterministic operations are written `kind: Action` with a
//! literal `action` and a mapping of `args` templates. These tests drive the
//! manifest through the parser, back to YAML, and through the temporal mapper,
//! and pin the refusals' sentences.

use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowMapper;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{StateName, Workflow};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

/// A one-state manifest whose state `STEP` is `kind: Action` with `action`
/// and the `args` lines given, and `spec.repositories` when `repositories` is
/// set.
fn manifest(action: &str, args: &[(&str, &str)], repositories: Option<u32>) -> String {
    let mut yaml = String::from(
        "apiVersion: 100monkeys.ai/v1\nkind: Workflow\nmetadata:\n  name: action-kind\n  version: \"1.0.0\"\nspec:\n",
    );
    if let Some(n) = repositories {
        yaml.push_str(&format!("  repositories: {n}\n"));
    }
    yaml.push_str("  initial_state: STEP\n  states:\n    STEP:\n      kind: Action\n");
    yaml.push_str(&format!("      action: {action}\n"));
    if !args.is_empty() {
        yaml.push_str("      args:\n");
        for (key, value) in args {
            yaml.push_str(&format!("        {key}: \"{value}\"\n"));
        }
    }
    yaml.push_str("      transitions: []\n");
    yaml
}

fn parse(yaml: &str) -> Workflow {
    WorkflowParser::parse_yaml(yaml).unwrap_or_else(|e| panic!("the manifest parses: {e}"))
}

fn refusal(yaml: &str) -> String {
    match WorkflowParser::parse_yaml(yaml) {
        Ok(_) => panic!("the manifest is refused"),
        Err(e) => e.to_string(),
    }
}

/// The state `STEP` of a parsed workflow, as the domain serialises it.
fn step_kind(workflow: &Workflow) -> serde_json::Value {
    let state = workflow
        .spec
        .states
        .get(&StateName::new("STEP").unwrap())
        .expect("the workflow has its STEP state");
    serde_json::to_value(&state.kind).expect("the state kind serialises")
}

/// Each of the four actions, with the arguments a valid manifest gives it and
/// whether it needs the run's repository.
fn the_four() -> Vec<(&'static str, Vec<(&'static str, &'static str)>, Option<u32>)> {
    vec![
        (
            "update_blackboard",
            vec![
                ("verdict", "{{AUDIT.output}}"),
                ("landed", "{{LAND.output.commit_sha}}"),
            ],
            None,
        ),
        ("repository_diff", vec![], Some(1)),
        (
            "repository_commit",
            vec![("message", "the-forge: {{{first_line input.task}}}")],
            Some(1),
        ),
        ("repository_land", vec![], Some(1)),
    ]
}

#[test]
fn each_action_parses_to_the_action_kind_and_round_trips() {
    for (action, args, repositories) in the_four() {
        let workflow = parse(&manifest(action, &args, repositories));
        let kind = step_kind(&workflow);
        assert_eq!(kind["kind"], "Action", "{action}: {kind}");
        assert_eq!(kind["action"], action, "{action}: {kind}");
        for (key, value) in &args {
            assert_eq!(kind["args"][key], *value, "{action}: {kind}");
        }
        assert_eq!(
            kind["args"].as_object().map(|m| m.len()).unwrap_or(0),
            args.len(),
            "{action}: {kind}"
        );

        let yaml = WorkflowParser::to_yaml(&workflow).expect("the workflow serialises to YAML");
        assert!(yaml.contains("kind: Action"), "{action}: {yaml}");
        let again = parse(&yaml);
        assert_eq!(step_kind(&again), kind, "{action} round-trips: {yaml}");
        assert_eq!(again.spec.repositories, workflow.spec.repositories);
    }
}

#[test]
fn an_unknown_action_is_refused_with_the_list_of_actions() {
    let sentence = refusal(&manifest("repository_push", &[], Some(1)));
    assert!(
        sentence.contains(
            "unknown action 'repository_push'; the actions are update_blackboard, repository_diff, repository_commit, repository_land"
        ),
        "{sentence}"
    );
}

#[test]
fn an_action_is_a_literal_never_a_template() {
    let sentence = refusal(&manifest("\"{{input.action}}\"", &[], Some(1)));
    assert!(
        sentence.contains("unknown action '{{input.action}}'; the actions are"),
        "{sentence}"
    );
}

#[test]
fn an_undeclared_argument_is_refused() {
    let sentence = refusal(&manifest(
        "repository_commit",
        &[("message", "m"), ("author", "someone")],
        Some(1),
    ));
    assert!(
        sentence.contains("action 'repository_commit' takes message; 'author' is not one"),
        "{sentence}"
    );

    let sentence = refusal(&manifest("repository_land", &[("branch", "main")], Some(1)));
    assert!(
        sentence.contains("action 'repository_land' takes no arguments; 'branch' is not one"),
        "{sentence}"
    );
}

#[test]
fn a_missing_required_argument_is_refused() {
    let sentence = refusal(&manifest("repository_commit", &[], Some(1)));
    assert!(
        sentence.contains("action 'repository_commit' needs the argument 'message'"),
        "{sentence}"
    );
}

#[test]
fn a_repository_action_in_a_workflow_without_repositories_is_refused() {
    for repositories in [None, Some(0)] {
        for (action, args) in [
            ("repository_diff", vec![]),
            ("repository_commit", vec![("message", "m")]),
            ("repository_land", vec![]),
        ] {
            let sentence = refusal(&manifest(action, &args, repositories));
            assert!(
                sentence.contains(&format!(
                    "action '{action}' works on the run's repository; this workflow declares none"
                )),
                "{action} with repositories {repositories:?}: {sentence}"
            );
        }
    }
    // update_blackboard needs nothing: it parses without repositories.
    parse(&manifest("update_blackboard", &[("k", "v")], None));
}

#[test]
fn the_mapper_sends_an_action_state_as_kind_action_with_its_action_and_args() {
    let workflow = parse(&manifest(
        "repository_commit",
        &[("message", "the-forge: {{{first_line input.task}}}")],
        Some(1),
    ));
    let definition =
        TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::consumer())
            .expect("the workflow maps");
    let state = serde_json::to_value(&definition.states["STEP"]).expect("the state serialises");
    assert_eq!(state["kind"], "Action", "{state}");
    assert_eq!(state["action"], "repository_commit", "{state}");
    assert_eq!(
        state["args"],
        serde_json::json!({"message": "the-forge: {{{first_line input.task}}}"}),
        "{state}"
    );
    assert!(state.get("command").is_none(), "{state}");
    assert!(state.get("env").is_none(), "{state}");
}

#[test]
fn the_mapper_validates_the_args_templates() {
    let workflow = parse(&manifest(
        "update_blackboard",
        &[("verdict", "{{#if AUDIT.output}}open")],
        None,
    ));
    let error = TemporalWorkflowMapper::validate_templates(&workflow)
        .expect_err("an unclosed block in an args template is refused");
    assert!(
        format!("{error:#}").contains("Invalid template in state STEP args verdict"),
        "{error:#}"
    );
}
