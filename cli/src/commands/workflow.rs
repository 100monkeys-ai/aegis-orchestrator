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
//! - `aegis workflow generate --input <text>` - Generate a workflow from natural language
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for workflow

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use uuid::Uuid;

use crate::commands::builtins;
use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};

const WORKFLOW_GENERATOR_WORKFLOW_NAME: &str = builtins::WORKFLOW_GENERATOR_WORKFLOW_NAME;

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

        /// Show verbose output (execution metadata, resolved agent IDs, etc.)
        #[arg(long, short = 'v')]
        verbose: bool,
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

    /// Generate a workflow from natural-language input
    Generate {
        /// Natural-language workflow objective
        #[arg(long, short = 'i', value_name = "INPUT")]
        input: String,

        /// Follow generator execution logs
        #[arg(short, long)]
        follow: bool,
    },
}

pub async fn handle_command(
    command: WorkflowCommand,
    config_path: Option<PathBuf>,
    host: &str,
    port: u16,
) -> Result<()> {
    match command {
        WorkflowCommand::Validate { file } => validate_workflow(file).await,
        WorkflowCommand::Deploy { file } => deploy_workflow(file, host, port).await,
        WorkflowCommand::Run {
            name,
            input,
            params,
            follow,
        } => run_workflow(name, input, params, follow, host, port).await,
        WorkflowCommand::List { long, labels } => list_workflows(long, labels, host, port).await,
        WorkflowCommand::Describe { name, output } => {
            describe_workflow(name, output, host, port).await
        }
        WorkflowCommand::Logs {
            execution_id,
            follow,
            transitions,
            verbose,
        } => stream_workflow_logs(execution_id, follow, transitions, verbose, host, port).await,
        WorkflowCommand::Delete { name, yes } => delete_workflow(name, yes, host, port).await,
        WorkflowCommand::Generate { input, follow } => {
            generate_workflow(input, follow, host, port, config_path.as_ref()).await
        }
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

/// Validate a workflow manifest file
async fn validate_workflow(file: PathBuf) -> Result<()> {
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

    println!("{}", "📋 Validating workflow manifest...".cyan());
    println!("   File: {}", file.display());
    println!();

    // Parse manifest
    let workflow =
        WorkflowParser::parse_file(&file).context("Failed to parse workflow manifest")?;

    // Validate for cycles
    use aegis_orchestrator_core::domain::workflow::WorkflowValidator;
    WorkflowValidator::check_for_cycles(&workflow).context("Workflow validation failed")?;

    // Success
    println!("{}", "✓ Workflow is valid!".green().bold());
    println!();
    println!("Workflow Details:");
    println!("  Name:        {}", workflow.metadata.name);
    if let Some(version) = &workflow.metadata.version {
        println!("  Version:     {version}");
    }
    if let Some(description) = &workflow.metadata.description {
        println!("  Description: {description}");
    }
    println!("  States:      {}", workflow.spec.states.len());
    println!("  Initial:     {}", workflow.spec.initial_state.as_str());

    // Count terminal states
    let terminal_count = workflow
        .spec
        .states
        .values()
        .filter(|s| s.transitions.is_empty())
        .count();
    println!("  Terminal:    {terminal_count} state(s)");

    Ok(())
}

/// Deploy a workflow to the registry
async fn deploy_workflow(file: PathBuf, host: &str, port: u16) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running(host, port).await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
            println!(
                "{}",
                format!("⚠ Daemon is running (PID: {pid}) but unhealthy: {error}").yellow()
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
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
    let workflow =
        WorkflowParser::parse_file(&file).context("Failed to parse workflow manifest")?;

    let workflow_name = workflow.metadata.name.clone();

    // Deploy via daemon API
    let client = DaemonClient::new(host, port)?;
    client
        .deploy_workflow(&file)
        .await
        .context("Failed to deploy workflow")?;

    println!("{}", "✓ Workflow deployed successfully!".green().bold());
    println!();
    println!("  Name: {workflow_name}");
    println!("  Run:  aegis workflow run {workflow_name}");

    Ok(())
}

/// Execute a registered workflow
async fn run_workflow(
    name: String,
    input_json: Option<String>,
    params: Vec<String>,
    follow: bool,
    host: &str,
    port: u16,
) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running(host, port).await;
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
        let parsed: serde_json::Value =
            serde_json::from_str(&json).context("Invalid JSON input")?;
        if let Some(obj) = parsed.as_object() {
            input_params.extend(obj.clone());
        }
    }

    // Parse individual parameters
    for param in params {
        let parts: Vec<&str> = param.splitn(2, '=').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid parameter format: '{param}'. Expected 'key=value'");
        }
        let key = parts[0].to_string();
        let value = parts[1].to_string();

        // Try to parse as JSON, fall back to string
        let json_value = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));

        input_params.insert(key, json_value);
    }

    println!("{}", "🚀 Starting workflow execution...".cyan());
    println!("   Workflow: {name}");
    if !input_params.is_empty() {
        println!("   Parameters:");
        for (key, value) in &input_params {
            println!("     {key}: {value}");
        }
    }
    println!();

    // Start execution via daemon API
    let client = DaemonClient::new(host, port)?;
    let execution_id = client
        .run_workflow(&name, serde_json::Value::Object(input_params))
        .await
        .context("Failed to start workflow execution")?;

    println!("{}", "✓ Workflow execution started!".green().bold());
    println!();
    println!("  Execution ID: {execution_id}");
    println!("  View logs:    aegis workflow logs {execution_id}");
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

async fn generate_workflow(
    input: String,
    follow: bool,
    host: &str,
    port: u16,
    config_path: Option<&PathBuf>,
) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
            println!(
                "{}",
                format!("⚠ Daemon is running (PID: {pid}) but unhealthy: {error}").yellow()
            );
            println!("Run 'aegis daemon status' for more info.");
            return Ok(());
        }
        _ => {
            println!(
                "{}",
                "Workflow generation requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let templates_root = builtins::resolve_templates_root(config_path);
    builtins::sync_generator_templates_to_disk(&templates_root)?;

    let client = DaemonClient::new(host, port)?;
    // Ensure built-ins are deployed (but don't force overwrite unless it's an update)
    builtins::deploy_all_builtins(&client, false).await?;

    println!(
        "{}",
        format!(
            "Generating workflow via built-in workflow '{WORKFLOW_GENERATOR_WORKFLOW_NAME}'..."
        )
        .cyan()
    );

    let execution_id = client
        .run_workflow(
            WORKFLOW_GENERATOR_WORKFLOW_NAME,
            serde_json::json!({
                "input": input
            }),
        )
        .await
        .context("Failed to start workflow generation execution")?;

    println!(
        "{}",
        format!("✓ Workflow generation execution started: {execution_id}").green()
    );

    if follow {
        client.stream_workflow_logs(execution_id).await?;
    } else {
        println!(
            "Follow generator workflow logs with:\n  aegis workflow logs {execution_id} --follow"
        );
    }

    Ok(())
}

/// List registered workflows
async fn list_workflows(long: bool, labels: Vec<String>, host: &str, port: u16) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running(host, port).await;
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

    let client = DaemonClient::new(host, port)?;
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
            println!("{}", format!("• {name}").green().bold());
            if let Some(version) = workflow.get("version").and_then(|v| v.as_str()) {
                println!("  Version:     {version}");
            }
            if let Some(desc) = workflow.get("description").and_then(|d| d.as_str()) {
                println!("  Description: {desc}");
            }
            if let Some(states) = workflow.get("state_count").and_then(|s| s.as_u64()) {
                println!("  States:      {states}");
            }
            println!();
        } else {
            println!("• {}", name.green());
        }
    }

    Ok(())
}

/// Describe a workflow (show YAML definition)
async fn describe_workflow(
    name: String,
    output_format: String,
    host: &str,
    port: u16,
) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running(host, port).await;
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

    let client = DaemonClient::new(host, port)?;
    let workflow_yaml = client
        .describe_workflow(&name)
        .await
        .context("Failed to get workflow details")?;

    match output_format.as_str() {
        "yaml" => {
            println!("{workflow_yaml}");
        }
        "json" => {
            let parsed: serde_yaml::Value =
                serde_yaml::from_str(&workflow_yaml).context("Failed to parse workflow YAML")?;
            let json =
                serde_json::to_string_pretty(&parsed).context("Failed to convert to JSON")?;
            println!("{json}");
        }
        _ => {
            anyhow::bail!("Invalid output format: '{output_format}'. Use 'yaml' or 'json'");
        }
    }

    Ok(())
}

/// Stream workflow execution logs
async fn stream_workflow_logs(
    execution_id: Uuid,
    follow: bool,
    transitions_only: bool,
    verbose: bool,
    host: &str,
    port: u16,
) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running(host, port).await;
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
    println!("   Execution ID: {execution_id}");
    if transitions_only {
        println!("   Filter:       State transitions only");
    }
    if verbose {
        println!("   Mode:         Verbose");
    }
    println!();

    let client = DaemonClient::new(host, port)?;

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
        println!("{logs}");
    }

    Ok(())
}

/// Delete a workflow from registry
async fn delete_workflow(
    name: String,
    skip_confirmation: bool,
    host: &str,
    port: u16,
) -> Result<()> {
    // Check daemon is running
    let daemon_status = check_daemon_running(host, port).await;
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

    // Confirmation prompt — run blocking stdin/stdout I/O on a dedicated
    // thread so we don't park the Tokio executor while waiting for user input.
    if !skip_confirmation {
        let name_for_prompt = name.clone();
        let confirmed = tokio::task::spawn_blocking(move || {
            use std::io::{self, Write};
            print!(
                "{}",
                format!("Delete workflow '{name_for_prompt}' (y/N)? ").yellow()
            );
            io::stdout().flush()?;
            let mut response = String::new();
            io::stdin().read_line(&mut response)?;
            Ok::<bool, anyhow::Error>(response.trim().eq_ignore_ascii_case("y"))
        })
        .await
        .context("Confirmation prompt failed")??;

        if !confirmed {
            println!("{}", "Cancelled.".yellow());
            return Ok(());
        }
    }

    let client = DaemonClient::new(host, port)?;
    client
        .delete_workflow(&name)
        .await
        .context("Failed to delete workflow")?;

    println!("{}", "✓ Workflow deleted successfully!".green().bold());

    Ok(())
}
