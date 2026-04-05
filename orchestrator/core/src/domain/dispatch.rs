// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Aegis Dispatch Protocol Domain (ADR-040)
//!
//! Value objects and types defining the bidirectional Dispatch Protocol channel
//! between the orchestrator (`InnerLoopGateway`) and the agent container (`bootstrap.py`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
/// Conversation message exchanged in the inner loop (ADR-038).
///
/// Defined in the domain layer so it can be referenced by both `AgentMessage`/`OrchestratorMessage`
/// (dispatch protocol) and `InnerLoopService` (application layer) without creating an upward
/// dependency from domain → application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// A single LLM tool call within a conversation message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

pub use crate::domain::shared_kernel::DispatchId;

/// Dispatch action vocabulary (extensible enum).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DispatchAction {
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_cwd")]
        cwd: String,
        #[serde(default)]
        env_additions: HashMap<String, String>,
        timeout_secs: u32,
        max_output_bytes: u64,
    },
    // Future: QueryEnv, Ping, StreamExec
}

fn default_cwd() -> String {
    "/workspace".to_string()
}

/// The outer message envelope for Agent -> Orchestrator messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    Generate {
        agent_id: String,
        execution_id: String,
        iteration_number: u8,
        prompt: String,
        #[serde(default)]
        messages: Vec<ConversationMessage>,
        #[serde(default = "default_model_alias")]
        model_alias: String,
    },
    DispatchResult {
        execution_id: String,
        dispatch_id: DispatchId,
        exit_code: i32,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        duration_ms: u64,
        truncated: bool,
    },
}

fn default_model_alias() -> String {
    "default".to_string()
}

/// The outer message envelope for Orchestrator -> Agent messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestratorMessage {
    Final {
        content: String,
        tool_calls_executed: u32,
        #[serde(default)]
        conversation: Vec<ConversationMessage>,
        /// Live tool trajectory from the inner loop, carried out so callers
        /// (e.g. validation pipeline) can use it without a DB fetch.
        #[serde(default)]
        trajectory: Vec<crate::domain::execution::TrajectoryStep>,
    },
    Dispatch {
        dispatch_id: DispatchId,
        #[serde(flatten)]
        action: DispatchAction,
    },
}

/// State machine for the inner loop (ADR-038).
#[derive(Debug, Clone)]
pub enum InnerLoopState {
    /// LLM called, awaiting response.
    AwaitingLlm,
    /// Dispatched a cmd.run to bootstrap.py, awaiting dispatch_result.
    AwaitingDispatchResult {
        dispatch_id: DispatchId,
        tool_call_id: String,
        dispatched_at: DateTime<Utc>,
        timeout_secs: u32,
    },
    /// Finished all tool calls, loop completed.
    Done,
}
