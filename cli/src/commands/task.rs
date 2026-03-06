// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agent task operations commands
//!
//! Commands: deploy, execute, status, logs, cancel
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for task

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;
use uuid::Uuid;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};

#[derive(Subcommand)]
pub enum TaskCommand {
    /// Deploy an agent from manifest file

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

        /// Show verbose output (e.g. LLM prompts)
        #[arg(short, long)]
        verbose: bool,
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

    /// Remove an execution
    Remove {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,
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

pub async fn handle_command(command: TaskCommand, host: &str, port: u16) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;

    if let Ok(DaemonStatus::Unhealthy { pid, error }) = &daemon_status {
        println!(
            "{}",
            format!("⚠ Daemon found (PID: {}) but unhealthy: {}", pid, error).yellow()
        );
    }

    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {
            info!("Delegating to daemon API");
            let client = DaemonClient::new(host, port)?;
            handle_command_daemon(command, client).await
        }
        _ => {
            anyhow::bail!("Daemon is not running. Start it with 'aegis-orchestrator daemon start'.")
        }
    }
}

async fn handle_command_daemon(command: TaskCommand, client: DaemonClient) -> Result<()> {
    match command {
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
            verbose,
        } => logs_daemon(execution_id, follow, errors_only, verbose, client).await,
        TaskCommand::Cancel {
            execution_id,
            force,
        } => cancel_daemon(execution_id, force, client).await,
        TaskCommand::Remove { execution_id } => remove_daemon(execution_id, client).await,
        TaskCommand::List { agent_id, limit } => list_daemon(agent_id, limit, client).await,
    }
}

// Implementations
async fn execute_daemon(
    agent: String,
    input: Option<String>,
    wait: bool,
    follow: bool,
    client: DaemonClient,
) -> Result<()> {
    // Parse agent (UUID or manifest path)
    // Parse agent (UUID, Name, or Manifest Path)
    let agent_id = if let Ok(uuid) = Uuid::parse_str(&agent) {
        uuid
    } else {
        // Try lookup by name
        if let Ok(Some(uuid)) = client.lookup_agent(&agent).await {
            uuid
        } else {
            // Deploy manifest and use resulting ID
            let manifest_path = PathBuf::from(&agent);
            if manifest_path.exists() {
                let manifest_content = tokio::fs::read_to_string(&manifest_path)
                    .await
                    .with_context(|| format!("Failed to read manifest: {:?}", manifest_path))?;

                let agent_manifest: aegis_orchestrator_sdk::AgentManifest =
                    serde_yaml::from_str(&manifest_content).context("Failed to parse manifest")?;

                // Deploy (will fail if name exists, so user sees error, which is good)
                match client.deploy_agent(agent_manifest, false).await {
                    Ok(id) => id,
                    Err(e) => {
                        // Simplify error for user
                        anyhow::bail!("Failed to deploy manifest: {}", e);
                    }
                }
            } else {
                anyhow::bail!("Agent '{}' not found and not a valid manifest path.", agent);
            }
        }
    };

    // Parse input
    let input_data = parse_input(input).await?;

    println!("Executing agent {}...", agent_id);

    let execution_id = client.execute_agent(agent_id, input_data).await?;

    println!(
        "{}",
        format!("✓ Execution started: {}", execution_id).green()
    );

    if follow {
        logs_daemon(execution_id, true, false, false, client).await?;
    } else if wait {
        println!("Waiting for completion...");
        wait_for_execution_completion(execution_id, &client).await?;
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
    verbose: bool,
    client: DaemonClient,
) -> Result<()> {
    client
        .stream_logs(execution_id, follow, errors_only, verbose)
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

// Helpers
async fn parse_input(input: Option<String>) -> Result<serde_json::Value> {
    match input {
        None => Ok(serde_json::json!({})),
        Some(s) if s.starts_with('@') => {
            let path = &s[1..];
            let content = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("Failed to read input file: {}", path))?;
            serde_json::from_str(&content).context("Failed to parse input JSON")
        }
        Some(s) => {
            // Try parsing as JSON first
            if let Ok(val) = serde_json::from_str(&s) {
                Ok(val)
            } else {
                // Fallback to string value
                Ok(serde_json::Value::String(s))
            }
        }
    }
}

async fn wait_for_execution_completion(execution_id: Uuid, client: &DaemonClient) -> Result<()> {
    const MAX_POLLS: u32 = 300;
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    for _ in 0..MAX_POLLS {
        let execution = client.get_execution(execution_id).await?;
        let normalized = execution.status.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "completed" | "failed" | "cancelled" | "canceled"
        ) {
            println!(
                "Execution {} finished with status: {}",
                execution.id, execution.status
            );
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    anyhow::bail!("Timed out waiting for execution {} to finish", execution_id);
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

async fn remove_daemon(execution_id: Uuid, client: DaemonClient) -> Result<()> {
    client.delete_execution(execution_id).await?;
    println!(
        "{}",
        format!("✓ Execution {} removed", execution_id).green()
    );
    Ok(())
}
