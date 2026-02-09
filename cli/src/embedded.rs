// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Embedded mode execution (when daemon is not running)
//!
//! Creates services in-process and executes commands directly.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use aegis_core::{
    application::{
        execution::StandardExecutionService, execution::ExecutionService,
        lifecycle::StandardAgentLifecycleService, agent::AgentLifecycleService,
    },
    domain::{
        node_config::NodeConfig,
        agent::AgentId,
        execution::{ExecutionInput, ExecutionId},
        supervisor::Supervisor,
    },
    infrastructure::{
        event_bus::EventBus,
        llm::registry::ProviderRegistry,
        repositories::{InMemoryAgentRepository, InMemoryExecutionRepository},
        runtime::DockerRuntime,
    },
};
use aegis_sdk::manifest::AgentManifest;

pub struct EmbeddedExecutor {
    agent_service: Arc<StandardAgentLifecycleService>,
    execution_service: Arc<StandardExecutionService>,
    event_bus: Arc<EventBus>,
    _llm_registry: Arc<ProviderRegistry>,
}

impl EmbeddedExecutor {
    pub async fn new(config_path: Option<PathBuf>) -> Result<Self> {
        // Load configuration
        let config = NodeConfig::load_or_default(config_path)
            .context("Failed to load configuration")?;

        config
            .validate()
            .context("Configuration validation failed")?;

        // Initialize services
        let agent_repo = Arc::new(InMemoryAgentRepository::new());
        let execution_repo = Arc::new(InMemoryExecutionRepository::new());
        let event_bus = Arc::new(EventBus::new(100));
        let llm_registry = Arc::new(
            ProviderRegistry::from_config(&config)
                .context("Failed to initialize LLM providers")?,
        );
        let runtime = Arc::new(
            DockerRuntime::new(config.runtime.bootstrap_script.clone())
                .context("Failed to initialize Docker runtime")?
        );
        let supervisor = Arc::new(Supervisor::new(runtime.clone()));

        let agent_service = Arc::new(StandardAgentLifecycleService::new(agent_repo.clone()));
        let execution_service = Arc::new(StandardExecutionService::new(
            agent_service.clone(),
            supervisor,
            execution_repo.clone(),
            event_bus.clone(),
            Arc::new(config.clone()),
            llm_registry.clone(),
        ));

        Ok(Self {
            agent_service,
            execution_service,
            event_bus,
            _llm_registry: llm_registry,
        })
    }

    pub async fn deploy_agent(&self, manifest: AgentManifest) -> Result<AgentId> {
        let core_manifest: aegis_core::domain::agent::AgentManifest = serde_json::from_value(serde_json::to_value(manifest).unwrap()).unwrap();
        self.agent_service.deploy_agent(core_manifest).await
    }

    pub async fn execute_agent(
        &self,
        agent_id: AgentId,
        input: serde_json::Value,
    ) -> Result<ExecutionId> {
        let execution_input = ExecutionInput {
             intent: None, // Or extract from input if applicable
             payload: input,
        };
        self.execution_service.start_execution(agent_id, execution_input).await
    }

    pub async fn get_execution(&self, execution_id: ExecutionId) -> Result<ExecutionInfo> {
        let exec = self.execution_service.get_execution(execution_id).await?;
        Ok(ExecutionInfo {
            id: exec.id,
            agent_id: exec.agent_id,
            status: format!("{:?}", exec.status),
        })
    }

    pub async fn cancel_execution(&self, execution_id: ExecutionId) -> Result<()> {
        self.execution_service.cancel_execution(execution_id).await
    }

    pub async fn delete_execution(&self, execution_id: ExecutionId) -> Result<()> {
        self.execution_service.delete_execution(execution_id).await
    }

    pub async fn list_executions(
        &self,
        _agent_id: Option<AgentId>,
        _limit: usize,
    ) -> Result<Vec<ExecutionInfo>> {
        // TODO: Implement list in ExecutionService
        // For MVP embedded, just return empty or implement a list method in service
        Ok(vec![])
    }

    pub async fn stream_logs(
        &self,
        execution_id: ExecutionId,
        follow: bool,
        errors_only: bool,
        verbose: bool,
    ) -> Result<()> {
        use colored::Colorize;
        // use tokio_stream::StreamExt; // Removed unused import if not used

        let mut receiver = self.event_bus.subscribe_execution(execution_id);

        println!("{}", "Streaming execution logs...".dimmed());

        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let event = DomainEvent::Execution(event);
                    
                    // Filter errors if requested
                    if errors_only && !is_error_event(&event) {
                        continue;
                    }

                    print_event(&event, verbose);

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
    pub async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
        self.agent_service.lookup_agent(name).await
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionInfo {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    pub status: String,
}

use aegis_core::infrastructure::event_bus::DomainEvent;

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

fn print_event(event: &DomainEvent, verbose: bool) {
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
                if verbose {
                    println!(
                        "{} {} - {}",
                        "Iteration".yellow(),
                        iteration_number,
                        action
                    );
                } else {
                    println!(
                        "{} {}",
                        "Iteration".yellow(),
                        iteration_number,
                    );
                }
            }
            aegis_core::domain::events::ExecutionEvent::IterationCompleted {
                iteration_number,
                output,
                ..
            } => {
                if verbose {
                    println!(
                        "{} {} {}\n{}",
                        "Iteration".yellow(),
                        iteration_number,
                        "completed".green(),
                        output.cyan()
                    );
                } else {
                    println!(
                        "{} {} {}",
                        "Iteration".yellow(),
                        iteration_number,
                        "completed".green()
                    );
                }
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
                final_output,
                ..
            } => {
                if verbose {
                    println!(
                        "{} ({} iterations)\n{}",
                        "Execution completed".bold().green(),
                        total_iterations,
                        final_output.cyan()
                    );
                } else {
                    println!(
                        "{} ({} iterations)",
                        "Execution completed".bold().green(),
                        total_iterations
                    );
                }
            }
            aegis_core::domain::events::ExecutionEvent::ExecutionFailed { reason, .. } => {
                println!("{} - {}", "Execution failed".bold().red(), reason);
            }
            aegis_core::domain::events::ExecutionEvent::ExecutionCancelled { .. } => {
                println!("{}", "Execution cancelled".bold().yellow());
            }
            aegis_core::domain::events::ExecutionEvent::ConsoleOutput { stream, content, .. } => {
                if verbose {
                    let prefix = if stream == "stderr" { "[STDERR]".red() } else { "[STDOUT]".cyan() };
                    println!("{} {}", prefix, content.trim_end());
                }
            }
            aegis_core::domain::events::ExecutionEvent::LlmInteraction { model, prompt, response, .. } => {
                if verbose {
                    println!(
                        "{} [{}]",
                        "LLM Interaction".purple().bold(),
                        model
                    );
                    println!("{}", "PROMPT:".dimmed());
                    println!("{}", prompt);
                    println!("{}", "RESPONSE:".dimmed());
                    println!("{}", response);
                    println!("{}", "-".repeat(40).dimmed());
                } else {
                    println!("{} [{}]", "LLM".purple(), model);
                }
            }
            aegis_core::domain::events::ExecutionEvent::InstanceSpawned {
                iteration_number,
                instance_id,
                ..
            } => {
                println!(
                    "{} {} {} {}",
                    "Iteration".yellow(),
                    iteration_number,
                    "spawned instance".cyan(),
                    instance_id.as_str().chars().take(12).collect::<String>()
                );
            }
            aegis_core::domain::events::ExecutionEvent::InstanceTerminated {
                iteration_number,
                instance_id,
                ..
            } => {
                println!(
                    "{} {} {} {}",
                    "Iteration".yellow(),
                    iteration_number,
                    "terminated instance".dimmed(),
                    instance_id.as_str().chars().take(12).collect::<String>()
                );
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
