// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Builtin Schema Tools
//!
//! Implements the `schema.get` and `schema.validate` builtin tool handlers.
//!
//! ## `schema.get`
//!
//! Returns the canonical JSON Schema for a manifest kind, derived at startup
//! from the Rust struct definitions via `schemars`.
//!
//! Input: `{ "key": "agent/manifest/v1" }`
//! Output: the full JSON Schema document
//!
//! ## `schema.validate`
//!
//! Validates a manifest YAML string against the canonical schema.
//!
//! Input: `{ "kind": "agent", "manifest_yaml": "<yaml text>" }`
//! Output: `{ "valid": true/false, "errors": ["..."] }`

use crate::application::schema_registry::SchemaRegistry;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::smcp_session::SmcpSessionError;
use serde_json::{json, Value};
use std::sync::Arc;

/// Handle the `schema.get` tool call.
///
/// Expects `args = { "key": "<schema-key>" }`.
/// Returns the full JSON Schema document as the tool result.
pub fn invoke_schema_get(
    registry: &Arc<SchemaRegistry>,
    args: &Value,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let key = args.get("key").and_then(Value::as_str).ok_or_else(|| {
        SmcpSessionError::MalformedPayload(
            "schema.get: missing required field 'key' (string)".to_string(),
        )
    })?;

    match registry.get(key) {
        Some(schema) => Ok(ToolInvocationResult::Direct(schema.clone())),
        None => {
            let msg = format!(
                "Unknown schema key '{key}'. Supported keys: agent/manifest/v1, workflow/manifest/v1"
            );
            Ok(ToolInvocationResult::Direct(json!({
                "error": msg,
                "supported_keys": ["agent/manifest/v1", "workflow/manifest/v1"]
            })))
        }
    }
}

/// Handle the `schema.validate` tool call.
///
/// Expects `args = { "kind": "agent", "manifest_yaml": "..." }`.
/// Returns `{ "valid": true/false, "errors": ["..."] }`.
pub fn invoke_schema_validate(
    registry: &Arc<SchemaRegistry>,
    args: &Value,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let kind = args.get("kind").and_then(Value::as_str).ok_or_else(|| {
        SmcpSessionError::MalformedPayload(
            "schema.validate: missing required field 'kind' (string)".to_string(),
        )
    })?;

    let manifest_yaml = args
        .get("manifest_yaml")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SmcpSessionError::MalformedPayload(
                "schema.validate: missing required field 'manifest_yaml' (string)".to_string(),
            )
        })?;

    let result = registry.validate(kind, manifest_yaml);

    Ok(ToolInvocationResult::Direct(json!({
        "valid": result.valid,
        "errors": result.errors,
    })))
}
