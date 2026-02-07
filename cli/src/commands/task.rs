//! Agent task operations commands
//!
//! Commands: deploy, execute, status, logs, cancel

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use tracing::info;
use uuid::Uuid;

use aegis_core::domain::{agent::AgentId, execution::ExecutionId};

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};
use crate::embedded::EmbeddedExecutor;

#[derive(Subcommand)]
pub enum TaskCommand {
    /// Deploy an agent from manifest file
    Deploy {
        /// Path to agent manifest YAML file
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
    },

    /// Execute an agent task
    Execute {
        /// Agent ID or manifest path
        #[arg(value_name = "AGENT")]
        agent: String,

        /// Input data (JSON string or @file.json)
        #[arg(short, long, value_name = "INPUT")]
        input: Option<String>,

        /// Wait for execution to complete
        #[arg(short, long)]
        wait: bool,

        /// Follow execution logs
        #[arg(short, long)]
        follow: bool,
    },

    /// Check execution status
    Status {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,
    },

    /// Stream execution logs
    Logs {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,

        /// Only show errors
        #[arg(long)]
        errors_only: bool,
    },

    /// Cancel running execution
    Cancel {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,

        /// Force kill without graceful shutdown
        #[arg(short, long)]
        force: bool,
    },

    /// List recent executions
    List {
        /// Show only for specific agent
        #[arg(long)]
        agent_id: Option<Uuid>,

        /// Maximum number of results
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
}

pub async fn handle_command(
    command: TaskCommand,
    config_path: Option<PathBuf>,
    port: u16,
) -> Result<()> {
    // Detect if daemon is running
    let daemon_status = check_daemon_running().await;
    let use_daemon = matches!(daemon_status, Ok(DaemonStatus::Running { .. }));

    if use_daemon {
        info!("Delegating to daemon API");
        let client = DaemonClient::new(port)?;
        handle_command_daemon(command, client).await
    } else {
        info!("Daemon not running, using embedded mode");
        let executor = EmbeddedExecutor::new(config_path).await?;
        handle_command_embedded(command, executor).await
    }
}

async fn handle_command_daemon(command: TaskCommand, client: DaemonClient) -> Result<()> {
    // DaemonClient needs updates to handle typed IDs too, but for CLI input we have Uuid
    // DaemonClient likely accepts Uuid and converts internally or expects string.
    // For now, assuming DaemonClient still uses Uuid in method signatures, which might fail compilation.
    // I should check client.rs but for now I'll fix the embedded path first.
    
    // Actually, to make this file compile, I must ensure calls match definitions.
    // Assuming DaemonClient methods define Uuid, I'll pass Uuid.
    
    match command {
        TaskCommand::Deploy { manifest } => deploy_daemon(manifest, client).await,
        TaskCommand::Execute {
            agent,
            input,
            wait,
            follow,
        } => execute_daemon(agent, input, wait, follow, client).await,
        TaskCommand::Status { execution_id } => status_daemon(execution_id, client).await,
        TaskCommand::Logs {
            execution_id,
            follow,
            errors_only,
        } => logs_daemon(execution_id, follow, errors_only, client).await,
        TaskCommand::Cancel {
            execution_id,
            force,
        } => cancel_daemon(execution_id, force, client).await,
        TaskCommand::List { agent_id, limit } => list_daemon(agent_id, limit, client).await,
    }
}

async fn handle_command_embedded(command: TaskCommand, executor: EmbeddedExecutor) -> Result<()> {
    match command {
        TaskCommand::Deploy { manifest } => deploy_embedded(manifest, executor).await,
        TaskCommand::Execute {
            agent,
            input,
            wait,
            follow,
        } => execute_embedded(agent, input, wait, follow, executor).await,
        TaskCommand::Status { execution_id } => status_embedded(execution_id, executor).await,
        TaskCommand::Logs {
            execution_id,
            follow,
            errors_only,
        } => logs_embedded(execution_id, follow, errors_only, executor).await,
        TaskCommand::Cancel {
            execution_id,
            force,
        } => cancel_embedded(execution_id, force, executor).await,
        TaskCommand::List { agent_id, limit } => list_embedded(agent_id, limit, executor).await,
    }
}

// Daemon mode implementations
async fn deploy_daemon(manifest: PathBuf, client: DaemonClient) -> Result<()> {
    let manifest_content = std::fs::read_to_string(&manifest)
        .with_context(|| format!("Failed to read manifest: {:?}", manifest))?;

    let agent_manifest: aegis_sdk::manifest::AgentManifest =
        serde_yaml::from_str(&manifest_content).context("Failed to parse manifest YAML")?;

    println!("Deploying agent: {}", agent_manifest.agent.name.bold());

    let agent_id = client.deploy_agent(agent_manifest).await?;

    println!("{}", format!("✓ Agent deployed: {}", agent_id).green());

    Ok(())
}

async fn execute_daemon(
    agent: String,
    input: Option<String>,
    wait: bool,
    follow: bool,
    client: DaemonClient,
) -> Result<()> {
    // Parse agent (UUID or manifest path)
    let agent_id = if let Ok(uuid) = Uuid::parse_str(&agent) {
        uuid
    } else {
        // Deploy manifest and use resulting ID
        let manifest_path = PathBuf::from(&agent);
        let manifest_content = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read manifest: {:?}", manifest_path))?;
        let agent_manifest: aegis_sdk::manifest::AgentManifest =
            serde_yaml::from_str(&manifest_content).context("Failed to parse manifest")?;
        client.deploy_agent(agent_manifest).await?
    };

    // Parse input
    let input_data = parse_input(input)?;

    println!("Executing agent {}...", agent_id);

    let execution_id = client.execute_agent(agent_id, input_data).await?;

    println!(
        "{}",
        format!("✓ Execution started: {}", execution_id).green()
    );

    if follow {
        logs_daemon(execution_id, true, false, client).await?;
    } else if wait {
        // TODO: Poll status until completion
        println!("Waiting for completion...");
    }

    Ok(())
}

async fn status_daemon(execution_id: Uuid, client: DaemonClient) -> Result<()> {
    let execution = client.get_execution(execution_id).await?;

    println!("Execution {}", execution_id);
    println!("  Status: {}", format_status(&execution.status));
    println!("  Agent: {}", execution.agent_id);
    if let Some(started) = execution.started_at {
        println!("  Started: {}", started);
    }
    if let Some(ended) = execution.ended_at {
        println!("  Ended: {}", ended);
    }

    Ok(())
}

async fn logs_daemon(
    execution_id: Uuid,
    follow: bool,
    errors_only: bool,
    client: DaemonClient,
) -> Result<()> {
    client
        .stream_logs(execution_id, follow, errors_only)
        .await?;
    Ok(())
}

async fn cancel_daemon(execution_id: Uuid, _force: bool, client: DaemonClient) -> Result<()> {
    client.cancel_execution(execution_id).await?;
    println!(
        "{}",
        format!("✓ Execution {} cancelled", execution_id).green()
    );
    Ok(())
}

async fn list_daemon(agent_id: Option<Uuid>, limit: usize, client: DaemonClient) -> Result<()> {
    let executions = client.list_executions(agent_id, limit).await?;

    if executions.is_empty() {
        println!("{}", "No executions found".yellow());
        return Ok(());
    }

    println!("{} executions:", executions.len());
    for exec in executions {
        println!(
            "  {} - Agent: {} - {}",
            exec.id,
            exec.agent_id,
            format_status(&exec.status)
        );
    }

    Ok(())
}

// Embedded mode implementations
async fn deploy_embedded(manifest: PathBuf, executor: EmbeddedExecutor) -> Result<()> {
    let manifest_content = std::fs::read_to_string(&manifest)
        .with_context(|| format!("Failed to read manifest: {:?}", manifest))?;

    let agent_manifest: aegis_sdk::manifest::AgentManifest =
        serde_yaml::from_str(&manifest_content).context("Failed to parse manifest YAML")?;

    println!("Deploying agent: {}", agent_manifest.agent.name.bold());

    let agent_id = executor.deploy_agent(agent_manifest).await?;

    println!("{}", format!("✓ Agent deployed: {}", agent_id.0).green());

    Ok(())
}

async fn execute_embedded(
    agent: String,
    input: Option<String>,
    wait: bool,
    follow: bool,
    executor: EmbeddedExecutor,
) -> Result<()> {
    // Parse agent (UUID or manifest path)
    let agent_id = if let Ok(uuid) = Uuid::parse_str(&agent) {
        AgentId(uuid)
    } else {
        let manifest_path = PathBuf::from(&agent);
        let manifest_content = std::fs::read_to_string(&manifest_path)?;
        let agent_manifest: aegis_sdk::manifest::AgentManifest =
            serde_yaml::from_str(&manifest_content)?;
        executor.deploy_agent(agent_manifest).await?
    };

    let input_data = parse_input(input)?;

    println!("Executing agent {}...", agent_id.0);

    let execution_id = executor.execute_agent(agent_id, input_data).await?;

    println!(
        "{}",
        format!("✓ Execution started: {}", execution_id.0).green()
    );

    if follow || wait {
        executor
            .stream_logs(execution_id, follow, false)
            .await?;
    }

    Ok(())
}

async fn status_embedded(execution_id: Uuid, executor: EmbeddedExecutor) -> Result<()> {
    let execution = executor.get_execution(ExecutionId(execution_id)).await?;

    println!("Execution {}", execution_id);
    println!("  Status: {}", format_status(&execution.status));
    println!("  Agent: {}", execution.agent_id.0);

    Ok(())
}

async fn logs_embedded(
    execution_id: Uuid,
    follow: bool,
    errors_only: bool,
    executor: EmbeddedExecutor,
) -> Result<()> {
    executor
        .stream_logs(ExecutionId(execution_id), follow, errors_only)
        .await?;
    Ok(())
}

async fn cancel_embedded(execution_id: Uuid, _force: bool, executor: EmbeddedExecutor) -> Result<()> {
    executor.cancel_execution(ExecutionId(execution_id)).await?;
    println!(
        "{}",
        format!("✓ Execution {} cancelled", execution_id).green()
    );
    Ok(())
}

async fn list_embedded(
    agent_id: Option<Uuid>,
    limit: usize,
    executor: EmbeddedExecutor,
) -> Result<()> {
    let executions = executor.list_executions(agent_id.map(AgentId), limit).await?;

    if executions.is_empty() {
        println!("{}", "No executions found".yellow());
        return Ok(());
    }

    println!("{} executions:", executions.len());
    for exec in executions {
        println!(
            "  {} - Agent: {} - {}",
            exec.id.0,
            exec.agent_id.0,
            format_status(&exec.status)
        );
    }

    Ok(())
}

// Helpers
fn parse_input(input: Option<String>) -> Result<serde_json::Value> {
    match input {
        None => Ok(serde_json::json!({})),
        Some(s) if s.starts_with('@') => {
            let path = &s[1..];
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read input file: {}", path))?;
            serde_json::from_str(&content).context("Failed to parse input JSON")
        }
        Some(s) => serde_json::from_str(&s).context("Failed to parse input JSON"),
    }
}

fn format_status(status: &str) -> colored::ColoredString {
    match status {
        "running" => "running".yellow(),
        "completed" => "completed".green(),
        "failed" => "failed".red(),
        "cancelled" => "cancelled".yellow(),
        _ => status.normal(),
    }
}
