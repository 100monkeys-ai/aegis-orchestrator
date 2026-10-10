// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The Forge as a built-in (AEGIS ADR-141 F1, F2, F4, F9, F10; Trigger T4).
//!
//! `cli/templates/workflows/the-forge.yaml` and its seven agents are listed in
//! `BUILTIN_WORKFLOWS` and `BUILTIN_AGENTS`, parse through the orchestrator's
//! own parsers, and hold F4's table of states and F10's tool lists.

use aegis_orchestrator::commands::builtins::{
    deploy_all_builtins, BUILTIN_AGENTS, BUILTIN_WORKFLOWS,
};
use aegis_orchestrator::daemon::DaemonClient;
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowMapper;
use aegis_orchestrator_core::domain::agent::ValidatorSpec;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    ConsensusStrategy, StateKind, TransitionRule, Workflow, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use std::collections::BTreeMap;

const FORGE: &str = "the-forge";

/// F10's seven agents, in the record's order.
const FORGE_AGENTS: [&str; 7] = [
    "requirements-analyst",
    "architect-agent",
    "tester-agent",
    "coder-agent",
    "code-reviewer-agent",
    "critic-agent",
    "security-auditor-agent",
];

/// The reading `fs.*` tools (`infrastructure/tool_router.rs`, the builtin
/// definitions marked `skip_judge`).
const READING_FS: [&str; 4] = ["fs.read", "fs.list", "fs.grep", "fs.glob"];

/// The writing `fs.*` tools (`domain/tool_requirement.rs` `WRITING_FS_TOOLS`).
const WRITING_FS: [&str; 5] = [
    "fs.write",
    "fs.edit",
    "fs.multi_edit",
    "fs.create_dir",
    "fs.delete",
];

/// The git tools no Forge agent may name: commit and landing are the
/// workflow's own steps.
const GIT_WRITE_TOOLS: [&str; 3] = ["aegis.git.commit", "aegis.git.push", "aegis.git.land"];

/// The commands a System state of the Forge may run: each is the workflow
/// interpreter's own, and none is a shell command in the core process.
const INTERPRETER_COMMANDS: [&str; 4] = [
    "repository_diff",
    "repository_commit",
    "repository_land",
    "update_blackboard",
];

fn listed<'a>(list: &'a [(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    list.iter().find(|(n, _)| *n == name).map(|(_, yaml)| *yaml)
}

fn on_disk(relative: &str) -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("templates")
            .join(relative),
    )
    .unwrap_or_default()
}

fn forge() -> Workflow {
    let yaml = listed(BUILTIN_WORKFLOWS, FORGE).expect("the-forge is in BUILTIN_WORKFLOWS");
    WorkflowParser::parse_yaml(yaml)
        .unwrap_or_else(|e| panic!("the orchestrator's parser refuses the-forge: {e}"))
}

fn state<'a>(workflow: &'a Workflow, name: &str) -> &'a WorkflowState {
    workflow
        .spec
        .states
        .iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, s)| s)
        .unwrap_or_else(|| panic!("the-forge has no state {name}"))
}

fn kind(state: &WorkflowState) -> String {
    match &state.kind {
        StateKind::Agent { agent, .. } => format!("Agent {agent}"),
        StateKind::System { command, .. } => format!("System {command}"),
        StateKind::Human { .. } => "Human".to_string(),
        StateKind::ParallelAgents { .. } => "ParallelAgents".to_string(),
        StateKind::ContainerRun { .. } => "ContainerRun".to_string(),
        StateKind::ParallelContainerRun { .. } => "ParallelContainerRun".to_string(),
        StateKind::Subworkflow { .. } => "Subworkflow".to_string(),
    }
}

/// A transition as `<condition json> -> <target>`, in order.
fn routes(state: &WorkflowState) -> Vec<String> {
    state
        .transitions
        .iter()
        .map(|t: &TransitionRule| {
            format!(
                "{} -> {}",
                serde_json::to_value(&t.condition).unwrap(),
                t.target.as_str()
            )
        })
        .collect()
}

fn feedback_of(state: &WorkflowState, target: &str) -> String {
    state
        .transitions
        .iter()
        .find(|t| t.target.as_str() == target)
        .and_then(|t| t.feedback.clone())
        .unwrap_or_default()
}

/// F1, F2, F9, F10: the template and its seven agents are listed, are the files
/// under `cli/templates/`, parse through the orchestrator's parsers, map to the
/// Temporal definition the startup deploy registers, and carry F1's labels and
/// F2's inputs.
#[test]
fn the_forge_and_its_seven_agents_are_listed_and_parse() {
    let mut complaints = Vec::new();

    match listed(BUILTIN_WORKFLOWS, FORGE) {
        None => complaints.push("the-forge is not in BUILTIN_WORKFLOWS".to_string()),
        Some(yaml) => {
            if yaml != on_disk("workflows/the-forge.yaml") {
                complaints.push(
                    "the listed the-forge is not cli/templates/workflows/the-forge.yaml"
                        .to_string(),
                );
            }
            match WorkflowParser::parse_yaml(yaml) {
                Err(e) => complaints.push(format!("the parser refuses the-forge: {e}")),
                Ok(workflow) => {
                    if let Err(e) = TemporalWorkflowMapper::to_temporal_definition(
                        &workflow,
                        &TenantId::system(),
                    ) {
                        complaints.push(format!("the-forge does not map: {e}"));
                    }
                    let m = &workflow.metadata;
                    if m.name != FORGE {
                        complaints.push(format!("metadata.name is {}", m.name));
                    }
                    if m.version.as_deref() != Some("2.0.0") {
                        complaints.push(format!("metadata.version is {:?}", m.version));
                    }
                    for (key, value) in [("builtin", "true"), ("category", "development")] {
                        if m.labels.get(key).map(String::as_str) != Some(value) {
                            complaints.push(format!("label {key} is {:?}", m.labels.get(key)));
                        }
                    }
                    let schema = m.input_schema.clone().unwrap_or_default();
                    for (field, ty) in [
                        ("task", "string"),
                        ("test_command", "string"),
                        ("test_image", "string"),
                        ("context", "string"),
                        ("draft_only", "boolean"),
                    ] {
                        if schema["properties"][field]["type"] != ty {
                            complaints.push(format!("input {field} is not a {ty}"));
                        }
                    }
                    let mut required: Vec<&str> = schema["required"]
                        .as_array()
                        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    required.sort_unstable();
                    if required != ["task", "test_command", "test_image"] {
                        complaints.push(format!("the required inputs are {required:?}"));
                    }
                    if schema["properties"]["draft_only"]["default"] != true {
                        complaints.push("draft_only does not default to true".to_string());
                    }
                    if workflow.spec.repositories != Some(1) {
                        complaints.push(format!(
                            "spec.repositories is {:?}",
                            workflow.spec.repositories
                        ));
                    }
                    if workflow.spec.context.get("max_code_iterations")
                        != Some(&serde_json::json!(5))
                    {
                        complaints
                            .push("spec.context carries no max_code_iterations 5".to_string());
                    }
                }
            }
        }
    }

    for name in FORGE_AGENTS {
        let Some(yaml) = listed(BUILTIN_AGENTS, name) else {
            complaints.push(format!("{name} is not in BUILTIN_AGENTS"));
            continue;
        };
        if yaml != on_disk(&format!("agents/{name}.yaml")) {
            complaints.push(format!(
                "the listed {name} is not cli/templates/agents/{name}.yaml"
            ));
        }
        match serde_yaml::from_str::<aegis_orchestrator_sdk::AgentManifest>(yaml) {
            Err(e) => complaints.push(format!("{name} does not parse as a manifest: {e}")),
            Ok(manifest) => {
                if let Err(e) = manifest.validate() {
                    complaints.push(format!("{name} does not validate: {e}"));
                }
            }
        }
        match AgentManifestParser::parse_yaml(yaml) {
            Err(e) => complaints.push(format!("the orchestrator's parser refuses {name}: {e}")),
            Ok(manifest) => {
                if manifest.metadata.name != name {
                    complaints.push(format!("{name} is named {}", manifest.metadata.name));
                }
                if manifest.metadata.labels.get("builtin").map(String::as_str) != Some("true") {
                    complaints.push(format!("{name} carries no builtin: \"true\""));
                }
                if let Err(e) = aegis_orchestrator_core::domain::tool_requirement::check(&manifest)
                {
                    complaints.push(format!("{name} is refused at deploy: {e}"));
                }
            }
        }
    }

    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// F4 and F9: the states, their kinds and transitions as the record's table
/// gives them; the two Human gates and no other; CODE's visit limit; AUDIT's
/// weights and threshold; no System state a shell command; `draft_only` true
/// ending at COMPLETE after COMMIT.
#[test]
fn the_forges_states_are_f4s_table() {
    let workflow = forge();
    let mut complaints = Vec::new();

    let kinds: BTreeMap<String, String> = workflow
        .spec
        .states
        .iter()
        .map(|(n, s)| (n.as_str().to_string(), kind(s)))
        .collect();
    let expected: BTreeMap<String, String> = [
        ("ANALYZE", "Agent requirements-analyst"),
        ("AWAIT_REQUIREMENTS_APPROVAL", "Human"),
        ("ARCHITECT", "Agent architect-agent"),
        ("AWAIT_ARCH_APPROVAL", "Human"),
        ("TEST", "Agent tester-agent"),
        ("CODE", "Agent coder-agent"),
        ("EXECUTE_TESTS", "ContainerRun"),
        ("DIFF", "System repository_diff"),
        ("AUDIT", "ParallelAgents"),
        ("COMMIT", "System repository_commit"),
        ("LAND", "System repository_land"),
        ("COMPLETE", "System update_blackboard"),
        ("FAILED", "System update_blackboard"),
        ("LAND_REFUSED", "System update_blackboard"),
    ]
    .into_iter()
    .map(|(n, k)| (n.to_string(), k.to_string()))
    .collect();
    if kinds != expected {
        complaints.push(format!("the states are {kinds:#?}"));
    }
    if workflow.spec.initial_state.as_str() != "ANALYZE" {
        complaints.push(format!(
            "the initial state is {}",
            workflow.spec.initial_state.as_str()
        ));
    }

    let on_success = |t: &str| format!("{{\"condition\":\"on_success\"}} -> {t}");
    let on_failure = |t: &str| format!("{{\"condition\":\"on_failure\"}} -> {t}");
    let yes = |t: &str| format!("{{\"condition\":\"input_equals_yes\"}} -> {t}");
    let no = |t: &str| format!("{{\"condition\":\"input_equals_no\"}} -> {t}");
    let table: Vec<(&str, Vec<String>)> = vec![
        (
            "ANALYZE",
            vec![
                on_success("AWAIT_REQUIREMENTS_APPROVAL"),
                on_failure("FAILED"),
            ],
        ),
        (
            "AWAIT_REQUIREMENTS_APPROVAL",
            vec![yes("ARCHITECT"), no("ANALYZE")],
        ),
        (
            "ARCHITECT",
            vec![on_success("AWAIT_ARCH_APPROVAL"), on_failure("FAILED")],
        ),
        ("AWAIT_ARCH_APPROVAL", vec![yes("TEST"), no("ARCHITECT")]),
        ("TEST", vec![on_success("CODE"), on_failure("FAILED")]),
        ("CODE", vec![on_success("EXECUTE_TESTS"), on_failure("FAILED")]),
        (
            "EXECUTE_TESTS",
            vec![
                "{\"condition\":\"exit_code_zero\"} -> DIFF".to_string(),
                "{\"condition\":\"exit_code_non_zero\"} -> CODE".to_string(),
            ],
        ),
        ("DIFF", vec![on_success("AUDIT"), on_failure("FAILED")]),
        (
            "AUDIT",
            vec![
                "{\"agreement\":0.8,\"condition\":\"consensus\",\"threshold\":0.95} -> COMMIT"
                    .to_string(),
                "{\"condition\":\"score_below\",\"threshold\":0.95} -> CODE".to_string(),
                // A score at or over 0.95 with agreement under 0.8 matches
                // neither: it re-enters CODE rather than ending the run.
                "{\"condition\":\"always\"} -> CODE".to_string(),
            ],
        ),
        // F9: a failed commit fails the run; a commit with `draft_only` true
        // or absent (its default) ends at COMPLETE; otherwise it lands.
        (
            "COMMIT",
            vec![
                on_failure("FAILED"),
                "{\"condition\":\"custom\",\"expression\":\"(default input.draft_only true)\"} -> COMPLETE"
                    .to_string(),
                on_success("LAND"),
            ],
        ),
        ("LAND", vec![on_success("COMPLETE"), on_failure("LAND_REFUSED")]),
        ("COMPLETE", vec![]),
        ("FAILED", vec![]),
        ("LAND_REFUSED", vec![]),
    ];
    for (name, expected_routes) in &table {
        let got = routes(state(&workflow, name));
        if &got != expected_routes {
            complaints.push(format!("{name} routes {got:#?}"));
        }
    }

    // The two gates, and no other Human state; each times out at 3600 s and
    // a "no" carries the person's feedback back.
    let gates: Vec<&str> = workflow
        .spec
        .states
        .iter()
        .filter(|(_, s)| matches!(s.kind, StateKind::Human { .. }))
        .map(|(n, _)| n.as_str())
        .collect();
    if gates.len() != 2 {
        complaints.push(format!("the Human states are {gates:?}"));
    }
    for (gate, back) in [
        ("AWAIT_REQUIREMENTS_APPROVAL", "ANALYZE"),
        ("AWAIT_ARCH_APPROVAL", "ARCHITECT"),
    ] {
        let s = state(&workflow, gate);
        if s.timeout != Some(std::time::Duration::from_secs(3600)) {
            complaints.push(format!("{gate} times out at {:?}", s.timeout));
        }
        if feedback_of(s, back) != "{{human.feedback}}" {
            complaints.push(format!("{gate}'s no to {back} carries no human feedback"));
        }
    }

    if state(&workflow, "CODE").max_state_visits != Some(5) {
        complaints.push("CODE's max_state_visits is not 5".to_string());
    }

    // Each agent state reads the repository at {{repository.path}}.
    for name in ["ANALYZE", "ARCHITECT", "TEST", "CODE"] {
        if let StateKind::Agent { input, .. } = &state(&workflow, name).kind {
            if !input.contains("{{repository.path}}") {
                complaints.push(format!(
                    "{name}'s input does not name {{{{repository.path}}}}"
                ));
            }
        }
    }

    // F4 and F11: a gate's "no, with feedback" re-enters ANALYZE or ARCHITECT
    // with the person's words, right after the line naming their answer.
    for name in ["ANALYZE", "ARCHITECT"] {
        if let StateKind::Agent { input, .. } = &state(&workflow, name).kind {
            let lines: Vec<&str> = input.lines().collect();
            let after_answer = lines
                .iter()
                .position(|l| l.starts_with("The person answered "))
                .map(|i| lines[i + 1..].iter().take(3).copied().collect::<Vec<_>>());
            let expected = [
                "{{#if human.feedback}}",
                "Their words: {{{human.feedback}}}",
                "{{/if}}",
            ];
            if after_answer.as_deref() != Some(&expected[..]) {
                complaints.push(format!(
                    "{name}'s input does not read human.feedback after the person's answer: {after_answer:?}"
                ));
            }
        }
    }

    match &state(&workflow, "EXECUTE_TESTS").kind {
        StateKind::ContainerRun {
            image,
            command,
            env,
            workdir,
            volumes,
            shell,
            network_mode,
            ..
        } => {
            if image != "{{input.test_image}}" {
                complaints.push(format!("EXECUTE_TESTS runs the image {image}"));
            }
            if !*shell {
                complaints.push("EXECUTE_TESTS is not shell: true".to_string());
            }
            // The suite is the input's test_command, given to the step's shell
            // through its environment, never spliced into the shell text.
            let runs_the_input = env.iter().any(|(k, v)| {
                v == "{{{input.test_command}}}" && command.join(" ").contains(&format!("\"${k}\""))
            });
            if !runs_the_input {
                complaints.push(format!(
                    "EXECUTE_TESTS does not run {{{{input.test_command}}}}: command {command:?}, env {env:?}"
                ));
            }
            if network_mode.as_deref() != Some("egress") {
                complaints.push(format!("EXECUTE_TESTS's network_mode is {network_mode:?}"));
            }
            match volumes.as_slice() {
                [mount] if mount.name == "repository" && !mount.read_only => {
                    if workdir.as_deref() != Some(mount.mount_path.as_str()) {
                        complaints.push(format!(
                            "EXECUTE_TESTS runs in {workdir:?}, not the repository's mount {}",
                            mount.mount_path
                        ));
                    }
                }
                other => complaints.push(format!("EXECUTE_TESTS mounts {other:?}")),
            }
        }
        other => complaints.push(format!("EXECUTE_TESTS is {other:?}")),
    }
    let stderr_feedback = feedback_of(state(&workflow, "EXECUTE_TESTS"), "CODE");
    for part in [
        "{{EXECUTE_TESTS.output.stderr}}",
        "{{EXECUTE_TESTS.output.stdout}}",
    ] {
        if !stderr_feedback.contains(part) {
            complaints.push(format!(
                "EXECUTE_TESTS's way back to CODE does not carry {part}"
            ));
        }
    }

    match &state(&workflow, "AUDIT").kind {
        StateKind::ParallelAgents {
            agents, consensus, ..
        } => {
            let weights: Vec<(&str, f64)> = agents
                .iter()
                .map(|a| (a.agent.as_str(), a.weight))
                .collect();
            if weights
                != [
                    ("code-reviewer-agent", 1.0),
                    ("critic-agent", 1.5),
                    ("security-auditor-agent", 2.0),
                ]
            {
                complaints.push(format!("AUDIT's judges are {weights:?}"));
            }
            if consensus.strategy != ConsensusStrategy::WeightedAverage
                || consensus.threshold != Some(0.95)
                || consensus.min_agreement_confidence != Some(0.8)
            {
                complaints.push(format!("AUDIT's consensus is {consensus:?}"));
            }
            for a in agents {
                for given in [
                    "{{{json ANALYZE.output}}}",
                    "{{{json ARCHITECT.output}}}",
                    "{{DIFF.output.diff}}",
                    "{{EXECUTE_TESTS.output.stdout}}",
                ] {
                    if !a.input.contains(given) {
                        complaints.push(format!("AUDIT gives {} no {given}", a.agent));
                    }
                }
            }
        }
        other => complaints.push(format!("AUDIT is {other:?}")),
    }
    if !feedback_of(state(&workflow, "AUDIT"), "CODE")
        .contains("AUDIT.consensus.metadata.individual_outputs")
    {
        complaints.push("AUDIT's way back to CODE does not carry the three reports".to_string());
    }

    // No System state of the Forge runs a shell command in the core process.
    for (name, s) in &workflow.spec.states {
        if let StateKind::System { command, .. } = &s.kind {
            if !INTERPRETER_COMMANDS.contains(&command.trim()) {
                complaints.push(format!(
                    "{} runs the shell command {command}",
                    name.as_str()
                ));
            }
        }
    }

    match &state(&workflow, "COMMIT").kind {
        StateKind::System { env, .. } => {
            if env.get("message").map(String::as_str)
                != Some("the-forge: {{{first_line input.task}}}")
            {
                complaints.push(format!("COMMIT's message is {:?}", env.get("message")));
            }
        }
        other => complaints.push(format!("COMMIT is {other:?}")),
    }

    for (terminal, outcome) in [
        ("COMPLETE", "complete"),
        ("FAILED", "failed"),
        ("LAND_REFUSED", "land_refused"),
    ] {
        match &state(&workflow, terminal).kind {
            StateKind::System { env, .. } => {
                if env.get("outcome").map(String::as_str) != Some(outcome) {
                    complaints.push(format!(
                        "{terminal} writes outcome {:?}",
                        env.get("outcome")
                    ));
                }
                for key in ["commit_sha", "branch", "sentence"] {
                    if !env.contains_key(key) {
                        complaints.push(format!("{terminal} does not write {key}"));
                    }
                }
            }
            other => complaints.push(format!("{terminal} is {other:?}")),
        }
    }

    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// F10: the five that only read list the reading `fs.*` tools and the two git
/// reading tools; `tester-agent` and `coder-agent` add the writing `fs.*`
/// tools and `cmd.run`; none lists a git write tool; each is validated by the
/// built-in `code-quality-judge`; each judge answers `{score, confidence,
/// reasoning, suggestions}`.
#[test]
fn the_forges_agents_carry_f10s_tools() {
    let mut complaints = Vec::new();
    let mut reader: Vec<&str> = READING_FS.to_vec();
    reader.extend(["aegis.git.status", "aegis.git.diff"]);
    let mut writer = reader.clone();
    writer.extend(WRITING_FS);
    writer.push("cmd.run");
    reader.sort_unstable();
    writer.sort_unstable();

    for name in FORGE_AGENTS {
        let Some(yaml) = listed(BUILTIN_AGENTS, name) else {
            complaints.push(format!("{name} is not in BUILTIN_AGENTS"));
            continue;
        };
        let manifest = match AgentManifestParser::parse_yaml(yaml) {
            Ok(m) => m,
            Err(e) => {
                complaints.push(format!("{name} does not parse: {e}"));
                continue;
            }
        };
        let mut tools: Vec<&str> = manifest.spec.tools.iter().map(String::as_str).collect();
        tools.sort_unstable();
        let expected = if matches!(name, "tester-agent" | "coder-agent") {
            &writer
        } else {
            &reader
        };
        if &tools != expected {
            complaints.push(format!("{name}'s tools are {tools:?}, not {expected:?}"));
        }
        for forbidden in GIT_WRITE_TOOLS {
            if tools.contains(&forbidden) {
                complaints.push(format!("{name} lists {forbidden}"));
            }
        }

        let judges: Vec<&str> = manifest
            .spec
            .execution
            .as_ref()
            .and_then(|e| e.validation.as_ref())
            .map(|v| {
                v.iter()
                    .filter_map(|s| match s {
                        ValidatorSpec::Semantic { judge_agent, .. } => Some(judge_agent.as_str()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if judges != ["code-quality-judge"] {
            complaints.push(format!("{name}'s validation judges are {judges:?}"));
        }

        if matches!(
            name,
            "code-reviewer-agent" | "critic-agent" | "security-auditor-agent"
        ) {
            let instruction = manifest
                .spec
                .task
                .as_ref()
                .and_then(|t| t.instruction.clone())
                .unwrap_or_default();
            for field in [
                "\"score\"",
                "\"confidence\"",
                "\"reasoning\"",
                "\"suggestions\"",
            ] {
                if !instruction.contains(field) {
                    complaints.push(format!("{name}'s verdict does not name {field}"));
                }
            }
            for given in ["requirements", "design", "diff", "test output"] {
                if !instruction.to_lowercase().contains(given) {
                    complaints.push(format!("{name} is not told it is given the {given}"));
                }
            }
        }
    }

    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// The CLI's deploy of the built-ins (`aegis update`, `aegis agent`, `aegis
/// workflow`) deploys the Forge: its loops are bounded by the engine's visit
/// limits, so the deploy path refuses no cycle.
#[tokio::test]
async fn the_cli_deploy_path_deploys_the_forge() {
    let mut server = mockito::Server::new_async().await;
    let agents = server
        .mock("POST", mockito::Matcher::Regex(r"^/v1/agents".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"agent_id":"00000000-0000-0000-0000-000000000001"}"#)
        .expect_at_least(1)
        .create_async()
        .await;
    let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = received.clone();
    let workflows = server
        .mock(
            "POST",
            mockito::Matcher::Regex(r"^/v1/workflows".to_string()),
        )
        .with_status(200)
        .with_body_from_request(move |request| {
            let body = request.body().map(|b| b.clone()).unwrap_or_default();
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            Vec::new()
        })
        .expect(BUILTIN_WORKFLOWS.len())
        .create_async()
        .await;

    let url = server.url();
    let (host, port) = url
        .rsplit_once(':')
        .expect("the mock server's url has a port");
    let client = DaemonClient::new(host, port.parse().expect("a port")).expect("a client");
    let deployed = deploy_all_builtins(&client, true).await;

    assert!(
        deployed.is_ok(),
        "the CLI deploy of the built-ins refused: {:#}",
        deployed.unwrap_err()
    );
    agents.assert_async().await;
    workflows.assert_async().await;
    let forges = received
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.contains(r#"name: "the-forge""#))
        .count();
    assert_eq!(
        forges, 1,
        "the Forge's manifest reached the daemon {forges} times"
    );
}
