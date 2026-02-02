use serde::{Deserialize, Serialize};

/// Common types used across the SDK.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Cold,
    Warm,
    Hot,
    Failed,
    Terminated,
}
