use crate::domain::execution::{Execution, ExecutionId, Iteration, ExecutionStatus, ExecutionInput};
use crate::domain::repository::ExecutionRepository;
use crate::domain::events::ExecutionEvent;
use crate::domain::agent::AgentId;
use crate::domain::supervisor::Supervisor;
use crate::domain::runtime::AgentRuntime;
use crate::application::agent::AgentLifecycleService;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;
use std::sync::Arc;
use chrono::Utc;

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
    repository: Arc<dyn ExecutionRepository>,
}

impl StandardExecutionService {
    pub fn new(
        runtime: Arc<dyn AgentRuntime>, 
        agent_service: Arc<dyn AgentLifecycleService>,
        supervisor: Arc<Supervisor>,
        repository: Arc<dyn ExecutionRepository>,
    ) -> Self {
        Self {
            runtime,
            agent_service,
            supervisor,
            repository,
        }
    }
}

#[async_trait]
impl ExecutionService for StandardExecutionService {
    async fn start_execution(&self, agent_id: AgentId, input: ExecutionInput) -> Result<ExecutionId> {
        // 1. Fetch Agent
        let agent = self.agent_service.get_agent(agent_id).await?;
        
        // 2. Create Execution Record
        let max_retries = if let Some(exec) = &agent.manifest.execution {
             exec.max_retries as u8
        } else {
            3 // Default
        };

        let mut execution = Execution::new(agent_id, input.clone(), max_retries);
        let execution_id = execution.id;
        
        // 3. Save initial state
        self.repository.save(execution.clone()).await?;

        // 4. Spawn Runtime
        // Map agent.manifest.agent.runtime (string) into RuntimeConfig if needed, 
        // or if AgentRuntime takes string/config.
        // Currently AgentRuntime::spawn takes RuntimeConfig.
        // But manifest.agent.runtime is a String (e.g. "python:3.11").
        // We need to construct RuntimeConfig.
        let runtime_config = crate::domain::runtime::RuntimeConfig {
            image: agent.manifest.agent.runtime.clone(),
            command: None, // Default
            env: agent.manifest.env.clone(),
        };

        let instance_id = match self.runtime.spawn(runtime_config).await {
            Ok(id) => id,
            Err(e) => {
                execution.fail();
                self.repository.save(execution.clone()).await?;
                return Err(anyhow!("Failed to spawn runtime: {}", e));
            }
        };

        execution.start();
        self.repository.save(execution).await?;
            
        // 5. Run Supervisor Loop
        let supervisor = self.supervisor.clone();
        let runtime = self.runtime.clone();
        let repository = self.repository.clone();
        let exec_input = input.clone();
        
        tokio::spawn(async move {
            match supervisor.run_loop(&instance_id, exec_input).await {
                Ok(_) => {
                    // Update to completed
                    if let Ok(exec_opt) = repository.find_by_id(execution_id).await {
                        if let Some(mut exec) = exec_opt {
                             exec.complete();
                             let _ = repository.save(exec).await;
                        }
                    }
                    let _ = runtime.terminate(&instance_id).await;
                },
                Err(_) => {
                    // Update to failed
                    if let Ok(exec_opt) = repository.find_by_id(execution_id).await {
                         if let Some(mut exec) = exec_opt {
                              exec.fail();
                              let _ = repository.save(exec).await;
                         }
                     }
                    let _ = runtime.terminate(&instance_id).await;
                }
            }
        });

        Ok(execution_id)
    }

    async fn get_execution(&self, id: ExecutionId) -> Result<Execution> {
        self.repository.find_by_id(id).await?
            .ok_or_else(|| anyhow!("Execution not found"))
    }

    async fn get_iterations(&self, exec_id: ExecutionId) -> Result<Vec<Iteration>> {
        let execution = self.get_execution(exec_id).await?;
        Ok(execution.iterations().to_vec())
    }

    async fn cancel_execution(&self, id: ExecutionId) -> Result<()> {
         // This would ideally signal the supervisor to stop too.
         // For now, just mark state.
         let mut execution = self.get_execution(id).await?;
         execution.status = ExecutionStatus::Cancelled;
         execution.ended_at = Some(Utc::now());
         self.repository.save(execution).await?;
         Ok(())
    }

    async fn stream_execution(&self, _id: ExecutionId) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
}
