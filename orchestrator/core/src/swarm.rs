use crate::agent::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// Unique identifier for a swarm (group of coordinated agents).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SwarmId(uuid::Uuid);

impl SwarmId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for SwarmId {
    fn default() -> Self {
        Self::new()
    }
}

/// A resource lock to prevent concurrent access conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceLock {
    pub resource_id: String,
    pub held_by: AgentId,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
}

/// Errors that can occur during swarm coordination.
#[derive(Debug, Error)]
pub enum SwarmError {
    #[error("Resource already locked by agent {0:?}")]
    ResourceLocked(AgentId),
    
    #[error("Lock not found for resource {0}")]
    LockNotFound(String),
    
    #[error("Agent {0:?} does not hold lock for resource {1}")]
    LockNotHeld(AgentId, String),
}

/// Coordinator for managing swarms of agents and their resource locks.
/// 
/// Prevents race conditions and conflicting writes when multiple agents
/// operate on shared resources.
pub struct SwarmCoordinator {
    locks: HashMap<String, ResourceLock>,
    swarms: HashMap<SwarmId, HashSet<AgentId>>,
}

impl SwarmCoordinator {
    pub fn new() -> Self {
        Self {
            locks: HashMap::new(),
            swarms: HashMap::new(),
        }
    }

    /// Attempt to acquire a lock on a resource.
    pub fn acquire_lock(
        &mut self,
        agent_id: AgentId,
        resource_id: String,
    ) -> Result<(), SwarmError> {
        if let Some(lock) = self.locks.get(&resource_id) {
            return Err(SwarmError::ResourceLocked(lock.held_by));
        }

        let lock = ResourceLock {
            resource_id: resource_id.clone(),
            held_by: agent_id,
            acquired_at: chrono::Utc::now(),
        };

        self.locks.insert(resource_id, lock);
        Ok(())
    }

    /// Release a lock on a resource.
    pub fn release_lock(
        &mut self,
        agent_id: AgentId,
        resource_id: &str,
    ) -> Result<(), SwarmError> {
        let lock = self
            .locks
            .get(resource_id)
            .ok_or_else(|| SwarmError::LockNotFound(resource_id.to_string()))?;

        if lock.held_by != agent_id {
            return Err(SwarmError::LockNotHeld(agent_id, resource_id.to_string()));
        }

        self.locks.remove(resource_id);
        Ok(())
    }

    /// Create a new swarm.
    pub fn create_swarm(&mut self) -> SwarmId {
        let swarm_id = SwarmId::new();
        self.swarms.insert(swarm_id, HashSet::new());
        swarm_id
    }

    /// Add an agent to a swarm.
    pub fn add_to_swarm(&mut self, swarm_id: SwarmId, agent_id: AgentId) {
        self.swarms
            .entry(swarm_id)
            .or_insert_with(HashSet::new)
            .insert(agent_id);
    }

    /// Get all agents in a swarm.
    pub fn get_swarm_agents(&self, swarm_id: SwarmId) -> Option<&HashSet<AgentId>> {
        self.swarms.get(&swarm_id)
    }
}

impl Default for SwarmCoordinator {
    fn default() -> Self {
        Self::new()
    }
}
