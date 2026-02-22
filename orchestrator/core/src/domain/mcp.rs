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
//! | [`ToolServerId`] | Identifies a long-running MCP server process |
//! | [`ToolInvocationId`] | Identifies a single tool call within an execution |
//! | [`ToolServer`] | Domain entity representing a registered MCP server process |
//! | [`ToolPolicy`] | Security constraints on tool usage (paths, domains, rate limits) |
//! | [`PolicyViolation`] | Describes why a tool call was rejected by `SecurityContext` |
//! | [`MCPError`] | JSON-RPC error value object returned to agents on failure |
//! | [`CredentialRef`] | Opaque reference to a credential in the secret store |
//!
//! ## Credential Isolation
//!
//! [`CredentialRef`] stores a *reference* (path/key) to the credential, not
//! the credential itself. Credentials are resolved by the orchestrator's
//! `SecretsManager` (ADR-034) just before spawning a tool server process;
//! they are never written to agent container memory.
//!
//! See ADR-033 (Orchestrator-Mediated MCP Tool Routing), ADR-035 (SMCP),
//! AGENTS.md §Tools & Integration Domain.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::events::{MCPToolEvent};

/// Unique identifier for a long-running MCP server process.
///
/// Created when a `ToolServer` is registered with the `ToolInvocationService`.
/// Used in `MCPToolEvent` variants to correlate server lifecycle events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolServerId(pub Uuid);

impl ToolServerId {
    /// Generate a new random `ToolServerId`.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

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

/// Opaque reference to a credential in the orchestrator's secret store.
///
/// Stores a *key path* only — never the credential value itself.
/// The orchestrator resolves this reference via `SecretsManager` (ADR-034)
/// just before spawning the tool server process, ensuring credentials are
/// never written to agent container memory (Credential Isolation, ADR-033).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRef {
    /// Which backend holds the credential.
    pub store_type: CredentialStoreType,
    /// Key or path within that backend (e.g. `"env:GMAIL_TOKEN"` or `"vault:tenant/kv/gmail"`).
    pub key: String,
}

/// Credential storage backend discriminant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialStoreType {
    /// Read from the orchestrator process's environment variables.
    /// Suitable for development; not recommended for production.
    Environment,
    /// Read from OpenBao (ADR-034). Requires Phase 4 implementation.
    OpenBao,
}

impl CredentialRef {
    pub fn from_env(key: &str) -> Self {
        Self {
            store_type: CredentialStoreType::Environment,
            key: key.to_string(),
        }
    }
    
    pub fn from_vault(path: &str) -> Self {
        Self {
            store_type: CredentialStoreType::OpenBao,
            key: format!("vault:{}", path),
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

/// Execution mode for a tool server
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    Local,  // Executed natively via FSAL on the agent's mounted volume
    Remote, // Executed via SMCP envelope to an external MCP server
}

/// Tool server status (enum value object)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolServerStatus {
    Stopped,      // Not running
    Starting,     // Spawning process
    Running,      // Healthy and available
    Unhealthy,    // Process alive but failing health checks
    Failed,       // Process crashed or killed
}

/// Invocation status (enum value object)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationStatus {
    Requested,    // Queued, not yet started
    Running,      // Currently executing
    Completed,    // Successful completion
    Failed,       // Error occurred
}

/// Policy violation types
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyViolation {
    ToolNotAllowed {
        tool_name: String,
        allowed_tools: Vec<String>,
    },
    ToolExplicitlyDenied {
        tool_name: String,
    },
    RateLimitExceeded {
        max_calls: u32,
        current_calls: u32,
    },
    PathOutsideBoundary {
        path: PathBuf,
        allowed_paths: Vec<PathBuf>,
    },
    PathTraversalAttempt {
        path: PathBuf,
    },
    DomainNotAllowed {
        domain: String,
        allowed_domains: Vec<String>,
    },
    MissingRequiredArgument(String),
    TimeoutExceeded {
        tool_name: String,
        max_duration: Duration,
    },
}

#[derive(Debug, Clone)]
pub enum DomainError {
    InvalidStateTransition {
        from: String,
        to: String,
    },
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStateTransition { from, to } => {
                write!(f, "Invalid state transition from {} to {}", from, to)
            }
        }
    }
}

impl std::error::Error for DomainError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub max_memory_mb: Option<u32>,
    pub max_cpu_shares: Option<u32>,
}

/// Represents a long-running MCP server process providing one or more tool capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolServer {
    // Identity
    pub id: ToolServerId,
    pub name: String,
    
    // Configuration
    pub execution_mode: ExecutionMode,
    pub executable_path: PathBuf,
    pub args: Vec<String>,
    pub capabilities: Vec<String>,
    
    // Lifecycle
    pub status: ToolServerStatus,
    pub process_id: Option<u32>,
    pub health_check_interval: Duration,
    pub last_health_check: Option<DateTime<Utc>>,
    
    // Security
    pub credentials: Option<CredentialRef>,
    pub resource_limits: ResourceLimits,
    
    // Metadata
    pub started_at: Option<DateTime<Utc>>,
    pub stopped_at: Option<DateTime<Utc>>,
}

impl ToolServer {
    pub fn from_config(config: &crate::domain::node_config::McpServerConfig) -> Self {
        let execution_mode = ExecutionMode::Local;

        let credentials = config.credentials.iter().next().map(|(_, v)| {
            if let Some(env_val) = v.strip_prefix("env:") {
                CredentialRef::from_env(env_val)
            } else if let Some(vault_val) = v.strip_prefix("vault:") {
                CredentialRef::from_vault(vault_val)
            } else {
                CredentialRef::from_env(v) 
            }
        });

        Self {
            id: ToolServerId::new(),
            name: config.name.clone(),
            execution_mode,
            executable_path: PathBuf::from(&config.executable),
            args: config.args.clone(),
            capabilities: config.capabilities.clone(),
            status: ToolServerStatus::Stopped,
            process_id: None,
            health_check_interval: Duration::from_secs(config.health_check.interval_seconds),
            last_health_check: None,
            credentials,
            resource_limits: ResourceLimits {
                max_memory_mb: Some(config.resource_limits.memory_mb),
                max_cpu_shares: Some(config.resource_limits.cpu_millicores),
            },
            started_at: None,
            stopped_at: None,
        }
    }

    pub fn start(&mut self) -> Result<MCPToolEvent, DomainError> {
        if self.status != ToolServerStatus::Stopped {
            return Err(DomainError::InvalidStateTransition {
                from: format!("{:?}", self.status),
                to: format!("{:?}", ToolServerStatus::Starting),
            });
        }
        
        self.status = ToolServerStatus::Starting;
        self.started_at = Some(Utc::now());
        
        Ok(MCPToolEvent::ServerStarted {
            server_id: self.id,
            name: self.name.clone(),
            process_id: self.process_id.unwrap_or(0), // Would be set by infrastructure before calling this ideally, but ADR defines it returning event
            started_at: self.started_at.unwrap(),
        })
    }
    
    pub fn can_invoke(&self, tool_name: &str) -> bool {
        self.status == ToolServerStatus::Running 
            && self.capabilities.iter().any(|cap| {
                if cap.ends_with(".*") {
                    let prefix = cap.trim_end_matches(".*");
                    tool_name.starts_with(prefix)
                } else {
                    cap == tool_name
                }
            })
    }
    
    pub fn record_health_check(&mut self, healthy: bool) -> Option<MCPToolEvent> {
        self.last_health_check = Some(Utc::now());
        
        if !healthy && self.status == ToolServerStatus::Running {
            self.status = ToolServerStatus::Unhealthy;
            return Some(MCPToolEvent::ServerUnhealthy {
                server_id: self.id,
                last_healthy: self.started_at,
            });
        }
        
        None
    }
}

/// Represents a single tool call from an agent execution, tracking full lifecycle and observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    // Identity
    pub id: ToolInvocationId,
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    
    // Tool details
    pub tool_name: String,
    pub server_id: ToolServerId,
    pub arguments: Value,
    
    // Lifecycle
    pub status: InvocationStatus,
    pub requested_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    
    // Results
    pub result: Option<Value>,
    pub error: Option<MCPError>,
    
    // Observability
    pub duration_ms: Option<u64>,
    pub bytes_transferred: Option<u64>,
    pub retries: u8,
}

impl ToolInvocation {
    pub fn new(execution_id: ExecutionId, agent_id: AgentId, server_id: ToolServerId, tool_name: String, arguments: Value) -> Self {
        Self {
            id: ToolInvocationId::new(),
            execution_id,
            agent_id,
            tool_name,
            server_id,
            arguments,
            status: InvocationStatus::Requested,
            requested_at: Utc::now(),
            started_at: None,
            completed_at: None,
            result: None,
            error: None,
            duration_ms: None,
            bytes_transferred: None,
            retries: 0,
        }
    }

    pub fn start(&mut self) -> Result<MCPToolEvent, DomainError> {
        if self.status != InvocationStatus::Requested {
            return Err(DomainError::InvalidStateTransition {
                from: format!("{:?}", self.status),
                to: format!("{:?}", InvocationStatus::Running),
            });
        }
        
        self.status = InvocationStatus::Running;
        self.started_at = Some(Utc::now());
        
        Ok(MCPToolEvent::InvocationStarted {
            invocation_id: self.id,
            server_id: self.server_id,
            tool_name: self.tool_name.clone(),
            started_at: self.started_at.unwrap(),
        })
    }
    
    pub fn complete(&mut self, result: Value) -> Result<MCPToolEvent, DomainError> {
        if self.status != InvocationStatus::Running {
            return Err(DomainError::InvalidStateTransition {
                from: format!("{:?}", self.status),
                to: format!("{:?}", InvocationStatus::Completed),
            });
        }
        
        self.completed_at = Some(Utc::now());
        self.duration_ms = self.started_at
            .map(|start| (self.completed_at.unwrap() - start).num_milliseconds() as u64);
        self.status = InvocationStatus::Completed;
        self.result = Some(result.clone());
        
        Ok(MCPToolEvent::InvocationCompleted {
            invocation_id: self.id,
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            result,
            duration_ms: self.duration_ms.unwrap_or(0),
            completed_at: self.completed_at.unwrap(),
        })
    }
    
    pub fn fail(&mut self, error: MCPError) -> Result<MCPToolEvent, DomainError> {
        if self.status != InvocationStatus::Running {
            return Err(DomainError::InvalidStateTransition {
                from: format!("{:?}", self.status),
                to: format!("{:?}", InvocationStatus::Failed),
            });
        }
        
        self.completed_at = Some(Utc::now());
        self.duration_ms = self.started_at
            .map(|start| (self.completed_at.unwrap() - start).num_milliseconds() as u64);
        self.status = InvocationStatus::Failed;
        self.error = Some(error.clone());
        
        Ok(MCPToolEvent::InvocationFailed {
            invocation_id: self.id,
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            error,
            failed_at: self.completed_at.unwrap(),
        })
    }
}

#[async_trait::async_trait]
pub trait ToolRegistry: Send + Sync {
    /// Retrieve all registered tool servers for a specific agent execution
    async fn get_tools_for_agent(&self, execution_id: ExecutionId) -> Result<Vec<ToolServer>, DomainError>;
    
    /// Register a new tool server
    async fn register_tool(&self, server: ToolServer) -> Result<(), DomainError>;
}

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
                max_calls: self.max_calls_per_execution,
                current_calls: current_call_count,
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
        if !self.allowed_paths.iter().any(|allowed| path.starts_with(allowed)) {
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
        assert!(policy.validate_invocation("filesystem.read", &args_read, 0).is_ok());

        let args_delete = json!({"path": "/workspace/test.txt"});
        let result = policy.validate_invocation("filesystem.delete", &args_delete, 0);
        assert!(matches!(result, Err(PolicyViolation::ToolExplicitlyDenied { .. })));

        let args_unknown = json!({});
        let result = policy.validate_invocation("unknown.tool", &args_unknown, 0);
        assert!(matches!(result, Err(PolicyViolation::ToolNotAllowed { .. })));
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
        assert!(matches!(result, Err(PolicyViolation::PathOutsideBoundary { .. })));

        let args_traversal = json!({"path": "/workspace/../etc/passwd"});
        let result = policy.validate_invocation("filesystem.read", &args_traversal, 0);
        assert!(matches!(result, Err(PolicyViolation::PathTraversalAttempt { .. })));
    }

    #[test]
    fn test_tool_server_state_transitions() {
        let mut server = ToolServer {
            id: ToolServerId::new(),
            name: "test-server".to_string(),
            execution_mode: ExecutionMode::Remote,
            executable_path: PathBuf::from("/bin/true"),
            args: vec![],
            capabilities: vec!["test.*".to_string()],
            status: ToolServerStatus::Stopped,
            process_id: None,
            health_check_interval: Duration::from_secs(30),
            last_health_check: None,
            credentials: None,
            resource_limits: ResourceLimits {
                max_memory_mb: None,
                max_cpu_shares: None,
            },
            started_at: None,
            stopped_at: None,
        };

        // Start should succeed
        assert!(server.start().is_ok());
        assert_eq!(server.status, ToolServerStatus::Starting);

        // Start again should fail
        assert!(server.start().is_err());
        
        server.status = ToolServerStatus::Running;
        
        // Health check failing should move it to Unhealthy
        server.record_health_check(false);
        assert_eq!(server.status, ToolServerStatus::Unhealthy);
    }

    #[test]
    fn test_tool_invocation_state_transitions() {
        let execution_id = ExecutionId::new();
        let server_id = ToolServerId::new();
        let mut invocation = ToolInvocation::new(
            execution_id,
            AgentId::new(),
            server_id, 
            "test.tool".to_string(), 
            json!({})
        );

        // Starts out Requested
        assert_eq!(invocation.status, InvocationStatus::Requested);

        // Complete should fail (not in Running state)
        assert!(invocation.complete(json!({"success": true})).is_err());

        // Start should succeed
        assert!(invocation.start().is_ok());
        assert_eq!(invocation.status, InvocationStatus::Running);
        assert!(invocation.started_at.is_some());

        // Complete should succeed
        assert!(invocation.complete(json!({"success": true})).is_ok());
        assert_eq!(invocation.status, InvocationStatus::Completed);
        assert!(invocation.completed_at.is_some());
        assert!(invocation.duration_ms.is_some());
        
        // Cannot fail if already completed
        assert!(invocation.fail(MCPError { code: 1, message: "err".to_string(), data: None }).is_err());
    }
}
