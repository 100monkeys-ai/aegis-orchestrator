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

    match file_ops
        .read_file_for_execution(execution_id, tenant_id, path)
        .await
    {
        Ok(content) => {
            let text = String::from_utf8_lossy(&content.data).to_string();
            Ok(ToolInvocationResult::Direct(json!({
                "status": "success",
                "path": raw_path,
                "content": text,
                "size_bytes": content.data.len(),
                "content_type": content.content_type,
            })))
        }
        Err(FileOperationsError::NotFound(msg)) => Ok(ToolInvocationResult::Direct(json!({
            "status": "error",
            "error": "not_found",
            "message": msg,
        }))),
        Err(FileOperationsError::Unauthorized) => Ok(ToolInvocationResult::Direct(json!({
            "status": "error",
            "error": "forbidden",
            "message": "execution does not belong to your tenant",
        }))),
        Err(e) => Err(SealSessionError::InternalError(format!(
            "aegis.execution.file: {e}"
        ))),
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
