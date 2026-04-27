// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Helper for parsing the `attachments` field from SEAL JSON-RPC tool call
//! arguments into a typed `Vec<AttachmentRef>`.
//!
//! ADR-113: Attachment routing must work uniformly across both the gRPC
//! `ExecuteAgentRequest` path and the SEAL JSON-RPC `/v1/seal/invoke` path.
//! The gRPC path parses attachments from the proto request directly; the SEAL
//! tool surface receives them as JSON inside the tool call args. This helper
//! is the single source of truth for that JSON-to-domain parsing so the SEAL
//! handlers (`aegis.task.execute`, `aegis.agent.generate`,
//! `aegis.execute.intent`) all funnel through the same code path. The merge
//! of attachments into the agent's prompt input JSON happens downstream in
//! `StandardExecutionService::prepare_execution_input` once the typed
//! `Vec<AttachmentRef>` reaches `ExecutionInput.attachments`.

use serde_json::Value;

use crate::domain::execution::AttachmentRef;
use crate::domain::seal_session::SealSessionError;

/// Parse the `attachments` field from SEAL tool call args.
///
/// Returns an empty `Vec` when the field is absent or explicitly `null`.
/// Returns `SealSessionError::InvalidArguments` when the field is present
/// but is not a JSON array, or when any element fails to deserialize as an
/// `AttachmentRef`. Strict parsing is intentional: silently dropping
/// malformed entries would mask client bugs that the caller needs to see.
pub(super) fn parse_attachments(args: &Value) -> Result<Vec<AttachmentRef>, SealSessionError> {
    let raw = match args.get("attachments") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(v) => v,
    };

    let arr = raw.as_array().ok_or_else(|| {
        SealSessionError::InvalidArguments(
            "'attachments' must be a JSON array of AttachmentRef objects".to_string(),
        )
    })?;

    let mut refs = Vec::with_capacity(arr.len());
    for (idx, item) in arr.iter().enumerate() {
        let r: AttachmentRef = serde_json::from_value(item.clone()).map_err(|e| {
            SealSessionError::InvalidArguments(format!(
                "attachments[{idx}] failed to parse as AttachmentRef: {e}"
            ))
        })?;
        refs.push(r);
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_attachments_field_returns_empty() {
        let args = json!({"agent_id": "x"});
        assert_eq!(
            parse_attachments(&args).unwrap(),
            Vec::<AttachmentRef>::new()
        );
    }

    #[test]
    fn null_attachments_field_returns_empty() {
        let args = json!({"attachments": null});
        assert_eq!(
            parse_attachments(&args).unwrap(),
            Vec::<AttachmentRef>::new()
        );
    }

    #[test]
    fn empty_array_returns_empty() {
        let args = json!({"attachments": []});
        assert_eq!(
            parse_attachments(&args).unwrap(),
            Vec::<AttachmentRef>::new()
        );
    }

    #[test]
    fn parses_single_attachment() {
        let volume_id = uuid::Uuid::new_v4();
        let args = json!({
            "attachments": [{
                "volume_id": volume_id.to_string(),
                "path": "/uploads/doc.txt",
                "name": "doc.txt",
                "mime_type": "text/plain",
                "size": 42,
                "sha256": "abc123",
            }]
        });
        let refs = parse_attachments(&args).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].volume_id.0, volume_id);
        assert_eq!(refs[0].path, "/uploads/doc.txt");
        assert_eq!(refs[0].name, "doc.txt");
        assert_eq!(refs[0].mime_type, "text/plain");
        assert_eq!(refs[0].size, 42);
        assert_eq!(refs[0].sha256.as_deref(), Some("abc123"));
    }

    #[test]
    fn parses_multiple_attachments() {
        let v1 = uuid::Uuid::new_v4();
        let v2 = uuid::Uuid::new_v4();
        let args = json!({
            "attachments": [
                {"volume_id": v1.to_string(), "path": "/a", "name": "a", "mime_type": "text/plain", "size": 1},
                {"volume_id": v2.to_string(), "path": "/b", "name": "b", "mime_type": "text/plain", "size": 2},
            ]
        });
        let refs = parse_attachments(&args).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].volume_id.0, v1);
        assert_eq!(refs[1].volume_id.0, v2);
        // Optional sha256 omitted -> None.
        assert!(refs[0].sha256.is_none());
    }

    #[test]
    fn non_array_attachments_is_rejected() {
        let args = json!({"attachments": "not-an-array"});
        let err = parse_attachments(&args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("must be a JSON array"), "msg={msg}");
    }

    #[test]
    fn malformed_attachment_element_is_rejected() {
        let args = json!({"attachments": [{"volume_id": "not-a-uuid"}]});
        let err = parse_attachments(&args).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("attachments[0]"), "msg={msg}");
    }
}
