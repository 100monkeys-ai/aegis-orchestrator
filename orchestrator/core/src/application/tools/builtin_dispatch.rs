// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::dispatch::DispatchAction;
use crate::domain::execution::ExecutionId;
use crate::domain::seal_session::SealSessionError;
use serde_json::Value;
use std::collections::HashMap;

/// Environment variable name prefixes that are always scrubbed before forwarding
/// env_additions to a dispatched exec.
const SCRUB_PREFIXES: &[&str] = &[
    "AWS_",
    "AZURE_",
    "GCP_",
    "GOOGLE_",
    "GITHUB_TOKEN",
    "CI_",
    "AEGIS_SECRET",
    "AEGIS_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];

/// Encodes a `cmd.run` tool call into a `DispatchAction::Exec`, scrubbing
/// sensitive environment variable additions before dispatch.
pub struct DispatchEncoder;

impl DispatchEncoder {
    /// Filter `env_additions` against built-in prefix denylist and any caller-
    /// supplied exact-match denylist entries.
    pub fn scrub_env(
        env_additions: HashMap<String, String>,
        extra_denylist: &[String],
    ) -> HashMap<String, String> {
        env_additions
            .into_iter()
            .filter(|(key, _)| {
                !SCRUB_PREFIXES.iter().any(|p| key.starts_with(p))
                    && !extra_denylist.iter().any(|d| d == key)
            })
            .collect()
    }

    /// Build a `DispatchAction::Exec` from the raw `cmd.run` tool call JSON args.
    pub fn encode(
        args: &Value,
        env_denylist: &[String],
    ) -> Result<DispatchAction, SealSessionError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::MalformedPayload(
                    "cmd.run: missing required field 'command'".to_string(),
                )
            })?
            .to_string();

        let extra_args: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let cwd = args
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace")
            .to_string();

        let env_additions: HashMap<String, String> = args
            .get("env_additions")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let timeout_secs: u32 = args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(600);

        let max_output_bytes: u64 = args
            .get("max_output_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(1_048_576);

        Ok(DispatchAction::Exec {
            command,
            args: extra_args,
            cwd,
            env_additions: Self::scrub_env(env_additions, env_denylist),
            timeout_secs,
            max_output_bytes,
        })
    }
}

pub fn invoke_cmd_run(
    args: &Value,
    _execution_id: ExecutionId,
) -> Result<ToolInvocationResult, SealSessionError> {
    let action = DispatchEncoder::encode(args, &[])?;
    Ok(ToolInvocationResult::DispatchRequired(action))
}
