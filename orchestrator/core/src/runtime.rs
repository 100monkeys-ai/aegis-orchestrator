use crate::agent::{Agent, AgentConfig, AgentId};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

/// Unique identifier for a runtime instance (container or micro-VM).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(String);

impl InstanceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Input to an agent task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInput {
    pub prompt: String,
    pub context: HashMap<String, serde_json::Value>,
}

/// Output from an agent task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutput {
    pub result: serde_json::Value,
    pub logs: Vec<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Errors that can occur during runtime operations.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("Failed to spawn instance: {0}")]
    SpawnFailed(String),
    
    #[error("Failed to execute task: {0}")]
    ExecutionFailed(String),
    
    #[error("Failed to terminate instance: {0}")]
    TerminationFailed(String),
    
    #[error("Instance not found: {0}")]
    InstanceNotFound(String),
    
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
}

/// The core abstraction for agent runtime environments.
/// 
/// This trait is implemented by both Docker (dev) and Firecracker (prod) runtimes,
/// ensuring consistent behavior across environments.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Boot a new agent instance (container or micro-VM).
    async fn spawn(&self, config: AgentConfig) -> Result<InstanceId, RuntimeError>;

    /// Send a prompt/task to the agent and wait for completion.
    async fn execute(
        &self,
        id: &InstanceId,
        input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError>;

    /// Destroy the instance securely, removing all traces.
    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError>;

    /// Get the current status of an instance.
    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError>;
}

/// Status information for a running instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub id: InstanceId,
    pub state: String,
    pub uptime_seconds: u64,
    pub memory_usage_mb: u64,
    pub cpu_usage_percent: f64,
}
