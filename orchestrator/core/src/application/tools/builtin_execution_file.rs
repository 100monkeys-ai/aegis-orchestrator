// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Builtin `aegis.execution.file` Tool
//!
//! Reads a single file from a completed execution's workspace volume post-mortem.
//! The tool looks up the volume by execution ownership and reads via FSAL.

use crate::application::file_operations_service::{FileOperationsError, FileOperationsService};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::seal_session::SealSessionError;
use crate::domain::tenant::TenantId;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Returns `true` if the MIME content type represents text-like content that
/// can be safely rendered as a UTF-8 string.
fn is_text_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type == "application/xml"
}

/// Normalize a file path by stripping the `/workspace/` or `/workspace` prefix
/// that agents typically use as their working directory mount point.
fn normalize_path(path: &str) -> &str {
    let trimmed = path.trim_start_matches('/');
    if let Some(rest) = trimmed.strip_prefix("workspace/") {
        rest
    } else if trimmed == "workspace" {
        "/"
    } else {
        path
    }
}

/// Invoke the `aegis.execution.file` builtin tool.
///
/// Expected args:
/// ```json
/// { "execution_id": "<uuid>", "path": "output.md" }
/// ```
pub async fn invoke_execution_file_tool(
    file_ops: &Arc<FileOperationsService>,
    tenant_id: &TenantId,
    args: &Value,
) -> Result<ToolInvocationResult, SealSessionError> {
    let execution_id_str = args
        .get("execution_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SealSessionError::MalformedPayload(
                "aegis.execution.file: missing required field 'execution_id' (string)".to_string(),
            )
        })?;

    let execution_uuid = uuid::Uuid::parse_str(execution_id_str).map_err(|e| {
        SealSessionError::InvalidArguments(format!(
            "aegis.execution.file: invalid execution_id UUID: {e}"
        ))
    })?;
    let execution_id = ExecutionId(execution_uuid);

    let raw_path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
        SealSessionError::MalformedPayload(
            "aegis.execution.file: missing required field 'path' (string)".to_string(),
        )
    })?;

    let path = normalize_path(raw_path);

    info!(
        %execution_id,
        path,
        tenant_id = %tenant_id,
        "aegis.execution.file: reading file"
    );

    match file_ops
        .read_file_for_execution(execution_id, tenant_id, path)
        .await
    {
        Ok(content) => {
            let size_bytes = content.data.len();
            let is_binary = !is_text_content_type(&content.content_type);
            let download_url = format!("/v1/executions/{}/files/{}", execution_id.0, raw_path);

            debug!(
                %execution_id,
                path,
                size_bytes,
                is_binary,
                content_type = %content.content_type,
                "aegis.execution.file: success"
            );

            let mut result = json!({
                "status": "success",
                "path": raw_path,
                "content_type": content.content_type,
                "is_binary": is_binary,
                "size_bytes": size_bytes,
                "download_url": download_url,
            });

            if !is_binary {
                let text = String::from_utf8_lossy(&content.data).to_string();
                result
                    .as_object_mut()
                    .expect("result is an object")
                    .insert("content".to_string(), Value::String(text));
            }

            Ok(ToolInvocationResult::Direct(result))
        }
        Err(FileOperationsError::NotFound(msg)) => {
            warn!(%execution_id, path, %msg, "aegis.execution.file: not found");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "not_found",
                "message": msg,
            })))
        }
        Err(FileOperationsError::Unauthorized) => {
            warn!(%execution_id, path, "aegis.execution.file: unauthorized");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "forbidden",
                "message": "execution does not belong to your tenant",
            })))
        }
        Err(FileOperationsError::InvalidPath(msg)) => {
            warn!(%execution_id, path, %msg, "aegis.execution.file: invalid path");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "invalid_path",
                "message": msg,
            })))
        }
        Err(FileOperationsError::FileTooLarge) => {
            warn!(%execution_id, path, "aegis.execution.file: file too large");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "file_too_large",
                "message": "file exceeds the maximum allowed size",
            })))
        }
        Err(FileOperationsError::Fsal(msg)) => {
            warn!(%execution_id, path, %msg, "aegis.execution.file: fsal error");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "storage_error",
                "message": format!("storage backend error: {msg}"),
            })))
        }
        Err(FileOperationsError::Repository(msg)) => {
            warn!(%execution_id, path, %msg, "aegis.execution.file: repository error");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "internal_error",
                "message": format!("repository error: {msg}"),
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_strips_workspace_prefix() {
        assert_eq!(normalize_path("/workspace/output.md"), "output.md");
        assert_eq!(normalize_path("workspace/output.md"), "output.md");
        assert_eq!(normalize_path("/workspace/deep/file.txt"), "deep/file.txt");
    }

    #[test]
    fn test_normalize_path_bare_workspace() {
        assert_eq!(normalize_path("/workspace"), "/");
        assert_eq!(normalize_path("workspace"), "/");
    }

    #[test]
    fn test_normalize_path_no_workspace_prefix() {
        assert_eq!(normalize_path("output.md"), "output.md");
        assert_eq!(normalize_path("/output.md"), "/output.md");
        assert_eq!(normalize_path("deep/file.txt"), "deep/file.txt");
    }
}
