// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A carbon-forge run's Nuclear Notes tree (AEGIS ADR-147)
//!
//! The interpreter's tool invocation service is the run's tree service: at
//! the start it materializes the named revision into a volume `tree` of the
//! run's own, every file verified against its SHA-256 (C4); it mounts that
//! volume for the run's agent states and steps at `/workspace`; and it
//! answers the run's `tree_diff`, `tree_write` and `tree_land` (C3, C5).
//! The write puts the run's tree on a lane of the run's own with
//! `trees.put` and `lane`; the landing lands that lane as one changeset
//! with `lanes.land` once the run's person approved the call
//! `aegis.tree.land` at the gate, as `aegis.git.land` is gated (AEGIS
//! ADR-141 F8). A conflict is answered in Nuclear Notes' own sentence.
//!
//! Nuclear Notes is reached only through the SEAL gateway's `InvokeTool`,
//! as the run's person, with the credential of the binding the run's
//! `contexts` chose for it (AEGIS ADR-132 H1 to H8); never by direct HTTP,
//! so a materialized entry's presigned `url` is never fetched and every
//! blob is read with `blobs.get`.

use std::collections::BTreeMap;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use super::gateway::GatewayActing;
use super::storage::GatedLanding;
use super::ToolInvocationService;
use crate::application::git_repo_service::{RepositoryActionAnswer, RepositoryActionError};
use crate::application::nfs_gateway::VolumeRegistration;
use crate::domain::execution::{ExecutionContexts, ExecutionId, CONTEXTS_INPUT_KEY};
use crate::domain::fsal::FsalAccessPolicy;
use crate::domain::run_tree::{
    blob_mismatch, parse_run_trees, sha256_hex, tree_lane, unified_diff, FileChange, RunTree,
    RunTreeError, RunTrees, TreeBlob, NUCLEAR_NOTES_SERVER, RUN_HOLDS_NO_TREE, TREES_INPUT_KEY,
};
use crate::domain::seal_session::{CallerAnswer, SealSessionError};
use crate::domain::shared_kernel::VolumeId;
use crate::domain::storage::{FileType, OpenMode, StorageProvider};
use crate::domain::tenant::TenantId;
use crate::domain::volume::{AccessMode, StorageClass, Volume, VolumeBackend, VolumeMount};
use crate::infrastructure::tool_router::{ARCHIVE_PUSH_TOOL, TREE_LAND_TOOL};

/// The most a blob read or a `blobs.put` carries: Nuclear Notes' part.
const PART: usize = 4 * 1024 * 1024;

/// The entries `trees.materialize` is asked for per page.
const MATERIALIZE_PAGE: u64 = 1000;

/// How long a run's tree volume is kept: a landing may wait days for its
/// person's approval.
const TREE_VOLUME_TTL_HOURS: i64 = 24 * 14;

/// The least a run's tree volume is sized, in MiB; a larger tree gets
/// twice its size.
const TREE_VOLUME_MIN_MB: u64 = 256;

/// The most a tree's diff carries.
const TREE_DIFF_MAX_BYTES: usize = 1024 * 1024;

/// The refusal of a diff larger than [`TREE_DIFF_MAX_BYTES`].
const TREE_DIFF_TOO_LARGE: &str = "the diff is larger than 1 MiB; narrow the change and ask again";

/// The refusal of a write with nothing changed.
const NOTHING_TO_WRITE: &str =
    "nothing to write: the run's tree is as the revision it started from left it";

/// The refusal of `aegis.tree.land` called by anything but a workflow's own
/// landing step.
const TREE_LAND_IS_THE_WORKFLOWS: &str =
    "aegis.tree.land is answered only for a workflow's own landing step";

/// The refusal of `aegis.archive.push` called by anything but a workflow's
/// own push step (AEGIS ADR-149 C2).
const ARCHIVE_PUSH_IS_THE_WORKFLOWS: &str =
    "aegis.archive.push is answered only for a workflow's own push step";

/// A run's cut is named `forge-<execution id>` when its state names none
/// (AEGIS ADR-149 C1): a cut name admits no slash (Nuclear Notes
/// `snapshot.ts` CUT_NAME_PATTERN).
const CUT_NAME_DEFAULT_PREFIX: &str = "forge-";

/// One entry of a tree's revision, as `trees.materialize` answers it.
#[derive(Debug, Clone, Deserialize)]
struct ManifestEntry {
    path: String,
    #[serde(default = "regular_mode")]
    mode: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    commit: Option<String>,
}

fn regular_mode() -> String {
    "100644".to_string()
}

impl ManifestEntry {
    /// A file the run's volume holds: a regular or an executable one. A
    /// symlink (its blob is its target) and a submodule link are carried
    /// as the revision has them.
    fn is_file(&self) -> bool {
        matches!(self.mode.as_str(), "100644" | "100755")
    }

    /// The entry as `trees.put` takes it.
    fn put_entry(&self, sha256: Option<&str>) -> Value {
        let mut entry = json!({ "path": self.path, "mode": self.mode });
        if let Some(sha) = sha256.or(self.sha256.as_deref()) {
            entry["sha256"] = json!(sha);
        }
        if let Some(commit) = &self.commit {
            entry["commit"] = json!(commit);
        }
        entry
    }
}

/// A run's tree as its own steps act on it.
struct TreeOfRun {
    run: uuid::Uuid,
    person: String,
    acting: GatewayActing,
    tree: RunTree,
    /// The volume the tree was materialized into; `None` for a tree named
    /// and not materialized (AEGIS ADR-149 C2).
    volume_id: Option<VolumeId>,
}

/// The sentence a tree step answers for a refusal its person can act on:
/// Nuclear Notes' own error, or no usable credential for it. `None`: a
/// failure of the node.
fn step_refusal(e: &SealSessionError) -> Option<String> {
    fn of(answer: &CallerAnswer) -> Option<String> {
        match answer {
            CallerAnswer::RemoteToolError(message)
            | CallerAnswer::CredentialBindingRequired { message }
            | CallerAnswer::CredentialRejected { message } => Some(message.clone()),
            CallerAnswer::Needs { answer, .. } => of(answer),
            _ => None,
        }
    }
    match e {
        SealSessionError::Answered { answer, .. } => of(answer),
        _ => None,
    }
}

/// Nuclear Notes' own error text, when the server refused the call.
fn remote_tool_error(e: &SealSessionError) -> Option<&str> {
    match e {
        SealSessionError::Answered {
            answer: CallerAnswer::RemoteToolError(message),
            ..
        } => Some(message),
        _ => None,
    }
}

/// The tool's own answer out of the server's `tools/call` result: Nuclear
/// Notes answers its JSON as the text of the result's content; a result
/// marked `isError` is the server's error.
fn tool_answer(result: Value) -> Result<Value, SealSessionError> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| {
            content
                .iter()
                .find_map(|item| item.get("text").and_then(Value::as_str))
        })
        .map(str::to_string);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = text.unwrap_or_else(|| "the tool answered an error".to_string());
        return Err(SealSessionError::InvalidArguments(message.clone())
            .answered(CallerAnswer::RemoteToolError(message)));
    }
    match text {
        Some(text) => Ok(serde_json::from_str(&text).unwrap_or(Value::String(text))),
        None => Ok(result),
    }
}

/// A volume's root as its storage provider names it, as a step's copy of
/// its run's attachments is written (AEGIS ADR-143 S4).
fn volume_root(volume: &Volume) -> Result<String, String> {
    match &volume.backend {
        VolumeBackend::HostPath { path } => {
            Ok(path.to_string_lossy().trim_end_matches('/').to_string())
        }
        VolumeBackend::SeaweedFS { remote_path, .. } => {
            Ok(remote_path.trim_end_matches('/').to_string())
        }
        VolumeBackend::OpenDal { .. } => Ok(format!(
            "/aegis/opendal/volumes/{}/{}",
            volume.tenant_id, volume.id
        )),
        VolumeBackend::Seal { .. } => {
            Err("a run's tree needs a volume on this node's storage".to_string())
        }
    }
}

/// A tree path that stays inside its volume: relative, with no empty,
/// `.` or `..` component.
fn safe_tree_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Every file under `root`, by its path relative to it, with its bytes.
async fn read_volume(
    storage: &dyn StorageProvider,
    root: &str,
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        let at = if dir.is_empty() {
            root.to_string()
        } else {
            format!("{root}/{dir}")
        };
        let entries = storage
            .readdir(&at)
            .await
            .map_err(|e| format!("the run's tree could not be listed at '{dir}': {e}"))?;
        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{dir}/{}", entry.name)
            };
            match entry.file_type {
                FileType::Directory => pending.push(path),
                FileType::File => {
                    let bytes = read_file(storage, &format!("{root}/{path}")).await?;
                    files.insert(path, bytes);
                }
                FileType::Symlink => {}
            }
        }
    }
    Ok(files)
}

/// The whole of the file at `path`.
async fn read_file(storage: &dyn StorageProvider, path: &str) -> Result<Vec<u8>, String> {
    let failed =
        |e: crate::domain::storage::StorageError| format!("'{path}' could not be read: {e}");
    let handle = storage
        .open_file(path, OpenMode::ReadOnly)
        .await
        .map_err(failed)?;
    let mut bytes = Vec::new();
    let read = async {
        loop {
            let chunk = storage
                .read_at(&handle, bytes.len() as u64, PART)
                .await
                .map_err(failed)?;
            if chunk.is_empty() {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<(), String>(())
    }
    .await;
    let _ = storage.close_file(&handle).await;
    read.map(|()| bytes)
}

/// Write `bytes` as the file `path`, its parent directories made first.
async fn write_file(storage: &dyn StorageProvider, path: &str, bytes: &[u8]) -> Result<(), String> {
    if let Some((parent, _)) = path.rsplit_once('/') {
        // A directory that already exists is answered as an error by some
        // providers; the file's creation says whether the path is usable.
        let _ = storage.create_directory(parent).await;
    }
    let failed =
        |e: crate::domain::storage::StorageError| format!("'{path}' could not be written: {e}");
    let handle = storage.create_file(path, 0o644).await.map_err(failed)?;
    let written = storage.write_at(&handle, 0, bytes).await;
    let _ = storage.close_file(&handle).await;
    written.map(|_| ()).map_err(failed)
}

impl ToolInvocationService {
    /// Call `tool` of Nuclear Notes in `workspace` for the run `run`, as
    /// `acting`: the tool's own answer, or the refusal.
    async fn nuclear_notes(
        &self,
        run: uuid::Uuid,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        workspace: &str,
        tool: &str,
        mut args: Value,
    ) -> Result<Value, SealSessionError> {
        if let Value::Object(map) = &mut args {
            map.insert("workspace".to_string(), json!(workspace));
        }
        let result = self
            .invoke_remote_tool_for_run(run, tenant_id, acting, NUCLEAR_NOTES_SERVER, tool, args)
            .await?;
        tool_answer(result)
    }

    /// The entries of `tree`'s revision, every page of them.
    async fn tree_manifest(
        &self,
        run: uuid::Uuid,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        tree: &RunTree,
    ) -> Result<Vec<ManifestEntry>, SealSessionError> {
        let mut entries = Vec::new();
        let mut offset: u64 = 0;
        loop {
            let mut args = json!({
                "path_or_id": tree.path,
                "revision_id": tree.revision,
                "limit": MATERIALIZE_PAGE,
            });
            if offset > 0 {
                args["offset"] = json!(offset);
            }
            let page = self
                .nuclear_notes(
                    run,
                    tenant_id,
                    acting,
                    &tree.workspace,
                    "trees.materialize",
                    args,
                )
                .await?;
            let listed: Vec<ManifestEntry> = serde_json::from_value(
                page.get("entries").cloned().unwrap_or(json!([])),
            )
            .map_err(|e| {
                SealSessionError::InternalError(format!(
                    "trees.materialize answered entries that do not read: {e}"
                ))
            })?;
            let count = listed.len() as u64;
            entries.extend(listed);
            match page.get("next_offset").and_then(Value::as_u64) {
                Some(next) if next > offset && count > 0 => offset = next,
                _ => break,
            }
        }
        Ok(entries)
    }

    /// The bytes of the blob `sha256`, read part by part.
    async fn blob_bytes(
        &self,
        run: uuid::Uuid,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        workspace: &str,
        sha256: &str,
    ) -> Result<Vec<u8>, SealSessionError> {
        let mut bytes = Vec::new();
        loop {
            let mut args = json!({ "sha256": sha256, "length": PART });
            if !bytes.is_empty() {
                args["offset"] = json!(bytes.len());
            }
            let part = self
                .nuclear_notes(run, tenant_id, acting, workspace, "blobs.get", args)
                .await?;
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(
                    part.get("content_base64")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )
                .map_err(|e| {
                    SealSessionError::InternalError(format!(
                        "blobs.get answered content that is not base64: {e}"
                    ))
                })?;
            let eof = part.get("eof").and_then(Value::as_bool).unwrap_or(true);
            let empty = chunk.is_empty();
            bytes.extend_from_slice(&chunk);
            if eof || empty {
                break;
            }
        }
        Ok(bytes)
    }

    /// Store `bytes` as a blob, verified against `sha256`: in one call up to
    /// a part, else by an upload session of parts.
    async fn put_blob(
        &self,
        run: uuid::Uuid,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        workspace: &str,
        sha256: &str,
        bytes: &[u8],
    ) -> Result<(), SealSessionError> {
        let encode = |part: &[u8]| base64::engine::general_purpose::STANDARD.encode(part);
        if bytes.len() <= PART {
            self.nuclear_notes(
                run,
                tenant_id,
                acting,
                workspace,
                "blobs.put",
                json!({ "content_base64": encode(bytes), "sha256": sha256 }),
            )
            .await?;
            return Ok(());
        }
        let begun = self
            .nuclear_notes(
                run,
                tenant_id,
                acting,
                workspace,
                "blobs.upload_begin",
                json!({ "sha256": sha256, "byte_size": bytes.len() }),
            )
            .await?;
        if begun.get("deduped").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        let upload_id = begun
            .get("upload_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SealSessionError::InternalError(
                    "blobs.upload_begin answered no upload_id".to_string(),
                )
            })?
            .to_string();
        for (index, part) in bytes.chunks(PART).enumerate() {
            self.nuclear_notes(
                run,
                tenant_id,
                acting,
                workspace,
                "blobs.upload_part",
                json!({
                    "upload_id": upload_id,
                    "part_number": index + 1,
                    "content_base64": encode(part),
                }),
            )
            .await?;
        }
        self.nuclear_notes(
            run,
            tenant_id,
            acting,
            workspace,
            "blobs.upload_complete",
            json!({ "upload_id": upload_id }),
        )
        .await?;
        Ok(())
    }

    /// Materialize `tree` into a new volume `tree` owned by the workflow
    /// execution `run`: each file of its revision read with `blobs.get`,
    /// refused unless its bytes hash to its entry, and written at its path.
    async fn materialize_tree(
        &self,
        tenant_id: &TenantId,
        run: uuid::Uuid,
        acting: &GatewayActing,
        tree: RunTree,
    ) -> Result<RunTree, RunTreeError> {
        let volumes = &self
            .user_volume_service
            .as_ref()
            .ok_or_else(|| {
                RunTreeError::Failed("this node has no volume service for a run's tree".to_string())
            })?
            .volume_service;
        let refused_or_failed = |e: SealSessionError| match step_refusal(&e) {
            Some(sentence) => RunTreeError::Refused(sentence),
            None => RunTreeError::Failed(format!("materializing the run's tree failed: {e}")),
        };
        let manifest = self
            .tree_manifest(run, tenant_id, acting, &tree)
            .await
            .map_err(refused_or_failed)?;
        if let Some(entry) = manifest.iter().find(|e| !safe_tree_path(&e.path)) {
            return Err(RunTreeError::Refused(format!(
                "the tree names the path '{}', which does not stay inside its volume",
                entry.path
            )));
        }
        let total: u64 = manifest.iter().filter_map(|e| e.size).sum();
        let size_mb = (total.saturating_mul(2))
            .div_ceil(1024 * 1024)
            .max(TREE_VOLUME_MIN_MB);
        let volume_id = volumes
            .create_volume(
                crate::domain::run_tree::RUN_TREE_VOLUME.to_string(),
                tenant_id.clone(),
                StorageClass::ephemeral_hours(TREE_VOLUME_TTL_HOURS),
                size_mb,
                crate::domain::volume::VolumeOwnership::workflow(run),
            )
            .await
            .map_err(|e| {
                RunTreeError::Failed(format!("the run's tree volume was not made: {e}"))
            })?;
        let filled = async {
            let volume = volumes.get_volume(volume_id).await.map_err(|e| {
                RunTreeError::Failed(format!("the run's tree volume was not found: {e}"))
            })?;
            let root = volume_root(&volume).map_err(RunTreeError::Failed)?;
            let storage = self.fsal.storage_provider().as_ref();
            let mut blobs = Vec::new();
            for entry in manifest.iter().filter(|e| e.is_file()) {
                let Some(expected) = entry.sha256.as_deref() else {
                    continue;
                };
                let bytes = self
                    .blob_bytes(run, tenant_id, acting, &tree.workspace, expected)
                    .await
                    .map_err(refused_or_failed)?;
                let actual = sha256_hex(&bytes);
                if actual != expected {
                    return Err(RunTreeError::Refused(blob_mismatch(
                        &entry.path,
                        expected,
                        &actual,
                    )));
                }
                write_file(storage, &format!("{root}/{}", entry.path), &bytes)
                    .await
                    .map_err(RunTreeError::Failed)?;
                blobs.push(TreeBlob {
                    path: entry.path.clone(),
                    sha256: actual,
                });
            }
            Ok(RunTree {
                volume_id: Some(volume_id.0.to_string()),
                blobs,
                ..tree
            })
        }
        .await;
        if filled.is_err() {
            if let Err(e) = volumes.delete_volume(volume_id).await {
                tracing::warn!(volume_id = %volume_id, error = %e, "A run's tree volume was not removed after its materialization failed");
            }
        }
        filled
    }

    /// The tree the workflow run `run` holds, read from its record in
    /// `tenant_id`: `Err` of `Ok` a refusal's sentence.
    async fn tree_of_run(
        &self,
        tenant_id: &TenantId,
        run: uuid::Uuid,
    ) -> Result<Result<TreeOfRun, String>, RepositoryActionError> {
        let executions =
            self.workflow_execution_repo
                .as_ref()
                .ok_or(RepositoryActionError::NotConfigured(
                    "the workflow execution store",
                ))?;
        let execution = executions
            .find_by_id_for_tenant(tenant_id, ExecutionId(run))
            .await
            .map_err(|e| RepositoryActionError::Failed(e.to_string()))?
            .ok_or(RepositoryActionError::RunNotFound(run))?;
        let trees = match execution.input.get(TREES_INPUT_KEY) {
            None => Vec::new(),
            Some(value) => match parse_run_trees(value) {
                Ok(trees) => trees,
                Err(sentence) => return Ok(Err(sentence)),
            },
        };
        // The first tree materialized into a volume, else the first named
        // (AEGIS ADR-149 C2: a tree bound by its name alone).
        let volume_of = |tree: &RunTree| {
            tree.volume_id
                .as_deref()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .map(VolumeId)
        };
        let Some(tree) = trees
            .iter()
            .find(|tree| volume_of(tree).is_some())
            .or_else(|| trees.first())
            .cloned()
        else {
            return Ok(Err(RUN_HOLDS_NO_TREE.to_string()));
        };
        let volume_id = volume_of(&tree);
        let person = execution.initiating_user_sub.clone().unwrap_or_default();
        let contexts = ExecutionContexts::from_value(execution.input.get(CONTEXTS_INPUT_KEY));
        Ok(Ok(TreeOfRun {
            run,
            acting: Self::run_acting(
                Some(person.as_str()),
                Some(execution.workflow_id.as_uuid()),
                contexts,
            ),
            person,
            tree,
            volume_id,
        }))
    }

    /// The volume the run's tree was materialized into: a tree named and not
    /// materialized has no files (AEGIS ADR-149 C2).
    fn volume_of(tree: &TreeOfRun) -> Result<VolumeId, RepositoryActionError> {
        tree.volume_id
            .ok_or_else(|| RepositoryActionError::Failed(RUN_HOLDS_NO_TREE.to_string()))
    }

    /// The run's tree volume's files, and the root they were read under.
    async fn files_of(
        &self,
        volume_id: VolumeId,
    ) -> Result<BTreeMap<String, Vec<u8>>, RepositoryActionError> {
        let volume = self
            .fsal
            .volume_repository()
            .find_by_id(volume_id)
            .await
            .map_err(|e| RepositoryActionError::Failed(e.to_string()))?
            .ok_or_else(|| {
                RepositoryActionError::Failed(format!(
                    "the run's tree volume {} was not found",
                    volume_id
                ))
            })?;
        let root = volume_root(&volume).map_err(RepositoryActionError::Failed)?;
        read_volume(self.fsal.storage_provider().as_ref(), &root)
            .await
            .map_err(RepositoryActionError::Failed)
    }

    /// One of carbon-forge's own steps on the run's tree (AEGIS ADR-147 C3,
    /// C5): `tree_diff` (the run's change against the revision it started
    /// from), `tree_write` (the run's tree to a lane of its own, with
    /// `message` as its intent) or `tree_land` (that lane landed as one
    /// changeset, through the person's approval). The answer's `branch` is
    /// the run's lane, its `git_ref` the tree's path and its `commit_sha` the
    /// landed changeset; a refusal answers its sentence.
    pub(super) async fn run_tree_action(
        &self,
        tenant_id: &TenantId,
        run: uuid::Uuid,
        action: &str,
        message: Option<&str>,
    ) -> Result<RepositoryActionAnswer, RepositoryActionError> {
        let tree = match self.tree_of_run(tenant_id, run).await? {
            Ok(tree) => tree,
            Err(sentence) => return Ok(RepositoryActionAnswer::refused(sentence)),
        };
        let answer = RepositoryActionAnswer {
            branch: tree_lane(run),
            git_ref: tree.tree.path.clone(),
            ..RepositoryActionAnswer::default()
        };
        let refused_or_failed =
            |e: SealSessionError, answer: RepositoryActionAnswer| match step_refusal(&e) {
                Some(sentence) => Ok(RepositoryActionAnswer {
                    sentence: Some(sentence),
                    ..answer
                }),
                None => Err(RepositoryActionError::Failed(e.to_string())),
            };
        if matches!(action, "tree_diff" | "tree_write") && tree.volume_id.is_none() {
            return Ok(RepositoryActionAnswer {
                sentence: Some(RUN_HOLDS_NO_TREE.to_string()),
                ..answer
            });
        }
        let string_of =
            |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        match action {
            "tree_diff" => match self.tree_diff(tenant_id, &tree).await? {
                Ok(diff) if diff.len() > TREE_DIFF_MAX_BYTES => Ok(RepositoryActionAnswer {
                    sentence: Some(TREE_DIFF_TOO_LARGE.to_string()),
                    ..answer
                }),
                Ok(diff) => Ok(RepositoryActionAnswer {
                    diff: Some(diff),
                    ..answer
                }),
                Err(e) => refused_or_failed(e, answer),
            },
            "tree_write" => {
                let message = message
                    .filter(|m| !m.trim().is_empty())
                    .ok_or(RepositoryActionError::NoMessage)?;
                match self.tree_write(tenant_id, &tree, message).await? {
                    Ok(None) => Ok(answer),
                    Ok(Some(sentence)) => Ok(RepositoryActionAnswer {
                        sentence: Some(sentence),
                        ..answer
                    }),
                    Err(e) => refused_or_failed(e, answer),
                }
            }
            // AEGIS ADR-149 C1: the landed run becomes a cut, named by the
            // state's `cut` (the step's message) or `forge-<execution id>`;
            // the cut rides `git_ref` and its snapshot `commit_sha`.
            "tree_cut" => {
                let name = message
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{CUT_NAME_DEFAULT_PREFIX}{run}"));
                match self.tree_cut(tenant_id, &tree, &name).await {
                    Ok(cut) => Ok(RepositoryActionAnswer {
                        commit_sha: string_of(&cut, "snapshot_id"),
                        git_ref: string_of(&cut, "cut").unwrap_or(name),
                        ..answer
                    }),
                    Err(e) => refused_or_failed(e, answer),
                }
            }
            // AEGIS ADR-149 C2: the cut rendered into the tree's git archive
            // and tagged; its commit rides `commit_sha`, its tag `git_ref`,
            // its snapshot `branch`, and the render's own answer `diff`.
            "archive_render" => {
                let cut = message
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .ok_or(RepositoryActionError::NoMessage)?;
                match self.archive_render(tenant_id, &tree, cut).await {
                    Ok(rendered) => Ok(RepositoryActionAnswer {
                        commit_sha: string_of(&rendered, "commit_sha1"),
                        branch: string_of(&rendered, "snapshot_id").unwrap_or_default(),
                        git_ref: rendered
                            .get("tag")
                            .and_then(|tag| string_of(tag, "name"))
                            .unwrap_or_default(),
                        diff: Some(rendered.to_string()),
                        ..answer
                    }),
                    Err(e) => refused_or_failed(e, answer),
                }
            }
            // AEGIS ADR-149 C2: the archive's push, a crossing of the
            // boundary, after the person's approval; its job rides
            // `commit_sha`, its remote `git_ref`, whether it queued `branch`.
            "archive_push" => {
                let execution_id = ExecutionId(run);
                let arguments = json!({ "tree": tree.tree.path });
                let pushed = match self
                    .land_at_gate(
                        tenant_id,
                        execution_id,
                        &tree.person,
                        ARCHIVE_PUSH_TOOL,
                        arguments.clone(),
                        || {
                            self.archive_push_for_interpreter(
                                tenant_id,
                                execution_id,
                                &tree.person,
                                &arguments,
                            )
                        },
                    )
                    .await?
                {
                    GatedLanding::Landed(pushed) => pushed,
                    GatedLanding::NotApproved(sentence) => {
                        return Ok(RepositoryActionAnswer {
                            sentence: Some(sentence),
                            ..answer
                        })
                    }
                };
                let queued = pushed.get("queued").and_then(Value::as_bool);
                Ok(RepositoryActionAnswer {
                    commit_sha: string_of(&pushed, "job_id"),
                    git_ref: string_of(&pushed, "remote").unwrap_or_default(),
                    branch: match queued {
                        Some(true) => "queued".to_string(),
                        Some(false) => "not queued".to_string(),
                        None => String::new(),
                    },
                    diff: pushed
                        .get("sentence")
                        .is_none()
                        .then(|| pushed.to_string()),
                    sentence: string_of(&pushed, "sentence"),
                })
            }
            "tree_land" => {
                let message = message
                    .filter(|m| !m.trim().is_empty())
                    .ok_or(RepositoryActionError::NoMessage)?;
                let execution_id = ExecutionId(run);
                let arguments = json!({
                    "tree": tree.tree.path,
                    "lane": tree_lane(run),
                    "intent": message,
                });
                let landed = match self
                    .land_at_gate(
                        tenant_id,
                        execution_id,
                        &tree.person,
                        TREE_LAND_TOOL,
                        arguments.clone(),
                        || {
                            self.tree_land_for_interpreter(
                                tenant_id,
                                execution_id,
                                &tree.person,
                                &arguments,
                            )
                        },
                    )
                    .await?
                {
                    GatedLanding::Landed(landed) => landed,
                    GatedLanding::NotApproved(sentence) => {
                        return Ok(RepositoryActionAnswer {
                            sentence: Some(sentence),
                            ..answer
                        })
                    }
                };
                Ok(RepositoryActionAnswer {
                    commit_sha: landed
                        .get("changeset_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    sentence: landed
                        .get("sentence")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ..answer
                })
            }
            other => Ok(RepositoryActionAnswer {
                sentence: Some(format!(
                    "unknown tree action '{other}': expected tree_diff, tree_write, tree_land, tree_cut, archive_render or archive_push"
                )),
                ..answer
            }),
        }
    }

    /// The run's change against its revision, as unified diffs of each file
    /// added, changed or removed, in path order.
    async fn tree_diff(
        &self,
        tenant_id: &TenantId,
        tree: &TreeOfRun,
    ) -> Result<Result<String, SealSessionError>, RepositoryActionError> {
        let manifest = match self
            .tree_manifest(tree.run, tenant_id, &tree.acting, &tree.tree)
            .await
        {
            Ok(manifest) => manifest,
            Err(e) => return Ok(Err(e)),
        };
        let files = self.files_of(Self::volume_of(tree)?).await?;
        let before: BTreeMap<&str, &ManifestEntry> = manifest
            .iter()
            .filter(|e| e.is_file())
            .map(|e| (e.path.as_str(), e))
            .collect();
        let mut paths: Vec<&str> = files.keys().map(String::as_str).collect();
        paths.extend(before.keys().filter(|p| !files.contains_key(**p)));
        paths.sort_unstable();
        paths.dedup();
        let mut diff = String::new();
        for path in paths {
            let after = files.get(path);
            let old = before.get(path).and_then(|e| e.sha256.as_deref());
            if let (Some(after), Some(old)) = (after, old) {
                if sha256_hex(after) == old {
                    continue;
                }
            }
            let old_bytes = match old {
                Some(sha) => match self
                    .blob_bytes(tree.run, tenant_id, &tree.acting, &tree.tree.workspace, sha)
                    .await
                {
                    Ok(bytes) => Some(bytes),
                    Err(e) => return Ok(Err(e)),
                },
                None => None,
            };
            diff.push_str(&unified_diff(&FileChange {
                path,
                before: old_bytes.as_deref(),
                after: after.map(Vec::as_slice),
            }));
            if diff.len() > TREE_DIFF_MAX_BYTES {
                break;
            }
        }
        Ok(Ok(diff))
    }

    /// The run's tree written to its own lane (AEGIS ADR-147 C5): each file
    /// added or changed stored as a blob, the lane made, and the tree's full
    /// list of entries put on it with `message` as the intent; the trunk is
    /// not written. `Some` of a refusal's sentence when nothing changed.
    async fn tree_write(
        &self,
        tenant_id: &TenantId,
        tree: &TreeOfRun,
        message: &str,
    ) -> Result<Result<Option<String>, SealSessionError>, RepositoryActionError> {
        let manifest = match self
            .tree_manifest(tree.run, tenant_id, &tree.acting, &tree.tree)
            .await
        {
            Ok(manifest) => manifest,
            Err(e) => return Ok(Err(e)),
        };
        let files = self.files_of(Self::volume_of(tree)?).await?;
        let by_path: BTreeMap<&str, &ManifestEntry> =
            manifest.iter().map(|e| (e.path.as_str(), e)).collect();
        let mut entries: BTreeMap<String, Value> = BTreeMap::new();
        let mut changed = false;
        for (path, bytes) in &files {
            let sha = sha256_hex(bytes);
            let entry = by_path.get(path.as_str()).filter(|e| e.is_file());
            let mode = entry.map(|e| e.mode.clone()).unwrap_or_else(regular_mode);
            if entry.and_then(|e| e.sha256.as_deref()) != Some(sha.as_str()) {
                changed = true;
                if let Err(e) = self
                    .put_blob(
                        tree.run,
                        tenant_id,
                        &tree.acting,
                        &tree.tree.workspace,
                        &sha,
                        bytes,
                    )
                    .await
                {
                    return Ok(Err(e));
                }
            }
            entries.insert(
                path.clone(),
                json!({ "path": path, "mode": mode, "sha256": sha }),
            );
        }
        for entry in &manifest {
            if entries.contains_key(&entry.path) {
                continue;
            }
            if entry.is_file() {
                // A file the run removed.
                changed = true;
            } else {
                entries.insert(entry.path.clone(), entry.put_entry(None));
            }
        }
        if !changed {
            return Ok(Ok(Some(NOTHING_TO_WRITE.to_string())));
        }
        let lane = tree_lane(tree.run);
        if let Err(e) = self
            .nuclear_notes(
                tree.run,
                tenant_id,
                &tree.acting,
                &tree.tree.workspace,
                "lanes.create",
                json!({ "label": lane }),
            )
            .await
        {
            // A lane a write of this run made already is written to again.
            if remote_tool_error(&e).is_none() {
                return Ok(Err(e));
            }
        }
        let put = self
            .nuclear_notes(
                tree.run,
                tenant_id,
                &tree.acting,
                &tree.tree.workspace,
                "trees.put",
                json!({
                    "path": tree.tree.path,
                    "entries": entries.into_values().collect::<Vec<_>>(),
                    "intent": message,
                    "lane": lane,
                }),
            )
            .await;
        Ok(put.map(|written| {
            let sequence = written.get("sequence").cloned().unwrap_or_default();
            tracing::info!(
                workflow_execution_id = %tree.run,
                lane = %lane,
                tree = %tree.tree.path,
                sequence = %sequence,
                "A run's tree was written to its lane"
            );
            None
        }))
    }

    /// The landing itself, for the workflow execution `execution_id`'s own
    /// step and its person `person` only (AEGIS ADR-147 C5): what
    /// `aegis.tree.land` runs once the person approved. Lands the run's own
    /// lane with the stored call's intent as one changeset and answers its
    /// id; Nuclear Notes' refusal (a conflict with the trunk among them) is
    /// answered in its own words under `sentence`, nothing landed.
    pub(super) async fn tree_land_for_interpreter(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        person: &str,
        arguments: &Value,
    ) -> Result<Value, SealSessionError> {
        let not_the_workflows =
            || SealSessionError::InvalidArguments(TREE_LAND_IS_THE_WORKFLOWS.to_string());
        let tree = match self.tree_of_run(tenant_id, execution_id.0).await {
            Ok(Ok(tree)) => tree,
            Ok(Err(sentence)) => return Ok(json!({ "sentence": sentence })),
            Err(RepositoryActionError::RunNotFound(_)) => return Err(not_the_workflows()),
            Err(e) => return Err(SealSessionError::InternalError(e.to_string())),
        };
        if tree.person.is_empty() || tree.person != person {
            return Err(not_the_workflows());
        }
        let intent = arguments
            .get("intent")
            .and_then(Value::as_str)
            .filter(|intent| !intent.trim().is_empty())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments("a landing needs its intent".to_string())
            })?;
        let lane = tree_lane(tree.run);
        match self
            .nuclear_notes(
                tree.run,
                tenant_id,
                &tree.acting,
                &tree.tree.workspace,
                "lanes.land",
                json!({ "label": lane, "intent": intent, "dry_run": false }),
            )
            .await
        {
            Ok(landed) => {
                let changeset_id = landed.get("changeset_id").cloned().unwrap_or_default();
                tracing::info!(
                    workflow_execution_id = %tree.run,
                    lane = %lane,
                    tree = %tree.tree.path,
                    changeset_id = %changeset_id,
                    "A run's lane was landed"
                );
                Ok(json!({
                    "changeset_id": changeset_id,
                    "lane": lane,
                    "tree": tree.tree.path,
                }))
            }
            Err(e) => match step_refusal(&e) {
                Some(sentence) => Ok(json!({ "sentence": sentence, "lane": lane })),
                None => Err(e),
            },
        }
    }
}

impl ToolInvocationService {
    /// The cut of the run's tree's workspace (AEGIS ADR-149 C1): a snapshot
    /// of its every entity's current revision, named `name`. Answers
    /// Nuclear Notes' own answer, the snapshot's header and its `cut`.
    async fn tree_cut(
        &self,
        tenant_id: &TenantId,
        tree: &TreeOfRun,
        name: &str,
    ) -> Result<Value, SealSessionError> {
        let cut = self
            .nuclear_notes(
                tree.run,
                tenant_id,
                &tree.acting,
                &tree.tree.workspace,
                "snapshots.cut",
                json!({ "name": name }),
            )
            .await?;
        tracing::info!(
            workflow_execution_id = %tree.run,
            tree = %tree.tree.path,
            cut = %name,
            snapshot_id = %cut.get("snapshot_id").cloned().unwrap_or_default(),
            "A run's workspace was cut"
        );
        Ok(cut)
    }

    /// The render of the cut `cut` of the run's tree into its git archive
    /// (AEGIS ADR-149 C2): the cut's revision rendered, main moved to it
    /// fast-forward only and tagged `nn/<cut>`. Answers Nuclear Notes' own
    /// answer.
    async fn archive_render(
        &self,
        tenant_id: &TenantId,
        tree: &TreeOfRun,
        cut: &str,
    ) -> Result<Value, SealSessionError> {
        let rendered = self
            .nuclear_notes(
                tree.run,
                tenant_id,
                &tree.acting,
                &tree.tree.workspace,
                "archives.render",
                json!({ "tree": tree.tree.path, "cut": cut }),
            )
            .await?;
        tracing::info!(
            workflow_execution_id = %tree.run,
            tree = %tree.tree.path,
            cut = %cut,
            commit_sha1 = %rendered.get("commit_sha1").cloned().unwrap_or_default(),
            "A run's cut was rendered into its tree's git archive"
        );
        Ok(rendered)
    }

    /// The push itself, for the workflow execution `execution_id`'s own step
    /// and its person `person` only (AEGIS ADR-149 C2): what
    /// `aegis.archive.push` runs once the person approved. Queues the push
    /// of the run's tree's git archive to its remote and answers the job;
    /// Nuclear Notes' refusal (no remote, no rendered cut, a role short of
    /// the workspace's admin) is answered in its own words under
    /// `sentence`, nothing queued.
    pub(super) async fn archive_push_for_interpreter(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        person: &str,
        arguments: &Value,
    ) -> Result<Value, SealSessionError> {
        let not_the_workflows =
            || SealSessionError::InvalidArguments(ARCHIVE_PUSH_IS_THE_WORKFLOWS.to_string());
        let tree = match self.tree_of_run(tenant_id, execution_id.0).await {
            Ok(Ok(tree)) => tree,
            Ok(Err(sentence)) => return Ok(json!({ "sentence": sentence })),
            Err(RepositoryActionError::RunNotFound(_)) => return Err(not_the_workflows()),
            Err(e) => return Err(SealSessionError::InternalError(e.to_string())),
        };
        if tree.person.is_empty() || tree.person != person {
            return Err(not_the_workflows());
        }
        if arguments.get("tree").and_then(Value::as_str) != Some(tree.tree.path.as_str()) {
            return Err(not_the_workflows());
        }
        match self
            .nuclear_notes(
                tree.run,
                tenant_id,
                &tree.acting,
                &tree.tree.workspace,
                "archives.push",
                json!({ "tree": tree.tree.path }),
            )
            .await
        {
            Ok(pushed) => {
                tracing::info!(
                    workflow_execution_id = %tree.run,
                    tree = %tree.tree.path,
                    job_id = %pushed.get("job_id").cloned().unwrap_or_default(),
                    queued = %pushed.get("queued").cloned().unwrap_or_default(),
                    "A run's tree's git archive push was queued"
                );
                Ok(pushed)
            }
            Err(e) => match step_refusal(&e) {
                Some(sentence) => Ok(json!({ "sentence": sentence, "tree": tree.tree.path })),
                None => Err(e),
            },
        }
    }
}

#[async_trait::async_trait]
impl RunTrees for ToolInvocationService {
    async fn materialize_for_run(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        contexts: &ExecutionContexts,
        requested: Vec<RunTree>,
    ) -> Result<Vec<RunTree>, RunTreeError> {
        let acting = Self::run_acting(person, None, contexts.clone());
        let mut filled = Vec::with_capacity(requested.len());
        for tree in requested {
            let tree = self.materialize_tree(tenant_id, run, &acting, tree).await?;
            tracing::info!(
                workflow_execution_id = %run,
                tree = %tree.path,
                revision = tree.revision,
                volume_id = ?tree.volume_id,
                blobs = %serde_json::to_string(&tree.blobs).unwrap_or_default(),
                "A run's tree was materialized"
            );
            filled.push(tree);
        }
        Ok(filled)
    }

    async fn mount_for_execution(
        &self,
        tenant_id: &TenantId,
        run: uuid::Uuid,
        execution: ExecutionId,
        mount_point: &str,
        read_only: bool,
    ) -> Result<Option<VolumeMount>, RunTreeError> {
        let tree = match self.tree_of_run(tenant_id, run).await {
            Ok(Ok(tree)) => tree,
            Ok(Err(_)) | Err(RepositoryActionError::RunNotFound(_)) => return Ok(None),
            Err(e) => return Err(RunTreeError::Failed(e.to_string())),
        };
        let Some(volume_id) = tree.volume_id else {
            return Ok(None);
        };
        let volume = self
            .fsal
            .volume_repository()
            .find_by_id(volume_id)
            .await
            .map_err(|e| RunTreeError::Failed(e.to_string()))?
            .ok_or_else(|| {
                RunTreeError::Failed(format!("the run's tree volume {} was not found", volume_id))
            })?;
        let access = if read_only {
            AccessMode::ReadOnly
        } else {
            AccessMode::ReadWrite
        };
        let mount = volume.to_mount(std::path::PathBuf::from(mount_point), access);
        self.volume_registry.register(VolumeRegistration {
            volume_id: mount.volume_id,
            execution_id: execution,
            workflow_execution_id: Some(run),
            run_repository: None,
            container_uid: 1000,
            container_gid: 1000,
            policy: FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: if read_only {
                    Vec::new()
                } else {
                    vec!["/*".to_string()]
                },
            },
            mount_point: mount.mount_point.clone(),
            remote_path: mount.remote_path.clone(),
        });
        Ok(Some(mount))
    }

    fn release_execution(&self, execution: ExecutionId) {
        self.volume_registry.deregister_execution(execution);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nuclear_notes_answer_is_the_text_of_its_result() {
        let answer = tool_answer(json!({
            "content": [{ "type": "text", "text": "{\"changeset_id\":\"c\"}" }]
        }))
        .unwrap();
        assert_eq!(answer, json!({ "changeset_id": "c" }));
        let error = tool_answer(json!({
            "content": [{ "type": "text", "text": "Lane x has 1 conflict(s)" }],
            "isError": true
        }))
        .unwrap_err();
        assert_eq!(remote_tool_error(&error), Some("Lane x has 1 conflict(s)"));
        assert_eq!(tool_answer(json!({ "a": 1 })).unwrap(), json!({ "a": 1 }));
    }

    #[test]
    fn a_tree_path_stays_inside_its_volume() {
        for path in ["README.md", "a/b/c.rs", ".github/workflows/ci.yml"] {
            assert!(safe_tree_path(path), "{path}");
        }
        for path in ["", "/etc/passwd", "a/../b", "./a", "a//b", ".."] {
            assert!(!safe_tree_path(path), "{path}");
        }
    }
}
