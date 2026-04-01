// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Tool Router
//!
//! Infrastructure layer implementation of MCP tool routing and server lifecycle management.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Routes agent tool requests to the correct MCP server process,
//!   manages server lifecycles (start/stop/health), and enforces agent-level
//!   tool authorization via the ToolRegistry.
//!
//! # Related ADRs
//!
//! - ADR-033: Orchestrator-Mediated MCP Tool Routing
//! - ADR-035: Secure Model Context Protocol (SMCP)
//! - ADR-038: Agent Iteration and Tool Gateway

use crate::domain::execution::ExecutionId;
use crate::domain::mcp::{DomainError, ToolRegistry, ToolServer, ToolServerId, ToolServerStatus};
use crate::domain::node_config::BuiltinDispatcherConfig;
use crate::domain::secrets::AccessContext;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::secrets_manager::SecretsManager;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, trace, warn};

// =============================================================================
// InMemoryToolRegistry — Concrete ToolRegistry implementation
// =============================================================================

/// In-memory implementation of the ToolRegistry domain trait.
/// Tracks which tool servers are available globally and per-execution.
/// Used by ToolRouter for agent-level authorization and by ToolServerManager
/// to persist server registrations.
pub struct InMemoryToolRegistry {
    servers_by_execution: Arc<RwLock<HashMap<ExecutionId, Vec<ToolServer>>>>,
    global_servers: Arc<RwLock<Vec<ToolServer>>>,
}

impl InMemoryToolRegistry {
    pub fn new() -> Self {
        Self {
            servers_by_execution: Arc::new(RwLock::new(HashMap::new())),
            global_servers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn add_global_server(&self, server: ToolServer) {
        let mut servers = self.global_servers.write().await;
        servers.push(server);
    }

    pub async fn add_agent_server(&self, execution_id: ExecutionId, server: ToolServer) {
        let mut map = self.servers_by_execution.write().await;
        map.entry(execution_id)
            .or_insert_with(Vec::new)
            .push(server);
    }
}

impl Default for InMemoryToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolRegistry for InMemoryToolRegistry {
    async fn get_tools_for_agent(
        &self,
        execution_id: ExecutionId,
    ) -> Result<Vec<ToolServer>, DomainError> {
        let map = self.servers_by_execution.read().await;
        let mut tools = map.get(&execution_id).cloned().unwrap_or_default();

        let globals = self.global_servers.read().await;
        tools.extend(globals.clone());

        Ok(tools)
    }

    async fn register_tool(&self, server: ToolServer) -> Result<(), DomainError> {
        self.add_global_server(server).await;
        Ok(())
    }
}

// =============================================================================
// RoutingError
// =============================================================================

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("Tool not found: {tool_name}. Available: {available_tools:?}")]
    ToolNotFound {
        tool_name: String,
        available_tools: Vec<String>,
    },

    #[error("Server {0:?} not found in active servers")]
    ServerNotFound(ToolServerId),

    #[error("Server {server_id:?} not ready. Current status: {status:?}")]
    ServerNotReady {
        server_id: ToolServerId,
        status: ToolServerStatus,
    },

    #[error("Agent not authorized to use tool '{tool_name}': {reason}")]
    AgentNotAuthorized { tool_name: String, reason: String },
}

// =============================================================================
// ToolRouter — Routes tool requests to the correct MCP server
// =============================================================================

/// Routes agent tool requests to appropriate MCP servers.
/// Uses the ToolRegistry to enforce agent-level authorization, and the shared
/// servers map + capabilities index for fast capability lookups.
pub struct ToolRouter {
    registry: Arc<dyn ToolRegistry>,
    servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
    capabilities_index: Arc<RwLock<HashMap<String, ToolServerId>>>,
    builtin_dispatchers: Vec<BuiltinDispatcherConfig>,
}

/// Read-only / low-risk tools that bypass the inner-loop semantic judge.
const SKIP_JUDGE_TOOLS: &[&str] = &[
    "fs.read",
    "fs.list",
    "fs.grep",
    "fs.glob",
    "web.search",
    "web.fetch",
    "aegis.schema.get",
    "aegis.schema.validate",
    "aegis.workflow.list",
    "aegis.workflow.export",
    "aegis.workflow.validate",
    "aegis.workflow.status",
    "aegis.workflow.logs",
    "aegis.workflow.executions.list",
    "aegis.workflow.executions.get",
    "aegis.workflow.wait",
    "aegis.workflow.cancel",
    "aegis.workflow.signal",
    "aegis.workflow.remove",
    "aegis.workflow.search",
    "aegis.agent.list",
    "aegis.agent.export",
    "aegis.agent.logs",
    "aegis.agent.search",
    "aegis.task.status",
    "aegis.task.list",
    "aegis.task.logs",
    "aegis.task.wait",
    "aegis.execute.status",
    "aegis.execute.wait",
    "aegis.tools.list",
    "aegis.tools.search",
    "aegis.system.info",
    "aegis.system.config",
];

/// Canonical registry of all builtin tool dispatchers.
///
/// Every tool name and its description live here. The daemon startup and
/// all other consumers derive their dispatcher list from this function —
/// no second list to keep in sync.
const BUILTIN_TOOL_DEFINITIONS: &[(&str, &str)] = &[
    ("cmd.run", "Executes a shell command inside the agent's ephemeral container environment. Use this to build, run, or analyze code locally."),
    ("fs.read", "Read the contents of a file at the given POSIX path from the mounted Workspace volume."),
    ("fs.write", "Write content to a file at the given POSIX path in the Workspace volume. Automatically creates missing parent directories."),
    ("fs.list", "List the contents of a directory in the Workspace volume."),
    ("fs.create_dir", "Creates a new directory along with any necessary parent directories."),
    ("fs.delete", "Deletes a file or directory."),
    ("fs.edit", "Performs an exact string replacement in a file."),
    ("fs.multi_edit", "Performs multiple sequential string replacements in a file."),
    ("fs.grep", "Recursively searches for a regex pattern within files in a given directory."),
    ("fs.glob", "Recursively matches files against a glob pattern."),
    ("web.search", "Performs an internet search query."),
    ("web.fetch", "Fetches content from a URL, optionally converting HTML to Markdown."),
    ("aegis.schema.get", "Returns the canonical JSON Schema for a manifest kind (agent or workflow)."),
    ("aegis.schema.validate", "Validates a manifest YAML string against its canonical JSON Schema."),
    ("aegis.agent.create", "Parses, validates, and deploys an Agent manifest to the registry."),
    ("aegis.agent.list", "Lists currently deployed agents and metadata."),
    ("aegis.agent.update", "Updates an existing Agent manifest in the registry."),
    ("aegis.agent.export", "Exports an Agent manifest by name."),
    ("aegis.agent.delete", "Removes a deployed agent from the registry by UUID."),
    ("aegis.agent.generate", "Generates an Agent manifest from a natural-language intent."),
    ("aegis.agent.logs", "Retrieve agent-level activity log snapshot."),
    ("aegis.agent.search", "Semantic search over deployed agents by natural-language query."),
    ("aegis.workflow.create", "Performs strict deterministic and semantic workflow validation, then registers on pass."),
    ("aegis.workflow.list", "Lists currently registered workflows and metadata."),
    ("aegis.workflow.validate", "Validate a workflow manifest against the schema."),
    ("aegis.workflow.update", "Updates an existing Workflow manifest in the registry."),
    ("aegis.workflow.export", "Exports a Workflow manifest by name."),
    ("aegis.workflow.delete", "Removes a registered workflow from the registry by name."),
    ("aegis.workflow.run", "Executes a registered workflow by name with optional input parameters."),
    ("aegis.workflow.generate", "Generates a Workflow manifest from a natural-language objective."),
    ("aegis.workflow.logs", "Returns paginated workflow execution events."),
    ("aegis.workflow.wait", "Polls a workflow execution until it reaches a terminal state and returns the result."),
    ("aegis.workflow.cancel", "Cancel a running workflow execution."),
    ("aegis.workflow.signal", "Send human input response to a paused workflow execution."),
    ("aegis.workflow.remove", "Remove a workflow execution record."),
    ("aegis.workflow.promote", "Promote a workflow from user scope to tenant scope."),
    ("aegis.workflow.demote", "Demote a workflow from tenant scope to user scope."),
    ("aegis.workflow.executions.list", "Lists workflow executions, optionally filtered."),
    ("aegis.workflow.executions.get", "Returns details of a specific workflow execution."),
    ("aegis.workflow.status", "Returns current status of a workflow execution."),
    ("aegis.workflow.search", "Semantic search over registered workflows."),
    ("aegis.task.execute", "Starts a new agent execution (task) by agent UUID or name."),
    ("aegis.task.status", "Returns the current status and output of an execution by UUID."),
    ("aegis.task.list", "Lists recent executions, optionally filtered by agent."),
    ("aegis.task.cancel", "Cancels an active agent execution by UUID."),
    ("aegis.task.remove", "Removes a completed or failed execution record by UUID."),
    ("aegis.task.logs", "Returns paginated execution events for a task by UUID."),
    ("aegis.task.wait", "Polls an execution until it reaches a terminal state and returns the result."),
    ("aegis.execute.intent", "Starts the intent-to-execution pipeline: discovers or generates an agent, writes code, executes in a container, and returns the formatted result."),
    ("aegis.execute.status", "Returns the current status of an intent-to-execution pipeline run."),
    ("aegis.execute.wait", "Alias for aegis.workflow.wait. Blocks until pipeline execution completes."),
    ("aegis.tools.list", "List all MCP tools available to your security context with pagination and optional source/category filtering."),
    ("aegis.tools.search", "Search for MCP tools by keyword, name pattern, source, category, or tags. Returns tools matching your query within your security context."),
    ("aegis.system.info", "Returns system version, status, and capabilities."),
    ("aegis.system.config", "Returns the current node configuration."),
];

impl ToolRouter {
    /// Returns the canonical list of all builtin tool dispatchers.
    /// This is the single source of truth — the daemon startup and
    /// all other consumers derive their dispatcher list from here.
    pub fn builtin_dispatchers() -> Vec<BuiltinDispatcherConfig> {
        BUILTIN_TOOL_DEFINITIONS
            .iter()
            .map(|(name, description)| {
                let skip_judge = SKIP_JUDGE_TOOLS.contains(name);
                BuiltinDispatcherConfig {
                    name: name.to_string(),
                    description: description.to_string(),
                    enabled: true,
                    capabilities: vec![crate::domain::node_config::CapabilityConfig {
                        name: name.to_string(),
                        skip_judge,
                    }],
                }
            })
            .collect()
    }

    fn is_supported_builtin_workflow_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "aegis.workflow.create"
                | "aegis.workflow.list"
                | "aegis.workflow.validate"
                | "aegis.workflow.update"
                | "aegis.workflow.export"
                | "aegis.workflow.delete"
                | "aegis.workflow.run"
                | "aegis.workflow.executions.list"
                | "aegis.workflow.executions.get"
                | "aegis.workflow.status"
                | "aegis.workflow.generate"
                | "aegis.workflow.logs"
                | "aegis.workflow.wait"
                | "aegis.workflow.cancel"
                | "aegis.workflow.signal"
                | "aegis.workflow.remove"
                | "aegis.workflow.promote"
                | "aegis.workflow.demote"
                | "aegis.execute.intent"
                | "aegis.execute.status"
                | "aegis.execute.wait"
        )
    }

    fn should_advertise_builtin_tool(tool_name: &str) -> bool {
        if tool_name.starts_with("aegis.workflow.") || tool_name.starts_with("aegis.execute.") {
            return Self::is_supported_builtin_workflow_tool(tool_name);
        }

        true
    }

    pub fn new(
        registry: Arc<dyn ToolRegistry>,
        servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
        builtin_dispatchers: Vec<BuiltinDispatcherConfig>,
    ) -> Self {
        Self {
            registry,
            servers,
            capabilities_index: Arc::new(RwLock::new(HashMap::new())),
            builtin_dispatchers,
        }
    }

    /// Find and authorize the server that can handle this tool for the given execution.
    ///
    /// 1. Query the registry for this agent's authorized tools
    /// 2. Verify the requested tool is in the agent's authorized set
    /// 3. Look up the server that provides this capability
    /// 4. Verify the server is running and healthy
    pub async fn route_tool(
        &self,
        execution_id: ExecutionId,
        tool_name: &str,
    ) -> Result<ToolServerId, RoutingError> {
        // Step 1: Fetch the agent's authorized tool servers from the registry
        let agents_tools = self
            .registry
            .get_tools_for_agent(execution_id)
            .await
            .map_err(|e| RoutingError::AgentNotAuthorized {
                tool_name: tool_name.to_string(),
                reason: format!("Could not load agent's tool authorization: {e}"),
            })?;

        // Step 2: Verify the agent is authorized to use this specific tool.
        // If the registry returns an empty set, all global tools are implicitly allowed
        // (this supports the MVP "default unrestricted" security context).
        // If the registry returns a non-empty set, the tool must appear in it.
        if !agents_tools.is_empty() {
            let authorized = agents_tools.iter().any(|srv| srv.can_invoke(tool_name));
            if !authorized {
                let available: Vec<String> = agents_tools
                    .iter()
                    .flat_map(|s| s.capabilities.clone())
                    .collect();
                return Err(RoutingError::AgentNotAuthorized {
                    tool_name: tool_name.to_string(),
                    reason: format!(
                        "Tool not in agent's authorized set. Authorized: {available:?}"
                    ),
                });
            }
        }

        // Step 3: Look up the server providing this capability from the live index
        let index = self.capabilities_index.read().await;

        // Try exact match first
        if let Some(server_id) = index.get(tool_name) {
            return self.verify_server_ready(*server_id).await;
        }

        // Try prefix match (e.g., "filesystem.read" matches "filesystem.*")
        for (capability, server_id) in index.iter() {
            if capability.ends_with(".*") {
                let prefix = capability.trim_end_matches(".*");
                if tool_name.starts_with(prefix) {
                    return self.verify_server_ready(*server_id).await;
                }
            }
        }

        Err(RoutingError::ToolNotFound {
            tool_name: tool_name.to_string(),
            available_tools: index.keys().cloned().collect(),
        })
    }

    /// Get server instance by ID
    pub async fn get_server(&self, server_id: ToolServerId) -> Option<ToolServer> {
        let servers = self.servers.read().await;
        servers.get(&server_id).cloned()
    }

    /// Verify server is running and ready to handle invocations
    async fn verify_server_ready(
        &self,
        server_id: ToolServerId,
    ) -> Result<ToolServerId, RoutingError> {
        let servers = self.servers.read().await;
        let server = servers
            .get(&server_id)
            .ok_or(RoutingError::ServerNotFound(server_id))?;

        if server.status != ToolServerStatus::Running {
            return Err(RoutingError::ServerNotReady {
                server_id,
                status: server.status.clone(),
            });
        }

        Ok(server_id)
    }

    /// Rebuild the capability → server_id index from the live servers map.
    /// Called after server registration or status changes.
    pub async fn rebuild_index(&self) {
        let servers = self.servers.read().await;
        let mut index = self.capabilities_index.write().await;
        index.clear();

        for (server_id, server) in servers.iter() {
            if server.status == ToolServerStatus::Running
                || server.status == ToolServerStatus::Stopped
            {
                for capability in &server.capabilities {
                    index.insert(capability.clone(), *server_id);
                }
            }
        }

        trace!("Rebuilt capabilities index with {} entries", index.len());
    }

    /// Add a server to the live servers map and rebuild the index
    pub async fn add_server(&self, server: ToolServer) -> anyhow::Result<()> {
        let id = server.id;
        let name = server.name.clone();
        {
            let mut servers = self.servers.write().await;
            servers.insert(id, server);
        }
        self.rebuild_index().await;
        info!("Added server '{}' ({:?}) to router", name, id);
        Ok(())
    }

    /// List all tools from running servers with their metadata
    pub async fn list_tools(&self) -> anyhow::Result<Vec<ToolMetadata>> {
        let servers = self.servers.read().await;
        let mut all_tools = Vec::new();

        for server in servers.values() {
            if server.status == ToolServerStatus::Running {
                for cap in &server.capabilities {
                    all_tools.push(ToolMetadata {
                        name: cap.clone(),
                        description: format!("Provided by MCP server '{}'", server.name),
                        input_schema: json!({ "type": "object" }),
                    });
                }
            }
        }

        for dispatcher in &self.builtin_dispatchers {
            for cap in &dispatcher.capabilities {
                if !Self::should_advertise_builtin_tool(&cap.name) {
                    continue;
                }

                let schema = match cap.name.as_str() {
                    "cmd.run" => json!({
                        "type": "object",
                        "properties": {
                            "command": {
                                "type": "string",
                                "description": "Command to execute"
                            }
                        },
                        "required": ["command"]
                    }),
                    "fs.read" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the file to read."
                            }
                        },
                        "required": ["path"]
                    }),
                    "fs.write" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the file to write."
                            },
                            "content": {
                                "type": "string",
                                "description": "String content to write to the file."
                            }
                        },
                        "required": ["path", "content"]
                    }),
                    "fs.list" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the directory to list."
                            }
                        },
                        "required": ["path"]
                    }),
                    "fs.create_dir" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the directory to create."
                            }
                        },
                        "required": ["path"]
                    }),
                    "fs.delete" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the file or directory to delete."
                            },
                            "recursive": {
                                "type": "boolean",
                                "description": "Set to true to delete a directory and all its contents."
                            }
                        },
                        "required": ["path"]
                    }),
                    "fs.edit" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the file to edit."
                            },
                            "target_content": {
                                "type": "string",
                                "description": "Exact string content to find and replace. Must match exactly once."
                            },
                            "replacement_content": {
                                "type": "string",
                                "description": "New string content to insert in place of target_content."
                            }
                        },
                        "required": ["path", "target_content", "replacement_content"]
                    }),
                    "fs.multi_edit" => json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Absolute or relative POSIX path of the file to edit."
                            },
                            "edits": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "target_content": { "type": "string" },
                                        "replacement_content": { "type": "string" }
                                    },
                                    "required": ["target_content", "replacement_content"]
                                },
                                "description": "Array of edits to apply sequentially."
                            }
                        },
                        "required": ["path", "edits"]
                    }),
                    "fs.grep" => json!({
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Regex pattern to search for."
                            },
                            "path": {
                                "type": "string",
                                "description": "Directory path to start the recursive search from."
                            }
                        },
                        "required": ["pattern", "path"]
                    }),
                    "fs.glob" => json!({
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Glob pattern to match files (e.g. *.rs)."
                            },
                            "path": {
                                "type": "string",
                                "description": "Directory path to start the recursive search from."
                            }
                        },
                        "required": ["pattern", "path"]
                    }),
                    "web.search" => json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query."
                            }
                        },
                        "required": ["query"]
                    }),
                    "web.fetch" => json!({
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "URL to fetch content from."
                            }
                        },
                        "required": ["url"]
                    }),
                    "aegis.schema.get" => json!({
                        "type": "object",
                        "properties": {
                            "key": {
                                "type": "string",
                                "description": "Schema key to retrieve. Supported: \"agent/manifest/v1\", \"workflow/manifest/v1\""
                            }
                        },
                        "required": ["key"]
                    }),
                    "aegis.schema.validate" => json!({
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "description": "Manifest kind to validate against. Supported: \"agent\", \"workflow\""
                            },
                            "manifest_yaml": {
                                "type": "string",
                                "description": "Full manifest YAML text to validate against the canonical schema."
                            }
                        },
                        "required": ["kind", "manifest_yaml"]
                    }),
                    "aegis.agent.create" => json!({
                        "type": "object",
                        "properties": {
                            "manifest_yaml": {
                                "type": "string",
                                "description": "Full Agent manifest YAML to parse, validate, and deploy."
                            },
                            "force": {
                                "type": "boolean",
                                "description": "Overwrite an existing deployed agent with the same name/version."
                            }
                        },
                        "required": ["manifest_yaml"]
                    }),
                    "aegis.agent.list" => json!({
                        "type": "object",
                        "properties": {}
                    }),
                    "aegis.agent.update" => json!({
                        "type": "object",
                        "properties": {
                            "manifest_yaml": {
                                "type": "string",
                                "description": "Full Agent manifest YAML to update an existing agent."
                            },
                            "force": {
                                "type": "boolean",
                                "description": "Overwrite an existing version if it already exists."
                            }
                        },
                        "required": ["manifest_yaml"]
                    }),
                    "aegis.agent.export" => json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Name of the agent to export."
                            }
                        },
                        "required": ["name"]
                    }),
                    "aegis.agent.delete" => json!({
                        "type": "object",
                        "properties": {
                            "agent_id": {
                                "type": "string",
                                "description": "UUID of the agent to remove."
                            }
                        },
                        "required": ["agent_id"]
                    }),
                    "aegis.agent.generate" => json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Natural-language intent for the agent to create."
                            }
                        },
                        "required": ["input"]
                    }),
                    "aegis.agent.logs" => json!({
                        "type": "object",
                        "properties": {
                            "agent_id": {
                                "type": "string",
                                "description": "UUID of the agent whose activity log should be retrieved."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of events to return.",
                                "default": 50
                            },
                            "offset": {
                                "type": "integer",
                                "description": "Zero-based starting offset into the activity log.",
                                "default": 0
                            }
                        },
                        "required": ["agent_id"]
                    }),
                    "aegis.workflow.list" => json!({
                        "type": "object",
                        "properties": {
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            },
                            "scope": {
                                "type": "string",
                                "enum": ["global", "visible"],
                                "description": "Optional scope filter. 'global' lists only global workflows. 'visible' lists user+tenant+global. Omit to list all for tenant."
                            },
                            "user_id": {
                                "type": "string",
                                "description": "Optional user ID for 'visible' scope filter."
                            }
                        }
                    }),
                    "aegis.workflow.validate" => json!({
                        "type": "object",
                        "properties": {
                            "manifest_yaml": {
                                "type": "string",
                                "description": "Full Workflow manifest YAML to parse and deterministically validate."
                            }
                        },
                        "required": ["manifest_yaml"]
                    }),
                    "aegis.workflow.update" => json!({
                        "type": "object",
                        "properties": {
                            "manifest_yaml": {
                                "type": "string",
                                "description": "Full Workflow manifest YAML to update an existing workflow."
                            },
                            "force": {
                                "type": "boolean",
                                "description": "Overwrite an existing version if it already exists."
                            }
                        },
                        "required": ["manifest_yaml"]
                    }),
                    "aegis.workflow.export" => json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Name of the workflow to export."
                            }
                        },
                        "required": ["name"]
                    }),
                    "aegis.workflow.delete" => json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Name of the workflow to delete."
                            }
                        },
                        "required": ["name"]
                    }),
                    "aegis.workflow.run" => json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Name of the workflow to execute."
                            },
                            "input": {
                                "type": "object",
                                "description": "Workflow input parameters."
                            },
                            "blackboard": {
                                "type": "object",
                                "description": "Optional blackboard overrides merged into the workflow execution before startup."
                            },
                            "version": {
                                "type": "string",
                                "description": "Optional semantic version of the workflow to execute. When omitted, the latest deployed version is used."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["name"]
                    }),
                    "aegis.workflow.executions.list" => json!({
                        "type": "object",
                        "properties": {
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results to return.",
                                "default": 20
                            },
                            "offset": {
                                "type": "integer",
                                "description": "Pagination offset.",
                                "default": 0
                            },
                            "workflow_id": {
                                "type": "string",
                                "description": "Optional workflow UUID or workflow name filter."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        }
                    }),
                    "aegis.workflow.executions.get" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow execution to inspect."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.workflow.status" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow execution to inspect."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.workflow.generate" => json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Natural-language workflow objective."
                            }
                        },
                        "required": ["input"]
                    }),
                    "aegis.workflow.wait" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow execution to wait for."
                            },
                            "poll_interval_seconds": {
                                "type": "integer",
                                "description": "Seconds between polls (default: 5)."
                            },
                            "timeout_seconds": {
                                "type": "integer",
                                "description": "Maximum wait time in seconds (default: 300)."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.workflow.cancel" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow execution to cancel."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.workflow.signal" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow execution to signal."
                            },
                            "response": {
                                "type": "string",
                                "description": "Human input response text to send to the paused workflow."
                            }
                        },
                        "required": ["execution_id", "response"]
                    }),
                    "aegis.workflow.remove" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow execution to remove."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.workflow.promote" => json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Workflow name or ID to promote."
                            },
                            "target_scope": {
                                "type": "string",
                                "enum": ["tenant", "global"],
                                "description": "Target scope (default: global)."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["name"]
                    }),
                    "aegis.workflow.demote" => json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Workflow name or ID to demote."
                            },
                            "target_scope": {
                                "type": "string",
                                "enum": ["tenant", "user"],
                                "description": "Target scope (default: tenant)."
                            },
                            "user_id": {
                                "type": "string",
                                "description": "Owner user ID when demoting to user scope."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["name"]
                    }),
                    "aegis.execute.intent" => json!({
                        "type": "object",
                        "properties": {
                            "intent": {
                                "type": "string",
                                "description": "Natural-language description of what to execute (e.g. 'resize images in /workspace to 800x600')."
                            },
                            "inputs": {
                                "type": "object",
                                "description": "Optional structured inputs passed to the pipeline."
                            },
                            "volume_id": {
                                "type": "string",
                                "description": "Optional persistent volume ID to use as workspace. When omitted, an ephemeral volume is created."
                            },
                            "language": {
                                "type": "string",
                                "enum": ["python", "javascript", "bash"],
                                "description": "Execution language (default: python)."
                            },
                            "timeout_seconds": {
                                "type": "integer",
                                "description": "Optional execution timeout in seconds."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["intent"]
                    }),
                    "aegis.execute.status" => json!({
                        "type": "object",
                        "properties": {
                            "pipeline_execution_id": {
                                "type": "string",
                                "description": "UUID of the pipeline execution to check."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Optional tenant identifier. Defaults to the local tenant."
                            }
                        },
                        "required": ["pipeline_execution_id"]
                    }),
                    "aegis.execute.wait" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the workflow/pipeline execution to wait for."
                            },
                            "poll_interval_seconds": {
                                "type": "integer",
                                "description": "Seconds between polls (default: 5)."
                            },
                            "timeout_seconds": {
                                "type": "integer",
                                "description": "Maximum wait time in seconds (default: 300)."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.task.execute" => json!({
                        "type": "object",
                        "properties": {
                            "agent_id": {
                                "type": "string",
                                "description": "UUID or Name of the agent to execute."
                            },
                            "input": {
                                "type": "object",
                                "description": "Task instructions and data for the agent. Pass the user's request as { \"prompt\": \"<the full task description>\" }. Always include this field with the user's intent.",
                                "properties": {
                                    "prompt": {
                                        "type": "string",
                                        "description": "The full task instructions or user request to pass to the agent."
                                    }
                                }
                            },
                            "version": {
                                "type": "string",
                                "description": "Optional semantic version of the agent to execute. When omitted, the latest deployed version is used."
                            }
                        },
                        "required": ["agent_id"]
                    }),
                    "aegis.task.status" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the execution to check."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.task.wait" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the execution to wait for."
                            },
                            "poll_interval_seconds": {
                                "type": "integer",
                                "description": "Seconds between status polls (default 10).",
                                "minimum": 1
                            },
                            "timeout_seconds": {
                                "type": "integer",
                                "description": "Maximum seconds to wait before returning a timeout (default 300).",
                                "minimum": 1
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.task.logs" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the execution whose event log should be retrieved."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of events to return.",
                                "default": 50
                            },
                            "offset": {
                                "type": "integer",
                                "description": "Zero-based starting offset into the persisted event log.",
                                "default": 0
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.task.list" => json!({
                        "type": "object",
                        "properties": {
                            "agent_id": {
                                "type": "string",
                                "description": "Optional UUID to filter by agent."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results.",
                                "default": 20
                            }
                        }
                    }),
                    "aegis.task.cancel" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the execution to cancel."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.task.remove" => json!({
                        "type": "object",
                        "properties": {
                            "execution_id": {
                                "type": "string",
                                "description": "UUID of the execution to remove."
                            }
                        },
                        "required": ["execution_id"]
                    }),
                    "aegis.system.info" => json!({
                        "type": "object",
                        "properties": {}
                    }),
                    "aegis.system.config" => json!({
                        "type": "object",
                        "properties": {}
                    }),
                    "aegis.agent.search" => json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural-language description of the agent you are looking for."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Tenant ID to search within. Defaults to current tenant."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum results (1-100, tier-dependent cap). Default: 10."
                            },
                            "min_score": {
                                "type": "number",
                                "description": "Minimum relevance score threshold (0.0-1.0). Default: 0.3."
                            },
                            "labels": {
                                "type": "object",
                                "description": "Label key-value pairs to filter by. All must match.",
                                "additionalProperties": { "type": "string" }
                            },
                            "status": {
                                "type": "string",
                                "description": "Filter by agent status.",
                                "enum": ["active", "paused", "failed"]
                            },
                            "include_platform_templates": {
                                "type": "boolean",
                                "description": "Include platform-provided template agents. Default: true."
                            }
                        },
                        "required": ["query"]
                    }),
                    "aegis.workflow.search" => json!({
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural-language description of the workflow you are looking for."
                            },
                            "tenant_id": {
                                "type": "string",
                                "description": "Tenant ID to search within. Defaults to current tenant."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum results (1-100, tier-dependent cap). Default: 10."
                            },
                            "min_score": {
                                "type": "number",
                                "description": "Minimum relevance score threshold (0.0-1.0). Default: 0.3."
                            },
                            "labels": {
                                "type": "object",
                                "description": "Label key-value pairs to filter by. All must match.",
                                "additionalProperties": { "type": "string" }
                            },
                            "include_platform_templates": {
                                "type": "boolean",
                                "description": "Include platform-provided template workflows. Default: true."
                            }
                        },
                        "required": ["query"]
                    }),
                    "aegis.workflow.create" => json!({
                        "type": "object",
                        "properties": {
                            "manifest_yaml": {
                                "type": "string",
                                "description": "Full Workflow manifest YAML to parse, validate, semantically judge, and register."
                            },
                            "force": {
                                "type": "boolean",
                                "description": "Overwrite an existing version if it already exists."
                            },
                            "task_context": {
                                "type": "string",
                                "description": "Optional task context to guide semantic judges."
                            },
                            "judge_agents": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Judge agent names to use for semantic validation."
                            },
                            "min_score": {
                                "type": "number",
                                "description": "Minimum consensus score required for deployment."
                            },
                            "min_confidence": {
                                "type": "number",
                                "description": "Minimum consensus confidence required for deployment."
                            }
                        },
                        "required": ["manifest_yaml"]
                    }),
                    _ => json!({ "type": "object" }),
                };

                all_tools.push(ToolMetadata {
                    name: cap.name.clone(),
                    description: dispatcher.description.clone(),
                    input_schema: schema,
                });
            }
        }

        Ok(all_tools)
    }

    /// Returns `true` if the operator has flagged `tool_name` to bypass the inner-loop
    /// semantic judge.  Checks builtin dispatchers first, then MCP server entries.
    ///
    /// Called by `ToolInvocationService::invoke_tool_internal` before running the
    /// `spec.execution.tool_validation` pipeline (see ADR-049 and NODE_CONFIGURATION_SPEC_V1.md).
    pub async fn is_skip_judge(&self, tool_name: &str) -> bool {
        // 1. Builtin dispatchers — iterate CapabilityConfig entries directly.
        for dispatcher in &self.builtin_dispatchers {
            for cap in &dispatcher.capabilities {
                if cap.name == tool_name && cap.skip_judge {
                    return true;
                }
            }
        }

        // 2. MCP server ToolServer entries — ask each server whether this tool is flagged.
        let servers = self.servers.read().await;
        for server in servers.values() {
            if server.is_skip_judge(tool_name) {
                return true;
            }
        }

        false
    }
}

// =============================================================================
// ToolMetadata — Tool discovery metadata
// =============================================================================

/// Tool metadata exposed to LLM prompts for tool discovery and schema injection.
///
/// Fields are serialized as camelCase to match the MCP protocol specification
/// (e.g., `input_schema` → `inputSchema`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

// =============================================================================
// ManagerError
// =============================================================================

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("Failed to start server '{0}': {1}")]
    StartFailed(String, String),

    #[error("Health check error for server '{0}': {1}")]
    HealthCheckError(String, String),
}

// =============================================================================
// ToolServerManager — Lifecycle management for MCP server processes
// =============================================================================

/// Manages the lifecycle of MCP server processes: starting, stopping,
/// health checking, and registering them into the ToolRegistry.
pub struct ToolServerManager {
    registry: Arc<dyn ToolRegistry>,
    servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
    event_bus: Arc<EventBus>,
    secrets_manager: Arc<SecretsManager>,
}

impl ToolServerManager {
    pub fn new(
        registry: Arc<dyn ToolRegistry>,
        servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
        event_bus: Arc<EventBus>,
        secrets_manager: Arc<SecretsManager>,
    ) -> Self {
        Self {
            registry,
            servers,
            event_bus,
            secrets_manager,
        }
    }

    /// Start all configured servers that are currently in Stopped state.
    /// Each successfully started server is registered into the ToolRegistry
    /// so that ToolRouter can authorize agent access to them.
    pub async fn start_all(&self) -> Result<Vec<ToolServerId>, ManagerError> {
        let mut servers = self.servers.write().await;
        let mut started = vec![];

        for (server_id, server) in servers.iter_mut() {
            if server.status == ToolServerStatus::Stopped {
                match self.start_server(server).await {
                    Ok(event) => {
                        // Register server into the ToolRegistry for agent authorization
                        if let Err(e) = self.registry.register_tool(server.clone()).await {
                            warn!(
                                "Failed to register server '{}' in registry: {}",
                                server.name, e
                            );
                        }

                        self.event_bus.publish_mcp_event(event);
                        info!(
                            "Started MCP server '{}' (pid: {:?})",
                            server.name, server.process_id
                        );
                        started.push(*server_id);
                    }
                    Err(e) => {
                        error!("Failed to start MCP server '{}': {}", server.name, e);
                    }
                }
            }
        }

        if started.is_empty() {
            info!("No MCP servers configured to start");
        } else {
            info!("Started {} MCP server(s)", started.len());
        }

        Ok(started)
    }

    /// Start a single MCP server process.
    /// Sets the domain state transitions properly: Stopped → Starting → Running.
    async fn start_server(
        &self,
        server: &mut ToolServer,
    ) -> Result<crate::domain::events::MCPToolEvent, ManagerError> {
        info!(
            "Starting MCP server '{}' from '{}'",
            server.name,
            server.executable_path.display()
        );

        // Transition to Starting state first (domain validation)
        let start_event = server
            .start()
            .map_err(|e| ManagerError::StartFailed(server.name.clone(), e.to_string()))?;

        // Spawn the actual process
        let mut command = tokio::process::Command::new(&server.executable_path);
        command.args(&server.args);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());

        // Inject credentials as environment variables (ADR-034 Keymaster pattern).
        // Credentials are resolved from the secret store and injected here;
        // the subprocess never receives vault tokens or raw secret paths.
        let cred_context = AccessContext::system("orchestrator");
        for (env_key, cred_ref) in &server.credentials {
            match self
                .secrets_manager
                .resolve_credential(cred_ref, &cred_context)
                .await
            {
                Ok(sensitive_value) => {
                    command.env(env_key, sensitive_value.expose());
                }
                Err(e) => {
                    warn!(
                        "Failed to resolve credential '{}' for server '{}': {}",
                        env_key, server.name, e
                    );
                }
            }
        }

        let child = command.spawn().map_err(|e| {
            // Revert domain state on spawn failure
            server.status = ToolServerStatus::Failed;
            ManagerError::StartFailed(
                server.name.clone(),
                format!(
                    "Failed to spawn process '{}': {}",
                    server.executable_path.display(),
                    e
                ),
            )
        })?;

        let pid = child.id().unwrap_or(0);

        // Update domain object with process details
        server.process_id = child.id();
        server.status = ToolServerStatus::Running;

        info!("MCP server '{}' spawned with PID {}", server.name, pid);

        // Return the start event with the actual PID
        if let crate::domain::events::MCPToolEvent::ServerStarted {
            server_id,
            name,
            process_id: _,
            started_at,
        } = start_event
        {
            Ok(crate::domain::events::MCPToolEvent::ServerStarted {
                server_id,
                name,
                process_id: pid,
                started_at,
            })
        } else {
            Ok(start_event)
        }
    }

    /// Background health check loop. Runs continuously, checking all Running
    /// servers at regular intervals. Marks servers as Unhealthy after failures.
    pub async fn health_check_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

        loop {
            interval.tick().await;

            let mut servers = self.servers.write().await;
            for server in servers.values_mut() {
                if server.status == ToolServerStatus::Running {
                    match self.check_server_health(server).await {
                        Ok(true) => {
                            trace!(
                                "Server '{}' healthy (pid: {:?})",
                                server.name,
                                server.process_id
                            );
                            server.record_health_check(true);
                        }
                        Ok(false) => {
                            warn!(
                                "Server '{}' unhealthy (pid: {:?})",
                                server.name, server.process_id
                            );
                            if let Some(evt) = server.record_health_check(false) {
                                self.event_bus.publish_mcp_event(evt);
                            }
                        }
                        Err(e) => {
                            error!("Health check error for server '{}': {}", server.name, e);
                            if let Some(evt) = server.record_health_check(false) {
                                self.event_bus.publish_mcp_event(evt);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Check if a server's process is still alive by checking PID existence.
    /// A more sophisticated implementation would send a JSON-RPC `tools/list`
    /// request via stdio, but checking process liveness is the baseline.
    async fn check_server_health(&self, server: &ToolServer) -> Result<bool, ManagerError> {
        match server.process_id {
            Some(pid) => {
                // Check if the process is still running by attempting to query it.
                // On Unix, we'd use kill(pid, 0). On Windows, OpenProcess.
                // tokio::process doesn't expose this directly, so we use a platform check.
                let is_alive = Self::is_process_alive(pid);
                if !is_alive {
                    warn!(
                        "MCP server '{}' process (PID {}) is no longer running",
                        server.name, pid
                    );
                }
                Ok(is_alive)
            }
            None => {
                // No PID means the server was never properly started
                warn!(
                    "MCP server '{}' has no PID — treating as unhealthy",
                    server.name
                );
                Ok(false)
            }
        }
    }

    /// Platform-specific process liveness check.
    fn is_process_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // Send signal 0 to check if process exists (no signal actually sent).
            // Uses `kill -0` via Command to avoid requiring the libc crate.
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        #[cfg(windows)]
        {
            // Use tasklist to check if PID exists
            match std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {}", pid), "/NH"])
                .output()
            {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // tasklist returns the process info if it exists,
                    // or "INFO: No tasks are running..." if it doesn't
                    !stdout.contains("No tasks")
                }
                Err(_) => {
                    // If tasklist itself fails, assume the process is alive
                    // to avoid false-positive unhealthy marks
                    true
                }
            }
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mcp::*;
    use crate::domain::node_config::{BuiltinDispatcherConfig, CapabilityConfig};
    use std::path::PathBuf;
    use std::time::Duration;

    fn make_test_server(name: &str, capabilities: Vec<&str>) -> ToolServer {
        ToolServer {
            id: ToolServerId::new(),
            name: name.to_string(),
            execution_mode: ExecutionMode::Remote,
            executable_path: PathBuf::from("/usr/local/bin/mcp-test"),
            args: vec![],
            capabilities: capabilities.into_iter().map(|s| s.to_string()).collect(),
            skip_judge_tools: std::collections::HashSet::new(),
            status: ToolServerStatus::Running,
            process_id: None,
            health_check_interval: Duration::from_secs(60),
            last_health_check: None,
            credentials: HashMap::new(),
            resource_limits: ResourceLimits {
                max_memory_mb: Some(512),
                max_cpu_shares: Some(1000),
            },
            started_at: None,
            stopped_at: None,
        }
    }

    #[tokio::test]
    async fn test_route_tool_exact_match() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(RwLock::new(HashMap::new()));
        let server = make_test_server("filesystem", vec!["filesystem.read", "filesystem.write"]);
        let server_id = server.id;
        servers.write().await.insert(server_id, server);

        let router = ToolRouter::new(registry, servers, vec![]);
        router.rebuild_index().await;

        let exec_id = ExecutionId::new();
        let result = router.route_tool(exec_id, "filesystem.read").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), server_id);
    }

    #[tokio::test]
    async fn test_route_tool_not_found() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(RwLock::new(HashMap::new()));
        let router = ToolRouter::new(registry, servers, vec![]);
        router.rebuild_index().await;

        let exec_id = ExecutionId::new();
        let result = router.route_tool(exec_id, "nonexistent.tool").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_route_tool_server_not_ready() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(RwLock::new(HashMap::new()));
        let mut server = make_test_server("gmail", vec!["gmail.send"]);
        server.status = ToolServerStatus::Failed;
        let server_id = server.id;
        servers.write().await.insert(server_id, server);

        let router = ToolRouter::new(registry, servers, vec![]);
        router.rebuild_index().await;

        let exec_id = ExecutionId::new();
        let result = router.route_tool(exec_id, "gmail.send").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_route_tool_agent_authorization() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let exec_id = ExecutionId::new();

        // Register a specific tool set for this agent
        let authorized_server = make_test_server("filesystem", vec!["filesystem.read"]);
        registry.add_agent_server(exec_id, authorized_server).await;

        let servers = Arc::new(RwLock::new(HashMap::new()));
        let router_server = make_test_server("gmail", vec!["gmail.send"]);
        let gmail_id = router_server.id;
        servers.write().await.insert(gmail_id, router_server);

        let router = ToolRouter::new(registry, servers, vec![]);
        router.rebuild_index().await;

        // Agent is only authorized for filesystem.read, not gmail.send
        let result = router.route_tool(exec_id, "gmail.send").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RoutingError::AgentNotAuthorized { .. }),
            "Expected AgentNotAuthorized, got {err:?}"
        );
        if let RoutingError::AgentNotAuthorized { tool_name, .. } = err {
            assert_eq!(tool_name, "gmail.send");
        }
    }

    #[tokio::test]
    async fn test_list_tools_only_running() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(RwLock::new(HashMap::new()));

        let running = make_test_server("filesystem", vec!["filesystem.read"]);
        let mut stopped = make_test_server("gmail", vec!["gmail.send"]);
        stopped.status = ToolServerStatus::Stopped;

        servers.write().await.insert(running.id, running);
        servers.write().await.insert(stopped.id, stopped);

        let router = ToolRouter::new(registry, servers, vec![]);
        let tools = router.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "filesystem.read");
    }

    #[tokio::test]
    async fn test_registry_global_and_per_execution() {
        let registry = InMemoryToolRegistry::new();
        let global_server = make_test_server("filesystem", vec!["filesystem.read"]);
        registry.add_global_server(global_server).await;

        let exec_id = ExecutionId::new();
        let agent_server = make_test_server("gmail", vec!["gmail.send"]);
        registry.add_agent_server(exec_id, agent_server).await;

        let tools = registry.get_tools_for_agent(exec_id).await.unwrap();
        assert_eq!(tools.len(), 2);

        // Different execution should only see globals
        let other_exec = ExecutionId::new();
        let other_tools = registry.get_tools_for_agent(other_exec).await.unwrap();
        assert_eq!(other_tools.len(), 1);
    }

    #[tokio::test]
    async fn test_list_tools_includes_aegis_authoring_tool_schemas() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(RwLock::new(HashMap::new()));

        let builtins = vec![
            BuiltinDispatcherConfig {
                name: "aegis.agent.create".to_string(),
                description: "Create and deploy agent manifests".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.agent.create".to_string(),
                    skip_judge: false,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.create".to_string(),
                description: "Create, validate, and register workflows".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.create".to_string(),
                    skip_judge: false,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.task.logs".to_string(),
                description: "Inspect persisted task execution events".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.task.logs".to_string(),
                    skip_judge: true,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.status".to_string(),
                description: "Inspect workflow execution state".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.status".to_string(),
                    skip_judge: true,
                }],
            },
        ];

        let router = ToolRouter::new(registry, servers, builtins);
        let tools = router.list_tools().await.unwrap();

        let agent_tool = tools.iter().find(|t| t.name == "aegis.agent.create");
        assert!(agent_tool.is_some(), "expected aegis.agent.create tool");
        let agent_schema = &agent_tool.unwrap().input_schema;
        assert_eq!(
            agent_schema["required"][0].as_str(),
            Some("manifest_yaml"),
            "manifest_yaml must be required for aegis.agent.create"
        );

        let workflow_tool = tools.iter().find(|t| t.name == "aegis.workflow.create");
        assert!(
            workflow_tool.is_some(),
            "expected aegis.workflow.create tool"
        );
        let workflow_schema = &workflow_tool.unwrap().input_schema;
        assert_eq!(
            workflow_schema["required"][0].as_str(),
            Some("manifest_yaml"),
            "manifest_yaml must be required for aegis.workflow.create"
        );
        assert!(
            workflow_schema["properties"]["judge_agents"].is_object(),
            "judge_agents property should be present in workflow schema"
        );

        let task_logs_tool = tools.iter().find(|t| t.name == "aegis.task.logs");
        assert!(task_logs_tool.is_some(), "expected aegis.task.logs tool");
        let task_logs_schema = &task_logs_tool.unwrap().input_schema;
        assert_eq!(
            task_logs_schema["required"][0].as_str(),
            Some("execution_id"),
            "execution_id must be required for aegis.task.logs"
        );
        assert_eq!(
            task_logs_schema["properties"]["limit"]["default"],
            json!(50)
        );
        assert_eq!(
            task_logs_schema["properties"]["offset"]["default"],
            json!(0)
        );

        let workflow_status_tool = tools.iter().find(|t| t.name == "aegis.workflow.status");
        assert!(
            workflow_status_tool.is_some(),
            "expected aegis.workflow.status tool"
        );
        let workflow_status_schema = &workflow_status_tool.unwrap().input_schema;
        assert_eq!(
            workflow_status_schema["required"][0].as_str(),
            Some("execution_id"),
            "execution_id must be required for aegis.workflow.status"
        );
    }

    #[tokio::test]
    async fn test_list_tools_advertises_implemented_workflow_builtins() {
        let registry = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(RwLock::new(HashMap::new()));

        let builtins = vec![
            BuiltinDispatcherConfig {
                name: "aegis.workflow.cancel".to_string(),
                description: "Cancel workflow executions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.cancel".to_string(),
                    skip_judge: false,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.signal".to_string(),
                description: "Signal workflow executions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.signal".to_string(),
                    skip_judge: false,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.remove".to_string(),
                description: "Remove workflow executions".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.remove".to_string(),
                    skip_judge: false,
                }],
            },
            BuiltinDispatcherConfig {
                name: "aegis.workflow.status".to_string(),
                description: "Inspect workflow execution state".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "aegis.workflow.status".to_string(),
                    skip_judge: true,
                }],
            },
        ];

        let router = ToolRouter::new(registry, servers, builtins);
        let tools = router.list_tools().await.unwrap();

        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.status"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.cancel"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.signal"));
        assert!(tools
            .iter()
            .any(|tool| tool.name == "aegis.workflow.remove"));
    }
}
