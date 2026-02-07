use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::{Agent, AgentId, AgentManifest};
use crate::domain::execution::{Execution, ExecutionId};
use crate::domain::repository::{AgentRepository, ExecutionRepository, RepositoryError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// In-memory implementation of AgentRepository
/// Suitable for development and testing
#[derive(Clone)]
pub struct InMemoryAgentRepository {
    agents: Arc<Mutex<HashMap<AgentId, Agent>>>,
}

impl InMemoryAgentRepository {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryAgentRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AgentRepository for InMemoryAgentRepository {
    async fn save(&self, agent: Agent) -> Result<(), RepositoryError> {
        let mut agents = self.agents.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        agents.insert(agent.id, agent);
        Ok(())
    }

    async fn find_by_id(&self, id: AgentId) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        Ok(agents.get(&id).cloned())
    }

    async fn find_by_name(&self, name: &str) -> Result<Option<Agent>, RepositoryError> {
        let agents = self.agents.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        Ok(agents.values().find(|a| a.name == name).cloned())
    }

    async fn list_all(&self) -> Result<Vec<Agent>, RepositoryError> {
        let agents = self.agents.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        Ok(agents.values().cloned().collect())
    }

    async fn delete(&self, id: AgentId) -> Result<(), RepositoryError> {
        let mut agents = self.agents.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        if agents.remove(&id).is_some() {
            Ok(())
        } else {
            Err(RepositoryError::NotFound(format!("Agent {:?} not found", id)))
        }
    }
}

// Keep the AgentLifecycleService implementation for backward compatibility
#[async_trait]
impl AgentLifecycleService for InMemoryAgentRepository {
    async fn deploy_agent(&self, manifest: AgentManifest) -> anyhow::Result<AgentId> {
        let agent = Agent::new(manifest);
        let id = agent.id;
        self.save(agent).await
            .map_err(|e| anyhow::anyhow!("Failed to save agent: {}", e))?;
        Ok(id)
    }

    async fn get_agent(&self, id: AgentId) -> anyhow::Result<Agent> {
        self.find_by_id(id).await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> anyhow::Result<()> {
        let mut agent = self.get_agent(id).await?;
        agent.update_manifest(manifest);
        self.save(agent).await
            .map_err(|e| anyhow::anyhow!("Failed to update agent: {}", e))
    }

    async fn delete_agent(&self, id: AgentId) -> anyhow::Result<()> {
        self.delete(id).await
            .map_err(|e| anyhow::anyhow!("Failed to delete agent: {}", e))
    }

    async fn list_agents(&self) -> anyhow::Result<Vec<Agent>> {
        self.list_all().await
            .map_err(|e| anyhow::anyhow!("Failed to list agents: {}", e))
    }

    async fn lookup_agent(&self, name: &str) -> anyhow::Result<Option<AgentId>> {
        let agent = self.find_by_name(name).await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))?;
        Ok(agent.map(|a| a.id))
    }
}

/// In-memory implementation of ExecutionRepository
/// Suitable for development and testing
#[derive(Clone)]
pub struct InMemoryExecutionRepository {
    executions: Arc<Mutex<HashMap<ExecutionId, Execution>>>,
}

impl InMemoryExecutionRepository {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryExecutionRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutionRepository for InMemoryExecutionRepository {
    async fn save(&self, execution: Execution) -> Result<(), RepositoryError> {
        let mut executions = self.executions.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        executions.insert(execution.id, execution);
        Ok(())
    }

    async fn find_by_id(&self, id: ExecutionId) -> Result<Option<Execution>, RepositoryError> {
        let executions = self.executions.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        Ok(executions.get(&id).cloned())
    }

    async fn find_by_agent(&self, agent_id: AgentId) -> Result<Vec<Execution>, RepositoryError> {
        let executions = self.executions.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        Ok(executions.values()
            .filter(|e| e.agent_id == agent_id)
            .cloned()
            .collect())
    }

    async fn find_recent(&self, limit: usize) -> Result<Vec<Execution>, RepositoryError> {
        let executions = self.executions.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        let mut execs: Vec<_> = executions.values().cloned().collect();
        execs.sort_by(|a, b| b.started_at.cmp(&a.started_at)); // Most recent first
        Ok(execs.into_iter().take(limit).collect())
    }

    async fn delete(&self, id: ExecutionId) -> Result<(), RepositoryError> {
        let mut executions = self.executions.lock()
            .map_err(|_| RepositoryError::Unknown("Mutex poisoned".to_string()))?;
        if executions.remove(&id).is_some() {
            Ok(())
        } else {
            Err(RepositoryError::NotFound(format!("Execution {:?} not found", id)))
        }
    }
}

// Keep old ExecutionRepository trait method for backward compatibility
impl InMemoryExecutionRepository {
    pub async fn get(&self, id: &ExecutionId) -> anyhow::Result<Option<Execution>> {
        self.find_by_id(*id).await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))
    }
}
