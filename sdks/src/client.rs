use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Client for interacting with the AEGIS orchestrator.
pub struct AegisClient {
    base_url: String,
    client: Client,
    api_key: Option<String>,
}

impl AegisClient {
    /// Create a new AEGIS client.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: Client::new(),
            api_key: None,
        }
    }

    /// Set the API key for authentication.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Deploy an agent to the AEGIS cloud.
    pub async fn deploy_agent(&self, manifest: &crate::AgentManifest) -> Result<DeploymentResponse> {
        let url = format!("{}/api/v1/agents", self.base_url);
        
        let mut req = self.client.post(&url).json(manifest);
        
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        
        let response = req.send().await?;
        let deployment = response.json().await?;
        
        Ok(deployment)
    }

    /// Execute a task on a deployed agent.
    pub async fn execute_task(
        &self,
        agent_id: &str,
        input: TaskInput,
    ) -> Result<TaskOutput> {
        let url = format!("{}/api/v1/agents/{}/execute", self.base_url, agent_id);
        
        let mut req = self.client.post(&url).json(&input);
        
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        
        let response = req.send().await?;
        let output = response.json().await?;
        
        Ok(output)
    }

    /// Get the status of an agent.
    pub async fn get_agent_status(&self, agent_id: &str) -> Result<AgentStatus> {
        let url = format!("{}/api/v1/agents/{}/status", self.base_url, agent_id);
        
        let mut req = self.client.get(&url);
        
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        
        let response = req.send().await?;
        let status = response.json().await?;
        
        Ok(status)
    }

    /// Terminate an agent instance.
    pub async fn terminate_agent(&self, agent_id: &str) -> Result<()> {
        let url = format!("{}/api/v1/agents/{}", self.base_url, agent_id);
        
        let mut req = self.client.delete(&url);
        
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", format!("Bearer {}", key));
        }
        
        req.send().await?;
        
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeploymentResponse {
    pub agent_id: String,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TaskInput {
    pub prompt: String,
    #[serde(default)]
    pub context: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TaskOutput {
    pub result: serde_json::Value,
    pub logs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    pub agent_id: String,
    pub state: String,
    pub uptime_seconds: u64,
}
