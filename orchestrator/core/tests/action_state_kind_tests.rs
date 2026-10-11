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

/// A one-state manifest whose state `STEP` is `kind: System` with `command`
/// written as a quoted YAML string.
fn system_manifest(command: &str) -> String {
    format!(
        "apiVersion: 100monkeys.ai/v1\nkind: Workflow\nmetadata:\n  name: system-kind\n  version: \"1.0.0\"\nspec:\n  repositories: 1\n  initial_state: STEP\n  states:\n    STEP:\n      kind: System\n      command: \"{command}\"\n      transitions: []\n"
    )
}

#[test]
fn a_system_state_written_as_an_action_is_refused() {
    for name in [
        "update_blackboard",
        "repository_diff",
        "repository_commit",
        "repository_land",
    ] {
        let sentence = refusal(&system_manifest(name));
        assert!(
            sentence.contains(&format!(
                "a System state runs a shell command; '{name}' is an action: write kind: Action, action: {name}"
            )),
            "{name}: {sentence}"
        );
    }
}

#[test]
fn a_system_state_written_as_an_action_with_surrounding_whitespace_is_refused() {
    let sentence = refusal(&system_manifest("  repository_land \\n"));
    assert!(
        sentence.contains(
            "a System state runs a shell command; 'repository_land' is an action: write kind: Action, action: repository_land"
        ),
        "{sentence}"
    );
}

#[test]
fn a_system_command_that_is_a_template_is_not_refused_at_parse() {
    // The rendered form is the worker's to refuse.
    let workflow = parse(&system_manifest("update_blackboard {{ x }}"));
    let kind = step_kind(&workflow);
    assert_eq!(kind["kind"], "System", "{kind}");
    assert_eq!(kind["command"], "update_blackboard {{ x }}", "{kind}");
}

#[test]
fn an_ordinary_shell_command_still_parses() {
    let workflow = parse(&system_manifest(
        "cargo test --workspace && echo update_blackboard",
    ));
    let kind = step_kind(&workflow);
    assert_eq!(kind["kind"], "System", "{kind}");
    assert_eq!(
        kind["command"], "cargo test --workspace && echo update_blackboard",
        "{kind}"
    );
}

#[test]
fn every_built_in_workflow_still_parses() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/templates/workflows");
    let mut parsed = 0;
    for entry in std::fs::read_dir(&dir).expect("the built-in workflows directory reads") {
        let path = entry.expect("a directory entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let yaml = std::fs::read_to_string(&path).expect("the built-in reads");
        WorkflowParser::parse_yaml(&yaml)
            .unwrap_or_else(|e| panic!("{} parses: {e}", path.display()));
        parsed += 1;
    }
    assert!(parsed >= 5, "the built-ins were found: {parsed}");
}

// The action `image_generate` (AEGIS ADR-143 I5): a row of the actions'
// table whose arguments are `prompt` (required), `alias`, `size`, `count` and
// `path`, which needs an image generation model on the node and touches no
// person's world. Whether the node has one is the registration's to refuse;
// the parse reads the manifest only.

/// `image_generate` with every argument it takes.
fn image_generate_args() -> Vec<(&'static str, &'static str)> {
    vec![
        ("prompt", "a lighthouse at dusk, {{input.style}}"),
        ("alias", "artist"),
        ("size", "1024x1024"),
        ("count", "{{input.count}}"),
        ("path", "generated/covers/"),
    ]
}

#[test]
fn image_generate_parses_with_each_argument_and_round_trips() {
    // Every argument, then the prompt alone, then the prompt with each
    // optional argument on its own; none needs `spec.repositories`.
    let every = image_generate_args();
    let mut cases = vec![every.clone(), vec![every[0]]];
    for optional in &every[1..] {
        cases.push(vec![every[0], *optional]);
    }
    for args in cases {
        let workflow = parse(&manifest("image_generate", &args, None));
        let kind = step_kind(&workflow);
        assert_eq!(kind["kind"], "Action", "{args:?}: {kind}");
        assert_eq!(kind["action"], "image_generate", "{args:?}: {kind}");
        for (key, value) in &args {
            assert_eq!(kind["args"][key], *value, "{args:?}: {kind}");
        }
        assert_eq!(
            kind["args"].as_object().map(|m| m.len()).unwrap_or(0),
            args.len(),
            "{args:?}: {kind}"
        );

        let yaml = WorkflowParser::to_yaml(&workflow).expect("the workflow serialises to YAML");
        assert!(yaml.contains("action: image_generate"), "{yaml}");
        let again = parse(&yaml);
        assert_eq!(step_kind(&again), kind, "{args:?} round-trips: {yaml}");
    }
}

#[test]
fn image_generate_without_a_prompt_is_refused() {
    for args in [vec![], vec![("alias", "artist"), ("count", "2")]] {
        let sentence = refusal(&manifest("image_generate", &args, None));
        assert!(
            sentence.contains("action 'image_generate' needs the argument 'prompt'"),
            "{args:?}: {sentence}"
        );
    }
}

#[test]
fn image_generate_refuses_an_undeclared_argument() {
    let sentence = refusal(&manifest(
        "image_generate",
        &[("prompt", "a lighthouse"), ("style", "oil")],
        None,
    ));
    assert!(
        sentence.contains(
            "action 'image_generate' takes prompt, alias, size, count, path; 'style' is not one"
        ),
        "{sentence}"
    );
}

#[test]
fn image_generate_is_in_the_list_of_actions() {
    let sentence = refusal(&manifest("image_edit", &[], None));
    assert!(
        sentence.contains(
            "unknown action 'image_edit'; the actions are update_blackboard, repository_diff, repository_commit, repository_land, image_generate"
        ),
        "{sentence}"
    );
}

#[test]
fn the_mapper_sends_image_generate_as_kind_action_with_its_args_templates() {
    let args = image_generate_args();
    let workflow = parse(&manifest("image_generate", &args, None));
    let definition =
        TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::consumer())
            .expect("the workflow maps");
    let state = serde_json::to_value(&definition.states["STEP"]).expect("the state serialises");
    assert_eq!(state["kind"], "Action", "{state}");
    assert_eq!(state["action"], "image_generate", "{state}");
    let expected: serde_json::Map<String, serde_json::Value> = args
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::from(*v)))
        .collect();
    assert_eq!(
        state["args"],
        serde_json::Value::Object(expected),
        "{state}"
    );
    assert!(state.get("command").is_none(), "{state}");

    TemporalWorkflowMapper::validate_templates(&workflow).expect("the args templates are valid");
    let broken = parse(&manifest(
        "image_generate",
        &[("prompt", "{{#if input.style}}a lighthouse")],
        None,
    ));
    let error = TemporalWorkflowMapper::validate_templates(&broken)
        .expect_err("an unclosed block in the prompt template is refused");
    assert!(
        format!("{error:#}").contains("Invalid template in state STEP args prompt"),
        "{error:#}"
    );
}
