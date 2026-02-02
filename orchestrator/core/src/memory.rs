use crate::agent::AgentId;
use serde::{Deserialize, Serialize};

/// A memory entry in the agent's persistent vector store (Cortex).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub agent_id: AgentId,
    pub content: String,
    pub embedding: Option<Vec<f32>>,
    pub metadata: serde_json::Value,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Query for semantic search in agent memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryQuery {
    pub query_text: String,
    pub limit: usize,
    pub filter: Option<serde_json::Value>,
}

/// Result from a memory search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub entry: MemoryEntry,
    pub similarity_score: f32,
}

/// The Cortex - persistent semantic memory for agents.
/// 
/// Automatically indexes session logs, tool outputs, and user feedback.
/// Agents query their history before acting to learn from past experiences.
pub trait MemoryStore: Send + Sync {
    /// Store a new memory entry.
    fn store(&mut self, entry: MemoryEntry) -> anyhow::Result<()>;

    /// Search memories by semantic similarity.
    fn search(&self, query: MemoryQuery) -> anyhow::Result<Vec<MemorySearchResult>>;

    /// Get all memories for an agent.
    fn get_agent_memories(&self, agent_id: AgentId) -> anyhow::Result<Vec<MemoryEntry>>;

    /// Clear all memories for an agent.
    fn clear_agent_memories(&mut self, agent_id: AgentId) -> anyhow::Result<()>;
}
