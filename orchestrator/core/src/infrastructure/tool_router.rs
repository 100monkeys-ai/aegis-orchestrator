// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

// ============================================================================
// ADR-033: Orchestrator-Mediated MCP Tool Routing
// ============================================================================
// Purpose: Secure routing of agent tool requests through orchestrator proxy
// Current Status: Implementing Phase 1 -> 2
// 
// Phase 1 Implementation (Current):
// - Standard MCP JSON-RPC protocol support
// - Tool server spawning and lifecycle management
// - Route tool calls from agents to appropriate MCP servers
// - Emit Domain events
//
// Phase 4 Enhancement (Future): 
// Integrate with ADR-034 (OpenBao) + ADR-035 (SMCP) for:
// - Dynamic credential management with auto-rotation
// - Cryptographically signed tool calls with non-repudiation
// - Fine-grained policy enforcement per SecurityContext
// - Full forensic audit trail of all tool invocations
//
// See: adrs/033-orchestrator-mediated-mcp-tool-routing.md
// ============================================================================

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde_json::{json, Value};
use anyhow::Result;

use crate::domain::mcp::{ToolServer, ToolServerId, ToolServerStatus, ToolInvocation, MCPError, ToolPolicy};
use crate::infrastructure::event_bus::EventBus;

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("Tool not found: {tool_name}")]
    ToolNotFound { tool_name: String, available_tools: Vec<String> },
    
    #[error("Server {0:?} not found")]
    ServerNotFound(ToolServerId),
    
    #[error("Server {server_id:?} not ready. Current status: {status:?}")]
    ServerNotReady { server_id: ToolServerId, status: ToolServerStatus },
    
    #[error("Policy violation: {0}")]
    PolicyViolation(String),
}

/// MCP Tool Router: Routes agent tool requests to appropriate servers
pub struct ToolRouter {
    servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
    capabilities_index: Arc<RwLock<HashMap<String, ToolServerId>>>,
    event_bus: Arc<EventBus>,
}

impl ToolRouter {
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            servers: Arc::new(RwLock::new(HashMap::new())),
            capabilities_index: Arc::new(RwLock::new(HashMap::new())),
            event_bus,
        }
    }

    /// Find server that can handle this tool
    pub async fn route_tool(&self, tool_name: &str) -> Result<ToolServerId, RoutingError> {
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
    
    /// Verify server is ready to handle invocations
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
    
    /// Rebuild capability index (called on server registration/update)
    pub async fn rebuild_index(&self) {
        let servers = self.servers.read().await;
        let mut index = self.capabilities_index.write().await;
        index.clear();
        
        for (server_id, server) in servers.iter() {
            for capability in &server.capabilities {
                index.insert(capability.clone(), *server_id);
            }
        }
    }
    
    /// Add server to registry
    pub async fn add_server(&self, server: ToolServer) -> Result<()> {
        let id = server.id;
        {
            let mut servers = self.servers.write().await;
            servers.insert(id, server);
        }
        self.rebuild_index().await;
        Ok(())
    }

    /// List available tools
    pub async fn list_tools(&self) -> Result<Vec<ToolMetadata>> {
        let servers = self.servers.read().await;
        let mut all_tools = Vec::new();
        
        for server in servers.values() {
            if server.status == ToolServerStatus::Running {
                for cap in &server.capabilities {
                    // In a full implementation, we'd fetch actual schemas from the MCP server
                    all_tools.push(ToolMetadata {
                        name: cap.clone(),
                        description: format!("Provided by {}", server.name),
                        input_schema: json!({ "type": "object" }),
                    });
                }
            }
        }
        
        Ok(all_tools)
    }
}

/// Tool metadata for discovery
#[derive(Debug, Clone)]
pub struct ToolMetadata {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("Failed to start server {0}")]
    StartFailed(String),
}

/// Tool Server Manager handles lifecycle and spawn logic
pub struct ToolServerManager {
    servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
    event_bus: Arc<EventBus>,
}

impl ToolServerManager {
    pub fn new(
        servers: Arc<RwLock<HashMap<ToolServerId, ToolServer>>>,
        event_bus: Arc<EventBus>
    ) -> Self {
        Self {
            servers,
            event_bus,
        }
    }

    /// Start all configured servers
    pub async fn start_all(&self) -> Result<Vec<ToolServerId>, ManagerError> {
        let mut servers = self.servers.write().await;
        let mut started = vec![];
        
        for (server_id, server) in servers.iter_mut() {
            if server.status == ToolServerStatus::Stopped {
                match self.start_server(server).await {
                    Ok(event) => {
                        self.event_bus.publish_mcp_event(event);
                        started.push(*server_id);
                    }
                    Err(e) => {
                        tracing::error!("Failed to start server {}: {}", server.name, e);
                        // Continue with other servers (non-blocking failure)
                    }
                }
            }
        }
        
        Ok(started)
    }
    
    /// Start a single server
    async fn start_server(&self, server: &mut ToolServer) -> Result<crate::domain::events::MCPToolEvent, ManagerError> {
        // [MVP Placeholder] Spawn process via infrastructure
        // Normally we'd use `tokio::process::Command` here and track the `Child` process
        
        // Update domain object
        server.process_id = Some(std::process::id()); // Placeholder PID
        server.status = ToolServerStatus::Running; // Marking Running immediately for mocking
        
        match server.start() {
            Ok(event) => Ok(event),
            Err(e) => Err(ManagerError::StartFailed(e.to_string())),
        }
    }
    
    /// Background health check loop
    pub async fn health_check_loop(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            
            let mut servers = self.servers.write().await;
            for server in servers.values_mut() {
                if server.status == ToolServerStatus::Running {
                    match self.check_server_health(server).await {
                        Ok(true) => {
                            tracing::trace!("Server {} healthy", server.name);
                            server.record_health_check(true);
                        }
                        Ok(false) => {
                            tracing::warn!("Server {} unhealthy", server.name);
                            if let Some(evt) = server.record_health_check(false) {
                                self.event_bus.publish_mcp_event(evt);
                            }
                        }
                        Err(e) => {
                            tracing::error!("Health check failed for {}: {}", server.name, e);
                        }
                    }
                }
            }
        }
    }
    
    /// Ping server with MCP health check
    async fn check_server_health(&self, _server: &ToolServer) -> Result<bool, ManagerError> {
        // [MVP Placeholder]
        Ok(true) 
    }
}
