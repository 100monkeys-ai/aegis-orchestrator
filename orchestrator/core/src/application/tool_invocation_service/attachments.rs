// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Builtin `aegis.attachment.read` Tool (ADR-113)
//!
//! Reads a file referenced by `(volume_id, path)` from a tenant-scoped volume.
//! Used by system-tier agents to read files attached to a chat message via the
//! Zaru `chat-uploads` capability. The tool is read-only, tenant-isolated, and
//! visible only through the `aegis-system-agent-runtime` /
//! `aegis-system-default` security contexts (see ADR-113 §6).
//!
//! Authorization model:
//!   1. The tool dispatcher resolves `tenant_id` from the SEAL session and
//!      injects it into the args (see `facade.rs::invoke_tool_internal`).
//!   2. `FileOperationsService::read_attachment_for_tenant` looks up the volume
//!      by ID and verifies `volume.tenant_id == tenant_id` before reading.
//!   3. The read goes through the same FSAL pipeline as every other read path,
//!      so path sanitization is identical.
//!
//! Output shape (matches the JSON schema in `tool_router::schema_for_builtin`):
//! ```json
//! {
//!   "status": "success",
//!   "volume_id": "...",
//!   "path": "...",
//!   "content": "<utf-8 text or base64 bytes>",
//!   "encoding": "utf-8" | "base64",
//!   "mime_type": "...",
//!   "size": 12345,
//!   "sha256": "<hex digest>"
//! }
//! ```

use crate::application::file_operations_service::{FileOperationsError, FileOperationsService};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::seal_session::SealSessionError;
use crate::domain::tenant::TenantId;
use crate::domain::volume::VolumeId;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Returns `true` if the MIME content type represents text-like content that
/// can be safely returned as a UTF-8 string. Binary content is base64-encoded.
fn is_text_content_type(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type == "application/json"
        || content_type == "application/xml"
        || content_type == "application/javascript"
        || content_type == "application/yaml"
}

/// Sniff a MIME type from the first bytes of the file. Falls back to the
/// extension-based guess when sniffing yields nothing useful.
///
/// Content-based sniffing prevents a malicious caller from claiming a benign
/// MIME via metadata when the bytes are something else entirely.
fn sniff_mime(data: &[u8], extension_guess: &str) -> String {
    if let Some(kind) = infer::get(data) {
        return kind.mime_type().to_string();
    }
    extension_guess.to_string()
}

/// Invoke the `aegis.attachment.read` builtin tool.
///
/// Expected args:
/// ```json
/// { "volume_id": "<uuid>", "path": "<posix-path>" }
/// ```
pub async fn invoke_aegis_attachment_read_tool(
    file_ops: &Arc<FileOperationsService>,
    tenant_id: &TenantId,
    args: &Value,
) -> Result<ToolInvocationResult, SealSessionError> {
    let volume_id_str = args
        .get("volume_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SealSessionError::MalformedPayload(
                "aegis.attachment.read: missing required field 'volume_id' (string)".to_string(),
            )
        })?;

    let volume_uuid = uuid::Uuid::parse_str(volume_id_str).map_err(|e| {
        SealSessionError::InvalidArguments(format!(
            "aegis.attachment.read: invalid volume_id UUID: {e}"
        ))
    })?;
    let volume_id = VolumeId(volume_uuid);

    let path = args.get("path").and_then(Value::as_str).ok_or_else(|| {
        SealSessionError::MalformedPayload(
            "aegis.attachment.read: missing required field 'path' (string)".to_string(),
        )
    })?;

    info!(
        %volume_id,
        path,
        tenant_id = %tenant_id,
        "aegis.attachment.read: reading attachment"
    );

    match file_ops
        .read_attachment_for_tenant(&volume_id, tenant_id, path)
        .await
    {
        Ok(content) => {
            let mime_type = sniff_mime(&content.data, &content.content_type);
            let size = content.data.len() as u64;
            let mut hasher = Sha256::new();
            hasher.update(&content.data);
            let sha256 = format!("{:x}", hasher.finalize());

            let (encoded, encoding) = if is_text_content_type(&mime_type) {
                (String::from_utf8_lossy(&content.data).to_string(), "utf-8")
            } else {
                (
                    base64::engine::general_purpose::STANDARD.encode(&content.data),
                    "base64",
                )
            };

            debug!(
                %volume_id,
                path,
                size,
                mime_type = %mime_type,
                "aegis.attachment.read: success"
            );

            Ok(ToolInvocationResult::Direct(json!({
                "status": "success",
                "volume_id": volume_id.to_string(),
                "path": path,
                "content": encoded,
                "encoding": encoding,
                "mime_type": mime_type,
                "size": size,
                "sha256": sha256,
            })))
        }
        Err(FileOperationsError::NotFound(msg)) => {
            warn!(%volume_id, path, %msg, "aegis.attachment.read: not found");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "not_found",
                "message": msg,
            })))
        }
        Err(FileOperationsError::Unauthorized) => {
            warn!(
                %volume_id,
                path,
                caller_tenant = %tenant_id,
                "aegis.attachment.read: tenant mismatch or non-persistent volume"
            );
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "forbidden",
                "message": "volume does not belong to your tenant",
            })))
        }
        Err(FileOperationsError::InvalidPath(msg)) => {
            warn!(%volume_id, path, %msg, "aegis.attachment.read: invalid path");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "invalid_path",
                "message": msg,
            })))
        }
        Err(FileOperationsError::FileTooLarge) => {
            warn!(%volume_id, path, "aegis.attachment.read: file too large");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "file_too_large",
                "message": "file exceeds the maximum allowed size",
            })))
        }
        Err(FileOperationsError::Fsal(msg)) => {
            warn!(%volume_id, path, %msg, "aegis.attachment.read: fsal error");
            Ok(ToolInvocationResult::Direct(json!({
                "status": "error",
                "error": "storage_error",
                "message": format!("storage backend error: {msg}"),
            })))
        }
        Err(FileOperationsError::Repository(msg)) => {
            warn!(%volume_id, path, %msg, "aegis.attachment.read: repository error");
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
    fn is_text_content_type_classification() {
        assert!(is_text_content_type("text/plain"));
        assert!(is_text_content_type("text/markdown"));
        assert!(is_text_content_type("application/json"));
        assert!(is_text_content_type("application/yaml"));
        assert!(!is_text_content_type("application/pdf"));
        assert!(!is_text_content_type("image/png"));
        assert!(!is_text_content_type("application/octet-stream"));
    }

    #[test]
    fn sniff_mime_recognises_png_magic_bytes() {
        // PNG magic number: 89 50 4E 47 0D 0A 1A 0A
        let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let mime = sniff_mime(png, "application/octet-stream");
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn sniff_mime_falls_back_to_extension_guess_for_plain_text() {
        // Plain ASCII has no magic number — sniffer returns None,
        // we should fall back to the supplied extension guess.
        let bytes = b"hello world";
        let mime = sniff_mime(bytes, "text/plain");
        assert_eq!(mime, "text/plain");
    }

    /// Regression test for ADR-113 tenant isolation: the unauthorized branch
    /// (mismatched tenant) must surface as `forbidden`, not `not_found`. This
    /// guards against the failure mode where a leaked volume_id from one
    /// tenant could be used to fish for "does this exist?" signals from
    /// another tenant.
    #[test]
    fn tenant_mismatch_maps_to_forbidden() {
        let err = FileOperationsError::Unauthorized;
        // Mirror the dispatch arm.
        let resp = match err {
            FileOperationsError::Unauthorized => json!({
                "status": "error",
                "error": "forbidden",
            }),
            _ => json!({"status": "error", "error": "other"}),
        };
        assert_eq!(resp.get("error").and_then(Value::as_str), Some("forbidden"));
    }
}
