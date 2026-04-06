// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Output Handler Domain Types (ADR-103)
//!
//! Defines [`OutputHandlerConfig`], the generalized egress handler fired after an
//! agent execution or workflow state completes. The handler is declared on the
//! [`crate::domain::agent::AgentSpec`] or on individual workflow [`crate::domain::workflow::StateKind`]
//! variants and is invoked by [`crate::application::output_handler_service::OutputHandlerService`]
//! after the `ExecutionCompleted` event is published.
//!
//! ## Variants
//!
//! | Variant | Description |
//! |---------|-------------|
//! | `Agent` | Spawn a named agent as a child execution to process/deliver the output |
//! | `Container` | Execute a container step to transform or deliver the output (ADR-103 phase 2) |
//! | `McpTool` | Invoke an MCP/SEAL tool (e.g. webhook_send, slack, email) (ADR-103 phase 2) |
//! | `Webhook` | HTTP POST of final output to an external URL |

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::domain::runtime::ContainerResources;
use crate::domain::workflow::RetryConfig;

/// Generalized egress handler fired after an agent execution or workflow state completes.
///
/// ADR-103: Agent Output Handler.
///
/// The handler is declared as an optional field on [`crate::domain::agent::AgentSpec`]
/// (`output_handler`) and on the `Agent`, `ContainerRun`, and `ParallelAgents`
/// [`crate::domain::workflow::StateKind`] variants. It is invoked by
/// [`crate::application::output_handler_service::OutputHandlerService`] immediately
/// after execution completes.
///
/// When `required` is `true` on any variant, a handler failure marks the
/// execution as failed. When `required` is `false` (the default), the handler
/// runs in a fire-and-forget mode and failures are only logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputHandlerConfig {
    /// Spawn a named agent as a child execution to process/deliver the output.
    Agent {
        /// Name or ID of the agent to spawn.
        agent_id: String,

        /// Handlebars input template evaluated with `{ "output": "<final_output>" }`.
        /// If absent the raw final output string is passed as the child input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_template: Option<String>,

        /// Maximum seconds to wait for the child agent to complete.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,

        /// When `true`, handler failure marks the parent execution as failed.
        /// When `false` (the default), failure is logged and the execution succeeds.
        #[serde(default)]
        required: bool,
    },

    /// Execute a container step to transform or deliver the output.
    ///
    /// **ADR-103 phase 2 — not yet implemented.** The runtime will panic with a
    /// descriptive message if this variant is used.
    Container {
        /// Container image reference (registry/repo:tag).
        image: String,

        /// Argv to execute inside the container.
        command: Vec<String>,

        /// Environment variables injected into the container.
        #[serde(default)]
        env: HashMap<String, String>,

        /// CPU, memory, and timeout resource limits.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resources: Option<ContainerResources>,

        /// When `true`, handler failure marks the parent execution as failed.
        #[serde(default)]
        required: bool,
    },

    /// Invoke an MCP/SEAL tool (e.g. `webhook_send`, `slack`, `email`).
    ///
    /// **ADR-103 phase 2 — not yet implemented.** The runtime will panic with a
    /// descriptive message if this variant is used.
    McpTool {
        /// Registered tool name (e.g. `"webhook_send"`, `"slack_message"`).
        tool_name: String,

        /// Handlebars-evaluated JSON arguments. `{{output}}` is available.
        arguments: serde_json::Value,

        /// Maximum seconds to wait for the tool invocation to complete.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,

        /// When `true`, handler failure marks the parent execution as failed.
        #[serde(default)]
        required: bool,
    },

    /// HTTP webhook — POST final output to an external URL.
    Webhook {
        /// Target URL.
        url: String,

        /// HTTP method (default: `"POST"`).
        #[serde(default = "default_http_method")]
        method: String,

        /// Additional HTTP headers.
        #[serde(default)]
        headers: HashMap<String, String>,

        /// Handlebars body template. Default: raw output string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_template: Option<String>,

        /// Request timeout in seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_seconds: Option<u64>,

        /// Retry policy for failed webhook deliveries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry: Option<RetryConfig>,

        /// When `true`, handler failure marks the parent execution as failed.
        #[serde(default)]
        required: bool,
    },
}

impl OutputHandlerConfig {
    /// Returns a human-readable discriminant name for telemetry and log messages.
    pub fn handler_type_name(&self) -> &'static str {
        match self {
            OutputHandlerConfig::Agent { .. } => "agent",
            OutputHandlerConfig::Container { .. } => "container",
            OutputHandlerConfig::McpTool { .. } => "mcp_tool",
            OutputHandlerConfig::Webhook { .. } => "webhook",
        }
    }

    /// Returns `true` when handler failure should mark the parent execution as failed.
    pub fn is_required(&self) -> bool {
        match self {
            OutputHandlerConfig::Agent { required, .. } => *required,
            OutputHandlerConfig::Container { required, .. } => *required,
            OutputHandlerConfig::McpTool { required, .. } => *required,
            OutputHandlerConfig::Webhook { required, .. } => *required,
        }
    }
}

fn default_http_method() -> String {
    "POST".to_string()
}
