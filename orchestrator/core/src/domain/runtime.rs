use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub image: String,
    pub command: Option<Vec<String>>,
    pub env: HashMap<String, String>,
    pub autopull: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub String);

impl InstanceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInput {
    pub prompt: String,
    pub context: HashMap<String, serde_json::Value>,
}

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
    pub timestamp: DateTime<Utc>,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    pub id: InstanceId,
    pub state: String,
    pub uptime_seconds: u64,
    pub memory_usage_mb: u64,
    pub cpu_usage_percent: f64,
}

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn spawn(&self, config: RuntimeConfig) -> Result<InstanceId, RuntimeError>;
    async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput, RuntimeError>;
    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError>;
    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError>;
}
