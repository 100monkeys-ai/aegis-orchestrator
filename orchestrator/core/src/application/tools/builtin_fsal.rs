use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::FsalError;
use crate::domain::fsal::{AegisFSAL, AegisFileHandle, CreateFsalFileRequest};
use crate::domain::seal_session::{CallerAnswer, InternalFailure, SealSessionError};
use crate::domain::storage::StorageError;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// An FSAL error of a `fs.*` tool, answered by its variant and the caller's
/// own `path` (AEGIS ADR-035, Update of 2026-10-04, R5). What the inner loop
/// and the log see is the text it always had, `"<context>: <error>"`.
fn fsal_refusal(context: &str, path_arg: &str, e: FsalError) -> SealSessionError {
    let answer = match &e {
        FsalError::VolumeNotFound(_)
        | FsalError::Storage(StorageError::FileNotFound(_))
        | FsalError::Storage(StorageError::NotFound(_)) => {
            CallerAnswer::NotFound(format!("Not found: '{path_arg}'."))
        }
        FsalError::Storage(StorageError::AlreadyExists(_)) => {
            CallerAnswer::Conflict(format!("Conflict: '{path_arg}' already exists."))
        }
        FsalError::PathSanitization(_) | FsalError::Storage(StorageError::InvalidPath(_)) => {
            CallerAnswer::InvalidArguments(format!(
                "Invalid tool arguments: '{path_arg}' is not a valid path in a volume."
            ))
        }
        FsalError::QuotaExceeded { .. }
        | FsalError::Storage(StorageError::QuotaExceeded { .. }) => {
            CallerAnswer::QuotaExceeded("Quota exceeded: the volume is full.".to_string())
        }
        FsalError::PolicyViolation(_) => CallerAnswer::PathNotAllowed(format!(
            "Policy violation: '{path_arg}' is not permitted by your filesystem policy."
        )),
        _ => CallerAnswer::Internal(InternalFailure::Server),
    };
    SealSessionError::InternalError(format!("{context}: {e}")).answered(answer)
}

/// Strips the volume mount-point prefix from a container-absolute path.
///
/// Converts paths like `/workspace/solution.py` → `/solution.py` so they are
/// volume-relative before being passed to AegisFSAL. Paths that do not begin
/// with the mount point are returned unchanged.
fn to_volume_relative(mount_point: &Path, path: &str) -> String {
    let base = mount_point
        .to_str()
        .unwrap_or("/workspace")
        .trim_end_matches('/');
    if let Some(stripped) = path.strip_prefix(base) {
        if stripped.is_empty() {
            return "/".to_string();
        }
        if stripped.starts_with('/') {
            return stripped.to_string();
        }
        return format!("/{stripped}");
    }
    path.to_string()
}

/// A one-based line number of an `fs.read` call: absent when the key is
/// missing or null, refused when it is anything but a whole number of 1 or
/// more.
fn line_number_arg(args: &Value, key: &str) -> Result<Option<usize>, SealSessionError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|n| *n >= 1)
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(format!(
                    "Invalid tool arguments: {key} must be a whole number of 1 or more, counted from the file's first line; got {value}."
                ))
            }),
    }
}

/// The answer of `fs.read` for the file's bytes `data`. Without `line_start`
/// and `line_end` it is the whole file. With either, it is the lines from
/// `line_start` (default 1) to `line_end` (default the last), one-based and
/// inclusive, followed by a line naming the range and the file's line count,
/// so that a long file can be read in parts.
fn read_answer(path_arg: &str, data: &[u8], args: &Value) -> Result<Value, SealSessionError> {
    let content = String::from_utf8_lossy(data).to_string();
    let first = line_number_arg(args, "line_start")?;
    let last = line_number_arg(args, "line_end")?;
    if first.is_none() && last.is_none() {
        return Ok(serde_json::json!({
            "status": "success",
            "path": path_arg,
            "content": content,
            "size_bytes": data.len()
        }));
    }
    let first = first.unwrap_or(1);
    if let Some(last) = last {
        if last < first {
            return Err(SealSessionError::InvalidArguments(format!(
                "Invalid tool arguments: line_end ({last}) is before line_start ({first})."
            )));
        }
    }

    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let line_count = lines.len();
    if first > line_count {
        return Ok(serde_json::json!({
            "status": "success",
            "path": path_arg,
            "content": format!(
                "No lines: the range starts at line {first} and {path_arg} has {line_count} lines."
            ),
            "line_start": first,
            "line_end": last.unwrap_or(first),
            "line_count": line_count,
            "size_bytes": data.len()
        }));
    }
    let last = last.unwrap_or(line_count).min(line_count);
    let mut selected: String = lines[first - 1..last].concat();
    if !selected.ends_with('\n') {
        selected.push('\n');
    }
    selected.push_str(&format!(
        "Lines {first} to {last} of {line_count} in {path_arg}."
    ));
    Ok(serde_json::json!({
        "status": "success",
        "path": path_arg,
        "content": selected,
        "line_start": first,
        "line_end": last,
        "line_count": line_count,
        "size_bytes": data.len()
    }))
}

pub async fn invoke_fs_tool(
    tool_name: &str,
    args: &Value,
    execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    volume_registry: &NfsVolumeRegistry,
) -> Result<ToolInvocationResult, SealSessionError> {
    let path_arg = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/workspace");
    // A path under a mount whose registration no longer holds for this
    // execution is refused by the mount's name: resolving it into the
    // `/workspace` volume would read and write the wrong tree.
    if let Some(mount_point) = volume_registry.unregistered_mount_of_path(execution_id, path_arg) {
        return Err(SealSessionError::NotFound(format!(
            "[AEGIS] {path_arg} is under {}, whose volume is not registered for this execution",
            mount_point.display()
        ))
        .answered(CallerAnswer::Internal(InternalFailure::Server)));
    }
    let vol_ctx = volume_registry
        .find_by_execution_and_path(execution_id, path_arg)
        .or_else(|| volume_registry.find_primary_workspace_by_execution(execution_id))
        .ok_or_else(|| {
            // Platform state, not the caller's: the execution's volume is the
            // orchestrator's to register (AEGIS ADR-035, Update of 2026-10-04, R5).
            SealSessionError::NotFound(format!("No volume registered for execution {execution_id}"))
                .answered(CallerAnswer::Internal(InternalFailure::Server))
        })?;

    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    match tool_name {
        "fs.write" => invoke_write(args, execution_id, fsal, vol_ctx).await,
        "fs.read" => {
            let path = to_volume_relative(&vol_ctx.mount_point, path_arg);

            let data = fsal
                .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
                .await
                .map_err(|e| fsal_refusal("FSAL read error", path_arg, e))?;

            read_answer(path_arg, &data, args).map(ToolInvocationResult::Direct)
        }
        "fs.list" => {
            let path = to_volume_relative(&vol_ctx.mount_point, path_arg);

            let entries = fsal
                .readdir(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    &path,
                    &vol_ctx.policy,
                    None,
                    None,
                    None,
                )
                .await
                .map_err(|e| fsal_refusal("FSAL readdir error", path_arg, e))?;

            let entries_json: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "file_type": format!("{:?}", e.file_type),
                    })
                })
                .collect();

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path_arg,
                "entries": entries_json
            })))
        }
        "fs.create_dir" => {
            let path = to_volume_relative(&vol_ctx.mount_point, path_arg);

            fsal.create_directory(
                vol_ctx.execution_id,
                vol_ctx.volume_id,
                &path,
                &vol_ctx.policy,
                None,
                None,
                None,
            )
            .await
            .map_err(|e| fsal_refusal("FSAL create_dir error", path_arg, e))?;

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path_arg
            })))
        }
        "fs.delete" => {
            let path = to_volume_relative(&vol_ctx.mount_point, path_arg);
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if recursive {
                fsal.delete_directory(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    &path,
                    &vol_ctx.policy,
                    None,
                    None,
                    None,
                )
                .await
                .map_err(|e| fsal_refusal("FSAL delete_directory error", path_arg, e))?;
            } else {
                fsal.delete_file(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    &path,
                    &vol_ctx.policy,
                    None,
                    None,
                    None,
                )
                .await
                .map_err(|e| fsal_refusal("FSAL delete_file error", path_arg, e))?;
            }

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path_arg
            })))
        }
        "fs.edit" => invoke_edit(args, execution_id, fsal, vol_ctx).await,
        "fs.multi_edit" => invoke_multi_edit(args, execution_id, fsal, vol_ctx).await,
        "fs.grep" => invoke_grep(args, execution_id, fsal, vol_ctx).await,
        "fs.glob" => invoke_glob(args, execution_id, fsal, vol_ctx).await,
        _ => Err(SealSessionError::InvalidArguments(format!(
            "Unknown fs tool: {tool_name}"
        ))),
    }
}

/// The replacement an `fs.edit` writes: when every line ending of the file
/// before the edit is CRLF, the replacement's bare LF become CRLF, so the
/// edited lines keep the file's ending. A file with no line ending, or with
/// mixed endings, takes the replacement as sent.
fn keep_crlf_line_ending<'a>(content: &str, replacement: &'a str) -> std::borrow::Cow<'a, str> {
    let line_feeds = content.matches('\n').count();
    let crlf = content.matches("\r\n").count();
    if line_feeds == 0 || crlf != line_feeds || !replacement.contains('\n') {
        return std::borrow::Cow::Borrowed(replacement);
    }
    let mut converted = String::with_capacity(replacement.len() + replacement.len() / 16);
    let mut previous = None;
    for c in replacement.chars() {
        if c == '\n' && previous != Some('\r') {
            converted.push('\r');
        }
        converted.push(c);
        previous = Some(c);
    }
    std::borrow::Cow::Owned(converted)
}

/// `fs.write`: a new file is created with the content as sent. An existing
/// file is overwritten in place and cut to the new length, and its line
/// ending is kept by the rule `fs.edit` follows: when every line ending of
/// the file is CRLF, the content's bare LF become CRLF and the answer says
/// so. A file with mixed endings takes the content as sent.
async fn invoke_write(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SealSessionError> {
    let path_arg = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/workspace");
    let path = to_volume_relative(&vol_ctx.mount_point, path_arg);
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");

    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    let existing = fsal
        .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .ok();

    let (content, crlf_kept) = match &existing {
        Some(data) => {
            let kept = keep_crlf_line_ending(&String::from_utf8_lossy(data), content);
            let changed = kept.len() != content.len();
            (kept, changed)
        }
        None => (std::borrow::Cow::Borrowed(content), false),
    };

    let bytes_written = if existing.is_some() {
        let written = fsal
            .write(&handle, &path, &vol_ctx.policy, 0, content.as_bytes())
            .await
            .map_err(|e| fsal_refusal("FSAL write error", path_arg, e))?;
        fsal.truncate(&handle, &path, &vol_ctx.policy, content.len() as u64)
            .await
            .map_err(|e| fsal_refusal("FSAL truncate error", path_arg, e))?;
        written
    } else {
        let _file_handle = fsal
            .create_file(CreateFsalFileRequest {
                execution_id: vol_ctx.execution_id,
                volume_id: vol_ctx.volume_id,
                path: &path,
                policy: &vol_ctx.policy,
                emit_event: false,
                caller_node_id: None,
                host_node_id: None,
                workflow_execution_id: None,
            })
            .await
            .map_err(|e| fsal_refusal("FSAL create_file error", path_arg, e))?;
        fsal.write(&handle, &path, &vol_ctx.policy, 0, content.as_bytes())
            .await
            .map_err(|e| fsal_refusal("FSAL write error", path_arg, e))?
    };

    let mut answer = serde_json::json!({
        "status": "success",
        "path": path_arg,
        "bytes_written": bytes_written
    });
    if crlf_kept {
        answer["message"] = Value::String("[AEGIS] line endings kept as CRLF".to_string());
    }
    Ok(ToolInvocationResult::Direct(answer))
}

async fn invoke_edit(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SealSessionError> {
    let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let path = to_volume_relative(&vol_ctx.mount_point, path_arg);
    let target = args
        .get("target_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let replacement = args
        .get("replacement_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    // Read current content
    let data = fsal
        .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .map_err(|e| fsal_refusal("Edit error (read)", path_arg, e))?;

    let content = String::from_utf8_lossy(&data).to_string();

    if !content.contains(target) {
        return Err(SealSessionError::InvalidArguments(
            "Target content not found in file".to_string(),
        ));
    }

    // Check for multiple occurrences
    let occurrences = content.matches(target).count();
    if occurrences > 1 {
        return Err(SealSessionError::InvalidArguments(
            "Target content exists multiple times in file. Be more specific.".to_string(),
        ));
    }

    let replacement = keep_crlf_line_ending(&content, replacement);
    let new_content = content.replace(target, &replacement);

    // Write back, then cut the file to the new length: a write at offset 0
    // leaves the old bytes past the new end in place.
    let _ = fsal
        .write(&handle, &path, &vol_ctx.policy, 0, new_content.as_bytes())
        .await
        .map_err(|e| fsal_refusal("Edit error (write)", path_arg, e))?;
    fsal.truncate(&handle, &path, &vol_ctx.policy, new_content.len() as u64)
        .await
        .map_err(|e| fsal_refusal("Edit error (truncate)", path_arg, e))?;

    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "path": path_arg,
        "message": "File edited successfully"
    })))
}

async fn invoke_multi_edit(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SealSessionError> {
    let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let path = to_volume_relative(&vol_ctx.mount_point, path_arg);
    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    let data = fsal
        .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .map_err(|e| fsal_refusal("Multi-edit error (read)", path_arg, e))?;

    let mut content = String::from_utf8(data)
        .map_err(|_| SealSessionError::InvalidArguments("File is not valid UTF-8".to_string()))?;

    let edits = args
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            SealSessionError::InvalidArguments("Missing or invalid 'edits' array".to_string())
        })?;

    let mut success_count = 0;

    for edit in edits {
        let target = edit
            .get("target_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replacement = edit
            .get("replacement_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if content.contains(target) {
            let occurrences = content.matches(target).count();
            if occurrences == 1 {
                content = content.replace(target, replacement);
                success_count += 1;
            } else {
                return Err(SealSessionError::InvalidArguments(format!(
                    "Target content '{target}' exists multiple times in file. Be more specific."
                )));
            }
        } else {
            return Err(SealSessionError::InvalidArguments(format!(
                "Target content '{target}' not found in file."
            )));
        }
    }

    // Write out the changes completely replacing the old payload
    // To ensure we don't leave trailing bytes, we must truncate or recreate.
    // fs.write writes at offset 0, but if the new file is smaller, SeaweedFS/OpenDAL
    // might not truncate by default. AegisFSAL create_file will truncate SeaweedFS (usually).
    // Let's call create_file to truncate, then write
    let _ = fsal
        .create_file(CreateFsalFileRequest {
            execution_id: vol_ctx.execution_id,
            volume_id: vol_ctx.volume_id,
            path: &path,
            policy: &vol_ctx.policy,
            emit_event: false,
            caller_node_id: None,
            host_node_id: None,
            workflow_execution_id: None,
        })
        .await
        .map_err(|e| fsal_refusal("Multi-edit error (truncate)", path_arg, e))?;

    let _ = fsal
        .write(&handle, &path, &vol_ctx.policy, 0, content.as_bytes())
        .await
        .map_err(|e| fsal_refusal("Multi-edit error (write)", path_arg, e))?;

    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "path": path_arg,
        "edits_applied": success_count
    })))
}

async fn invoke_grep(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SealSessionError> {
    let pattern_str = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let path = to_volume_relative(&vol_ctx.mount_point, path_arg);

    // Parse regex
    let regex = regex::Regex::new(pattern_str)
        .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid regex pattern: {e}")))?;

    // We will do a recursive walk using AegisFSAL
    let mut matches = Vec::new();
    let mut stack = vec![path];
    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    while let Some(current_dir) = stack.pop() {
        if matches.len() > 1000 {
            break;
        } // safety limit

        let entries = match fsal
            .readdir(
                vol_ctx.execution_id,
                vol_ctx.volume_id,
                &current_dir,
                &vol_ctx.policy,
                None,
                None,
                None,
            )
            .await
        {
            Ok(e) => e,
            Err(_) => continue, // skip unreadable dirs
        };

        for entry in entries {
            let full_path = if current_dir == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", current_dir, entry.name)
            };

            if entry.file_type == crate::domain::storage::FileType::Directory {
                stack.push(full_path);
            } else if entry.file_type == crate::domain::storage::FileType::File {
                // Read file
                if let Ok(data) = fsal
                    .read(&handle, &full_path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
                    .await
                {
                    if let Ok(content) = String::from_utf8(data) {
                        for (i, line) in content.lines().enumerate() {
                            if regex.is_match(line) {
                                matches.push(serde_json::json!({
                                    "file": full_path,
                                    "line_number": i + 1,
                                    "line": line.trim()
                                }));
                                if matches.len() > 1000 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "matches": matches,
        "count": matches.len()
    })))
}

async fn invoke_glob(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SealSessionError> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
    let path_arg = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");
    let path = to_volume_relative(&vol_ctx.mount_point, path_arg);

    // We will do a generic recursive walk using AegisFSAL
    // For simplicity, we just do string suffix/prefix matching or Regex if globset isn't used
    // A simple glob-to-regex converter:
    let regex_pattern = pattern
        .replace(".", "\\.")
        .replace("*", ".*")
        .replace("?", ".");
    let regex = regex::Regex::new(&format!("^{regex_pattern}$"))
        .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid glob pattern: {e}")))?;

    let mut matches = Vec::new();
    let mut stack = vec![path];

    while let Some(current_dir) = stack.pop() {
        if matches.len() > 1000 {
            break;
        }

        let entries = match fsal
            .readdir(
                vol_ctx.execution_id,
                vol_ctx.volume_id,
                &current_dir,
                &vol_ctx.policy,
                None,
                None,
                None,
            )
            .await
        {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries {
            let full_path = if current_dir == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{}/{}", current_dir, entry.name)
            };

            // Match against filename (like standard glob) or full path (if pattern has /)
            let target_str = if pattern.contains('/') {
                &full_path
            } else {
                &entry.name
            };

            if regex.is_match(target_str) {
                matches.push(full_path.clone());
            }

            if entry.file_type == crate::domain::storage::FileType::Directory {
                stack.push(full_path);
            }
        }
    }

    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "matches": matches,
        "count": matches.len()
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A path under a mount whose volume holds no registration for the
    /// calling execution is refused by the mount's name; it is never
    /// resolved into the execution's `/workspace` volume.
    #[tokio::test]
    async fn a_path_under_a_mount_unregistered_for_the_caller_is_refused_by_name() {
        use crate::application::nfs_gateway::EventBusPublisher;
        use crate::application::nfs_gateway::{NfsGatewayService, VolumeRegistration};
        use crate::domain::fsal::FsalAccessPolicy;
        use crate::domain::volume::VolumeId;
        use crate::infrastructure::event_bus::EventBus;
        use crate::infrastructure::repositories::InMemoryVolumeRepository;
        use crate::infrastructure::storage::LocalHostStorageProvider;

        let storage_root = tempfile::tempdir().unwrap();
        let gateway = NfsGatewayService::new(
            Arc::new(LocalHostStorageProvider::new(storage_root.path()).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(EventBusPublisher::new(Arc::new(
                EventBus::with_default_capacity(),
            ))),
            Some(0),
        );
        let coder = ExecutionId::new();
        let workspace = VolumeId::new();
        let repository = VolumeId::new();
        for (volume_id, mount_point) in [(workspace, "/workspace"), (repository, "/repo")] {
            gateway.register_volume(VolumeRegistration {
                volume_id,
                execution_id: coder,
                workflow_execution_id: None,
                container_uid: 1000,
                container_gid: 1000,
                policy: FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: vec!["/*".to_string()],
                },
                mount_point: std::path::PathBuf::from(mount_point),
                remote_path: String::new(),
            });
        }
        // Another step's end deregistered the repository volume.
        gateway.deregister_volume(repository);

        let answer = invoke_fs_tool(
            "fs.list",
            &json!({"path": "/repo/.git"}),
            coder,
            gateway.fsal(),
            gateway.volume_registry(),
        )
        .await;

        let sentence =
            "[AEGIS] /repo/.git is under /repo, whose volume is not registered for this execution";
        match answer {
            Err(refused) => assert!(
                refused.to_string().contains(sentence),
                "the refusal does not name the unregistered mount: {refused}"
            ),
            Ok(ToolInvocationResult::Direct(listed)) => {
                panic!("the path under /repo was resolved into another volume: {listed}")
            }
            Ok(_) => panic!("the path under /repo was resolved into another volume"),
        }
    }

    /// A file of `n` lines, `line 1` to `line n`, each ending in a newline.
    fn numbered_lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn fs_read_with_a_range_returns_only_those_lines_and_names_the_range() {
        let file = numbered_lines(20);
        let answer = read_answer(
            "/workspace/notes.txt",
            file.as_bytes(),
            &json!({"path": "/workspace/notes.txt", "line_start": 5, "line_end": 8}),
        )
        .expect("a range inside the file is read");
        let content = answer["content"].as_str().unwrap_or_default();
        assert_eq!(
            content,
            "line 5\nline 6\nline 7\nline 8\nLines 5 to 8 of 20 in /workspace/notes.txt.",
            "fs.read with line_start 5 and line_end 8 must return those four lines and a line naming the range and the file's line count"
        );
        assert_eq!(answer["line_start"], json!(5));
        assert_eq!(answer["line_end"], json!(8));
        assert_eq!(answer["line_count"], json!(20));
        assert_eq!(answer["size_bytes"], json!(file.len()));
    }

    #[test]
    fn fs_read_without_a_range_returns_the_whole_file_as_before() {
        let file = numbered_lines(20);
        let answer = read_answer(
            "/workspace/notes.txt",
            file.as_bytes(),
            &json!({"path": "/workspace/notes.txt"}),
        )
        .expect("a whole file is read");
        assert_eq!(
            answer,
            json!({
                "status": "success",
                "path": "/workspace/notes.txt",
                "content": file,
                "size_bytes": file.len()
            })
        );
    }

    #[test]
    fn fs_read_range_open_at_one_end_reads_from_the_first_or_to_the_last_line() {
        let file = numbered_lines(20);
        let to_end = read_answer("/w/f", file.as_bytes(), &json!({"line_start": 19}))
            .expect("line_start alone reads to the end");
        assert_eq!(
            to_end["content"],
            json!("line 19\nline 20\nLines 19 to 20 of 20 in /w/f.")
        );
        let from_start = read_answer("/w/f", file.as_bytes(), &json!({"line_end": 2}))
            .expect("line_end alone reads from line 1");
        assert_eq!(
            from_start["content"],
            json!("line 1\nline 2\nLines 1 to 2 of 20 in /w/f.")
        );
    }

    #[test]
    fn fs_read_range_past_the_end_is_cut_at_the_last_line_or_says_the_file_is_shorter() {
        let file = "a\nb\nc"; // three lines, the last with no newline
        let cut = read_answer(
            "/w/f",
            file.as_bytes(),
            &json!({"line_start": 2, "line_end": 99}),
        )
        .expect("a range running past the end is read to the last line");
        assert_eq!(cut["content"], json!("b\nc\nLines 2 to 3 of 3 in /w/f."));
        assert_eq!(cut["line_end"], json!(3));
        let beyond = read_answer(
            "/w/f",
            file.as_bytes(),
            &json!({"line_start": 10, "line_end": 12}),
        )
        .expect("a range after the last line is answered, not refused");
        assert_eq!(
            beyond["content"],
            json!("No lines: the range starts at line 10 and /w/f has 3 lines.")
        );
    }

    #[test]
    fn fs_read_refuses_a_range_that_is_not_one_based_or_runs_backwards() {
        let file = numbered_lines(5);
        for args in [
            json!({"line_start": 0}),
            json!({"line_end": -1}),
            json!({"line_start": "2"}),
            json!({"line_start": 4, "line_end": 2}),
        ] {
            assert!(
                matches!(
                    read_answer("/w/f", file.as_bytes(), &args),
                    Err(SealSessionError::InvalidArguments(_))
                ),
                "fs.read must refuse {args} as invalid arguments"
            );
        }
    }

    /// An `fs.edit` over a host-path volume of its own: the FSAL on a scratch
    /// root, the execution owning the volume, and the context the tool is
    /// handed.
    struct EditFixture {
        root: tempfile::TempDir,
        volume: crate::domain::volume::VolumeId,
        fsal: Arc<AegisFSAL>,
        context: crate::infrastructure::nfs::server::NfsVolumeContext,
    }

    struct NoStorageEvents;

    #[async_trait::async_trait]
    impl crate::domain::fsal::EventPublisher for NoStorageEvents {
        async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
    }

    async fn edit_fixture(file: &str, bytes: &[u8]) -> EditFixture {
        use crate::domain::repository::VolumeRepository;
        use crate::domain::volume::{StorageClass, Volume, VolumeBackend, VolumeOwnership};

        let root = tempfile::tempdir().expect("scratch root");
        let execution = ExecutionId::new();
        let volumes =
            Arc::new(crate::infrastructure::repositories::InMemoryVolumeRepository::new());
        let mut volume = Volume::new(
            "workspace".to_string(),
            crate::domain::tenant::TenantId::consumer(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: std::path::PathBuf::from("/placeholder"),
            },
            64 * 1024 * 1024,
            VolumeOwnership::execution(execution),
        )
        .expect("volume");
        volume.backend = VolumeBackend::HostPath {
            path: std::path::PathBuf::from(format!("/hosts/{}", volume.id)),
        };
        volume.mark_available().expect("available");
        volumes.save(&volume).await.expect("saved");
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(
                crate::infrastructure::storage::LocalHostStorageProvider::new(root.path())
                    .expect("storage root"),
            ),
            volumes as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let fixture = EditFixture {
            root,
            volume: volume.id,
            fsal,
            context: crate::infrastructure::nfs::server::NfsVolumeContext {
                execution_id: execution,
                volume_id: volume.id,
                workflow_execution_id: None,
                container_uid: 1000,
                container_gid: 1000,
                policy: crate::domain::fsal::FsalAccessPolicy {
                    read: vec!["/**".to_string()],
                    write: vec!["/**".to_string()],
                },
                mount_point: std::path::PathBuf::from("/workspace"),
                remote_path: String::new(),
            },
        };
        let on_disk = fixture.on_disk(file);
        std::fs::create_dir_all(on_disk.parent().expect("a parent")).expect("dirs");
        std::fs::write(on_disk, bytes).expect("written");
        fixture
    }

    impl EditFixture {
        fn on_disk(&self, file: &str) -> std::path::PathBuf {
            self.root
                .path()
                .join(format!("hosts/{}", self.volume))
                .join(file)
        }

        async fn edit(&self, file: &str, target: &str, replacement: &str) -> Vec<u8> {
            invoke_edit(
                &json!({
                    "path": format!("/workspace/{file}"),
                    "target_content": target,
                    "replacement_content": replacement,
                }),
                self.context.execution_id,
                &self.fsal,
                self.context.clone(),
            )
            .await
            .expect("the edit is applied");
            std::fs::read(self.on_disk(file)).expect("the edited file")
        }

        /// `fs.write` of `content` to `file`: the bytes on disk after it and
        /// the tool's answer.
        async fn write(&self, file: &str, content: &str) -> (Vec<u8>, Value) {
            let answer = invoke_write(
                &json!({
                    "path": format!("/workspace/{file}"),
                    "content": content,
                }),
                self.context.execution_id,
                &self.fsal,
                self.context.clone(),
            )
            .await
            .expect("the write is applied");
            let ToolInvocationResult::Direct(answer) = answer else {
                panic!("fs.write answers directly");
            };
            let on_disk = std::fs::read(self.on_disk(file)).expect("the written file");
            (on_disk, answer)
        }
    }

    /// Run `e946aeab`'s README edit: the file ends every line in CRLF, the
    /// model's replacement of one line carries bare LF. Before the fix the
    /// text was replaced as sent, so the new lines ended in LF and the old
    /// line's `\r` was left after the new last line.
    #[tokio::test]
    async fn fs_edit_keeps_a_crlf_files_line_ending_for_an_lf_replacement() {
        let readme =
            "# Examples\r\n\r\n**Learn by example. Build with confidence.**\r\n\r\nMore.\r\n";
        let fixture = edit_fixture("README.md", readme.as_bytes()).await;

        let edited = fixture
            .edit(
                "README.md",
                "**Learn by example. Build with confidence.**",
                "**Learn by example. Build with confidence.**\n\nThis line shows the loop end-to-end.",
            )
            .await;

        let edited = String::from_utf8(edited).expect("utf-8");
        assert_eq!(
            edited,
            "# Examples\r\n\r\n**Learn by example. Build with confidence.**\r\n\r\nThis line shows the loop end-to-end.\r\n\r\nMore.\r\n",
            "an LF replacement in a file whose every line ends in CRLF must be written with CRLF, leaving no stray \\r"
        );
        assert!(
            !edited.replace("\r\n", "").contains('\r')
                && !edited.replace("\r\n", "").contains('\n'),
            "every line ending must be CRLF: {edited:?}"
        );
    }

    /// A file with mixed line endings is edited as sent.
    #[tokio::test]
    async fn fs_edit_leaves_a_mixed_ending_files_replacement_as_sent() {
        let fixture = edit_fixture("mixed.txt", b"one\r\ntwo\nthree\r\n").await;
        let edited = fixture.edit("mixed.txt", "two", "2a\n2b").await;
        assert_eq!(edited, b"one\r\n2a\n2b\nthree\r\n");
    }

    /// An edit that shortens a file leaves no stale tail: before the fix the
    /// shorter text was written at offset 0 over the old bytes and the file
    /// kept its old length.
    #[tokio::test]
    async fn fs_edit_that_shortens_a_file_leaves_no_stale_tail() {
        let fixture = edit_fixture("notes.txt", b"alpha beta gamma delta\n").await;
        let edited = fixture
            .edit("notes.txt", "alpha beta gamma delta", "alpha")
            .await;
        assert_eq!(
            String::from_utf8_lossy(&edited),
            "alpha\n",
            "the edited file must end where the new text ends"
        );
    }

    /// Run `6b8987b3`'s README: CODE rewrote the whole file with `fs.write`
    /// and every CRLF line became LF. A write over a file whose every line
    /// ends in CRLF keeps that ending, as `fs.edit` does, and says so.
    #[tokio::test]
    async fn fs_write_over_a_crlf_file_keeps_its_crlf_line_ending() {
        let fixture = edit_fixture("README.md", b"# Examples\r\n\r\nOne.\r\n").await;

        let (written, answer) = fixture
            .write("README.md", "# Examples\n\nOne.\nTwo.\n")
            .await;

        assert_eq!(
            String::from_utf8_lossy(&written),
            "# Examples\r\n\r\nOne.\r\nTwo.\r\n",
            "an LF write over a file whose every line ends in CRLF must be written with CRLF"
        );
        assert_eq!(
            answer["message"], "[AEGIS] line endings kept as CRLF",
            "the answer names the kept line ending: {answer}"
        );
    }

    /// A new file is written as sent, and the answer says nothing of line
    /// endings.
    #[tokio::test]
    async fn fs_write_of_a_new_file_writes_the_content_as_sent() {
        let fixture = edit_fixture("README.md", b"# Examples\r\n").await;

        let (written, answer) = fixture.write("notes/new.txt", "one\ntwo\n").await;

        assert_eq!(written, b"one\ntwo\n");
        assert!(answer.get("message").is_none(), "{answer}");
    }

    /// A write over a file with mixed line endings is written as sent.
    #[tokio::test]
    async fn fs_write_over_a_mixed_ending_file_writes_the_content_as_sent() {
        let fixture = edit_fixture("mixed.txt", b"one\r\ntwo\nthree\r\n").await;

        let (written, answer) = fixture.write("mixed.txt", "1\n2\n").await;

        assert_eq!(written, b"1\n2\n");
        assert!(answer.get("message").is_none(), "{answer}");
    }
}
