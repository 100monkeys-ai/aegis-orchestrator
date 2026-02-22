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

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Serialize, Deserialize};
use serde_json::{json, Value};
use tracing::{info, warn, error, trace};
use crate::domain::mcp::{ToolServer, ToolServerId, ToolServerStatus, ToolRegistry, DomainError};
use crate::domain::execution::ExecutionId;
use crate::infrastructure::event_bus::EventBus;
use async_trait::async_trait;

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
        map.entry(execution_id).or_insert_with(Vec::new).push(server);
    }
}

#[async_trait]
impl ToolRegistry for InMemoryToolRegistry {
    async fn get_tools_for_agent(&self, execution_id: ExecutionId) -> Result<Vec<ToolServer>, DomainError> {
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
    ToolNotFound { tool_name: String, available_tools: Vec<String> },
    
    #[error("Server {0:?} not found in active servers")]
    ServerNotFound(ToolServerId),
    
    #[error("Server {server_id:?} not ready. Current status: {status:?}")]
    ServerNotReady { server_id: ToolServerId, status: ToolServerStatus },
    
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
}

impl ToolRouter {
    pub fn new(
        registry: Arc<dyn ToolRegistry>,
        servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
    ) -> Self {
        Self {
            registry,
            servers,
            capabilities_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Find and authorize the server that can handle this tool for the given execution.
    ///
    /// 1. Query the registry for this agent's authorized tools
    /// 2. Verify the requested tool is in the agent's authorized set
    /// 3. Look up the server that provides this capability
    /// 4. Verify the server is running and healthy
    pub async fn route_tool(&self, execution_id: ExecutionId, tool_name: &str) -> Result<ToolServerId, RoutingError> {
        // Step 1: Fetch the agent's authorized tool servers from the registry
        let agents_tools = self.registry.get_tools_for_agent(execution_id).await
            .map_err(|e| RoutingError::AgentNotAuthorized {
                tool_name: tool_name.to_string(),
                reason: format!("Could not load agent's tool authorization: {}", e),
            })?;
        
        // Step 2: Verify the agent is authorized to use this specific tool.
        // If the registry returns an empty set, all global tools are implicitly allowed
        // (this supports the MVP "default unrestricted" security context).
        // If the registry returns a non-empty set, the tool must appear in it.
        if !agents_tools.is_empty() {
            let authorized = agents_tools.iter().any(|srv| srv.can_invoke(tool_name));
            if !authorized {
                let available: Vec<String> = agents_tools.iter()
                    .flat_map(|s| s.capabilities.clone())
                    .collect();
                return Err(RoutingError::AgentNotAuthorized {
                    tool_name: tool_name.to_string(),
                    reason: format!("Tool not in agent's authorized set. Authorized: {:?}", available),
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
    async fn verify_server_ready(&self, server_id: ToolServerId) -> Result<ToolServerId, RoutingError> {
        let servers = self.servers.read().await;
        let server = servers.get(&server_id)
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
            if server.status == ToolServerStatus::Running || server.status == ToolServerStatus::Stopped {
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
        
        Ok(all_tools)
    }
}

// =============================================================================
// ToolMetadata — Tool discovery metadata
// =============================================================================

/// Tool metadata exposed to LLM prompts for tool discovery and schema injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl ToolServerManager {
    pub fn new(
        registry: Arc<dyn ToolRegistry>,
        servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
        event_bus: Arc<EventBus>
    ) -> Self {
        Self {
            registry,
            servers,
            event_bus,
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
                            warn!("Failed to register server '{}' in registry: {}", server.name, e);
                        }
                        
                        self.event_bus.publish_mcp_event(event);
                        info!("Started MCP server '{}' (pid: {:?})", server.name, server.process_id);
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
    async fn start_server(&self, server: &mut ToolServer) -> Result<crate::domain::events::MCPToolEvent, ManagerError> {
        info!("Starting MCP server '{}' from '{}'", server.name, server.executable_path.display());
        
        // Transition to Starting state first (domain validation)
        let start_event = server.start()
            .map_err(|e| ManagerError::StartFailed(server.name.clone(), e.to_string()))?;
        
        // Spawn the actual process
        let mut command = tokio::process::Command::new(&server.executable_path);
        command.args(&server.args);
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        
        let child = command
            .spawn()
            .map_err(|e| {
                // Revert domain state on spawn failure
                server.status = ToolServerStatus::Failed;
                ManagerError::StartFailed(
                    server.name.clone(),
                    format!("Failed to spawn process '{}': {}", server.executable_path.display(), e),
                )
            })?;
            
        let pid = child.id().unwrap_or(0);

        // Update domain object with process details
        server.process_id = child.id();
        server.status = ToolServerStatus::Running;
        
        info!("MCP server '{}' spawned with PID {}", server.name, pid);
        
        // Return the start event with the actual PID
        if let crate::domain::events::MCPToolEvent::ServerStarted { server_id, name, process_id: _, started_at } = start_event {
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
                            trace!("Server '{}' healthy (pid: {:?})", server.name, server.process_id);
                            server.record_health_check(true);
                        }
                        Ok(false) => {
                            warn!("Server '{}' unhealthy (pid: {:?})", server.name, server.process_id);
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
                    warn!("MCP server '{}' process (PID {}) is no longer running", server.name, pid);
                }
                Ok(is_alive)
            }
            None => {
                // No PID means the server was never properly started
                warn!("MCP server '{}' has no PID — treating as unhealthy", server.name);
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
            status: ToolServerStatus::Running,
            process_id: None,
            health_check_interval: Duration::from_secs(60),
            last_health_check: None,
            credentials: None,
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

        let router = ToolRouter::new(registry, servers);
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
        let router = ToolRouter::new(registry, servers);
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

        let router = ToolRouter::new(registry, servers);
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

        let router = ToolRouter::new(registry, servers);
        router.rebuild_index().await;

        // Agent is only authorized for filesystem.read, not gmail.send
        let result = router.route_tool(exec_id, "gmail.send").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            RoutingError::AgentNotAuthorized { tool_name, .. } => {
                assert_eq!(tool_name, "gmail.send");
            }
            other => panic!("Expected AgentNotAuthorized, got {:?}", other),
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

        let router = ToolRouter::new(registry, servers);
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
}
