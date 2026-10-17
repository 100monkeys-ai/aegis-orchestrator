// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A workflow run's Nuclear Notes code tree (AEGIS ADR-147)
//!
//! `carbon-forge` works on a Nuclear Notes code tree instead of a git
//! repository. A start names it by the dispatch key `trees` (C2): one entry
//! naming its workspace, its path and the revision the run starts from. The
//! start materializes that revision into a volume of the run's own, named
//! `tree` and mounted at `/workspace` (C4), and records on the entry the
//! volume and the blob each file was verified against. The run's DIFF,
//! COMMIT and LAND are the interpreter's `tree_diff`, `tree_write` and
//! `tree_land` (C3, C5): the write puts the change on a lane of the run's
//! own, the landing lands that lane after the person's approval.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::domain::execution::ExecutionContexts;
use crate::domain::tenant::TenantId;
use crate::domain::volume::VolumeMount;

/// The reserved input key by which a start names its run's tree (C2).
pub const TREES_INPUT_KEY: &str = "trees";

/// The shape a start's `trees` takes (C2).
pub const TREES_SHAPE: &str = "[{workspace, path, revision}]";

/// The volume name by which a ContainerRun step mounts its run's tree (C4):
/// the run's, never a declared volume.
pub const RUN_TREE_VOLUME: &str = "tree";

/// Where an agent state of the run sees its tree (C4).
pub const RUN_TREE_MOUNT: &str = "/workspace";

/// The remote MCP server a tree lives on.
pub const NUCLEAR_NOTES_SERVER: &str = "nuclear-notes";

/// The refusal of a start naming a tree on a node that cannot materialize
/// one.
pub const NO_TREE_SERVICE: &str =
    "this node has no Nuclear Notes tree service, so a run cannot be given trees";

/// The refusal of a tree step in a run that names no tree.
pub const RUN_HOLDS_NO_TREE: &str = "this run holds no tree";

/// The refusal of `trees` given to anything but a workflow's start.
pub const TREES_ARE_A_WORKFLOWS: &str =
    "trees are given to a workflow run at its start, never to an agent execution";

/// The sentence refusing a `trees` of any other shape (C2).
pub fn trees_refusal() -> String {
    format!(
        "trees must be {TREES_SHAPE}: one tree, naming its Nuclear Notes workspace, its path and the revision the run starts from"
    )
}

/// A tree as a start names it: only these three keys.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestedTree {
    workspace: String,
    path: String,
    revision: i64,
}

/// A run's tree as its record carries it: as named, and once materialized
/// the volume it was materialized into and the blob each file was verified
/// against (C4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTree {
    pub workspace: String,
    pub path: String,
    pub revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blobs: Vec<TreeBlob>,
}

/// One file of a materialized tree and the blob it was verified against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeBlob {
    pub path: String,
    pub sha256: String,
}

/// The trees a start names (C2): none, or exactly one entry of
/// `{workspace, path, revision}`; any other shape is refused by
/// [`trees_refusal`]. A materialized entry (one carrying a volume) is the
/// platform's and is never taken from a caller.
pub fn parse_requested_trees(value: &Value) -> Result<Vec<RunTree>, String> {
    let Value::Array(entries) = value else {
        return Err(trees_refusal());
    };
    if entries.len() > 1 {
        return Err(trees_refusal());
    }
    entries
        .iter()
        .map(|entry| {
            let tree: RequestedTree =
                serde_json::from_value(entry.clone()).map_err(|_| trees_refusal())?;
            if tree.workspace.trim().is_empty() || tree.path.trim().is_empty() {
                return Err(trees_refusal());
            }
            Ok(RunTree {
                workspace: tree.workspace,
                path: tree.path,
                revision: tree.revision,
                volume_id: None,
                blobs: Vec::new(),
            })
        })
        .collect()
}

/// The trees a run's record carries, as its start wrote them.
pub fn parse_run_trees(value: &Value) -> Result<Vec<RunTree>, String> {
    serde_json::from_value(value.clone()).map_err(|_| trees_refusal())
}

/// The lane a run writes its change to (C5): the run's own.
pub fn tree_lane(run: uuid::Uuid) -> String {
    format!("carbon-forge-{run}")
}

/// The SHA-256 of `bytes`, lowercase hex, as Nuclear Notes names a blob.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The refusal of a blob whose bytes do not hash to its entry (C4).
pub fn blob_mismatch(path: &str, expected: &str, actual: &str) -> String {
    format!(
        "the blob of '{path}' does not hash to its entry: the tree names SHA-256 {expected}, its bytes hash to {actual}"
    )
}

/// Why a run's tree could not be prepared or mounted.
#[derive(Debug, thiserror::Error)]
pub enum RunTreeError {
    /// A refusal in its own sentence.
    #[error("{0}")]
    Refused(String),
    /// A failure of the node.
    #[error("{0}")]
    Failed(String),
}

/// Where a run's tree is materialized and mounted (C4): the interpreter's
/// tool invocation service, which reaches Nuclear Notes through the SEAL
/// gateway as the run's person, with the binding the run's `contexts` chose.
#[async_trait]
pub trait RunTrees: Send + Sync {
    /// Materialize each of `requested` into a volume `tree` owned by the
    /// workflow execution `run`, every file verified against its SHA-256;
    /// answers the entries with their volume and blobs filled.
    async fn materialize_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        contexts: &ExecutionContexts,
        requested: Vec<RunTree>,
    ) -> Result<Vec<RunTree>, RunTreeError>;

    /// The mount of the workflow execution `run`'s tree at `mount_point`,
    /// registered for `execution` (an agent state or a step of the run) so
    /// its container reaches the volume as the run's; `None` when the run
    /// holds no tree.
    async fn mount_for_execution(
        &self,
        tenant_id: &TenantId,
        run: uuid::Uuid,
        execution: crate::domain::execution::ExecutionId,
        mount_point: &str,
        read_only: bool,
    ) -> Result<Option<VolumeMount>, RunTreeError>;

    /// Forget every registration [`RunTrees::mount_for_execution`] made for
    /// `execution`, once its container ended.
    fn release_execution(&self, execution: crate::domain::execution::ExecutionId);
}

// ============================================================================
// The diff of a run's tree (C3's tree_diff)
// ============================================================================

/// The most lines either side of one file is compared line by line; a
/// longer file is shown as replaced whole.
const DIFF_COMPARED_CELLS: usize = 4_000_000;

/// Lines of context around a change.
const DIFF_CONTEXT: usize = 3;

/// One file's change: its path, its bytes before (`None`: added) and after
/// (`None`: removed).
pub struct FileChange<'a> {
    pub path: &'a str,
    pub before: Option<&'a [u8]>,
    pub after: Option<&'a [u8]>,
}

/// The unified diff of `change`, as `git diff` shows a text file; a file
/// that is not UTF-8 text is named as a binary change.
pub fn unified_diff(change: &FileChange<'_>) -> String {
    let path = change.path;
    let from = if change.before.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_string()
    };
    let to = if change.after.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".to_string()
    };
    let text = |bytes: Option<&[u8]>| match bytes {
        None => Some(String::new()),
        Some(bytes) => std::str::from_utf8(bytes).ok().map(str::to_string),
    };
    let (Some(before), Some(after)) = (text(change.before), text(change.after)) else {
        return format!("Binary files {from} and {to} differ\n");
    };
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let new: Vec<&str> = after.split_inclusive('\n').collect();
    let mut out = format!("--- {from}\n+++ {to}\n");
    for hunk in hunks(&edits(&old, &new)) {
        out.push_str(&hunk);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edit<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// The edits turning `old` into `new`: their common head and tail kept,
/// the middle by a longest common subsequence when it is small enough, else
/// replaced whole.
fn edits<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Edit<'a>> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&old[head..old.len() - tail], &new[head..new.len() - tail]);
    let mut out: Vec<Edit<'a>> = old[..head].iter().map(|l| Edit::Same(l)).collect();
    if a.len().saturating_mul(b.len()) > DIFF_COMPARED_CELLS {
        out.extend(a.iter().map(|l| Edit::Removed(l)));
        out.extend(b.iter().map(|l| Edit::Added(l)));
    } else {
        // lcs[i][j]: the longest common subsequence of a[i..] and b[j..].
        let width = b.len() + 1;
        let mut lcs = vec![0u32; (a.len() + 1) * width];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i * width + j] = if a[i] == b[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                out.push(Edit::Same(a[i]));
                i += 1;
                j += 1;
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                out.push(Edit::Removed(a[i]));
                i += 1;
            } else {
                out.push(Edit::Added(b[j]));
                j += 1;
            }
        }
        out.extend(a[i..].iter().map(|l| Edit::Removed(l)));
        out.extend(b[j..].iter().map(|l| Edit::Added(l)));
    }
    out.extend(old[old.len() - tail..].iter().map(|l| Edit::Same(l)));
    out
}

/// The hunks of `edits`, each with its `@@` header and its context.
fn hunks(edits: &[Edit<'_>]) -> Vec<String> {
    let changed: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, e)| !matches!(e, Edit::Same(_)))
        .map(|(i, _)| i)
        .collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for index in changed {
        let start = index.saturating_sub(DIFF_CONTEXT);
        let end = (index + DIFF_CONTEXT + 1).min(edits.len());
        match ranges.last_mut() {
            Some(last) if start <= last.1 => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    // The line number each edit starts at, before and after.
    let mut old_line = Vec::with_capacity(edits.len());
    let mut new_line = Vec::with_capacity(edits.len());
    let (mut o, mut n) = (1usize, 1usize);
    for edit in edits {
        old_line.push(o);
        new_line.push(n);
        match edit {
            Edit::Same(_) => {
                o += 1;
                n += 1;
            }
            Edit::Removed(_) => o += 1,
            Edit::Added(_) => n += 1,
        }
    }
    ranges
        .into_iter()
        .map(|(start, end)| {
            let slice = &edits[start..end];
            let old_count = slice
                .iter()
                .filter(|e| !matches!(e, Edit::Added(_)))
                .count();
            let new_count = slice
                .iter()
                .filter(|e| !matches!(e, Edit::Removed(_)))
                .count();
            let at = |line: usize, count: usize| if count == 0 { line - 1 } else { line };
            let mut hunk = format!(
                "@@ -{},{} +{},{} @@\n",
                at(old_line[start], old_count),
                old_count,
                at(new_line[start], new_count),
                new_count
            );
            for edit in slice {
                let (mark, line) = match edit {
                    Edit::Same(line) => (' ', *line),
                    Edit::Removed(line) => ('-', *line),
                    Edit::Added(line) => ('+', *line),
                };
                hunk.push(mark);
                hunk.push_str(line);
                if !line.ends_with('\n') {
                    hunk.push_str("\n\\ No newline at end of file\n");
                }
            }
            hunk
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_start_names_one_tree_by_workspace_path_and_revision() {
        let trees =
            parse_requested_trees(&json!([{ "workspace": "w", "path": "code/x", "revision": 7 }]))
                .unwrap();
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].revision, 7);
        assert!(parse_requested_trees(&json!([])).unwrap().is_empty());
        for refused in [
            json!({ "workspace": "w", "path": "p", "revision": 1 }),
            json!([{ "workspace": "w", "path": "p" }]),
            json!([{ "workspace": "w", "path": "p", "revision": 1, "volume_id": "v" }]),
            json!([{ "workspace": "", "path": "p", "revision": 1 }]),
            json!([
                { "workspace": "w", "path": "p", "revision": 1 },
                { "workspace": "w", "path": "q", "revision": 1 }
            ]),
        ] {
            let refusal = parse_requested_trees(&refused).unwrap_err();
            assert!(refusal.contains(TREES_SHAPE), "{refused}: {refusal}");
        }
    }

    #[test]
    fn the_diff_of_a_changed_line_shows_it_with_its_context() {
        let diff = unified_diff(&FileChange {
            path: "README.md",
            before: Some(b"a\nb\nc\n"),
            after: Some(b"a\nB\nc\nd\n"),
        });
        assert_eq!(
            diff,
            "--- a/README.md\n+++ b/README.md\n@@ -1,3 +1,4 @@\n a\n-b\n+B\n c\n+d\n"
        );
    }

    #[test]
    fn an_added_and_a_removed_file_are_diffed_against_nothing() {
        let added = unified_diff(&FileChange {
            path: "new.txt",
            before: None,
            after: Some(b"x\n"),
        });
        assert_eq!(added, "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,1 @@\n+x\n");
        let removed = unified_diff(&FileChange {
            path: "old.txt",
            before: Some(b"x\n"),
            after: None,
        });
        assert_eq!(
            removed,
            "--- a/old.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n"
        );
        let binary = unified_diff(&FileChange {
            path: "b.bin",
            before: Some(&[0xff, 0xfe]),
            after: Some(&[0xff]),
        });
        assert_eq!(binary, "Binary files a/b.bin and b/b.bin differ\n");
    }
}
