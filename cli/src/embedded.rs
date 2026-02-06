//! Embedded mode execution (when daemon is not running)
//!
//! Creates services in-process and executes commands directly.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use aegis_core::{
    application::{
        execution_service::StandardExecutionService, lifecycle::AgentLifecycleService,
    },
    domain::node_config::NodeConfig,
    infrastructure::{
        event_bus::EventBus,
        llm::registry::ProviderRegistry,
        repositories::{InMemoryAgentRepository, InMemoryExecutionRepository},
    },
};
use aegis_sdk::manifest::AgentManifest;

pub struct EmbeddedExecutor {
    agent_service: Arc<AgentLifecycleService<InMemoryAgentRepository>>,
    execution_service: Arc<StandardExecutionService>,
    event_bus: Arc<EventBus>,
    _llm_registry: Arc<ProviderRegistry>,
}

impl EmbeddedExecutor {
    pub async fn new(config_path: Option<PathBuf>) -> Result<Self> {
        // Load configuration
        let config = NodeConfig::discover(config_path.as_deref())
            .await
            .context("Failed to load configuration")?;

        config
            .validate()
            .await
            .context("Configuration validation failed")?;

        // Initialize services
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let execution_repo = Arc::new(InMemoryExecutionRepository::new());
        let event_bus = Arc::new(EventBus::new());
        let llm_registry = Arc::new(
            ProviderRegistry::from_config(&config)
                .await
                .context("Failed to initialize LLM providers")?,
        );

        let agent_service = Arc::new(AgentLifecycleService::new(agent_repo.clone()));
        let execution_service = Arc::new(StandardExecutionService::new(
            execution_repo.clone(),
            agent_repo.clone(),
            event_bus.clone(),
        ));

        Ok(Self {
            agent_service,
            execution_service,
            event_bus,
            _llm_registry: llm_registry,
        })
    }

    pub async fn deploy_agent(&self, manifest: AgentManifest) -> Result<Uuid> {
        // Convert SDK manifest to domain Agent
        // TODO: Implement conversion
        anyhow::bail!("Deploy not yet implemented in embedded mode")
    }

    pub async fn execute_agent(
        &self,
        _agent_id: Uuid,
        _input: serde_json::Value,
    ) -> Result<Uuid> {
        // TODO: Implement
        anyhow::bail!("Execute not yet implemented in embedded mode")
    }

    pub async fn get_execution(&self, _execution_id: Uuid) -> Result<ExecutionInfo> {
        // TODO: Implement
        anyhow::bail!("Get execution not yet implemented in embedded mode")
    }

    pub async fn cancel_execution(&self, _execution_id: Uuid) -> Result<()> {
        // TODO: Implement
        anyhow::bail!("Cancel execution not yet implemented in embedded mode")
    }

    pub async fn list_executions(
        &self,
        _agent_id: Option<Uuid>,
        _limit: usize,
    ) -> Result<Vec<ExecutionInfo>> {
        // TODO: Implement
        anyhow::bail!("List executions not yet implemented in embedded mode")
    }

    pub async fn stream_logs(
        &self,
        execution_id: Uuid,
        follow: bool,
        errors_only: bool,
    ) -> Result<()> {
        use colored::Colorize;
        use tokio_stream::StreamExt;

        let mut receiver = self.event_bus.subscribe_execution(execution_id);

        println!("{}", "Streaming execution logs...".dimmed());

        loop {
            match receiver.recv().await {
                Ok(event) => {
                    // Filter errors if requested
                    if errors_only && !is_error_event(&event) {
                        continue;
                    }

                    print_event(&event);

                    // Exit if not following and execution complete
                    if !follow && is_terminal_event(&event) {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("{}", format!("Stream error: {}", e).red());
                    break;
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionInfo {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub status: String,
}

use aegis_core::domain::events::DomainEvent;

fn is_error_event(event: &DomainEvent) -> bool {
    matches!(
        event,
        DomainEvent::Execution(
            aegis_core::domain::events::ExecutionEvent::IterationFailed { .. }
                | aegis_core::domain::events::ExecutionEvent::ExecutionFailed { .. }
        ) | DomainEvent::Policy(
            aegis_core::domain::events::PolicyEvent::PolicyViolationAttempted { .. }
        )
    )
}

fn is_terminal_event(event: &DomainEvent) -> bool {
    matches!(
        event,
        DomainEvent::Execution(
            aegis_core::domain::events::ExecutionEvent::ExecutionCompleted { .. }
                | aegis_core::domain::events::ExecutionEvent::ExecutionFailed { .. }
                | aegis_core::domain::events::ExecutionEvent::ExecutionCancelled { .. }
        )
    )
}

fn print_event(event: &DomainEvent) {
    use colored::Colorize;

    match event {
        DomainEvent::Execution(exec_event) => match exec_event {
            aegis_core::domain::events::ExecutionEvent::ExecutionStarted { .. } => {
                println!("{}", "Execution started".bold());
            }
            aegis_core::domain::events::ExecutionEvent::IterationStarted {
                iteration_number,
                action,
                ..
            } => {
                println!(
                    "{} {} - {}",
                    "Iteration".yellow(),
                    iteration_number,
                    action
                );
            }
            aegis_core::domain::events::ExecutionEvent::IterationCompleted {
                iteration_number,
                ..
            } => {
                println!(
                    "{} {} {}",
                    "Iteration".yellow(),
                    iteration_number,
                    "completed".green()
                );
            }
            aegis_core::domain::events::ExecutionEvent::IterationFailed {
                iteration_number,
                error,
                ..
            } => {
                println!(
                    "{} {} {} - {:?}",
                    "Iteration".yellow(),
                    iteration_number,
                    "failed".red(),
                    error
                );
            }
            aegis_core::domain::events::ExecutionEvent::RefinementApplied {
                iteration_number,
                ..
            } => {
                println!(
                    "{} {} {}",
                    "Iteration".yellow(),
                    iteration_number,
                    "refinement applied".cyan()
                );
            }
            aegis_core::domain::events::ExecutionEvent::ExecutionCompleted {
                total_iterations,
                ..
            } => {
                println!(
                    "{} ({} iterations)",
                    "Execution completed".bold().green(),
                    total_iterations
                );
            }
            aegis_core::domain::events::ExecutionEvent::ExecutionFailed { reason, .. } => {
                println!("{} - {}", "Execution failed".bold().red(), reason);
            }
            aegis_core::domain::events::ExecutionEvent::ExecutionCancelled { .. } => {
                println!("{}", "Execution cancelled".bold().yellow());
            }
        },
        DomainEvent::Policy(policy_event) => match policy_event {
            aegis_core::domain::events::PolicyEvent::PolicyViolationAttempted {
                violation_type,
                details,
                ..
            } => {
                println!(
                    "{} {:?} - {}",
                    "Policy violation attempted".red(),
                    violation_type,
                    details
                );
            }
            aegis_core::domain::events::PolicyEvent::PolicyViolationBlocked {
                violation_type,
                ..
            } => {
                println!(
                    "{} {:?}",
                    "Policy violation blocked".red().bold(),
                    violation_type
                );
            }
        },
        _ => {
            // Other event types
            println!("{:?}", event);
        }
    }
}
