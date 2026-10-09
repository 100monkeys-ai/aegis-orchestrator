// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # MCP Domain Types (BC-4 Tools, ADR-033/035)
//!
//! Domain types for the MCP (Model Context Protocol) Tool Integration bounded
//! context. The orchestrator acts as an **Orchestrator Proxy** — all agent tool
//! calls are routed through it; agents never access tool servers or external
//! APIs directly (see ADR-033 §1).
//!
//! ## Key Types
//!
//! | Type | Role |
//! |------|------|
//! | [`ToolInvocationId`] | Identifies a single tool call within an execution |
//! | [`ToolPolicy`] | Security constraints on tool usage (paths, domains, rate limits) |
//! | [`PolicyViolation`] | Describes why a tool call was rejected by `SecurityContext` |
//! | [`MCPError`] | JSON-RPC error value object returned to agents on failure |
//! | [`CredentialRef`] | Opaque reference to a credential in the secret store |
//!
//! ## Credential Isolation
//!
//! [`CredentialRef`] stores a *reference* (path/key) to the credential, not
//! the credential itself. Credentials are resolved by the orchestrator's
//! `SecretsManager` (ADR-034); they are never written to agent container
//! memory.
//!
//! The orchestrator runs no MCP server of its own: an external tool comes
//! through the SEAL gateway (AEGIS ADR-132, Update G1 and G4).
//!
//! See ADR-033 (Orchestrator-Mediated MCP Tool Routing), ADR-035 (SEAL),
//! AGENTS.md §Tools & Integration Domain.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// Unique identifier for a single MCP tool invocation.
///
/// Spans the lifetime of one `tool/invoke` JSON-RPC call. Used in
/// `MCPToolEvent::InvocationRequested` / `InvocationCompleted` pairs for
/// end-to-end correlation in audit logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolInvocationId(pub Uuid);

impl ToolInvocationId {
    /// Generate a new random `ToolInvocationId`.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ToolInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Opaque reference to a credential in the orchestrator's secret store.
///
/// Stores a *key path* only — never the credential value itself.
/// The orchestrator resolves this reference via `SecretsManager` (ADR-034),
/// ensuring credentials are never written to agent container memory
/// (Credential Isolation, ADR-033).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRef {
    /// Which backend holds the credential.
    pub store_type: CredentialStoreType,
    /// Key or path within that backend (e.g. `"env:GMAIL_TOKEN"` or `"secret:tenant/kv/gmail"`).
    pub key: String,
}

/// Credential storage backend discriminant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialStoreType {
    /// Read from the orchestrator process's environment variables.
    /// Suitable for development; not recommended for production.
    Environment,
    /// Read from the orchestrator-managed secret store ACL (ADR-034).
    SecretStore,
}

impl CredentialRef {
    pub fn from_env(key: &str) -> Self {
        Self {
            store_type: CredentialStoreType::Environment,
            key: key.to_string(),
        }
    }

    pub fn from_secret_store(path: &str) -> Self {
        Self {
            store_type: CredentialStoreType::SecretStore,
            key: format!("secret:{path}"),
        }
    }
}

/// JSON-RPC error value returned to agents when a tool invocation fails.
///
/// Follows the JSON-RPC 2.0 error object schema. The `code` field uses
/// MCP-standard codes (e.g. `-32603` for internal error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPError {
    /// JSON-RPC error code.
    pub code: i32,
    /// Human-readable error message (never contains credentials).
    pub message: String,
    /// Optional structured context data for debugging.
    pub data: Option<Value>,
}

/// Re-export from the canonical owner (BC-4 Security Context).
/// Kept here so that existing `use crate::domain::mcp::PolicyViolation` paths
/// continue to compile. New code should import from
/// `crate::domain::security_context::PolicyViolation` instead.
pub use crate::domain::security_context::PolicyViolation;

fn extract_domain(url: &str) -> String {
    if let Ok(url) = url::Url::parse(url) {
        url.host_str().unwrap_or("").to_string()
    } else {
        "".to_string()
    }
}

/// Defines agent-specific constraints on tool usage, enforced before invocation reaches MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicy {
    // Allowlists
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,

    // Path constraints (for filesystem tools)
    pub allowed_paths: Vec<PathBuf>,
    pub deny_path_traversal: bool,

    // Network constraints (for external tools)
    pub allowed_domains: Vec<String>,

    // Rate limiting
    pub max_calls_per_execution: u32,
    pub max_calls_per_tool: HashMap<String, u32>,
    pub timeout_per_call: Duration,
}

impl ToolPolicy {
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        self.allowed_tools.iter().any(|allowed| {
            if allowed.ends_with(".*") {
                let prefix = allowed.trim_end_matches(".*");
                tool_name.starts_with(prefix)
            } else {
                allowed == tool_name
            }
        })
    }

    pub fn is_tool_denied(&self, tool_name: &str) -> bool {
        self.denied_tools.iter().any(|denied| {
            if denied.ends_with(".*") {
                let prefix = denied.trim_end_matches(".*");
                tool_name.starts_with(prefix)
            } else {
                denied == tool_name
            }
        })
    }

    pub fn validate_invocation(
        &self,
        tool_name: &str,
        arguments: &Value,
        current_call_count: u32,
    ) -> Result<(), PolicyViolation> {
        // 1. Check allowlist
        if !self.is_tool_allowed(tool_name) {
            return Err(PolicyViolation::ToolNotAllowed {
                tool_name: tool_name.to_string(),
                allowed_tools: self.allowed_tools.clone(),
            });
        }

        // 2. Check denylist (explicit denials override allowlist)
        if self.is_tool_denied(tool_name) {
            return Err(PolicyViolation::ToolExplicitlyDenied {
                tool_name: tool_name.to_string(),
            });
        }

        // 3. Rate limit check
        if current_call_count >= self.max_calls_per_execution {
            return Err(PolicyViolation::RateLimitExceeded {
                resource_type: "SealToolCall".to_string(),
                bucket: "per_execution".to_string(),
                limit: self.max_calls_per_execution as u64,
                current: current_call_count as u64,
                retry_after_seconds: 0,
            });
        }

        // 4. Tool-specific validation
        if tool_name.starts_with("filesystem.") {
            self.validate_filesystem_access(arguments)?;
        }

        if tool_name.starts_with("web-search.") {
            self.validate_network_access(arguments)?;
        }

        Ok(())
    }

    fn validate_filesystem_access(&self, arguments: &Value) -> Result<(), PolicyViolation> {
        let path = arguments
            .get("path")
            .and_then(Value::as_str)
            .ok_or(PolicyViolation::MissingRequiredArgument("path".to_string()))?;

        let path = PathBuf::from(path);

        // Check path traversal attempts
        if self.deny_path_traversal && path.to_str().unwrap_or("").contains("..") {
            return Err(PolicyViolation::PathTraversalAttempt { path });
        }

        // Check against allowed volume boundaries
        if !self
            .allowed_paths
            .iter()
            .any(|allowed| path.starts_with(allowed))
        {
            return Err(PolicyViolation::PathOutsideBoundary {
                path,
                allowed_paths: self.allowed_paths.clone(),
            });
        }

        Ok(())
    }

    fn validate_network_access(&self, arguments: &Value) -> Result<(), PolicyViolation> {
        // Extract domain from arguments (tool-specific logic)
        if let Some(url) = arguments.get("url").and_then(Value::as_str) {
            let domain = extract_domain(url);
            if !self.allowed_domains.iter().any(|d| domain.ends_with(d)) {
                return Err(PolicyViolation::DomainNotAllowed {
                    domain,
                    allowed_domains: self.allowed_domains.clone(),
                });
            }
        }

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool Input Contract (BC-4/BC-12, ADR-055)
// ─────────────────────────────────────────────────────────────────────────────

/// Domain value object encoding the **required-argument contract** for every
/// known built-in and management tool exposed via the SEAL tool path (ADR-055).
///
/// # Why this lives in the domain layer
///
/// Which parameters a tool *requires* is a fact about the tool's interface —
/// it is domain knowledge, not orchestration logic.  Placing it here keeps the
/// Application Layer (`ToolInvocationService`) free of per-tool business rules
/// and makes the contracts discoverable, testable, and evolvable independently
/// of routing or execution plumbing.
///
/// # Usage
///
/// ```rust,ignore
/// ToolInputContract::validate("aegis.agent.create", &args)
///     .map_err(|msg| SealSessionError::InvalidArguments(msg))?;
/// ```
pub struct ToolInputContract;

/// One tool's declaration to the approval gate, part of its input contract
/// (AEGIS ADR-126, Update of 2026-10-04, clause 1).
pub struct ApprovalDeclaration {
    pub tool: &'static str,
    pub binding_argument: Option<&'static str>,
    pub approval_summary: Option<&'static [&'static str]>,
}

/// The built-in tools' declarations to the approval gate: `mail.send` and
/// `mail.reply` act through the mailbox their `mailbox` argument names, and
/// a person reads the mailbox, the recipients, the subject and the body
/// before answering (AEGIS ADR-125's Update of 2026-10-07 clause 3 and its
/// Update of 2026-10-07 (3) clause 12). `mail.delete` acts through the same
/// argument and shows the mailbox, the thread, and the thread's subject and
/// senders, which its admission reads before the gate (its Update of
/// 2026-10-08 (4) clause 20); `mail.archive` declares the same (its Update
/// of 2026-10-08 (5) clause 29). The four calendar writes act through the
/// calendar account their `account` argument names, and a person reads the
/// event, its time and its attendees before answering; for update, delete
/// and respond the admission reads those values from the event before the
/// gate (AEGIS ADR-138 K7, K7a). A gateway or MCP tool declares the same
/// keys on its capability entry in the node configuration.
pub const APPROVAL_DECLARATIONS: &[ApprovalDeclaration] = &[
    ApprovalDeclaration {
        tool: "mail.send",
        binding_argument: Some("mailbox"),
        approval_summary: Some(&["mailbox", "to", "cc", "subject", "body"]),
    },
    ApprovalDeclaration {
        tool: "mail.reply",
        binding_argument: Some("mailbox"),
        approval_summary: Some(&["mailbox", "to", "cc", "subject", "body"]),
    },
    ApprovalDeclaration {
        tool: "mail.delete",
        binding_argument: Some("mailbox"),
        approval_summary: Some(&["mailbox", "thread_id", "subject", "from"]),
    },
    ApprovalDeclaration {
        tool: "mail.archive",
        binding_argument: Some("mailbox"),
        approval_summary: Some(&["mailbox", "thread_id", "subject", "from"]),
    },
    ApprovalDeclaration {
        tool: "calendar.create",
        binding_argument: Some("account"),
        approval_summary: Some(&[
            "account",
            "calendar_id",
            "title",
            "start",
            "end",
            "attendees",
            "location",
        ]),
    },
    ApprovalDeclaration {
        tool: "calendar.update",
        binding_argument: Some("account"),
        approval_summary: Some(&[
            "account",
            "event_id",
            "current_title",
            "current_start",
            "title",
            "start",
            "end",
            "attendees",
        ]),
    },
    ApprovalDeclaration {
        tool: "calendar.delete",
        binding_argument: Some("account"),
        approval_summary: Some(&["account", "event_id", "title", "start", "end", "attendees"]),
    },
    ApprovalDeclaration {
        tool: "calendar.respond",
        binding_argument: Some("account"),
        approval_summary: Some(&[
            "account",
            "event_id",
            "title",
            "start",
            "organizer",
            "repeats",
            "response",
        ]),
    },
];

impl ToolInputContract {
    /// Returns the required parameter names for `tool_name`, or an empty slice
    /// for tools with no required parameters or unknown tool names.
    ///
    /// Unknown names return `&[]` — the call is passed through; the router
    /// will reject truly unrecognised tools with an appropriate routing error.
    pub fn required_fields(tool_name: &str) -> &'static [&'static str] {
        match tool_name {
            "aegis.execute" => &["prompt"],
            "aegis.agent.create" | "aegis.agent.update" => &["manifest_yaml"],
            "aegis.agent.export" => &["name"],
            "aegis.agent.delete" | "aegis.task.execute" | "aegis.agent.logs" => &["agent_id"],
            "aegis.agent.generate" | "aegis.workflow.generate" => &["input"],
            "aegis.workflow.validate" => &["manifest_yaml"],
            "aegis.workflow.create" | "aegis.workflow.update" => &["manifest_yaml"],
            "aegis.workflow.export" | "aegis.workflow.delete" | "aegis.workflow.run" => &["name"],
            "aegis.workflow.logs"
            | "aegis.workflow.status"
            | "aegis.workflow.executions.get"
            | "aegis.workflow.cancel"
            | "aegis.workflow.remove" => &["execution_id"],
            "aegis.workflow.signal" => &["execution_id", "response"],
            "aegis.task.status" | "aegis.task.wait" | "aegis.task.logs" | "aegis.task.cancel"
            | "aegis.task.remove" => &["execution_id"],
            "aegis.goal.cancel" => &["goal_id"],
            "aegis.schedule.create" => &["name", "target_kind", "target"],
            "aegis.schedule.get"
            | "aegis.schedule.update"
            | "aegis.schedule.pause"
            | "aegis.schedule.resume"
            | "aegis.schedule.delete"
            | "aegis.schedule.runs" => &["schedule_id"],
            "aegis.document.render" => &["content", "format"],
            "aegis.schema.get" => &["key"],
            "aegis.schema.validate" => &["kind", "manifest_yaml"],
            "cmd.run" => &["command"],
            "fs.read" | "fs.list" | "fs.create_dir" | "fs.delete" => &["path"],
            "fs.write" => &["path", "content"],
            "fs.edit" => &["path", "target_content", "replacement_content"],
            "fs.multi_edit" => &["path", "edits"],
            "fs.grep" | "fs.glob" => &["pattern", "path"],
            "web.search" => &["query"],
            "web.fetch" => &["url"],
            "mail.list" => &["mailbox"],
            "mail.read" | "mail.label" | "mail.delete" | "mail.archive" => {
                &["mailbox", "thread_id"]
            }
            "mail.draft" => &["mailbox", "body"],
            "mail.send" => &["mailbox", "to", "subject", "body"],
            "mail.reply" => &["mailbox", "thread_id", "to", "subject", "body"],
            "mail.attachment" => &["mailbox", "uid", "part"],
            "calendar.calendars" => &["account"],
            "calendar.list" => &["account", "calendar_id"],
            "calendar.read" | "calendar.update" | "calendar.delete" => {
                &["account", "calendar_id", "event_id"]
            }
            "calendar.create" => &["account", "calendar_id", "title", "start", "end"],
            "calendar.respond" => &["account", "calendar_id", "event_id", "response"],
            "aegis.tools.list" | "aegis.tools.search" => &[],
            _ => &[],
        }
    }

    /// What `tool_name`'s input contract declares to the approval gate: the
    /// argument naming the binding the call acts through, and the arguments
    /// its summary lists (AEGIS ADR-126, Update of 2026-10-04, clause 1).
    /// A tool declares them in [`APPROVAL_DECLARATIONS`]; an undeclared tool
    /// gets the empty contract (the gate's fallback).
    pub fn approval_contract(tool_name: &str) -> crate::domain::tool_approval::ApprovalContract {
        APPROVAL_DECLARATIONS
            .iter()
            .find(|d| d.tool == tool_name)
            .map(|d| crate::domain::tool_approval::ApprovalContract {
                binding_argument: d.binding_argument.map(str::to_string),
                approval_summary: d
                    .approval_summary
                    .map(|names| names.iter().map(|n| n.to_string()).collect()),
            })
            .unwrap_or_default()
    }

    /// Validates that `args` satisfies the input contract for `tool_name`.
    ///
    /// Returns `Ok(())` when all required fields are present and non-null.
    /// Returns `Err(message)` with a human-readable description of the first
    /// missing or null field; the caller maps this to the appropriate error type.
    ///
    /// An additional semantic check is applied to `cmd.run`: the `command`
    /// value must be a non-empty, non-whitespace-only string so that the
    /// Dispatch Protocol never spawns a shell with a blank command.
    pub fn validate(tool_name: &str, args: &Value) -> Result<(), String> {
        for field in Self::required_fields(tool_name) {
            let present = args.get(*field).map(|v| !v.is_null()).unwrap_or(false);
            if !present {
                return Err(format!(
                    "required field '{field}' is missing or null for tool '{tool_name}'"
                ));
            }
        }

        // Semantic constraint: a whitespace-only command produces a no-op or
        // provokes confusing shell errors; reject it before dispatch.
        if tool_name == "cmd.run"
            && args
                .get("command")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return Err("'command' must be a non-empty string for 'cmd.run'".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_tool_policy_allowlist() {
        let policy = ToolPolicy {
            allowed_tools: vec!["filesystem.*".to_string(), "web-search.search".to_string()],
            denied_tools: vec!["filesystem.delete".to_string()],
            allowed_paths: vec![PathBuf::from("/workspace")],
            deny_path_traversal: true,
            allowed_domains: vec!["rust-lang.org".to_string()],
            max_calls_per_execution: 10,
            max_calls_per_tool: HashMap::new(),
            timeout_per_call: Duration::from_secs(30),
        };

        let args_read = json!({"path": "/workspace/test.txt"});
        assert!(policy
            .validate_invocation("filesystem.read", &args_read, 0)
            .is_ok());

        let args_delete = json!({"path": "/workspace/test.txt"});
        let result = policy.validate_invocation("filesystem.delete", &args_delete, 0);
        assert!(matches!(
            result,
            Err(PolicyViolation::ToolExplicitlyDenied { .. })
        ));

        let args_unknown = json!({});
        let result = policy.validate_invocation("unknown.tool", &args_unknown, 0);
        assert!(matches!(
            result,
            Err(PolicyViolation::ToolNotAllowed { .. })
        ));
    }

    #[test]
    fn test_tool_policy_filesystem_boundaries() {
        let policy = ToolPolicy {
            allowed_tools: vec!["filesystem.*".to_string()],
            denied_tools: vec![],
            allowed_paths: vec![PathBuf::from("/workspace")],
            deny_path_traversal: true,
            allowed_domains: vec![],
            max_calls_per_execution: 10,
            max_calls_per_tool: HashMap::new(),
            timeout_per_call: Duration::from_secs(30),
        };

        let args_outside = json!({"path": "/etc/passwd"});
        let result = policy.validate_invocation("filesystem.read", &args_outside, 0);
        assert!(matches!(
            result,
            Err(PolicyViolation::PathOutsideBoundary { .. })
        ));

        let args_traversal = json!({"path": "/workspace/../etc/passwd"});
        let result = policy.validate_invocation("filesystem.read", &args_traversal, 0);
        assert!(matches!(
            result,
            Err(PolicyViolation::PathTraversalAttempt { .. })
        ));
    }

    #[test]
    fn test_tool_input_contract_requires_execution_id_for_task_logs() {
        assert_eq!(
            ToolInputContract::required_fields("aegis.task.logs"),
            &["execution_id"]
        );

        let validation = ToolInputContract::validate("aegis.task.logs", &json!({}));
        assert!(validation.is_err());
        assert_eq!(
            validation.unwrap_err(),
            "required field 'execution_id' is missing or null for tool 'aegis.task.logs'"
        );

        assert!(ToolInputContract::validate(
            "aegis.task.logs",
            &json!({"execution_id":"00000000-0000-0000-0000-000000000000"})
        )
        .is_ok());
    }

    #[test]
    fn test_tool_input_contract_requires_execution_id_for_workflow_status() {
        assert_eq!(
            ToolInputContract::required_fields("aegis.workflow.status"),
            &["execution_id"]
        );

        let validation = ToolInputContract::validate("aegis.workflow.status", &json!({}));
        assert!(validation.is_err());
        assert_eq!(
            validation.unwrap_err(),
            "required field 'execution_id' is missing or null for tool 'aegis.workflow.status'"
        );

        assert!(ToolInputContract::validate(
            "aegis.workflow.status",
            &json!({"execution_id":"00000000-0000-0000-0000-000000000000"})
        )
        .is_ok());
    }
}
