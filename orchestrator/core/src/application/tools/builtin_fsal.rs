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
pub(crate) fn fsal_refusal(context: &str, path_arg: &str, e: FsalError) -> SealSessionError {
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

/// The answer of `fs.read`, `fs.edit`, `fs.multi_edit` and `fs.write` to a
/// `path_arg` that is a directory: the tool is not run, and the refusal names
/// the tool that lists a directory. The caller is answered the sentence, and
/// the inner loop sees it as an invalid-arguments refusal; it carries the
/// caller's own path and never the storage path.
fn directory_refusal(tool: &str, path_arg: &str) -> SealSessionError {
    let sentence = format!(
        "[AEGIS] {tool} was not run: {path_arg} is a directory, not a file; fs.list lists a directory."
    );
    SealSessionError::InvalidArguments(sentence.clone())
        .answered(CallerAnswer::InvalidArguments(sentence))
}

/// The answer of `fs.write`, `fs.edit`, `fs.multi_edit`, `fs.create_dir` and
/// `fs.delete` to a `path_arg` outside the run's repository mounts `mounts`,
/// for a workflow's agent state whose run holds repositories (AEGIS ADR-141,
/// Update of 2026-10-15): the tool is not run, and the refusal names the
/// caller's path and every mount it may write under.
fn outside_repository_refusal(path_arg: &str, mounts: &[std::path::PathBuf]) -> SealSessionError {
    let under = mounts
        .iter()
        .map(|mount| mount.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sentence = format!(
        "'{path_arg}' is outside this run's repository; this step writes only under {under}"
    );
    SealSessionError::InvalidArguments(sentence.clone())
        .answered(CallerAnswer::InvalidArguments(sentence))
}

/// [`fsal_refusal`] for the read of a file by `tool`, a directory refused by
/// [`directory_refusal`].
fn file_read_refusal(tool: &str, context: &str, path_arg: &str, e: FsalError) -> SealSessionError {
    match e {
        FsalError::Storage(StorageError::IsDirectory(_)) => directory_refusal(tool, path_arg),
        e => fsal_refusal(context, path_arg, e),
    }
}

/// Strips the volume mount-point prefix from a container-absolute path.
///
/// Converts paths like `/workspace/solution.py` → `/solution.py` so they are
/// volume-relative before being passed to AegisFSAL. Paths that do not begin
/// with the mount point are returned unchanged.
pub(crate) fn to_volume_relative(mount_point: &Path, path: &str) -> String {
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

/// The volume a `fs.*` tool's `path_arg` is read from or written to: the
/// execution's most specific registered mount above it, else its primary
/// `/workspace` volume.
pub(crate) fn resolve_volume(
    execution_id: ExecutionId,
    path_arg: &str,
    volume_registry: &NfsVolumeRegistry,
) -> Result<crate::infrastructure::nfs::server::NfsVolumeContext, SealSessionError> {
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
    volume_registry
        .find_by_execution_and_path(execution_id, path_arg)
        .or_else(|| volume_registry.find_primary_workspace_by_execution(execution_id))
        .ok_or_else(|| {
            // Platform state, not the caller's: the execution's volume is the
            // orchestrator's to register (AEGIS ADR-035, Update of 2026-10-04, R5).
            SealSessionError::NotFound(format!("No volume registered for execution {execution_id}"))
                .answered(CallerAnswer::Internal(InternalFailure::Server))
        })
}

pub async fn invoke_fs_tool(
    tool_name: &str,
    args: &Value,
    execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    volume_registry: &NfsVolumeRegistry,
) -> Result<ToolInvocationResult, SealSessionError> {
    // `fs.search` resolves its own root: with no `path` it searches the
    // run's repository (AEGIS ADR-145 S2).
    if tool_name == "fs.search" {
        return super::builtin_search::invoke_search(args, execution_id, fsal, volume_registry)
            .await;
    }
    let path_arg = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("/workspace");
    let vol_ctx = resolve_volume(execution_id, path_arg, volume_registry)?;

    // A workflow's agent state whose run holds repositories writes only
    // under them (AEGIS ADR-141, Update of 2026-10-15): refused before any
    // storage call, so nothing outside is created, changed or deleted.
    if matches!(
        tool_name,
        "fs.write" | "fs.edit" | "fs.multi_edit" | "fs.create_dir" | "fs.delete"
    ) && vol_ctx.run_repository.is_none()
    {
        let mounts = volume_registry.run_repository_mounts(execution_id);
        if !mounts.is_empty() {
            return Err(outside_repository_refusal(path_arg, &mounts));
        }
    }

    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    match tool_name {
        "fs.write" => invoke_write(args, execution_id, fsal, vol_ctx).await,
        "fs.read" => {
            let path = to_volume_relative(&vol_ctx.mount_point, path_arg);

            let data = fsal
                .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
                .await
                .map_err(|e| file_read_refusal("fs.read", "FSAL read error", path_arg, e))?;

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

/// What an `fs.edit` or `fs.multi_edit` target matched: the byte range in
/// the file, the replacement to write there, and the note the answer
/// carries when the target's escaped line endings were decoded.
struct TargetMatch<'a> {
    start: usize,
    end: usize,
    replacement: std::borrow::Cow<'a, str>,
    escapes_decoded: bool,
}

/// Why a target matched nothing it could replace.
enum TargetMiss {
    NotFound(String),
    Multiple,
}

const ESCAPES_DECODED: &str = "[AEGIS] the target's escaped line endings were read as newlines";

/// The single occurrence of `target` in `content` as a byte range: first
/// byte for byte, then with every CRLF of the file and of the target read
/// as LF, the match mapped back to the file's bytes.
fn find_once(content: &str, target: &str) -> Result<Option<(usize, usize)>, TargetMiss> {
    match content.matches(target).count() {
        1 => {
            let start = content.find(target).unwrap_or_default();
            return Ok(Some((start, start + target.len())));
        }
        0 => {}
        _ => return Err(TargetMiss::Multiple),
    }
    if !content.contains("\r\n") {
        return Ok(None);
    }
    // The file with each CRLF read as LF, and for every byte of it the
    // offset of the same byte in the file (one past the end last).
    let mut normalised = String::with_capacity(content.len());
    let mut offsets = Vec::with_capacity(content.len() + 1);
    let bytes = content.as_bytes();
    for (i, c) in content.char_indices() {
        if c == '\r' && bytes.get(i + 1) == Some(&b'\n') {
            continue;
        }
        normalised.push(c);
        offsets.extend((0..c.len_utf8()).map(|k| i + k));
    }
    offsets.push(content.len());
    let target = target.replace("\r\n", "\n");
    if target.is_empty() {
        return Ok(None);
    }
    match normalised.matches(target.as_str()).count() {
        1 => {
            let at = normalised.find(target.as_str()).unwrap_or_default();
            // The match ends one past its last byte, before any `\r` the
            // file holds after it.
            Ok(Some((offsets[at], offsets[at + target.len() - 1] + 1)))
        }
        0 => Ok(None),
        _ => Err(TargetMiss::Multiple),
    }
}

/// A match never splits a CRLF pair of the file. A span that opens on the
/// LF of a pair takes its `\r`, and a replacement that opens on a bare LF
/// gets that `\r` back; a span that closes on the `\r` of a pair takes its
/// LF, and a replacement that closes on a `\r` gets that LF back. A target
/// matched byte for byte can open on the LF of a CRLF: without this the
/// file's `\r` stayed before the replacement, which wrote its own line
/// ending after it, leaving `\r\r\n`.
fn keep_crlf_pairs<'a>(
    content: &str,
    start: usize,
    end: usize,
    replacement: std::borrow::Cow<'a, str>,
) -> (usize, usize, std::borrow::Cow<'a, str>) {
    let bytes = content.as_bytes();
    let (mut start, mut end, mut replacement) = (start, end, replacement);
    if start > 0 && bytes.get(start) == Some(&b'\n') && bytes[start - 1] == b'\r' {
        start -= 1;
        if replacement.starts_with('\n') {
            replacement.to_mut().insert(0, '\r');
        }
    }
    if end > start && bytes[end - 1] == b'\r' && bytes.get(end) == Some(&b'\n') {
        end += 1;
        if replacement.ends_with('\r') {
            replacement.to_mut().push('\n');
        }
    }
    (start, end, replacement)
}

/// Whether `text` carries a backslash followed by `r`, `n` or `t`.
fn has_escaped_whitespace(text: &str) -> bool {
    text.contains("\\r") || text.contains("\\n") || text.contains("\\t")
}

/// `text` with each two-character `\r`, `\n` and `\t` read as the character
/// it names; a doubled backslash is kept as it is.
fn decode_escaped_whitespace(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            decoded.push(c);
            continue;
        }
        match chars.peek() {
            Some('r') => decoded.push('\r'),
            Some('n') => decoded.push('\n'),
            Some('t') => decoded.push('\t'),
            Some('\\') => decoded.push_str("\\\\"),
            _ => {
                decoded.push(c);
                continue;
            }
        }
        chars.next();
    }
    decoded
}

/// Cut a line for an answer: at most 200 characters.
fn shown_line(line: &str) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.chars().count() > 200 {
        format!("{}…", line.chars().take(200).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The sentence for a target found nowhere in the file: the first line of
/// the target that differs from the file where the target's opening lines
/// match it best, with that line's number and the file's text.
fn first_differing_line(content: &str, target: &str) -> String {
    let file_lines: Vec<&str> = content
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let target = target.replace("\r\n", "\n");
    let target_lines: Vec<&str> = target.split('\n').collect();
    let first = target_lines[0];
    let last = target_lines.len() - 1;
    // The target's line k against the file's line: its first line may start
    // mid-line, its last may end mid-line, a one-line target sits anywhere.
    let line_matches = |k: usize, line: &str| -> bool {
        let t = target_lines[k];
        match (k == 0, k == last) {
            (true, true) => line.contains(t),
            (true, false) => line.ends_with(t),
            (false, true) => line.starts_with(t),
            (false, false) => line == t,
        }
    };
    let mut best: Option<(usize, usize)> = None;
    for start in 0..file_lines.len() {
        if !line_matches(0, file_lines[start]) {
            continue;
        }
        let run = (0..target_lines.len())
            .take_while(|&k| {
                file_lines
                    .get(start + k)
                    .is_some_and(|line| line_matches(k, line))
            })
            .count();
        if best.is_none_or(|(_, b)| run > b) {
            best = Some((start, run));
        }
    }
    match best {
        None => format!(
            "Target content not found in file: no line of the file contains the target's first line {:?}.",
            shown_line(first)
        ),
        Some((start, run)) => match file_lines.get(start + run) {
            Some(line) => format!(
                "Target content not found in file: the target's first {run} line(s) match the file from line {}, then line {} of the file reads {:?} where the target has {:?}.",
                start + 1,
                start + run + 1,
                shown_line(line),
                shown_line(target_lines[run])
            ),
            None => format!(
                "Target content not found in file: the target's first {run} line(s) match the file from line {}, and the file ends there where the target goes on with {:?}.",
                start + 1,
                shown_line(target_lines[run])
            ),
        },
    }
}

/// Where an `fs.edit` or `fs.multi_edit` target falls in `content`, and the
/// replacement to write there with the file's line ending
/// ([`keep_crlf_line_ending`]). The target is looked up as sent, then with
/// line endings normalised; when it is still not found and the target or
/// the replacement carries an escaped `\r`, `\n` or `\t`, both are decoded
/// once and the lookup runs again. Either way the match keeps the file's
/// CRLF pairs whole ([`keep_crlf_pairs`]).
fn locate_target<'a>(
    content: &str,
    target: &str,
    replacement: &'a str,
) -> Result<TargetMatch<'a>, TargetMiss> {
    if let Some((start, end)) = find_once(content, target)? {
        let (start, end, replacement) = keep_crlf_pairs(
            content,
            start,
            end,
            keep_crlf_line_ending(content, replacement),
        );
        return Ok(TargetMatch {
            start,
            end,
            replacement,
            escapes_decoded: false,
        });
    }
    if has_escaped_whitespace(target) || has_escaped_whitespace(replacement) {
        let decoded_target = decode_escaped_whitespace(target);
        if let Some((start, end)) = find_once(content, &decoded_target)? {
            let decoded = decode_escaped_whitespace(replacement);
            let replacement = keep_crlf_line_ending(content, &decoded).into_owned();
            let (start, end, replacement) =
                keep_crlf_pairs(content, start, end, std::borrow::Cow::Owned(replacement));
            return Ok(TargetMatch {
                start,
                end,
                replacement,
                escapes_decoded: true,
            });
        }
        return Err(TargetMiss::NotFound(first_differing_line(
            content,
            &decoded_target,
        )));
    }
    Err(TargetMiss::NotFound(first_differing_line(content, target)))
}

/// The `target_content` and `replacement_content` of an `fs.edit` call or
/// of one `fs.multi_edit` edit, or, when either is missing, not a string or
/// empty, the keys it sent. A missing key must not read as an empty target,
/// which matches everywhere.
fn edit_pair(edit: &Value) -> Result<(&str, &str), String> {
    fn text<'v>(edit: &'v Value, key: &str) -> Option<&'v str> {
        edit.get(key)
            .and_then(|v| v.as_str())
            .filter(|text| !text.is_empty())
    }
    match (
        text(edit, "target_content"),
        text(edit, "replacement_content"),
    ) {
        (Some(target), Some(replacement)) => Ok((target, replacement)),
        _ => {
            let keys: Vec<String> = edit
                .as_object()
                .map(|object| object.keys().map(|key| format!("`{key}`")).collect())
                .unwrap_or_default();
            Err(if keys.is_empty() {
                "no keys".to_string()
            } else {
                keys.join(", ")
            })
        }
    }
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

    // A file that cannot be read is created; a directory is refused.
    let existing = match fsal
        .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
    {
        Ok(data) => Some(data),
        Err(FsalError::Storage(StorageError::IsDirectory(_))) => {
            return Err(directory_refusal("fs.write", path_arg));
        }
        Err(_) => None,
    };

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
    let (target, replacement) = edit_pair(args).map_err(|sent| {
        SealSessionError::InvalidArguments(format!(
            "[AEGIS] fs.edit needs `target_content` and `replacement_content`; the call sent {sent}"
        ))
    })?;

    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    // Read current content
    let data = fsal
        .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .map_err(|e| file_read_refusal("fs.edit", "Edit error (read)", path_arg, e))?;

    let content = String::from_utf8_lossy(&data).to_string();

    let found = locate_target(&content, target, replacement).map_err(|miss| match miss {
        TargetMiss::NotFound(sentence) => SealSessionError::InvalidArguments(sentence),
        TargetMiss::Multiple => SealSessionError::InvalidArguments(
            "Target content exists multiple times in file. Be more specific.".to_string(),
        ),
    })?;
    let new_content = format!(
        "{}{}{}",
        &content[..found.start],
        found.replacement,
        &content[found.end..]
    );

    // Write back, then cut the file to the new length: a write at offset 0
    // leaves the old bytes past the new end in place.
    let _ = fsal
        .write(&handle, &path, &vol_ctx.policy, 0, new_content.as_bytes())
        .await
        .map_err(|e| fsal_refusal("Edit error (write)", path_arg, e))?;
    fsal.truncate(&handle, &path, &vol_ctx.policy, new_content.len() as u64)
        .await
        .map_err(|e| fsal_refusal("Edit error (truncate)", path_arg, e))?;

    let message = if found.escapes_decoded {
        format!("File edited successfully. {ESCAPES_DECODED}")
    } else {
        "File edited successfully".to_string()
    };
    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "path": path_arg,
        "message": message
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

    let edits = args
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            SealSessionError::InvalidArguments("Missing or invalid 'edits' array".to_string())
        })?;
    let pairs = edits
        .iter()
        .enumerate()
        .map(|(i, edit)| {
            edit_pair(edit).map_err(|sent| {
                SealSessionError::InvalidArguments(format!(
                    "[AEGIS] fs.multi_edit needs `target_content` and `replacement_content` in every edit; edit {} of {} sent {sent}",
                    i + 1,
                    edits.len()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let data = fsal
        .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .map_err(|e| file_read_refusal("fs.multi_edit", "Multi-edit error (read)", path_arg, e))?;

    let mut content = String::from_utf8(data)
        .map_err(|_| SealSessionError::InvalidArguments("File is not valid UTF-8".to_string()))?;

    let mut success_count = 0;
    let mut escapes_decoded = false;

    for (target, replacement) in pairs {
        match locate_target(&content, target, replacement) {
            Ok(found) => {
                escapes_decoded |= found.escapes_decoded;
                content = format!(
                    "{}{}{}",
                    &content[..found.start],
                    found.replacement,
                    &content[found.end..]
                );
                success_count += 1;
            }
            Err(TargetMiss::Multiple) => {
                return Err(SealSessionError::InvalidArguments(format!(
                    "Target content '{target}' exists multiple times in file. Be more specific."
                )));
            }
            Err(TargetMiss::NotFound(sentence)) => {
                return Err(SealSessionError::InvalidArguments(format!(
                    "Edit {} of {}: {sentence}",
                    success_count + 1,
                    edits.len()
                )));
            }
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

    let mut answer = serde_json::json!({
        "status": "success",
        "path": path_arg,
        "edits_applied": success_count
    });
    if escapes_decoded {
        answer["message"] = Value::String(ESCAPES_DECODED.to_string());
    }
    Ok(ToolInvocationResult::Direct(answer))
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

    let mut matches = Vec::new();
    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    // A file path searches that file: listing it as a directory fails, and
    // the walk skipped it, answering 0 for text the file holds.
    let is_file = fsal
        .getattr(
            vol_ctx.execution_id,
            vol_ctx.volume_id,
            &path,
            vol_ctx.container_uid,
            vol_ctx.container_gid,
            vol_ctx.workflow_execution_id,
        )
        .await
        .is_ok_and(|attrs| attrs.file_type == crate::domain::storage::FileType::File);
    if is_file {
        let data = fsal
            .read(&handle, &path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
            .await
            .map_err(|e| fsal_refusal("FSAL read error", path_arg, e))?;
        if let Ok(content) = String::from_utf8(data) {
            for (i, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    matches.push(serde_json::json!({
                        "file": path,
                        "line_number": i + 1,
                        "line": line.trim()
                    }));
                    if matches.len() > 1000 {
                        break;
                    }
                }
            }
        }
        return Ok(ToolInvocationResult::Direct(serde_json::json!({
            "status": "success",
            "matches": matches,
            "count": matches.len()
        })));
    }

    // We will do a recursive walk using AegisFSAL
    let mut stack = vec![path];

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
                run_repository: None,
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
                run_repository: None,
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

        /// `fs.edit` of `target` to `replacement` in `file`: the tool's answer
        /// or its refusal, and the bytes on disk after it.
        async fn edit_answer(
            &self,
            file: &str,
            target: &str,
            replacement: &str,
        ) -> (Result<Value, SealSessionError>, Vec<u8>) {
            let answer = invoke_edit(
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
            .map(|answer| match answer {
                ToolInvocationResult::Direct(answer) => answer,
                _ => panic!("fs.edit answers directly"),
            });
            let on_disk = std::fs::read(self.on_disk(file)).expect("the file");
            (answer, on_disk)
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

    /// Run `250cda5c`'s README: a multi-line target sent with bare LF in a
    /// file whose every line ends in CRLF. Before the fix the target was
    /// looked up byte for byte and was not found.
    #[tokio::test]
    async fn fs_edit_finds_an_lf_target_in_a_crlf_file_and_keeps_crlf() {
        let readme = "# Examples\r\n\r\nOne line.\r\nTwo line.\r\n\r\nMore.\r\n";
        let fixture = edit_fixture("README.md", readme.as_bytes()).await;

        let (answer, edited) = fixture
            .edit_answer(
                "README.md",
                "One line.\nTwo line.",
                "One line.\nNew line.\nTwo line.",
            )
            .await;

        answer.expect("an LF target matches the file's CRLF lines");
        assert_eq!(
            String::from_utf8_lossy(&edited),
            "# Examples\r\n\r\nOne line.\r\nNew line.\r\nTwo line.\r\n\r\nMore.\r\n",
            "the match is replaced in the file's bytes and the replacement written with CRLF"
        );
    }

    /// Run `250cda5c`'s second CODE visit: the target carried the two
    /// characters backslash and r, backslash and n, where the file has real
    /// CRLF, and ten edits were answered "Target content not found in file".
    #[tokio::test]
    async fn fs_edit_reads_a_targets_escaped_line_endings_as_newlines() {
        let readme = "- API keys (OpenAI, GitHub, etc.)\r\n\r\n\r\nThis change validates.\r\n\r\n\r\n## Next\r\n";
        let fixture = edit_fixture("README.md", readme.as_bytes()).await;

        let (answer, edited) = fixture
            .edit_answer(
                "README.md",
                "etc.)\n\\r\\n\\r\\nThis change validates.\\r\\n\\r\\n\\r\\n## Next",
                "etc.)\\r\\n\\r\\nThis change validates.\\r\\n\\r\\n## Next",
            )
            .await;

        let answer = answer.expect("the escaped target is decoded and matched");
        assert_eq!(
            String::from_utf8_lossy(&edited),
            "- API keys (OpenAI, GitHub, etc.)\r\n\r\nThis change validates.\r\n\r\n## Next\r\n",
            "the decoded replacement is written with the file's CRLF"
        );
        assert!(
            answer["message"]
                .as_str()
                .unwrap_or_default()
                .contains("[AEGIS] the target's escaped line endings were read as newlines"),
            "the answer names the decoding: {answer}"
        );
    }

    /// A target that is still not found answers with the first line that
    /// differs, its number and the file's text, so the model can see the
    /// mismatch; the file is left as it was.
    #[tokio::test]
    async fn fs_edit_of_a_target_not_found_names_the_first_differing_line() {
        let fixture = edit_fixture("notes.txt", b"alpha\r\nbeta\r\ngamma\r\n").await;

        let (answer, on_disk) = fixture
            .edit_answer("notes.txt", "alpha\nbeta\ndelta", "x")
            .await;

        let refusal = answer
            .expect_err("the target is not in the file")
            .to_string();
        assert!(
            refusal.contains("Target content not found in file")
                && refusal.contains("line 3 of the file reads \"gamma\"")
                && refusal.contains("\"delta\""),
            "the refusal names the first differing line: {refusal}"
        );
        assert_eq!(on_disk, b"alpha\r\nbeta\r\ngamma\r\n");
    }

    /// `fs.multi_edit` matches as `fs.edit` does: its first edit, an LF
    /// target in a CRLF file, is found, and its second, found nowhere, is
    /// answered with the first differing line. The refusal comes before
    /// anything is written, so the file is left as it was.
    #[tokio::test]
    async fn fs_multi_edit_finds_an_lf_target_in_a_crlf_file_and_names_a_miss() {
        let fixture = edit_fixture("notes.txt", b"one\r\ntwo\r\nthree\r\n").await;

        let refusal = invoke_multi_edit(
            &json!({
                "path": "/workspace/notes.txt",
                "edits": [
                    { "target_content": "one\ntwo", "replacement_content": "one\n2\ntwo" },
                    { "target_content": "two\nfour", "replacement_content": "x" },
                ],
            }),
            fixture.context.execution_id,
            &fixture.fsal,
            fixture.context.clone(),
        )
        .await
        .expect_err("the second edit is not in the file")
        .to_string();

        assert!(
            refusal.contains("Edit 2 of 2")
                && refusal.contains("line 4 of the file reads \"three\""),
            "the first edit matched and the second names its differing line: {refusal}"
        );
        let on_disk = std::fs::read(fixture.on_disk("notes.txt")).expect("the file");
        assert_eq!(on_disk, b"one\r\ntwo\r\nthree\r\n");
    }

    /// Run `53ae66ac`'s first edit: the target opens on the LF of a CRLF
    /// line ending and runs to the end of a line. Before the fix the match
    /// began at that LF, so the file's `\r` stayed before it and the
    /// replacement's own CRLF followed it, leaving `\r\r\n`; a replacement
    /// that drops the line ending left the `\r` alone before the text.
    #[tokio::test]
    async fn fs_edit_on_a_crlf_file_leaves_no_lone_cr() {
        let see = "See the [Workflow Manifest Reference](https://docs.100monkeys.ai/docs/reference/workflow-manifest) and the [Building Workflows guide](https://docs.100monkeys.ai/docs/guides/building-workflows) for full documentation.";
        let sentence = "You can also deploy and run the the-forge workflow using `aegis workflow deploy ./agents/workflows/the-forge.yaml`.";
        let readme = format!(
            "aegis workflow status <execution-id> --follow\r\n```\r\n\r\n{see}\r\n\r\nMore.\r\n"
        );
        let lone_cr = |text: &str| text.replace("\r\n", "").contains('\r');

        let fixture = edit_fixture("README.md", readme.as_bytes()).await;
        let edited = fixture
            .edit(
                "README.md",
                &format!("\n{see}"),
                &format!("\n{sentence}\n\n{see}"),
            )
            .await;
        let edited = String::from_utf8(edited).expect("utf-8");
        assert!(!lone_cr(&edited), "the edit left a lone \\r: {edited:?}");
        assert_eq!(
            edited,
            format!("aegis workflow status <execution-id> --follow\r\n```\r\n\r\n{sentence}\r\n\r\n{see}\r\n\r\nMore.\r\n")
        );

        let fixture = edit_fixture("README.md", readme.as_bytes()).await;
        let joined = fixture.edit("README.md", &format!("\n{see}"), see).await;
        let joined = String::from_utf8(joined).expect("utf-8");
        assert!(!lone_cr(&joined), "the edit left a lone \\r: {joined:?}");
        assert_eq!(
            joined,
            format!(
                "aegis workflow status <execution-id> --follow\r\n```\r\n{see}\r\n\r\nMore.\r\n"
            )
        );
    }

    /// Run `53ae66ac` sent four `fs.edit` calls with `target` in place of
    /// `target_content`; the missing key read as an empty target, which
    /// matched everywhere and was answered "exists multiple times". A call
    /// without both keys is refused before the file is read, naming the keys
    /// it sent; `fs.multi_edit` refuses an edit the same way.
    #[tokio::test]
    async fn fs_edit_without_target_content_is_refused_naming_the_keys_sent() {
        let fixture = edit_fixture("notes.txt", b"one\r\ntwo\r\n").await;

        let refusal = invoke_edit(
            &json!({
                "path": "/workspace/notes.txt",
                "target": "one",
                "replacement": "1",
            }),
            fixture.context.execution_id,
            &fixture.fsal,
            fixture.context.clone(),
        )
        .await
        .expect_err("the call names neither key")
        .to_string();
        assert!(
            refusal.contains("[AEGIS] fs.edit needs `target_content` and `replacement_content`; the call sent `path`, `replacement`, `target`"),
            "the refusal names the keys the call sent: {refusal}"
        );

        let (answer, _) = fixture.edit_answer("notes.txt", "", "1").await;
        let refusal = answer.expect_err("an empty target").to_string();
        assert!(
            refusal.contains("[AEGIS] fs.edit needs `target_content` and `replacement_content`; the call sent `path`, `replacement_content`, `target_content`"),
            "an empty target is refused the same way: {refusal}"
        );

        let refusal = invoke_multi_edit(
            &json!({
                "path": "/workspace/notes.txt",
                "edits": [
                    { "target_content": "one", "replacement_content": "1" },
                    { "target": "two", "replacement_content": "2" },
                ],
            }),
            fixture.context.execution_id,
            &fixture.fsal,
            fixture.context.clone(),
        )
        .await
        .expect_err("the second edit names no target_content")
        .to_string();
        assert!(
            refusal.contains("[AEGIS] fs.multi_edit needs `target_content` and `replacement_content` in every edit; edit 2 of 2 sent `replacement_content`, `target`"),
            "the refusal names the edit and the keys it sent: {refusal}"
        );
        let on_disk = std::fs::read(fixture.on_disk("notes.txt")).expect("the file");
        assert_eq!(on_disk, b"one\r\ntwo\r\n");
    }

    /// Run `53ae66ac`'s CODE child grepped `README.md` by its own path and
    /// was answered 0 for text the same pattern found in it by its
    /// directory: the walk listed the path as a directory, and a file
    /// cannot be listed. A file path searches that file.
    #[tokio::test]
    async fn fs_grep_with_a_file_path_searches_that_file() {
        let fixture = edit_fixture(
            "README.md",
            b"# Examples\r\n```bash\r\nSee the [Workflow Manifest Reference](x)\r\n",
        )
        .await;

        let answer = invoke_grep(
            &json!({
                "path": "/workspace/README.md",
                "pattern": "See the \\[Workflow Manifest Reference\\]",
            }),
            fixture.context.execution_id,
            &fixture.fsal,
            fixture.context.clone(),
        )
        .await
        .expect("the grep runs");
        let ToolInvocationResult::Direct(answer) = answer else {
            panic!("fs.grep answers directly");
        };
        assert_eq!(answer["count"], 1, "the file holds the text once: {answer}");
        assert_eq!(answer["matches"][0]["line_number"], 3, "{answer}");
        assert_eq!(answer["matches"][0]["file"], "/README.md", "{answer}");
    }

    /// The refusal of a `fs.*` call naming a directory: the tool called and
    /// the `path` the agent sent, verbatim.
    fn directory_sentence(tool: &str, path: &str) -> String {
        format!("[AEGIS] {tool} was not run: {path} is a directory, not a file; fs.list lists a directory.")
    }

    /// A host-path volume holding a directory `sub/` (with one file in it),
    /// registered at `/workspace` for the fixture's execution, as a run's
    /// volume is when its tools are called.
    async fn directory_fixture() -> (EditFixture, NfsVolumeRegistry) {
        let fixture = edit_fixture("sub/kept.txt", b"kept\n").await;
        let registry = NfsVolumeRegistry::new();
        registry.register(crate::application::nfs_gateway::VolumeRegistration {
            volume_id: fixture.volume,
            execution_id: fixture.context.execution_id,
            workflow_execution_id: None,
            run_repository: None,
            container_uid: 1000,
            container_gid: 1000,
            policy: fixture.context.policy.clone(),
            mount_point: std::path::PathBuf::from("/workspace"),
            remote_path: String::new(),
        });
        (fixture, registry)
    }

    /// Asserts `answer` is the directory refusal of `tool` for `path`: the
    /// caller's answer is the sentence, the inner loop's text is the
    /// sentence as an invalid-arguments refusal, and neither names the
    /// storage root.
    fn assert_directory_refusal(
        answer: Result<ToolInvocationResult, SealSessionError>,
        tool: &str,
        path: &str,
        storage_root: &std::path::Path,
    ) {
        let sentence = directory_sentence(tool, path);
        let refused = match answer {
            Err(refused) => refused,
            Ok(ToolInvocationResult::Direct(ran)) => {
                panic!("{tool} of the directory {path} was run: {ran}")
            }
            Ok(_) => panic!("{tool} of the directory {path} was run"),
        };
        let shown = refused.to_string();
        let answered = refused.refusal().message;
        assert_eq!(
            (shown.as_str(), answered.as_str()),
            (
                format!("Invalid tool arguments: {sentence}").as_str(),
                sentence.as_str()
            ),
            "{tool} of the directory {path} was not refused naming fs.list"
        );
        let root = storage_root.display().to_string();
        assert!(
            !shown.contains(&root) && !answered.contains(&root) && !shown.contains("/hosts/"),
            "the refusal of {tool} names the storage path: {shown}"
        );
    }

    /// Regression (fs-read-of-volume-root-returns-filer-html-2026-10-14): an
    /// `fs.read` of a directory was answered by the provider, the local one
    /// with its IO error and the filer with its HTML listing. It is refused
    /// with the sentence naming `fs.list`.
    #[tokio::test]
    async fn fs_read_of_a_directory_is_refused_naming_fs_list() {
        let (fixture, registry) = directory_fixture().await;

        for path in ["/workspace", "/workspace/sub"] {
            let answer = invoke_fs_tool(
                "fs.read",
                &json!({"path": path}),
                fixture.context.execution_id,
                &fixture.fsal,
                &registry,
            )
            .await;
            assert_directory_refusal(answer, "fs.read", path, fixture.root.path());
        }
    }

    /// The tools that read a file before they change it refuse a directory
    /// the same way, and change nothing: `fs.write` no longer takes the
    /// failed read for a missing file and tries to create one in its place.
    #[tokio::test]
    async fn fs_edit_and_fs_write_of_a_directory_are_refused_and_change_nothing() {
        let (fixture, registry) = directory_fixture().await;
        let path = "/workspace/sub";

        for (tool, args) in [
            (
                "fs.edit",
                json!({"path": path, "target_content": "kept", "replacement_content": "gone"}),
            ),
            (
                "fs.multi_edit",
                json!({"path": path, "edits": [{"target_content": "kept", "replacement_content": "gone"}]}),
            ),
            ("fs.write", json!({"path": path, "content": "written\n"})),
        ] {
            let answer = invoke_fs_tool(
                tool,
                &args,
                fixture.context.execution_id,
                &fixture.fsal,
                &registry,
            )
            .await;
            assert_directory_refusal(answer, tool, path, fixture.root.path());
        }

        assert!(
            fixture.on_disk("sub").is_dir(),
            "sub/ is no longer a directory"
        );
        assert_eq!(
            std::fs::read(fixture.on_disk("sub/kept.txt")).expect("sub/kept.txt"),
            b"kept\n",
            "the file in sub/ changed"
        );
    }

    /// A run's volumes as a workflow's agent state holds them: the agent's own
    /// volume at `/workspace` and, when `repository` is set, the run's
    /// repository `app` at `/workspace/app`. Each volume holds `x` ("kept\n").
    /// A new file is written into a new directory: the local provider refuses
    /// to create a parent directory that already exists.
    struct RunFixture {
        root: tempfile::TempDir,
        execution: ExecutionId,
        workspace: crate::domain::volume::VolumeId,
        repository: crate::domain::volume::VolumeId,
        fsal: Arc<AegisFSAL>,
        registry: NfsVolumeRegistry,
    }

    impl RunFixture {
        async fn new(repository_label: Option<&str>) -> Self {
            use crate::domain::repository::VolumeRepository;
            use crate::domain::volume::{StorageClass, Volume, VolumeBackend, VolumeOwnership};

            let root = tempfile::tempdir().expect("scratch root");
            let execution = ExecutionId::new();
            let volumes =
                Arc::new(crate::infrastructure::repositories::InMemoryVolumeRepository::new());
            let mut ids = Vec::new();
            for name in ["workspace", "repository"] {
                let mut volume = Volume::new(
                    name.to_string(),
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
                let dir = root.path().join(format!("hosts/{}", volume.id));
                std::fs::create_dir_all(&dir).expect("dirs");
                std::fs::write(dir.join("x"), b"kept\n").expect("x written");
                ids.push(volume.id);
            }
            let fsal = Arc::new(AegisFSAL::new(
                Arc::new(
                    crate::infrastructure::storage::LocalHostStorageProvider::new(root.path())
                        .expect("storage root"),
                ),
                volumes as Arc<dyn VolumeRepository>,
                Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
                Arc::new(NoStorageEvents),
            ));
            let registry = NfsVolumeRegistry::new();
            for (volume_id, mount_point, run_repository) in [
                (ids[0], "/workspace", None),
                (
                    ids[1],
                    "/workspace/app",
                    repository_label.map(str::to_string),
                ),
            ] {
                registry.register(crate::application::nfs_gateway::VolumeRegistration {
                    volume_id,
                    execution_id: execution,
                    workflow_execution_id: Some(uuid::Uuid::new_v4()),
                    run_repository,
                    container_uid: 1000,
                    container_gid: 1000,
                    policy: crate::domain::fsal::FsalAccessPolicy {
                        read: vec!["/**".to_string()],
                        write: vec!["/**".to_string()],
                    },
                    mount_point: std::path::PathBuf::from(mount_point),
                    remote_path: String::new(),
                });
            }
            Self {
                root,
                execution,
                workspace: ids[0],
                repository: ids[1],
                fsal,
                registry,
            }
        }

        fn on_disk(
            &self,
            volume: crate::domain::volume::VolumeId,
            file: &str,
        ) -> std::path::PathBuf {
            self.root.path().join(format!("hosts/{volume}")).join(file)
        }

        async fn call(
            &self,
            tool: &str,
            args: Value,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            invoke_fs_tool(tool, &args, self.execution, &self.fsal, &self.registry).await
        }
    }

    /// Asserts `answer` is the refusal of a write to `path` outside the run's
    /// repository at `/workspace/app`: the caller and the inner loop are both
    /// given the sentence naming the path and the mount.
    fn assert_outside_repository_refusal(
        answer: Result<ToolInvocationResult, SealSessionError>,
        tool: &str,
        path: &str,
    ) {
        let sentence = format!(
            "'{path}' is outside this run's repository; this step writes only under /workspace/app"
        );
        let refused = match answer {
            Err(refused) => refused,
            Ok(ToolInvocationResult::Direct(ran)) => {
                panic!("{tool} of {path}, outside the run's repository, was run: {ran}")
            }
            Ok(_) => panic!("{tool} of {path}, outside the run's repository, was run"),
        };
        assert_eq!(
            (refused.to_string(), refused.refusal().message),
            (format!("Invalid tool arguments: {sentence}"), sentence),
            "{tool} of {path} was not refused as outside the run's repository"
        );
    }

    /// Regression (forge-coder-writes-outside-repository-and-claims-pass-2026-10-14):
    /// the Forge's coder wrote `/workspace/README.md`, outside the run's
    /// repository at `/workspace/aegis-examples`, and the write landed in the
    /// agent's own volume. It is refused, and nothing is written.
    #[tokio::test]
    async fn a_run_states_write_outside_its_repository_is_refused_and_nothing_is_written() {
        let run = RunFixture::new(Some("app")).await;

        let answer = run
            .call(
                "fs.write",
                json!({"path": "/workspace/README.md", "content": "# Examples\n"}),
            )
            .await;

        assert_outside_repository_refusal(answer, "fs.write", "/workspace/README.md");
        assert!(
            !run.on_disk(run.workspace, "README.md").exists(),
            "README.md was written to the agent's own volume"
        );
    }

    /// Every tool that writes refuses a path outside the run's repository the
    /// same way, and a write with no `path` (the agent's `/workspace`) too;
    /// nothing is created, changed or deleted.
    #[tokio::test]
    async fn each_writing_fs_tool_outside_the_repository_is_refused() {
        let run = RunFixture::new(Some("app")).await;

        for (tool, path, args) in [
            (
                "fs.edit",
                "/workspace/x",
                json!({"path": "/workspace/x", "target_content": "kept", "replacement_content": "gone"}),
            ),
            (
                "fs.multi_edit",
                "/workspace/x",
                json!({"path": "/workspace/x", "edits": [{"target_content": "kept", "replacement_content": "gone"}]}),
            ),
            (
                "fs.create_dir",
                "/workspace/made",
                json!({"path": "/workspace/made"}),
            ),
            ("fs.delete", "/workspace/x", json!({"path": "/workspace/x"})),
            ("fs.write", "/workspace", json!({"content": "written\n"})),
        ] {
            let answer = run.call(tool, args).await;
            assert_outside_repository_refusal(answer, tool, path);
        }

        assert_eq!(
            std::fs::read(run.on_disk(run.workspace, "x")).expect("x is still there"),
            b"kept\n",
            "x in the agent's own volume changed"
        );
        assert!(
            !run.on_disk(run.workspace, "made").exists(),
            "made/ was created in the agent's own volume"
        );
        assert_eq!(
            outside_repository_refusal(
                "/tmp/y",
                &[
                    std::path::PathBuf::from("/workspace/a"),
                    std::path::PathBuf::from("/workspace/b")
                ]
            )
            .refusal()
            .message,
            "'/tmp/y' is outside this run's repository; this step writes only under /workspace/a, /workspace/b",
            "several mounts are named comma-separated"
        );
    }

    /// A write under the run's repository is written there, as before.
    #[tokio::test]
    async fn a_run_states_write_inside_its_repository_is_written() {
        let run = RunFixture::new(Some("app")).await;

        let answer = run
            .call(
                "fs.write",
                json!({"path": "/workspace/app/docs/README.md", "content": "# Examples\n"}),
            )
            .await;

        assert!(
            answer.is_ok(),
            "the write under the repository was refused: {:?}",
            answer.err().map(|e| e.to_string())
        );
        assert_eq!(
            std::fs::read(run.on_disk(run.repository, "docs/README.md"))
                .expect("README.md in the repository"),
            b"# Examples\n"
        );
    }

    /// Reading outside the run's repository is unchanged.
    #[tokio::test]
    async fn reading_outside_the_repository_is_unchanged() {
        let run = RunFixture::new(Some("app")).await;

        let Ok(ToolInvocationResult::Direct(read)) =
            run.call("fs.read", json!({"path": "/workspace/x"})).await
        else {
            panic!("fs.read of /workspace/x was not answered");
        };
        assert_eq!(read["content"], "kept\n", "{read}");

        let Ok(ToolInvocationResult::Direct(listed)) =
            run.call("fs.list", json!({"path": "/workspace"})).await
        else {
            panic!("fs.list of /workspace was not answered");
        };
        assert!(
            listed.to_string().contains("\"x\""),
            "x is not listed: {listed}"
        );
    }

    /// An execution with no run repository registered writes its own
    /// `/workspace` as before.
    #[tokio::test]
    async fn an_execution_without_run_repositories_writes_its_workspace_as_before() {
        let run = RunFixture::new(None).await;

        let answer = run
            .call(
                "fs.write",
                json!({"path": "/workspace/docs/README.md", "content": "# Examples\n"}),
            )
            .await;

        assert!(
            answer.is_ok(),
            "the write was refused: {:?}",
            answer.err().map(|e| e.to_string())
        );
        assert_eq!(
            std::fs::read(run.on_disk(run.workspace, "docs/README.md"))
                .expect("README.md in /workspace"),
            b"# Examples\n"
        );
    }
}
