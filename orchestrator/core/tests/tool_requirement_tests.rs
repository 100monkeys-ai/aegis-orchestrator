// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool Requirement Tests (BC-1, AEGIS ADR-005 O4 and O5)
//!
//! An agent whose instruction requires writing files or running commands and
//! that declares no tools is refused (O4); an agent declaring a filesystem
//! tool that writes and no read-write volume is refused (O5). The manifests
//! are parsed from YAML the way `aegis.agent.create` parses them, and the
//! built-in templates are read from disk, the files the daemon deploys.

use aegis_orchestrator_core::domain::agent::{Agent, AgentManifest};
use aegis_orchestrator_core::domain::tool_requirement::{check, refusal_of};
use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;

/// The shape of `delivery-itinerary-pdf-agent` (3b367333) as exported on
/// 2026-10-05: a persistent `/workspace` volume and no `spec.tools`.
fn delivery_itinerary(tools: &str) -> AgentManifest {
    let yaml = format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: delivery-itinerary-pdf-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: |
      You produce a delivery itinerary as a PDF from {{{{intent}}}}.
      Write a Python script that renders the PDF, run it, and verify the file
      exists and starts with %PDF.
{tools}
  volumes:
    - name: workspace
      storage_class: persistent
      type: seaweedfs
      mount_path: /workspace
      access_mode: read-write
      size_limit: 1Gi
"#
    );
    AgentManifestParser::parse_yaml(&yaml).expect("the delivery itinerary manifest parses")
}

/// The shape of `unit-conversion-agent` (7112c203, 2026-10-06): an
/// instruction that writes `/workspace/convert.py` and no volume.
fn unit_conversion(tools: &str) -> AgentManifest {
    let yaml = format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: unit-conversion-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: |
      You convert between units. Write the conversion as Python to
      /workspace/convert.py, run it, and return the printed result.
{tools}
"#
    );
    AgentManifestParser::parse_yaml(&yaml).expect("the unit conversion manifest parses")
}

const NO_TOOLS: &str = "";
const FS_WRITE: &str = "  tools:\n    - fs.write";
const WRITE_AND_RUN: &str = "  tools:\n    - fs.write\n    - cmd.run";

#[test]
fn o4_refuses_a_tool_less_agent_that_declares_a_read_write_volume() {
    let refusal = check(&delivery_itinerary(NO_TOOLS))
        .expect_err("O4 must refuse a tool-less agent that declares a read-write volume");
    assert_eq!(
        refusal.to_string(),
        "Agent 'delivery-itinerary-pdf-agent' is refused (tool-requirement/no-tools): its instruction \
         requires writing files or running commands and it declares no tools; it declares the \
         read-write volume 'workspace' at /workspace. Declare the tools it needs in spec.tools \
         (fs.write to write files, cmd.run to run commands)."
    );
}

#[test]
fn o4_refuses_a_tool_less_agent_whose_instruction_names_a_workspace_file() {
    let refusal = check(&unit_conversion(NO_TOOLS)).expect_err(
        "O4 must refuse a tool-less agent whose instruction names a file under /workspace",
    );
    assert_eq!(
        refusal.to_string(),
        "Agent 'unit-conversion-agent' is refused (tool-requirement/no-tools): its instruction requires \
         writing files or running commands and it declares no tools; its instruction names the \
         workspace file /workspace/convert.py. Declare the tools it needs in spec.tools \
         (fs.write to write files, cmd.run to run commands)."
    );
}

#[test]
fn o4_accepts_the_same_agent_once_it_declares_fs_write() {
    assert_eq!(check(&delivery_itinerary(FS_WRITE)), Ok(()));
}

#[test]
fn o4_accepts_a_tool_less_agent_that_only_answers_in_text() {
    let manifest = AgentManifestParser::parse_yaml(
        r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: copywriter-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Write a short product description for the request in {{intent}}.
"#,
    )
    .expect("the copywriter manifest parses");
    assert_eq!(check(&manifest), Ok(()));
}

#[test]
fn o4_reports_every_trigger_that_fired() {
    let manifest = AgentManifestParser::parse_yaml(
        r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: report-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Save the data to /workspace/data.csv and the chart to /workspace/out/chart.png.
    prompt_template: "{{instruction}} Then read /workspace/data.csv again. {{input}}"
  security:
    network:
      mode: none
    filesystem:
      read: []
      write: ["/workspace"]
    resources:
      cpu: 1000
      memory: 512Mi
      timeout: 300s
  volumes:
    - name: scratch
      storage_class: ephemeral
      mount_path: /workspace
      access_mode: read-write
      size_limit: 1Gi
      ttl_hours: 1
    - name: reference
      storage_class: ephemeral
      mount_path: /workspace/reference
      access_mode: read-only
      size_limit: 1Gi
      ttl_hours: 1
"#,
    )
    .expect("the report manifest parses");
    let refusal =
        check(&manifest).expect_err("O4 must refuse a tool-less agent on every trigger at once");
    assert_eq!(
        refusal.to_string(),
        "Agent 'report-agent' is refused (tool-requirement/no-tools): its instruction requires writing \
         files or running commands and it declares no tools; it declares the read-write volume \
         'scratch' at /workspace; it declares security.filesystem.write /workspace; its \
         instruction names the workspace file /workspace/data.csv; its instruction names the \
         workspace file /workspace/out/chart.png. Declare the tools it needs in spec.tools \
         (fs.write to write files, cmd.run to run commands)."
    );
}

#[test]
fn o5_refuses_a_writing_fs_tool_without_a_read_write_volume() {
    let refusal = check(&unit_conversion(WRITE_AND_RUN))
        .expect_err("O5 must refuse an agent declaring a writing fs tool and no read-write volume");
    assert_eq!(
        refusal.to_string(),
        "Agent 'unit-conversion-agent' is refused (tool-requirement/no-volume): it declares fs.write and \
         no read-write volume, so those tools have nothing to write to in a run of its own. \
         Declare a read-write volume with mount_path /workspace; inside a workflow it yields to \
         the workflow's workspace."
    );
}

#[test]
fn o5_refuses_when_the_only_volume_is_read_only_and_accepts_a_reader() {
    let yaml = |tools: &str| {
        format!(
            r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: dataset-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
  task:
    instruction: Work on the files in /workspace for {{{{intent}}}}.
{tools}
  volumes:
    - name: datasets
      storage_class: ephemeral
      mount_path: /workspace
      access_mode: read-only
      size_limit: 1Gi
      ttl_hours: 1
"#
        )
    };
    let editor = AgentManifestParser::parse_yaml(&yaml("  tools:\n    - fs.read\n    - fs.edit"))
        .expect("the editor manifest parses");
    let refusal =
        check(&editor).expect_err("O5 must refuse fs.edit when the only volume is read-only");
    assert!(
        refusal
            .to_string()
            .contains("it declares fs.edit and no read-write volume"),
        "O5's sentence names the writing tool: {refusal}"
    );
    let reader = AgentManifestParser::parse_yaml(&yaml("  tools:\n    - fs.read"))
        .expect("the reader manifest parses");
    assert_eq!(
        check(&reader),
        Ok(()),
        "a reader of a read-only volume is not refused"
    );
}

#[test]
fn every_builtin_agent_template_passes() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../cli/templates/agents");
    let mut on_disk = 0usize;
    let mut checked = 0usize;
    let mut refused = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the built-in agent templates directory reads") {
        let path = entry.expect("a directory entry reads").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        on_disk += 1;
        let yaml = std::fs::read_to_string(&path).expect("a built-in template reads");
        let manifest = AgentManifestParser::parse_yaml(&yaml)
            .unwrap_or_else(|e| panic!("built-in template {} parses: {e}", path.display()));
        checked += 1;
        if let Err(refusal) = check(&manifest) {
            refused.push(refusal.to_string());
        }
    }
    assert!(
        on_disk > 0,
        "no built-in agent template was found under {dir}"
    );
    assert_eq!(
        checked, on_disk,
        "every built-in template on disk was checked"
    );
    assert!(
        refused.is_empty(),
        "a built-in agent template is refused at deploy: {refused:#?}"
    );
}

/// AEGIS ADR-005 O6: an agent stored before O4 and O5 landed, in the shape
/// `unit-conversion-agent` had on 2026-10-06 (`fs.write`, `cmd.run`,
/// `fs.read`, no volume), answers O5's sentence; the same agent with a
/// read-write volume answers nothing.
#[test]
fn o6_an_existing_agent_of_unit_conversion_agents_earlier_shape_answers_o5s_sentence() {
    const EARLIER: &str = "  tools:\n    - fs.write\n    - cmd.run\n    - fs.read";
    let mut complaints = Vec::new();
    match refusal_of(&Agent::new(unit_conversion(EARLIER))) {
        Some(refusal) => {
            let expected =
                "Agent 'unit-conversion-agent' is refused (tool-requirement/no-volume): it \
                            declares fs.write and no read-write volume, so those tools have \
                            nothing to write to in a run of its own. Declare a read-write volume \
                            with mount_path /workspace; inside a workflow it yields to the \
                            workflow's workspace.";
            if refusal.to_string() != expected {
                complaints.push(format!("the sentence was {refusal}"));
            }
        }
        None => complaints.push(
            "an existing agent of unit-conversion-agent's earlier shape answered no refusal"
                .to_string(),
        ),
    }
    let mut with_volume = unit_conversion(EARLIER);
    with_volume.spec.volumes = delivery_itinerary(FS_WRITE).spec.volumes;
    if let Some(refusal) = refusal_of(&Agent::new(with_volume)) {
        complaints.push(format!(
            "the agent with a read-write volume was refused: {refusal}"
        ));
    }
    match refusal_of(&Agent::new(unit_conversion(NO_TOOLS))) {
        Some(refusal) if refusal.to_string().contains("(tool-requirement/no-tools)") => {}
        other => complaints.push(format!("the tool-less shape answered {other:?}")),
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
