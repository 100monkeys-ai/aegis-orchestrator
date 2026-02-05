use crate::application::agent::AgentLifecycleService;
use crate::domain::agent::{Agent, AgentId, AgentManifest};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

#[async_trait]
impl AgentLifecycleService for InMemoryAgentRepository {
    async fn deploy_agent(&self, manifest: AgentManifest) -> Result<AgentId> {
        let agent = Agent::new(manifest.name.clone(), manifest);
        let id = agent.id;
        let mut agents = self.agents.lock().map_err(|_| anyhow!("Mutex poisoned"))?;
        agents.insert(id, agent);
        Ok(id)
    }

    async fn get_agent(&self, id: AgentId) -> Result<Agent> {
        let agents = self.agents.lock().map_err(|_| anyhow!("Mutex poisoned"))?;
        agents.get(&id).cloned().ok_or_else(|| anyhow!("Agent not found"))
    }

    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()> {
        let mut agents = self.agents.lock().map_err(|_| anyhow!("Mutex poisoned"))?;
        if let Some(agent) = agents.get_mut(&id) {
            agent.update_manifest(manifest);
            Ok(())
        } else {
            Err(anyhow!("Agent not found"))
        }
    }

    async fn delete_agent(&self, id: AgentId) -> Result<()> {
        let mut agents = self.agents.lock().map_err(|_| anyhow!("Mutex poisoned"))?;
        if agents.remove(&id).is_some() {
            Ok(())
        } else {
            Err(anyhow!("Agent not found"))
        }
    }

    async fn list_agents(&self) -> Result<Vec<Agent>> {
        let agents = self.agents.lock().map_err(|_| anyhow!("Mutex poisoned"))?;
        Ok(agents.values().cloned().collect())
    }
}
