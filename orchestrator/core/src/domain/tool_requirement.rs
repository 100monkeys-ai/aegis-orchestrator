// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Tool Requirement Domain Service (BC-1 Agent Lifecycle)
//!
//! Refuses an agent manifest whose declarations cannot do the work it asks
//! for (AEGIS ADR-005, Update of 2026-10-06, clauses O4 and O5).
//!
//! - **O4:** an agent that declares no tools is given no tool schemas at run
//!   time, so it can only return text. When such an agent declares a writable
//!   volume, declares `security.filesystem.write`, or names a file under
//!   `/workspace` in its instruction or prompt template, its instruction
//!   requires writing files or running commands, and it is refused.
//! - **O5:** an agent that declares a filesystem tool that writes, and no
//!   read-write volume, has no workspace to write to in a run of its own, and
//!   it is refused. Inside a workflow a declared `/workspace` volume yields to
//!   the workflow's workspace, so declaring one costs a workflow step nothing.
//!
//! Every trigger that fired is reported, not only the first.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** A pure rule over [`AgentManifest`], called by the agent
//!   lifecycle service at deploy and at update.

use crate::domain::agent::{Agent, AgentManifest};
use regex::Regex;
use std::fmt;
use std::sync::LazyLock;

/// The filesystem tools that write to a volume (O5).
pub const WRITING_FS_TOOLS: [&str; 5] = [
    "fs.write",
    "fs.edit",
    "fs.multi_edit",
    "fs.create_dir",
    "fs.delete",
];

/// A file named under `/workspace`: at least one path segment ending in a
/// name with an extension, such as `/workspace/out/report.pdf`.
static WORKSPACE_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/workspace(?:/[A-Za-z0-9_.\-]+)*/[A-Za-z0-9_\-]+\.[A-Za-z0-9]{1,8}\b")
        .expect("the workspace file pattern compiles")
});

/// A manifest refused by this rule. Its `Display` is the refusal sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequirementRefusal {
    sentence: String,
}

impl fmt::Display for ToolRequirementRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sentence)
    }
}

impl std::error::Error for ToolRequirementRefusal {}

/// The platform reads every access mode other than `read-only` as read-write
/// (`application/execution.rs`, where a volume becomes a mount).
fn is_writable(access_mode: &str) -> bool {
    access_mode != "read-only"
}

/// Check a manifest against clauses O4 and O5.
pub fn check(manifest: &AgentManifest) -> Result<(), ToolRequirementRefusal> {
    let spec = &manifest.spec;
    let name = &manifest.metadata.name;

    if spec.tools.is_empty() {
        let mut reasons = Vec::new();
        for volume in spec.volumes.iter().filter(|v| is_writable(&v.access_mode)) {
            reasons.push(format!(
                "it declares the read-write volume '{}' at {}",
                volume.name, volume.mount_path
            ));
        }
        let fs_write = spec
            .security
            .as_ref()
            .map(|s| s.filesystem.write.as_slice())
            .unwrap_or_default();
        if !fs_write.is_empty() {
            reasons.push(format!(
                "it declares security.filesystem.write {}",
                fs_write.join(", ")
            ));
        }
        let mut files: Vec<&str> = Vec::new();
        if let Some(task) = &spec.task {
            for text in [&task.instruction, &task.prompt_template]
                .into_iter()
                .flatten()
            {
                for m in WORKSPACE_FILE.find_iter(text) {
                    if !files.contains(&m.as_str()) {
                        files.push(m.as_str());
                    }
                }
            }
        }
        for file in files {
            reasons.push(format!("its instruction names the workspace file {file}"));
        }
        if !reasons.is_empty() {
            return Err(ToolRequirementRefusal {
                sentence: format!(
                    "Agent '{name}' is refused (AEGIS ADR-005 O4): its instruction requires \
                     writing files or running commands and it declares no tools; {}. Declare \
                     the tools it needs in spec.tools (fs.write to write files, cmd.run to run \
                     commands).",
                    reasons.join("; ")
                ),
            });
        }
        return Ok(());
    }

    let writing: Vec<&str> = spec
        .tools
        .iter()
        .map(String::as_str)
        .filter(|t| WRITING_FS_TOOLS.contains(t))
        .collect();
    let has_writable_volume = spec.volumes.iter().any(|v| is_writable(&v.access_mode));
    if !writing.is_empty() && !has_writable_volume {
        return Err(ToolRequirementRefusal {
            sentence: format!(
                "Agent '{name}' is refused (AEGIS ADR-005 O5): it declares {} and no read-write \
                 volume, so those tools have nothing to write to in a run of its own. Declare a \
                 read-write volume with mount_path /workspace; inside a workflow it yields to \
                 the workflow's workspace.",
                writing.join(", ")
            ),
        });
    }
    Ok(())
}

/// The refusal O4 or O5 answers for an agent already deployed (AEGIS ADR-005
/// O6): an agent deployed before the rule landed is refused at the start of
/// each execution and carries the sentence on `aegis.agent.list` and
/// `aegis.agent.search`.
pub fn refusal_of(agent: &Agent) -> Option<ToolRequirementRefusal> {
    check(&agent.manifest).err()
}
