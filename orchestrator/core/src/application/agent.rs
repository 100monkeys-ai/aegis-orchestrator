use crate::domain::agent::{Agent, AgentId, AgentManifest};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait AgentLifecycleService: Send + Sync {
    async fn deploy_agent(&self, manifest: AgentManifest) -> Result<AgentId>;
    async fn get_agent(&self, id: AgentId) -> Result<Agent>;
    async fn update_agent(&self, id: AgentId, manifest: AgentManifest) -> Result<()>;
    async fn delete_agent(&self, id: AgentId) -> Result<()>;
    async fn list_agents(&self) -> Result<Vec<Agent>>;
}
