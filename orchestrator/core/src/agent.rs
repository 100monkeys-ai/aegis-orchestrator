use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// A unique identifier for an agent instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for an AEGIS agent, derived from agent.yaml manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub runtime: String,
    pub memory_enabled: bool,
    pub permissions: Permissions,
    pub tools: Vec<String>,
    pub env: HashMap<String, String>,
}

/// Security permissions for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permissions {
    pub network: NetworkPermissions,
    pub filesystem: FilesystemPermissions,
    pub execution_time_seconds: u64,
    pub memory_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPermissions {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemPermissions {
    pub read: Vec<String>,
    pub write: Vec<String>,
}

/// The lifecycle state of an agent instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    /// Agent definition exists but no runtime instance
    Cold,
    /// Runtime instance pre-booted but not executing
    Warm,
    /// Currently executing a task
    Hot,
    /// Execution failed
    Failed,
    /// Successfully terminated
    Terminated,
}

/// An agent instance with its current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub config: AgentConfig,
    pub state: AgentState,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            id: AgentId::new(),
            config,
            state: AgentState::Cold,
            created_at: chrono::Utc::now(),
        }
    }

    pub fn transition_to(&mut self, state: AgentState) {
        self.state = state;
    }
}
