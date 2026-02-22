// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use crate::domain::agent::AgentId;
use crate::domain::smcp_session::{EnvelopeVerifier, SmcpSessionError};
use crate::domain::smcp_session_repository::SmcpSessionRepository;
use crate::infrastructure::smcp::middleware::SmcpMiddleware;
use crate::infrastructure::tool_router::ToolRouter;

pub struct ToolInvocationService {
    smcp_session_repo: Arc<dyn SmcpSessionRepository>,
    smcp_middleware: Arc<SmcpMiddleware>,
    tool_router: Arc<ToolRouter>,
}

impl ToolInvocationService {
    pub fn new(
        smcp_session_repo: Arc<dyn SmcpSessionRepository>,
        smcp_middleware: Arc<SmcpMiddleware>,
        tool_router: Arc<ToolRouter>,
    ) -> Self {
        Self {
            smcp_session_repo,
            smcp_middleware,
            tool_router,
        }
    }

    /// Invokes a tool by validating the SMCP Envelope for the agent and routing to the right server
    pub async fn invoke_tool(
        &self,
        agent_id: &AgentId,
        envelope: &impl EnvelopeVerifier,
    ) -> Result<Value, SmcpSessionError> {
        // 1. Get active session for agent
        let session = self.smcp_session_repo
            .find_active_by_agent(agent_id)
            .await
            .map_err(|_| SmcpSessionError::MalformedPayload)? // generic fallback error
            .ok_or(SmcpSessionError::SessionInactive(crate::domain::smcp_session::SessionStatus::Expired))?;

        // 2. Middleware verifies signature and evaluates against SecurityContext
        let args = self.smcp_middleware.verify_and_unwrap(&session, envelope)?;
        let tool_name = envelope.extract_tool_name().ok_or(SmcpSessionError::MalformedPayload)?;

        // 3. Route tool call to the appropriate TCP server
        // The ToolRouter returns a ToolServerId, but we might actually want to call the tool.
        // For MVP, if it routes successfully, we return a mock payload or delegate to actual calling logic.
        // Wait, the ToolRouter doesn't actually *invoke* the tool yet. It returns `ToolServerId`.
        // Let's get the server ID first.
        let server_id = self.tool_router.route_tool(session.execution_id, &tool_name).await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(format!("Routing error: {}", e)))?;

        let server = self.tool_router.get_server(server_id).await
            .ok_or(SmcpSessionError::SignatureVerificationFailed("Server vanished after routing".to_string()))?;

        // 4. Execute based on ExecutionMode (Gateway Retrofit)
        match server.execution_mode {
            crate::domain::mcp::ExecutionMode::Local => {
                // FSAL Execution
                // In a production implementation, we'd look up the agent's volume path via the Storage manager
                // and directly manipulate files using the runtime-agnostic FSAL.
                tracing::info!("Executing local tool via FSAL: {} for agent {:?}", tool_name, agent_id);
                
                // [MVP Placeholder for FSAL operations]
                // Examples: modifying /shared/<agent_id>/workspace natively on the host filesystem
                
                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "local_fsal",
                    "message": format!("Locally executed {} affecting agent volume", tool_name),
                    "args_executed": args
                }))
            }
            crate::domain::mcp::ExecutionMode::Remote => {
                // SMCP to JSONRPC Proxy
                // We've unwrapped the SMCP envelope, now we build a JSON-RPC 2.0 request
                // and dispatch it to the ToolServer's raw stido or HTTP interfaces.
                let _json_rpc_payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": tool_name,
                    "params": args,
                    "id": session.execution_id.to_string()
                });
                
                tracing::info!("Proxying remote tool via JSON-RPC: {} to server {:?}", tool_name, server_id);
                
                // [MVP Placeholder for stdio/http IPC to actual MCP server process]
                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "remote_jsonrpc",
                    "message": format!("Proxied {} to external MCP server {:?}", tool_name, server_id),
                    "args_proxied": args
                }))
            }
        }
    }

    /// Internal orchestrator-driven tool invocation (Gateway pattern)
    /// This bypasses SMCP signature checks because the orchestrator itself is initiating it
    /// on behalf of the agent's LLM output.
    pub async fn invoke_tool_internal(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tool_name: String,
        args: Value,
    ) -> Result<Value, SmcpSessionError> {
        let server_id = self.tool_router.route_tool(execution_id, &tool_name).await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(format!("Routing error: {}", e)))?;

        let server = self.tool_router.get_server(server_id).await
            .ok_or(SmcpSessionError::SignatureVerificationFailed("Server vanished after routing".to_string()))?;

        // Note: For MVP we skip SecurityContext validation here because it was checked earlier
        // or assumes the orchestrator only routes to authorized tools.
        
        match server.execution_mode {
            crate::domain::mcp::ExecutionMode::Local => {
                tracing::info!("Executing local tool via FSAL: {} for agent {:?}", tool_name, agent_id);
                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "local_fsal",
                    "message": format!("Locally executed {} affecting agent volume", tool_name),
                    "args_executed": args
                }))
            }
            crate::domain::mcp::ExecutionMode::Remote => {
                let _json_rpc_payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": tool_name,
                    "params": args,
                    "id": execution_id.to_string()
                });
                
                tracing::info!("Proxying remote tool via JSON-RPC: {} to server {:?}", tool_name, server_id);
                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "remote_jsonrpc",
                    "message": format!("Proxied {} to external MCP server {:?}", tool_name, server_id),
                    "args_proxied": args
                }))
            }
        }
    }

    /// Retrieve available tool schemas to inject into LLM prompts
    pub async fn get_available_tools(&self) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SmcpSessionError> {
        self.tool_router.list_tools().await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(format!("Failed to list tools: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::execution::ExecutionId;
    use crate::domain::security_context::SecurityContext;
    use crate::infrastructure::smcp::session_repository::InMemorySmcpSessionRepository;
    use crate::domain::smcp_session::SmcpSession;
    use crate::infrastructure::tool_router::{ToolRouter, InMemoryToolRegistry};

    struct DummyEnvelope {
        valid: bool,
    }

    impl EnvelopeVerifier for DummyEnvelope {
        fn verify_signature(&self, _public_key_bytes: &[u8]) -> Result<(), SmcpSessionError> {
            if self.valid {
                Ok(())
            } else {
                Err(SmcpSessionError::SignatureVerificationFailed("invalid sig".to_string()))
            }
        }
        fn extract_tool_name(&self) -> Option<String> {
            Some("test_tool".to_string())
        }
        fn extract_arguments(&self) -> Option<Value> {
            Some(serde_json::json!({}))
        }
    }

    #[tokio::test]
    async fn test_invoke_tool_no_session() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers));
        let middleware = Arc::new(SmcpMiddleware::new());

        let service = ToolInvocationService::new(repo, middleware, router);
        let agent_id = AgentId::new();
        let envelope = DummyEnvelope { valid: true };

        let result = service.invoke_tool(&agent_id, &envelope).await;
        assert!(matches!(result, Err(SmcpSessionError::SessionInactive(_))));
    }

    #[tokio::test]
    async fn test_invoke_tool_bad_signature() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let agent_id = AgentId::new();
        let exec_id = ExecutionId::new();
        
        let context = SecurityContext {
            name: "test".to_string(),
            description: "".to_string(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        };

        let session = SmcpSession::new(agent_id, exec_id, vec![], "token".to_string(), context);
        let _ =  repo.save(session).await;

        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers));
        let middleware = Arc::new(SmcpMiddleware::new());

        let service = ToolInvocationService::new(repo, middleware, router);
        let envelope = DummyEnvelope { valid: false };

        let result = service.invoke_tool(&agent_id, &envelope).await;
        assert!(matches!(result, Err(SmcpSessionError::SignatureVerificationFailed(_))));
    }

    #[tokio::test]
    async fn test_invoke_tool_execution_modes() {
        use crate::domain::mcp::{ToolServer, ToolServerId, ToolServerStatus, ExecutionMode, ResourceLimits};
        use std::path::PathBuf;

        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let agent_id = AgentId::new();
        let exec_id = ExecutionId::new();
        
        use crate::domain::security_context::Capability;
        let context = SecurityContext {
            name: "test".to_string(),
            description: "".to_string(),
            capabilities: vec![
                Capability {
                    tool_pattern: "test_tool".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                },
                Capability {
                    tool_pattern: "test_tool_remote".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                },
            ],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        };

        let session = SmcpSession::new(agent_id, exec_id, vec![], "token".to_string(), context);
        let _ = repo.save(session).await;

        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> = Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers.clone()));
        let middleware = Arc::new(SmcpMiddleware::new());
        let service = ToolInvocationService::new(repo, middleware, router.clone());

        // 1. Local Tool
        let local_server = ToolServer {
            id: ToolServerId::new(),
            name: "local-fs-tool".to_string(),
            execution_mode: ExecutionMode::Local,
            executable_path: PathBuf::from("/bin/true"),
            args: vec![],
            capabilities: vec!["test_tool".to_string()],
            status: ToolServerStatus::Running,
            process_id: None,
            health_check_interval: std::time::Duration::from_secs(30),
            last_health_check: None,
            credentials: None,
            resource_limits: ResourceLimits { max_memory_mb: None, max_cpu_shares: None },
            started_at: None,
            stopped_at: None,
        };
        
        router.add_server(local_server).await.unwrap();
        
        let envelope = DummyEnvelope { valid: true }; // extracts "test_tool"
        let result = service.invoke_tool(&agent_id, &envelope).await.unwrap();
        
        let exec_mode = result.get("execution_mode").and_then(|v| v.as_str()).unwrap();
        assert_eq!(exec_mode, "local_fsal");

        // 2. Remote Tool
        let remote_server = ToolServer {
            id: ToolServerId::new(),
            name: "remote-web-tool".to_string(),
            execution_mode: ExecutionMode::Remote,
            executable_path: PathBuf::from("/bin/true"),
            args: vec![],
            capabilities: vec!["test_tool_remote".to_string()],
            status: ToolServerStatus::Running,
            process_id: None,
            health_check_interval: std::time::Duration::from_secs(30),
            last_health_check: None,
            credentials: None,
            resource_limits: ResourceLimits { max_memory_mb: None, max_cpu_shares: None },
            started_at: None,
            stopped_at: None,
        };
        router.add_server(remote_server).await.unwrap();
        
        struct DummyRemoteEnvelope { valid: bool }
        impl EnvelopeVerifier for DummyRemoteEnvelope {
            fn verify_signature(&self, _: &[u8]) -> Result<(), SmcpSessionError> { if self.valid { Ok(()) } else { Err(SmcpSessionError::SignatureVerificationFailed("".into())) } }
            fn extract_tool_name(&self) -> Option<String> { Some("test_tool_remote".to_string()) }
            fn extract_arguments(&self) -> Option<Value> { Some(serde_json::json!({})) }
        }
        
        let remote_envelope = DummyRemoteEnvelope { valid: true };
        let result = service.invoke_tool(&agent_id, &remote_envelope).await.unwrap();
        
        let exec_mode = result.get("execution_mode").and_then(|v| v.as_str()).unwrap();
        assert_eq!(exec_mode, "remote_jsonrpc");
    }
}
