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
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;
use uuid::Uuid;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};
use crate::output::{render_serialized, structured_output_unsupported, OutputFormat};

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

        /// Natural-language steering for the agent
        #[arg(long)]
        intent: Option<String>,

        /// Context override dictionary (JSON/YAML string or @file)
        #[arg(long, value_name = "DICT")]
        context: Option<String>,

        /// Target a specific agent version (default: latest)
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,

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

pub async fn handle_command(
    command: TaskCommand,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;

    if let Ok(DaemonStatus::Unhealthy { pid, error }) = &daemon_status {
        println!(
            "{}",
            format!("⚠ Daemon found (PID: {pid}) but unhealthy: {error}").yellow()
        );
    }

    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {
            info!("Delegating to daemon API");
            let auth_key = crate::auth::require_key().await?;
            let client = DaemonClient::new(host, port)?.with_auth(auth_key);
            handle_command_daemon(command, client, output_format).await
        }
        _ => {
            anyhow::bail!("Daemon is not running. Start it with 'aegis-orchestrator daemon start'.")
        }
    }
}

#[derive(Serialize)]
struct TaskExecuteOutput {
    agent_id: Uuid,
    execution_id: Uuid,
    wait: bool,
    follow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_execution: Option<crate::daemon::client::ExecutionInfo>,
}

#[derive(Serialize)]
struct TaskMutationOutput {
    execution_id: Uuid,
    status: &'static str,
}

#[derive(Serialize)]
struct TaskListOutput {
    count: usize,
    executions: Vec<crate::daemon::client::ExecutionInfo>,
}

async fn handle_command_daemon(
    command: TaskCommand,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        TaskCommand::Execute {
            agent,
            input,
            intent,
            context,
            version,
            wait,
            follow,
        } => {
            execute_daemon(
                agent,
                input,
                intent,
                context,
                version,
                wait,
                follow,
                client,
                output_format,
            )
            .await
        }
        TaskCommand::Status { execution_id } => {
            status_daemon(execution_id, client, output_format).await
        }
        TaskCommand::Logs {
            execution_id,
            follow,
            errors_only,
            verbose,
        } => {
            if output_format.is_structured() {
                structured_output_unsupported("aegis task logs", output_format)
            } else {
                logs_daemon(execution_id, follow, errors_only, verbose, client).await
            }
        }
        TaskCommand::Cancel {
            execution_id,
            force,
        } => cancel_daemon(execution_id, force, client, output_format).await,
        TaskCommand::Remove { execution_id } => {
            remove_daemon(execution_id, client, output_format).await
        }
        TaskCommand::List { agent_id, limit } => {
            list_daemon(agent_id, limit, client, output_format).await
        }
    }
}

// Implementations
#[allow(clippy::too_many_arguments)]
async fn execute_daemon(
    agent: String,
    input: Option<String>,
    intent: Option<String>,
    context: Option<String>,
    version: Option<String>,
    wait: bool,
    follow: bool,
    client: DaemonClient,
    output_format: OutputFormat,
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
            if tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
                let manifest_content = tokio::fs::read_to_string(&manifest_path)
                    .await
                    .with_context(|| format!("Failed to read manifest: {manifest_path:?}"))?;

                let agent_manifest: aegis_orchestrator_sdk::AgentManifest =
                    serde_yaml::from_str(&manifest_content).context("Failed to parse manifest")?;

                // Deploy (will fail if name exists, so user sees error, which is good)
                match client.deploy_agent(agent_manifest, false, None).await {
                    Ok(id) => id,
                    Err(e) => {
                        // Simplify error for user
                        anyhow::bail!("Failed to deploy manifest: {e}");
                    }
                }
            } else {
                anyhow::bail!("Agent '{agent}' not found and not a valid manifest path.");
            }
        }
    };

    // Parse input
    let input_data = parse_input(input).await?;
    let context_overrides = parse_object_input(context, "context override").await?;

    if output_format.is_structured() && follow {
        return structured_output_unsupported("aegis task execute --follow", output_format);
    }

    if !output_format.is_structured() {
        println!("Executing agent {agent_id}...");
    }

    let execution_id = client
        .execute_agent(
            agent_id,
            input_data,
            intent,
            context_overrides,
            version.as_deref(),
        )
        .await?;

    if follow {
        logs_daemon(execution_id, true, false, false, client).await?;
    } else if wait {
        if !output_format.is_structured() {
            println!("{}", format!("✓ Execution started: {execution_id}").green());
            println!("Waiting for completion...");
        }
        let final_execution =
            wait_for_execution_completion(execution_id, &client, output_format).await?;
        if output_format.is_structured() {
            return render_serialized(
                output_format,
                &TaskExecuteOutput {
                    agent_id,
                    execution_id,
                    wait,
                    follow,
                    final_execution: Some(final_execution),
                },
            );
        }
    } else if output_format.is_structured() {
        return render_serialized(
            output_format,
            &TaskExecuteOutput {
                agent_id,
                execution_id,
                wait,
                follow,
                final_execution: None,
            },
        );
    } else {
        println!("{}", format!("✓ Execution started: {execution_id}").green());
    }

    Ok(())
}

async fn status_daemon(
    execution_id: Uuid,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let execution = client.get_execution(execution_id).await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &execution);
    }

    println!("Execution {execution_id}");
    println!("  Status: {}", format_status(&execution.status));
    println!("  Agent: {}", execution.agent_id);
    if let Some(started) = execution.started_at {
        println!("  Started: {started}");
    }
    if let Some(ended) = execution.ended_at {
        println!("  Ended: {ended}");
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

async fn cancel_daemon(
    execution_id: Uuid,
    _force: bool,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    client.cancel_execution(execution_id).await?;
    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &TaskMutationOutput {
                execution_id,
                status: "cancelled",
            },
        );
    }
    println!(
        "{}",
        format!("✓ Execution {execution_id} cancelled").green()
    );
    Ok(())
}

async fn list_daemon(
    agent_id: Option<Uuid>,
    limit: usize,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let executions = client.list_executions(agent_id, limit).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &TaskListOutput {
                count: executions.len(),
                executions,
            },
        );
    }

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
                .with_context(|| format!("Failed to read input file: {path}"))?;
            parse_json_or_yaml(&content).context("Failed to parse input JSON/YAML")
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

async fn parse_object_input(
    input: Option<String>,
    label: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(raw) = input else {
        return Ok(None);
    };

    let value = if let Some(path) = raw.strip_prefix('@') {
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read {label} file: {path}"))?;
        parse_json_or_yaml(&content)
            .with_context(|| format!("Failed to parse {label} JSON/YAML"))?
    } else {
        parse_json_or_yaml(&raw).with_context(|| format!("Failed to parse {label} JSON/YAML"))?
    };

    if !value.is_object() {
        anyhow::bail!("{label} must be a JSON/YAML object");
    }

    Ok(Some(value))
}

fn parse_json_or_yaml(input: &str) -> Result<serde_json::Value> {
    serde_json::from_str(input)
        .or_else(|_| serde_yaml::from_str(input))
        .context("Invalid JSON or YAML")
}

async fn wait_for_execution_completion(
    execution_id: Uuid,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<crate::daemon::client::ExecutionInfo> {
    const MAX_POLLS: u32 = 300;
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    for _ in 0..MAX_POLLS {
        let execution = client.get_execution(execution_id).await?;
        let normalized = execution.status.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "completed" | "failed" | "cancelled" | "canceled"
        ) {
            if !output_format.is_structured() {
                println!(
                    "Execution {} finished with status: {}",
                    execution.id, execution.status
                );
            }
            return Ok(execution);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    anyhow::bail!("Timed out waiting for execution {execution_id} to finish");
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

async fn remove_daemon(
    execution_id: Uuid,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    client.delete_execution(execution_id).await?;
    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &TaskMutationOutput {
                execution_id,
                status: "removed",
            },
        );
    }
    println!("{}", format!("✓ Execution {execution_id} removed").green());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parse_input_supports_yaml_files() {
        let path = std::env::temp_dir().join(format!("aegis-task-input-{}.yaml", Uuid::new_v4()));
        tokio::fs::write(&path, "message: hello\ncount: 2\n")
            .await
            .unwrap();

        let parsed = parse_input(Some(format!("@{}", path.display())))
            .await
            .unwrap();
        assert_eq!(parsed["message"], serde_json::json!("hello"));
        assert_eq!(parsed["count"], serde_json::json!(2));

        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn parse_object_input_rejects_scalar_values() {
        let err = parse_object_input(Some("hello".to_string()), "context override")
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("context override must be a JSON/YAML object"));
    }
}
