// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The program an agent carries, `spec.program` (AEGIS ADR-005 O7a), and the
//! tool call a text answer waits for (AEGIS ADR-135 D5, ADR-005 O7e).

use aegis_orchestrator_core::domain::agent::{AgentManifest, ToolCallRequirement};

/// A manifest whose `spec` ends with `tail` (indented two spaces).
fn manifest(tail: &str) -> AgentManifest {
    let yaml = format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: sum-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Run the program and present its output.
{tail}
"#
    );
    serde_yaml::from_str(&yaml).unwrap_or_else(|e| panic!("the test manifest parses: {e}\n{yaml}"))
}

const CMD_RUN: &str = "  tools:\n    - cmd.run\n";

fn program(files: &str, run: &str) -> String {
    format!("  program:\n    files:\n{files}    run: {run}\n    sample_input: {{\"amounts\": [3, 4.5]}}\n")
}

const SOLVE: &str = "      - path: solve.py\n        content: print(7.5)\n";

/// O7a: a program is carried as `files`, `run` and `sample_input`, and every
/// declaration it cannot be carried with is refused, each with its sentence.
#[test]
fn a_program_is_carried_and_each_refusal_names_its_reason() {
    let mut complaints = Vec::new();

    let carried = manifest(&format!(
        "{CMD_RUN}{}",
        program(
            "      - path: solve.py\n        content: print(7.5)\n      - path: bin/run.sh\n        content: python solve.py\n        executable: true\n",
            "python /opt/aegis/program/solve.py"
        )
    ));
    match carried.validate() {
        Ok(()) => {
            let program = carried.spec.program.as_ref().expect("spec.program parsed");
            if program.files.len() != 2
                || !program.files[1].executable
                || program.files[0].executable
            {
                complaints.push(format!(
                    "the files did not parse as written: {:?}",
                    program.files
                ));
            }
            if program.sample_input != Some(serde_json::json!({"amounts": [3, 4.5]})) {
                complaints.push(format!("sample_input: {:?}", program.sample_input));
            }
        }
        Err(e) => complaints.push(format!("a carried program was refused: {e}")),
    }

    let refused = [
        (
            format!("{CMD_RUN}  program:\n    files: []\n    run: python x.py\n"),
            "spec.program: it carries no files".to_string(),
        ),
        (
            format!("{CMD_RUN}{}", program("      - path: /etc/solve.py\n        content: x\n", "python x.py")),
            "spec.program: the file path '/etc/solve.py' must be relative to /opt/aegis/program"
                .to_string(),
        ),
        (
            format!("{CMD_RUN}{}", program("      - path: ../solve.py\n        content: x\n", "python x.py")),
            "spec.program: the file path '../solve.py' must not contain '..', '.' or an empty segment"
                .to_string(),
        ),
        (
            format!("{CMD_RUN}{}", program(&format!("{SOLVE}{SOLVE}"), "python x.py")),
            "spec.program: the file path 'solve.py' is declared twice".to_string(),
        ),
        (
            format!("{CMD_RUN}{}", program("      - path: input.json\n        content: x\n", "python x.py")),
            "spec.program: the file path 'input.json' is where the execution's input is placed"
                .to_string(),
        ),
        (
            format!("{CMD_RUN}{}", program(SOLVE, "\"  \"")),
            "spec.program: run is empty".to_string(),
        ),
        (
            format!(
                "{CMD_RUN}{}",
                program(
                    &format!(
                        "      - path: big.py\n        content: \"{}\"\n",
                        "x".repeat(256 * 1024 + 1)
                    ),
                    "python x.py"
                )
            ),
            "spec.program: its files hold 262145 bytes, over the 262144-byte limit".to_string(),
        ),
        (
            format!("  tools:\n    - fs.read\n{}", program(SOLVE, "python x.py")),
            "spec.program: an agent carrying a program must declare cmd.run in spec.tools to run it"
                .to_string(),
        ),
    ];
    for (tail, expected) in refused {
        match manifest(&tail).validate() {
            Ok(()) => complaints.push(format!("accepted, expected \"{expected}\"")),
            Err(e) if e == expected => {}
            Err(e) => complaints.push(format!("refused with \"{e}\", expected \"{expected}\"")),
        }
    }

    let unknown: Result<AgentManifest, _> = serde_yaml::from_str(
        "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: a\n  version: \"1\"\nspec:\n  runtime:\n    language: python\n    version: \"3.11\"\n  program:\n    files: []\n    run: x\n    entrypoint: y\n",
    );
    if unknown.is_ok() {
        complaints.push("a program field the schema does not know was accepted".to_string());
    }

    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

/// O7e: an agent carrying a program must run it; an agent declaring
/// `require_tool_call` must call a tool; any other answers text freely.
#[test]
fn the_tool_call_a_text_answer_waits_for() {
    let mut complaints = Vec::new();
    let carrying = manifest(&format!(
        "{CMD_RUN}{}",
        program(SOLVE, "python /opt/aegis/program/solve.py")
    ));
    let expected =
        ToolCallRequirement::RunProgram("python /opt/aegis/program/solve.py".to_string());
    if ToolCallRequirement::of(&carrying) != expected {
        complaints.push(format!(
            "a program agent requires {:?}",
            ToolCallRequirement::of(&carrying)
        ));
    }
    let executor = manifest(&format!(
        "{CMD_RUN}  execution:\n    mode: iterative\n    max_iterations: 3\n    require_tool_call: true\n"
    ));
    if ToolCallRequirement::of(&executor) != ToolCallRequirement::AnyTool {
        complaints.push(format!(
            "an executor requires {:?}",
            ToolCallRequirement::of(&executor)
        ));
    }
    let free = manifest(CMD_RUN);
    if ToolCallRequirement::of(&free) != ToolCallRequirement::None {
        complaints.push(format!(
            "a plain agent requires {:?}",
            ToolCallRequirement::of(&free)
        ));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
