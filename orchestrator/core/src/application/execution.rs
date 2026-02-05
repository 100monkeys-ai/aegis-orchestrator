use crate::domain::execution::{Execution, ExecutionId, Iteration, ExecutionStatus};
use crate::domain::events::ExecutionEvent;
use crate::domain::agent::{AgentId, Agent, RuntimeConfig};
use crate::domain::supervisor::Supervisor;
use crate::domain::runtime::{AgentRuntime, InstanceId, TaskInput};
use crate::application::agent::AgentLifecycleService;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use chrono::Utc;

#[derive(Debug, Clone)]
pub struct ExecutionInput {
    pub input: String,
}

#[async_trait]
pub trait ExecutionService: Send + Sync {
    async fn start_execution(&self, agent_id: AgentId, input: ExecutionInput) -> Result<ExecutionId>;
    async fn get_execution(&self, id: ExecutionId) -> Result<Execution>;
    async fn get_iterations(&self, exec_id: ExecutionId) -> Result<Vec<Iteration>>;
    async fn cancel_execution(&self, id: ExecutionId) -> Result<()>;
    async fn stream_execution(&self, id: ExecutionId) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>>;
}

pub struct StandardExecutionService {
    runtime: Arc<dyn AgentRuntime>,
    agent_service: Arc<dyn AgentLifecycleService>,
    supervisor: Arc<Supervisor>,
}

impl StandardExecutionService {
    pub fn new(
        runtime: Arc<dyn AgentRuntime>, 
        agent_service: Arc<dyn AgentLifecycleService>,
        supervisor: Arc<Supervisor>
    ) -> Self {
        Self {
            runtime,
            agent_service,
            supervisor,
        }
    }
}

#[async_trait]
impl ExecutionService for StandardExecutionService {
    async fn start_execution(&self, agent_id: AgentId, input: ExecutionInput) -> Result<ExecutionId> {
        // 1. Fetch Agent
        let agent = self.agent_service.get_agent(agent_id).await?; // Assuming async, need to check trait
        
        // 2. Spawn Runtime
        let instance_id = self.runtime.spawn(agent.manifest.runtime).await
            .map_err(|e| anyhow!("Failed to spawn runtime: {}", e))?;
            
        // 3. Run Supervisor Loop
        // In a real system, this would be spawned in background and execution ID returned immediately
        // For now, we mimic async execution by just returning ID, but strictly logic runs here? 
        // No, start_execution implies async start.
        // We'll spawn a tokio task.
        
        let supervisor = self.supervisor.clone();
        let runtime = self.runtime.clone();
        let prompt = input.input.clone();
        
        tokio::spawn(async move {
            match supervisor.run_loop(&instance_id, prompt).await {
                Ok(_) => {
                    // Log success
                    // Cleanup
                    let _ = runtime.terminate(&instance_id).await;
                },
                Err(_) => {
                    // Log error
                    let _ = runtime.terminate(&instance_id).await;
                }
            }
        });

        // Return a new ExecutionId (mocked for now since DB isn't hooked up to store the record)
        Ok(ExecutionId::new())
    }

    async fn get_execution(&self, _id: ExecutionId) -> Result<Execution> {
        Err(anyhow!("Not implemented"))
    }

    async fn get_iterations(&self, _exec_id: ExecutionId) -> Result<Vec<Iteration>> {
        Err(anyhow!("Not implemented"))
    }

    async fn cancel_execution(&self, _id: ExecutionId) -> Result<()> {
        Err(anyhow!("Not implemented"))
    }

    async fn stream_execution(&self, _id: ExecutionId) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
}
