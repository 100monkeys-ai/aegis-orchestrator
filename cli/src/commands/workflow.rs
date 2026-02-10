// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Workflow command implementations
//!
//! This module provides CLI commands for managing workflow definitions and executions.
//! Workflows enable declarative multi-agent coordination using FSM state machines.
//!
//! # Commands
//!
//! - `aegis workflow validate <file>` - Parse and validate workflow manifest
//! - `aegis workflow deploy <file>` - Deploy workflow to registry
//! - `aegis workflow run <name>` - Execute a registered workflow
//! - `aegis workflow list` - List registered workflows
//! - `aegis workflow describe <name>` - Show workflow details
//! - `aegis workflow logs <execution_id>` - Stream workflow execution logs

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use uuid::Uuid;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};

#[derive(Subcommand)]
pub enum WorkflowCommand {
    /// Validate a workflow manifest file
    Validate {
        /// Path to workflow manifest YAML file
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// Deploy a workflow to the registry
    Deploy {
        /// Path to workflow manifest YAML file
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },

    /// Execute a registered workflow
    Run {
        /// Workflow name
        #[arg(value_name = "NAME")]
        name: String,

        /// Workflow input parameters (JSON string)
        #[arg(long, short = 'i', value_name = "JSON")]
        input: Option<String>,

        /// Individual parameters (key=value)
        #[arg(long = "param", short = 'p', value_name = "KEY=VALUE")]
        params: Vec<String>,

        /// Follow execution logs
        #[arg(long, short = 'f')]
        follow: bool,
    },

    /// List all registered workflows
    List {
        /// Show detailed information
        #[arg(long, short = 'l')]
        long: bool,

        /// Filter by label (key=value)
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
    },

    /// Describe a workflow (show YAML definition)
    Describe {
        /// Workflow name
        #[arg(value_name = "NAME")]
        name: String,

        /// Output format (yaml, json)
        #[arg(long, short = 'o', default_value = "yaml")]
        output: String,
    },

    /// Stream workflow execution logs
    Logs {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,

        /// Show state transitions only
        #[arg(long)]
        transitions: bool,
    },

    /// Delete a workflow from registry
    Delete {
        /// Workflow name
        #[arg(value_name = "NAME")]
        name: String,

        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

pub async fn handle_command(
    command: WorkflowCommand,
    _config_path: Option<PathBuf>,
    port: u16,
) -> Result<()> {
    match command {
        WorkflowCommand::Validate { file } => validate_workflow(file).await,
        WorkflowCommand::Deploy { file } => deploy_workflow(file, port).await,
        WorkflowCommand::Run {
            name,
            input,
            params,
            follow,
        } => run_workflow(name, input, params, follow, port).await,
        WorkflowCommand::List { long, labels } => list_workflows(long, labels, port).await,
        WorkflowCommand::Describe { name, output } => describe_workflow(name, output, port).await,
        WorkflowCommand::Logs {
            execution_id,
            follow,
            transitions,
        } => stream_workflow_logs(execution_id, follow, transitions, port).await,
        WorkflowCommand::Delete { name, yes } => delete_workflow(name, yes, port).await,
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

/// Validate a workflow manifest file
async fn validate_workflow(file: PathBuf) -> Result<()> {
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;

    println!("{}", "📋 Validating workflow manifest...".cyan());
    println!("   File: {}", file.display());
    println!();

    // Parse manifest
    let workflow = WorkflowParser::parse_file(&file)
        .context("Failed to parse workflow manifest")?;

    // Validate for cycles
    use aegis_core::domain::workflow::WorkflowValidator;
    WorkflowValidator::check_for_cycles(&workflow)
        .context("Workflow validation failed")?;

    // Success
    println!("{}", "✓ Workflow is valid!".green().bold());
    println!();
    println!("Workflow Details:");
    println!("  Name:        {}", workflow.metadata.name);
    if let Some(version) = &workflow.metadata.version {
        println!("  Version:     {}", version);
    }
    if let Some(description) = &workflow.metadata.description {
        println!("  Description: {}", description);
    }
    println!("  States:      {}", workflow.spec.states.len());
    println!(
        "  Initial:     {}",
        workflow.spec.initial_state.as_str()
    );

    // Count terminal states
    let terminal_count = workflow
        .spec
        .states
        .values()
        .filter(|s| s.transitions.is_empty())
        .count();
    println!("  Terminal:    {} state(s)", terminal_count);

    Ok(())
}

/// Deploy a workflow to the registry
async fn deploy_workflow(file: PathBuf, port: u16) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
            println!(
                "{}",
                format!(
                    "⚠ Daemon is running (PID: {}) but unhealthy: {}",
                    pid, error
                )
                .yellow()
            );
            println!("Run 'aegis daemon status' for more info.");
            return Ok(());
        }
        _ => {
            println!(
                "{}",
                "Workflow deployment requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    println!("{}", "📤 Deploying workflow...".cyan());
    println!("   File: {}", file.display());
    println!();

    // First validate locally
    use aegis_core::infrastructure::workflow_parser::WorkflowParser;
    let workflow = WorkflowParser::parse_file(&file)
        .context("Failed to parse workflow manifest")?;

    let workflow_name = workflow.metadata.name.clone();

    // Deploy via daemon API
    let client = DaemonClient::new(port)?;
    client
        .deploy_workflow(&file)
        .await
        .context("Failed to deploy workflow")?;

    println!("{}", "✓ Workflow deployed successfully!".green().bold());
    println!();
    println!("  Name: {}", workflow_name);
    println!("  Run:  aegis workflow run {}", workflow_name);

    Ok(())
}

/// Execute a registered workflow
async fn run_workflow(
    name: String,
    input_json: Option<String>,
    params: Vec<String>,
    follow: bool,
    port: u16,
) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Workflow execution requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    // Parse input parameters
    let mut input_params = serde_json::Map::new();

    // Parse JSON input if provided
    if let Some(json) = input_json {
        let parsed: serde_json::Value = serde_json::from_str(&json)
            .context("Invalid JSON input")?;
        if let Some(obj) = parsed.as_object() {
            input_params.extend(obj.clone());
        }
    }

    // Parse individual parameters
    for param in params {
        let parts: Vec<&str> = param.splitn(2, '=').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid parameter format: '{}'. Expected 'key=value'", param);
        }
        let key = parts[0].to_string();
        let value = parts[1].to_string();
        
        // Try to parse as JSON, fall back to string
        let json_value = serde_json::from_str(&value)
            .unwrap_or_else(|_| serde_json::Value::String(value));
        
        input_params.insert(key, json_value);
    }

    println!("{}", "🚀 Starting workflow execution...".cyan());
    println!("   Workflow: {}", name);
    if !input_params.is_empty() {
        println!("   Parameters:");
        for (key, value) in &input_params {
            println!("     {}: {}", key, value);
        }
    }
    println!();

    // Start execution via daemon API
    let client = DaemonClient::new(port)?;
    let execution_id = client
        .run_workflow(&name, serde_json::Value::Object(input_params))
        .await
        .context("Failed to start workflow execution")?;

    println!("{}", "✓ Workflow execution started!".green().bold());
    println!();
    println!("  Execution ID: {}", execution_id);
    println!("  View logs:    aegis workflow logs {}", execution_id);
    println!();

    if follow {
        println!("{}", "📡 Streaming logs...".cyan());
        println!();
        client
            .stream_workflow_logs(execution_id)
            .await
            .context("Failed to stream logs")?;
    }

    Ok(())
}

/// List registered workflows
async fn list_workflows(long: bool, labels: Vec<String>, port: u16) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Listing workflows requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let client = DaemonClient::new(port)?;
    let workflows = client
        .list_workflows()
        .await
        .context("Failed to list workflows")?;

    if workflows.is_empty() {
        println!("{}", "No workflows registered.".yellow());
        println!();
        println!("Deploy a workflow:");
        println!("  aegis workflow deploy <file>");
        return Ok(());
    }

    // Filter by labels if provided
    let label_filters: Vec<(&str, &str)> = labels
        .iter()
        .filter_map(|l| {
            let parts: Vec<&str> = l.splitn(2, '=').collect();
            if parts.len() == 2 {
                Some((parts[0], parts[1]))
            } else {
                None
            }
        })
        .collect();

    println!("{}", "📋 Registered Workflows".cyan().bold());
    println!();

    for workflow in workflows {
        // Apply label filters
        if !label_filters.is_empty() {
            let matches_all = label_filters.iter().all(|(key, value)| {
                workflow
                    .get("labels")
                    .and_then(|l| l.get(*key))
                    .and_then(|v| v.as_str())
                    .map(|v| v == *value)
                    .unwrap_or(false)
            });
            if !matches_all {
                continue;
            }
        }

        let name = workflow
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");

        if long {
            println!("{}", format!("• {}", name).green().bold());
            if let Some(version) = workflow.get("version").and_then(|v| v.as_str()) {
                println!("  Version:     {}", version);
            }
            if let Some(desc) = workflow.get("description").and_then(|d| d.as_str()) {
                println!("  Description: {}", desc);
            }
            if let Some(states) = workflow.get("state_count").and_then(|s| s.as_u64()) {
                println!("  States:      {}", states);
            }
            println!();
        } else {
            println!("• {}", name.green());
        }
    }

    Ok(())
}

/// Describe a workflow (show YAML definition)
async fn describe_workflow(name: String, output_format: String, port: u16) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Describing workflows requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let client = DaemonClient::new(port)?;
    let workflow_yaml = client
        .describe_workflow(&name)
        .await
        .context("Failed to get workflow details")?;

    match output_format.as_str() {
        "yaml" => {
            println!("{}", workflow_yaml);
        }
        "json" => {
            let parsed: serde_yaml::Value = serde_yaml::from_str(&workflow_yaml)
                .context("Failed to parse workflow YAML")?;
            let json = serde_json::to_string_pretty(&parsed)
                .context("Failed to convert to JSON")?;
            println!("{}", json);
        }
        _ => {
            anyhow::bail!("Invalid output format: '{}'. Use 'yaml' or 'json'", output_format);
        }
    }

    Ok(())
}

/// Stream workflow execution logs
async fn stream_workflow_logs(
    execution_id: Uuid,
    follow: bool,
    transitions_only: bool,
    port: u16,
) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Streaming logs requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    println!("{}", "📡 Streaming workflow logs...".cyan());
    println!("   Execution ID: {}", execution_id);
    if transitions_only {
        println!("   Filter:       State transitions only");
    }
    println!();

    let client = DaemonClient::new(port)?;
    
    if follow {
        client
            .stream_workflow_logs(execution_id)
            .await
            .context("Failed to stream logs")?;
    } else {
        // Fetch logs once
        let logs = client
            .get_workflow_logs(execution_id)
            .await
            .context("Failed to get logs")?;
        println!("{}", logs);
    }

    Ok(())
}

/// Delete a workflow from registry
async fn delete_workflow(name: String, skip_confirmation: bool, port: u16) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Deleting workflows requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    // Confirmation prompt
    if !skip_confirmation {
        use std::io::{self, Write};
        print!(
            "{}",
            format!("Delete workflow '{}' (y/N)? ", name).yellow()
        );
        io::stdout().flush()?;

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;

        if !response.trim().eq_ignore_ascii_case("y") {
            println!("{}", "Cancelled.".yellow());
            return Ok(());
        }
    }

    let client = DaemonClient::new(port)?;
    client
        .delete_workflow(&name)
        .await
        .context("Failed to delete workflow")?;

    println!("{}", "✓ Workflow deleted successfully!".green().bold());

    Ok(())
}
