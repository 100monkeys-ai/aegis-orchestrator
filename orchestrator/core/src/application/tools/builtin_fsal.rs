use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{AegisFSAL, AegisFileHandle};
use crate::domain::smcp_session::SmcpSessionError;
use serde_json::Value;
use std::sync::Arc;

pub async fn invoke_fs_tool(
    tool_name: &str,
    args: &Value,
    execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    volume_registry: &NfsVolumeRegistry,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let vol_ctx = volume_registry
        .find_by_execution(execution_id)
        .ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(format!(
                "No volume registered for execution {}",
                execution_id
            ))
        })?;

    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    match tool_name {
        "fs.write" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");

            let _file_handle = fsal
                .create_file(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    path,
                    &vol_ctx.policy,
                )
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "FSAL create_file error: {}",
                        e
                    ))
                })?;

            let bytes_written = fsal
                .write(&handle, path, &vol_ctx.policy, 0, content.as_bytes())
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "FSAL write error: {}",
                        e
                    ))
                })?;

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path,
                "bytes_written": bytes_written
            })))
        }
        "fs.read" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

            let data = fsal
                .read(&handle, path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!("FSAL read error: {}", e))
                })?;

            let content = String::from_utf8_lossy(&data).to_string();
            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path,
                "content": content,
                "size_bytes": data.len()
            })))
        }
        "fs.list" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");

            let entries = fsal
                .readdir(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    path,
                    &vol_ctx.policy,
                )
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "FSAL readdir error: {}",
                        e
                    ))
                })?;

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
                "path": path,
                "entries": entries_json
            })))
        }
        "fs.create_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");

            fsal.create_directory(
                vol_ctx.execution_id,
                vol_ctx.volume_id,
                path,
                &vol_ctx.policy,
            )
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!(
                    "FSAL create_dir error: {}",
                    e
                ))
            })?;

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path
            })))
        }
        "fs.delete" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if recursive {
                fsal.delete_directory(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    path,
                    &vol_ctx.policy,
                )
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "FSAL delete_directory error: {}",
                        e
                    ))
                })?;
            } else {
                fsal.delete_file(
                    vol_ctx.execution_id,
                    vol_ctx.volume_id,
                    path,
                    &vol_ctx.policy,
                )
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "FSAL delete_file error: {}",
                        e
                    ))
                })?;
            }

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "path": path
            })))
        }
        "fs.edit" => invoke_edit(args, execution_id, fsal, vol_ctx).await,
        "fs.multi_edit" => invoke_multi_edit(args, execution_id, fsal, vol_ctx).await,
        "fs.grep" => invoke_grep(args, execution_id, fsal, vol_ctx).await,
        "fs.glob" => invoke_glob(args, execution_id, fsal, vol_ctx).await,
        _ => Err(SmcpSessionError::SignatureVerificationFailed(format!(
            "Unknown fs tool: {}",
            tool_name
        ))),
    }
}

async fn invoke_edit(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
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
        .read(&handle, path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Edit error (read): {}", e))
        })?;

    let content = String::from_utf8_lossy(&data).to_string();

    if !content.contains(target) {
        return Err(SmcpSessionError::SignatureVerificationFailed(
            "Target content not found in file".to_string(),
        ));
    }

    // Check for multiple occurrences
    let occurrences = content.matches(target).count();
    if occurrences > 1 {
        return Err(SmcpSessionError::SignatureVerificationFailed(
            "Target content exists multiple times in file. Be more specific.".to_string(),
        ));
    }

    let new_content = content.replace(target, replacement);

    // Write back
    let _ = fsal
        .write(&handle, path, &vol_ctx.policy, 0, new_content.as_bytes())
        .await
        .map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Edit error (write): {}", e))
        })?;

    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "path": path,
        "message": "File edited successfully"
    })))
}

async fn invoke_multi_edit(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let handle = AegisFileHandle::new(vol_ctx.execution_id, vol_ctx.volume_id, "/");

    let data = fsal
        .read(&handle, path, &vol_ctx.policy, 0, 10 * 1024 * 1024)
        .await
        .map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Multi-edit error (read): {}", e))
        })?;

    let mut content = String::from_utf8(data).map_err(|_| {
        SmcpSessionError::SignatureVerificationFailed("File is not valid UTF-8".to_string())
    })?;

    let edits = args
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "Missing or invalid 'edits' array".to_string(),
            )
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
                return Err(SmcpSessionError::SignatureVerificationFailed(format!(
                    "Target content '{}' exists multiple times in file. Be more specific.",
                    target
                )));
            }
        } else {
            return Err(SmcpSessionError::SignatureVerificationFailed(format!(
                "Target content '{}' not found in file.",
                target
            )));
        }
    }

    // Write out the changes completely replacing the old payload
    // To ensure we don't leave trailing bytes, we must truncate or recreate.
    // fs.write writes at offset 0, but if the new file is smaller, SeaweedFS/OpenDAL
    // might not truncate by default. AegisFSAL create_file will truncate SeaweedFS (usually).
    // Let's call create_file to truncate, then write
    let _ = fsal
        .create_file(
            vol_ctx.execution_id,
            vol_ctx.volume_id,
            path,
            &vol_ctx.policy,
        )
        .await
        .map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!(
                "Multi-edit error (truncate): {}",
                e
            ))
        })?;

    let _ = fsal
        .write(&handle, path, &vol_ctx.policy, 0, content.as_bytes())
        .await
        .map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!(
                "Multi-edit error (write): {}",
                e
            ))
        })?;

    Ok(ToolInvocationResult::Direct(serde_json::json!({
        "status": "success",
        "path": path,
        "edits_applied": success_count
    })))
}

async fn invoke_grep(
    args: &Value,
    _execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    vol_ctx: crate::infrastructure::nfs::server::NfsVolumeContext,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let pattern_str = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");

    // Parse regex
    let regex = regex::Regex::new(pattern_str).map_err(|e| {
        SmcpSessionError::SignatureVerificationFailed(format!("Invalid regex pattern: {}", e))
    })?;

    // We will do a recursive walk using AegisFSAL
    let mut matches = Vec::new();
    let mut stack = vec![path.to_string()];
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
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("/");

    // We will do a generic recursive walk using AegisFSAL
    // For simplicity, we just do string suffix/prefix matching or Regex if globset isn't used
    // A simple glob-to-regex converter:
    let regex_pattern = pattern
        .replace(".", "\\.")
        .replace("*", ".*")
        .replace("?", ".");
    let regex = regex::Regex::new(&format!("^{}$", regex_pattern)).map_err(|e| {
        SmcpSessionError::SignatureVerificationFailed(format!("Invalid glob pattern: {}", e))
    })?;

    let mut matches = Vec::new();
    let mut stack = vec![path.to_string()];

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
