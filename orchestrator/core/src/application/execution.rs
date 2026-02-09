// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::execution::{Execution, ExecutionId, Iteration, ExecutionStatus, ExecutionInput};
use crate::domain::repository::ExecutionRepository;
use crate::domain::events::ExecutionEvent;
use crate::domain::agent::AgentId;
use crate::domain::supervisor::{Supervisor, SupervisorObserver};
use crate::application::agent::AgentLifecycleService;
use crate::infrastructure::event_bus::{EventBus, DomainEvent};
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
    async fn stream_agent_events(&self, id: AgentId) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>>;
    async fn list_executions(&self, agent_id: Option<AgentId>, limit: usize) -> Result<Vec<Execution>>;
    async fn delete_execution(&self, id: ExecutionId) -> Result<()>;
}

pub struct StandardExecutionService {
    agent_service: Arc<dyn AgentLifecycleService>,
    supervisor: Arc<Supervisor>,
    repository: Arc<dyn ExecutionRepository>,
    event_bus: Arc<EventBus>,
    config: Arc<crate::domain::node_config::NodeConfig>,
}

impl StandardExecutionService {
    pub fn new(
        agent_service: Arc<dyn AgentLifecycleService>,
        supervisor: Arc<Supervisor>,
        repository: Arc<dyn ExecutionRepository>,
        event_bus: Arc<EventBus>,
        config: Arc<crate::domain::node_config::NodeConfig>,
    ) -> Self {
        Self {
            agent_service,
            supervisor,
            repository,
            event_bus,
            config,
        }
    }
}

struct ExecutionMonitor {
    execution_id: ExecutionId,
    agent_id: AgentId,
    repository: Arc<dyn ExecutionRepository>,
    event_bus: Arc<EventBus>,
}

#[async_trait]
impl SupervisorObserver for ExecutionMonitor {
    async fn on_iteration_start(&self, iteration: u8, action: &str) {
        let now = Utc::now();
        // Update DB
        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            {
                let _ = exec.start_iteration(action.to_string());
            }
            let _ = self.repository.save(&exec).await;
        }
        // Emit Event
        self.event_bus.publish_execution_event(ExecutionEvent::IterationStarted {
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            iteration_number: iteration,
            action: action.to_string(),
            started_at: now,
        });
    }

    async fn on_console_output(&self, iteration: u8, stream: &str, content: &str) {
        let now = Utc::now();
        // Update DB? Iteration struct doesn't have a list of logs yet, but we should eventually add it.
        // For now, we mainly want to stream it to the user.
        // TODO: Persist logs in Execution struct if needed.
        
        self.event_bus.publish_execution_event(ExecutionEvent::ConsoleOutput {
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            iteration_number: iteration,
            stream: stream.to_string(),
            content: content.to_string(),
            timestamp: now,
        });
    }

    async fn on_iteration_complete(&self, iteration: u8, output: &str) {
        let now = Utc::now();
        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            exec.complete_iteration(output.to_string());
            let _ = self.repository.save(&exec).await;
        }
        self.event_bus.publish_execution_event(ExecutionEvent::IterationCompleted {
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            iteration_number: iteration,
            output: output.to_string(),
            completed_at: now,
        });
    }

    async fn on_iteration_fail(&self, iteration: u8, error: &str) {
        let now = Utc::now();
        // Map string error to IterationError
        let iter_error = crate::domain::execution::IterationError {
            message: error.to_string(),
            details: None,
        };
        
        if let Ok(Some(mut exec)) = self.repository.find_by_id(self.execution_id).await {
            exec.fail_iteration(iter_error.clone());
            let _ = self.repository.save(&exec).await;
        }

        self.event_bus.publish_execution_event(ExecutionEvent::IterationFailed {
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            iteration_number: iteration,
            error: iter_error,
            failed_at: now,
        });
    }

    async fn on_instance_spawned(&self, iteration: u8, instance_id: &crate::domain::runtime::InstanceId) {
        let now = Utc::now();
        self.event_bus.publish_execution_event(ExecutionEvent::InstanceSpawned {
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            iteration_number: iteration,
            instance_id: instance_id.clone(),
            spawned_at: now,
        });
    }

    async fn on_instance_terminated(&self, iteration: u8, instance_id: &crate::domain::runtime::InstanceId) {
        let now = Utc::now();
        self.event_bus.publish_execution_event(ExecutionEvent::InstanceTerminated {
            execution_id: self.execution_id,
            agent_id: self.agent_id,
            iteration_number: iteration,
            instance_id: instance_id.clone(),
            terminated_at: now,
        });
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
        self.repository.save(&execution).await?;

        // Emit Started Event
        self.event_bus.publish_execution_event(ExecutionEvent::ExecutionStarted {
            execution_id,
            agent_id,
            started_at: Utc::now(),
        });

        // 4. Spawn Runtime
        let mut env = agent.manifest.env.clone();
        
        // Inject instruction from default task if available
        if let Some(task_spec) = &agent.manifest.task {
             if let Some(instr) = &task_spec.instruction {
                 env.insert("AEGIS_AGENT_INSTRUCTION".to_string(), instr.clone());
             }
        }
        // Fallback to description if not set above (or overwrite if desired, logic depends on precedence)
        // If instruction is missing, we try description
        if !env.contains_key("AEGIS_AGENT_INSTRUCTION") {
            if let Some(desc) = &agent.manifest.agent.description {
                env.insert("AEGIS_AGENT_INSTRUCTION".to_string(), desc.clone());
            }
        }
        
        // Inject other metadata
        env.insert("AEGIS_AGENT_ID".to_string(), agent_id.0.to_string());
        env.insert("AEGIS_EXECUTION_ID".to_string(), execution_id.0.to_string());
        
        // Inject Orchestrator URL
        // We use host.docker.internal as the generic host, but port comes from config.
        let port = self.config.network.as_ref().map(|n| n.port).unwrap_or(8000);
        let url = format!("http://host.docker.internal:{}", port);
        env.insert("AEGIS_ORCHESTRATOR_URL".to_string(), url);
        
        let runtime_config = crate::domain::runtime::RuntimeConfig {
            image: agent.manifest.agent.runtime.clone(),
            command: None, // Default
            env,
            autopull: agent.manifest.agent.autopull,
        };

        // NOTE: We no longer spawn the instance here.
        // The Supervisor will spawn fresh instances per iteration.

        execution.start();
        self.repository.save(&execution).await?;
            
        // 5. Run Supervisor Loop
        let supervisor = self.supervisor.clone();
        let repository = self.repository.clone();
        let event_bus = self.event_bus.clone();
        let exec_input = input.clone();
        
        let monitor = Arc::new(ExecutionMonitor {
            execution_id,
            agent_id,
            repository: repository.clone(),
            event_bus: event_bus.clone(),
        });

        tokio::spawn(async move {
            match supervisor.run_loop(runtime_config, exec_input, max_retries as u32, monitor).await {
                Ok(final_output) => {
                    // Update to completed
                    if let Ok(exec_opt) = repository.find_by_id(execution_id).await {
                        if let Some(mut exec) = exec_opt {
                             exec.complete();
                             let total_iterations = exec.iterations().len() as u8;
                             let _ = repository.save(&exec).await;

                             event_bus.publish_execution_event(ExecutionEvent::ExecutionCompleted {
                                 execution_id,
                                 agent_id,
                                 final_output: final_output.clone(),
                                 total_iterations,
                                 completed_at: Utc::now(),
                             });
                        }
                    }
                    // NOTE: No need to terminate instance here - Supervisor handles it
                },
                Err(e) => {
                    // Update to failed
                    if let Ok(exec_opt) = repository.find_by_id(execution_id).await {
                         if let Some(mut exec) = exec_opt {
                              exec.fail(e.to_string());
                              let total_iterations = exec.iterations().len() as u8;
                              let _ = repository.save(&exec).await;

                              event_bus.publish_execution_event(ExecutionEvent::ExecutionFailed {
                                 execution_id,
                                 agent_id,
                                 reason: e.to_string(),
                                 total_iterations,
                                 failed_at: Utc::now(),
                             });
                         }
                     }
                    // NOTE: No need to terminate instance here - Supervisor handles it
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
         let mut execution = self.get_execution(id).await?;
         execution.status = ExecutionStatus::Cancelled;
         execution.ended_at = Some(Utc::now());
         self.repository.save(&execution).await?;
         
         self.event_bus.publish_execution_event(ExecutionEvent::ExecutionCancelled {
             execution_id: id,
             agent_id: execution.agent_id,
             reason: None,
             cancelled_at: Utc::now(),
         });
         Ok(())
    }

    async fn stream_execution(&self, _id: ExecutionId) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
    }

    async fn stream_agent_events(&self, _id: AgentId) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
        // The implementation will be done in the server handler for now because `Stream` requires `'static` usually or intricate lifetimes.
        // Or we can implement it here properly.
    }

    async fn list_executions(&self, agent_id: Option<AgentId>, limit: usize) -> Result<Vec<Execution>> {
        if let Some(aid) = agent_id {
            let executions = self.repository.find_by_agent(aid).await?;
            // Apply limit manually since repo doesn't support limit on find_by_agent yet
            Ok(executions.into_iter().take(limit).collect())
        } else {
            Ok(self.repository.find_recent(limit).await?)
        }
    }

    async fn delete_execution(&self, id: ExecutionId) -> Result<()> {
        self.repository.delete(id).await?;
        Ok(())
    }
}
