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
        // Check if agent with same name exists
        if let Some(existing) = self.repository.find_by_name(&manifest.agent.name).await? {
            // Need to decide: Update existing or Fail?
            // "Deploy" usually implies creating or updating.
            // If ID matches, it's an update (handled by update_agent).
            // If ID is different but name matches, it's a conflict.
            
            // For now, let's enforce uniqueness strictly for new deployments.
            // If the user wants to update, they should use update_agent or we can support upsert logic here.
            // But since AgentId is generated on 'new', we can't easily match unless searching by name IS the identity.
            // Let's return error if name exists.
            
            anyhow::bail!("Agent with name '{}' already exists (ID: {}). Use 'agent update' to modify it.", existing.name, existing.id.0);
        }

        // Create new agent from manifest
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

    async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
        let agent = self.repository.find_by_name(name).await
            .map_err(|e| anyhow::anyhow!("Repository error: {}", e))?;
        Ok(agent.map(|a| a.id))
    }
}
