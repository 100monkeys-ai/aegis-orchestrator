use crate::domain::agent::{Agent, AgentId, AgentManifest};
use crate::domain::repository::AgentRepository;
use crate::application::agent::AgentLifecycleService;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

pub struct StandardAgentLifecycleService {
    repository: Arc<dyn AgentRepository>,
}

impl StandardAgentLifecycleService {
    pub fn new(repository: Arc<dyn AgentRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait]
impl AgentLifecycleService for StandardAgentLifecycleService {
    async fn deploy_agent(&self, manifest: AgentManifest) -> Result<AgentId> {
        // Create new agent from manifest
        // For now, we assume simple mapping. Domain logic might go here.
        let agent = Agent::new(manifest);
        self.repository.save(agent.clone()).await?; // Agent might not be Copy, so clone or move. save takes value.
        Ok(agent.id)
    }

    async fn get_agent(&self, id: AgentId) -> Result<Agent> {
        self.repository.find_by_id(id).await?
            .ok_or_else(|| anyhow::anyhow!("Agent not found"))
    }

    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()> {
        let mut agent = self.get_agent(id).await?;
        agent.update_manifest(manifest);
        self.repository.save(agent).await?;
        Ok(())
    }

    async fn delete_agent(&self, id: AgentId) -> Result<()> {
        self.repository.delete(id).await?;
        Ok(())
    }

    async fn list_agents(&self) -> Result<Vec<Agent>> {
        self.repository.list_all().await.map_err(|e| anyhow::anyhow!("Failed to list agents: {}", e))
    }
}
