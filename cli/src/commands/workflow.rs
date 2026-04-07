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
//! - `aegis workflow deploy <file> [--force]` - Deploy workflow to registry
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
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

use crate::commands::builtins;
use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};
use crate::output::{render_serialized, structured_output_unsupported, OutputFormat};

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

        /// Overwrite an existing workflow with the same name/version
        #[arg(long, short = 'f')]
        force: bool,

        /// Workflow visibility scope (user, tenant, global). Default: tenant.
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
    },

    /// Execute a registered workflow
    Run {
        /// Workflow name
        #[arg(value_name = "NAME")]
        name: String,

        /// Natural-language steering for this execution
        #[arg(long, value_name = "TEXT")]
        intent: Option<String>,

        /// Workflow input parameters (JSON string)
        #[arg(long, short = 'i', value_name = "JSON")]
        input: Option<String>,

        /// Individual parameters (key=value)
        #[arg(long = "param", short = 'p', value_name = "KEY=VALUE")]
        params: Vec<String>,

        /// Blackboard override dictionary (JSON/YAML string or @file)
        #[arg(long, value_name = "DICT")]
        blackboard: Option<String>,

        /// Target a specific workflow version (default: latest)
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,

        /// Follow execution logs
        #[arg(long, short = 'f')]
        follow: bool,

        /// Wait for execution to complete
        #[arg(long, short = 'w')]
        wait: bool,
    },

    /// List all registered workflows
    List {
        /// Show detailed information
        #[arg(long, short = 'l')]
        long: bool,

        /// Filter by label (key=value)
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,

        /// Filter by scope (global, tenant, user)
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,

        /// Show all visible workflows (user + tenant + global)
        #[arg(long)]
        visible: bool,
    },

    /// Describe a workflow (show YAML definition)
    Describe {
        /// Workflow name
        #[arg(value_name = "NAME")]
        name: String,
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

        /// Only show workflow failures
        #[arg(long)]
        errors_only: bool,
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

    /// Check workflow execution status
    Status {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,
    },

    /// Send human input to a paused workflow execution
    Signal {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,

        /// Response payload to send to the waiting Human state
        #[arg(long, short = 'r', value_name = "TEXT")]
        response: String,
    },

    /// Cancel a workflow execution
    Cancel {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,
    },

    /// Remove a workflow execution
    Remove {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,
    },

    /// Promote a workflow's visibility scope
    Promote {
        /// Workflow name or ID
        #[arg(value_name = "NAME_OR_ID")]
        name_or_id: String,

        /// Target scope to promote to (tenant, global)
        #[arg(long, value_name = "SCOPE")]
        to: String,
    },

    /// Demote a workflow's visibility scope
    Demote {
        /// Workflow name or ID
        #[arg(value_name = "NAME_OR_ID")]
        name_or_id: String,

        /// Target scope to demote to (tenant, user)
        #[arg(long, value_name = "SCOPE")]
        to: String,
    },

    /// Manage workflow executions
    Executions {
        #[command(subcommand)]
        command: ExecutionsCommand,
    },
}

#[derive(Subcommand)]
pub enum ExecutionsCommand {
    /// List recent workflow executions
    List {
        /// Maximum number of results to return
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Filter by workflow name or UUID
        #[arg(long, value_name = "NAME_OR_UUID")]
        workflow: Option<String>,

        /// Show detailed information (current state, timestamps)
        #[arg(long, short = 'l')]
        long: bool,
    },

    /// Get workflow execution details
    #[command(alias = "status")]
    Get {
        /// Execution ID
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: Uuid,
    },
}

pub async fn handle_command(
    command: WorkflowCommand,
    config_path: Option<PathBuf>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        WorkflowCommand::Validate { file } => validate_workflow(file, output_format).await,
        WorkflowCommand::Deploy { file, force, scope } => {
            deploy_workflow(file, force, scope, host, port, output_format).await
        }
        WorkflowCommand::Run {
            name,
            intent,
            input,
            params,
            blackboard,
            version,
            follow,
            wait,
        } => {
            run_workflow(
                WorkflowRunRequest {
                    name,
                    intent,
                    input_json: input,
                    params,
                    blackboard,
                    version,
                    follow,
                    wait,
                },
                host,
                port,
                output_format,
            )
            .await
        }
        WorkflowCommand::List {
            long,
            labels,
            scope,
            visible,
        } => list_workflows(long, labels, scope, visible, host, port, output_format).await,
        WorkflowCommand::Describe { name } => {
            describe_workflow(name, output_format, host, port).await
        }
        WorkflowCommand::Logs {
            execution_id,
            follow,
            transitions,
            verbose,
            errors_only,
        } => {
            if output_format.is_structured() {
                structured_output_unsupported("aegis workflow logs", output_format)
            } else {
                stream_workflow_logs(
                    execution_id,
                    follow,
                    transitions,
                    verbose,
                    errors_only,
                    host,
                    port,
                )
                .await
            }
        }
        WorkflowCommand::Delete { name, yes } => {
            delete_workflow(name, yes, host, port, output_format).await
        }
        WorkflowCommand::Generate { input, follow } => {
            generate_workflow(
                input,
                follow,
                host,
                port,
                config_path.as_ref(),
                output_format,
            )
            .await
        }
        WorkflowCommand::Promote { name_or_id, to } => {
            change_workflow_scope(name_or_id, to, "promote", host, port, output_format).await
        }
        WorkflowCommand::Demote { name_or_id, to } => {
            change_workflow_scope(name_or_id, to, "demote", host, port, output_format).await
        }
        WorkflowCommand::Executions { command } => {
            handle_executions_command(command, host, port, output_format).await
        }
        WorkflowCommand::Status { execution_id } => {
            get_workflow_execution(execution_id, host, port, output_format).await
        }
        WorkflowCommand::Signal {
            execution_id,
            response,
        } => signal_workflow_execution(execution_id, response, host, port, output_format).await,
        WorkflowCommand::Cancel { execution_id } => {
            cancel_workflow_execution(execution_id, host, port, output_format).await
        }
        WorkflowCommand::Remove { execution_id } => {
            remove_workflow_execution(execution_id, host, port, output_format).await
        }
    }
}

async fn handle_executions_command(
    command: ExecutionsCommand,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        ExecutionsCommand::List {
            limit,
            workflow,
            long,
        } => list_workflow_executions(limit, workflow, long, host, port, output_format).await,
        ExecutionsCommand::Get { execution_id } => {
            get_workflow_execution(execution_id, host, port, output_format).await
        }
    }
}

// ============================================================================
// Command Implementations
// ============================================================================

#[derive(Serialize)]
struct WorkflowValidateOutput {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    states: usize,
    initial_state: String,
    terminal_states: usize,
}

#[derive(Serialize)]
struct WorkflowDeployOutput {
    name: String,
    file: String,
    force: bool,
}

#[derive(Serialize)]
struct WorkflowRunOutput {
    name: String,
    execution_id: Uuid,
    follow: bool,
    wait: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_execution: Option<crate::daemon::client::WorkflowExecutionInfo>,
}

struct WorkflowRunRequest {
    name: String,
    intent: Option<String>,
    input_json: Option<String>,
    params: Vec<String>,
    blackboard: Option<String>,
    version: Option<String>,
    follow: bool,
    wait: bool,
}

#[derive(Serialize)]
struct WorkflowListOutput {
    count: usize,
    workflows: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct WorkflowExecutionsOutput {
    count: usize,
    executions: Vec<crate::daemon::client::WorkflowExecutionInfo>,
}

#[derive(Serialize)]
struct WorkflowDeleteOutput {
    name: String,
    status: &'static str,
}

#[derive(Serialize)]
struct WorkflowExecutionMutationOutput {
    execution_id: Uuid,
    status: &'static str,
}

#[derive(Serialize)]
struct WorkflowGenerateOutput {
    workflow_name: &'static str,
    execution_id: Uuid,
    follow: bool,
    generated_workflows_root: String,
    generated_agents_root: String,
}

/// Validate a workflow manifest file
async fn validate_workflow(file: PathBuf, output_format: OutputFormat) -> Result<()> {
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

    if !output_format.is_structured() {
        println!("{}", "📋 Validating workflow manifest...".cyan());
        println!("   File: {}", file.display());
        println!();
    }

    // Parse manifest
    let workflow =
        WorkflowParser::parse_file(&file).context("Failed to parse workflow manifest")?;

    // Validate for cycles
    use aegis_orchestrator_core::domain::workflow::WorkflowValidator;
    WorkflowValidator::check_for_cycles(&workflow).context("Workflow validation failed")?;

    let terminal_count = workflow
        .spec
        .states
        .values()
        .filter(|s| s.transitions.is_empty())
        .count();

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowValidateOutput {
                name: workflow.metadata.name.clone(),
                version: workflow.metadata.version.clone(),
                description: workflow.metadata.description.clone(),
                states: workflow.spec.states.len(),
                initial_state: workflow.spec.initial_state.to_string(),
                terminal_states: terminal_count,
            },
        );
    }

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
    println!("  Terminal:    {terminal_count} state(s)");

    Ok(())
}

/// Deploy a workflow to the registry
async fn deploy_workflow(
    file: PathBuf,
    force: bool,
    scope: Option<String>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
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

    if !output_format.is_structured() {
        println!("{}", "📤 Deploying workflow...".cyan());
        println!("   File: {}", file.display());
        if force {
            println!("   Mode: overwrite existing workflow");
        }
        if let Some(ref s) = scope {
            println!("   Scope: {s}");
        }
        println!();
    }

    // First validate locally
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
    let workflow =
        WorkflowParser::parse_file(&file).context("Failed to parse workflow manifest")?;

    let workflow_name = workflow.metadata.name.clone();

    // Deploy via daemon API
    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    client
        .deploy_workflow_with_force_and_scope(&file, force, scope.as_deref())
        .await
        .context("Failed to deploy workflow")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowDeployOutput {
                name: workflow_name,
                file: file.display().to_string(),
                force,
            },
        );
    }

    println!("{}", "✓ Workflow deployed successfully!".green().bold());
    println!();
    println!("  Name: {workflow_name}");
    println!("  Run:  aegis workflow run {workflow_name}");

    Ok(())
}

/// Execute a registered workflow
async fn run_workflow(
    request: WorkflowRunRequest,
    host: &str,
    port: u16,
    output_format: OutputFormat,
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
    if let Some(json) = request.input_json {
        let parsed = parse_json_or_yaml_input(&json).await?;
        if let Some(obj) = parsed.as_object() {
            input_params.extend(obj.clone());
        } else {
            anyhow::bail!("Workflow input must be a JSON/YAML object");
        }
    }

    // Parse individual parameters
    for param in request.params {
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

    if output_format.is_structured() && request.follow {
        return structured_output_unsupported("aegis workflow run --follow", output_format);
    }

    if !output_format.is_structured() {
        println!("{}", "🚀 Starting workflow execution...".cyan());
        println!("   Workflow: {}", request.name);
        if !input_params.is_empty() {
            println!("   Parameters:");
            for (key, value) in &input_params {
                println!("     {key}: {value}");
            }
        }
        println!();
    }

    // Start execution via daemon API
    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let blackboard = parse_optional_object_input(request.blackboard, "workflow blackboard").await?;
    let execution_id = client
        .run_workflow(
            &request.name,
            serde_json::Value::Object(input_params),
            blackboard,
            request.version.as_deref(),
            request.intent,
        )
        .await
        .context("Failed to start workflow execution")?;

    if request.follow {
        println!("{}", "📡 Streaming logs...".cyan());
        println!();
        client
            .stream_workflow_logs(
                execution_id,
                crate::daemon::client::WorkflowLogOptions {
                    transitions_only: false,
                    errors_only: false,
                    verbose: false,
                },
            )
            .await
            .context("Failed to stream logs")?;
    } else if request.wait {
        if !output_format.is_structured() {
            println!(
                "{}",
                format!("✓ Workflow execution started: {execution_id}").green()
            );
            println!("Waiting for completion...");
        }
        let final_execution =
            wait_for_workflow_execution_completion(execution_id, &client, output_format).await?;
        if output_format.is_structured() {
            return render_serialized(
                output_format,
                &WorkflowRunOutput {
                    name: request.name,
                    execution_id,
                    follow: request.follow,
                    wait: request.wait,
                    final_execution: Some(final_execution),
                },
            );
        }
    } else if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowRunOutput {
                name: request.name,
                execution_id,
                follow: request.follow,
                wait: request.wait,
                final_execution: None,
            },
        );
    }

    println!("{}", "✓ Workflow execution started!".green().bold());
    println!();
    println!("  Execution ID: {execution_id}");
    println!("  View logs:    aegis workflow logs {execution_id}");
    println!();

    Ok(())
}

async fn parse_json_or_yaml_input(raw: &str) -> Result<serde_json::Value> {
    if let Some(path) = raw.strip_prefix('@') {
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read input file: {path}"))?;
        parse_json_or_yaml(&content)
    } else {
        parse_json_or_yaml(raw)
    }
}

async fn parse_optional_object_input(
    raw: Option<String>,
    label: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let parsed = if let Some(path) = raw.strip_prefix('@') {
        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read {label} file: {path}"))?;
        parse_json_or_yaml(&content)?
    } else {
        parse_json_or_yaml(&raw)?
    };

    if !parsed.is_object() {
        anyhow::bail!("{label} must be a JSON/YAML object");
    }

    Ok(Some(parsed))
}

async fn wait_for_workflow_execution_completion(
    execution_id: Uuid,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<crate::daemon::client::WorkflowExecutionInfo> {
    const MAX_POLLS: u32 = 300;
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    for _ in 0..MAX_POLLS {
        let execution = client.get_workflow_execution(execution_id).await?;
        let normalized = execution.status.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "completed" | "failed" | "cancelled" | "canceled" | "timed_out"
        ) {
            if !output_format.is_structured() {
                println!(
                    "Workflow execution {} finished with status: {}",
                    execution.execution_id, execution.status
                );
            }
            return Ok(execution);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    anyhow::bail!("Timed out waiting for workflow execution {execution_id} to finish");
}

fn parse_json_or_yaml(input: &str) -> Result<serde_json::Value> {
    serde_json::from_str(input)
        .or_else(|_| serde_yaml::from_str(input))
        .context("Invalid JSON or YAML")
}

async fn generate_workflow(
    input: String,
    follow: bool,
    host: &str,
    port: u16,
    config_path: Option<&PathBuf>,
    output_format: OutputFormat,
) -> Result<()> {
    if output_format.is_structured() && follow {
        return structured_output_unsupported("aegis workflow generate --follow", output_format);
    }

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
    let generated_root = builtins::resolve_generated_root(config_path);
    let generated_workflows_root = generated_root.join("workflows");
    let generated_agents_root = generated_root.join("agents");
    builtins::sync_generator_templates_to_disk(&templates_root)?;

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    // Ensure built-ins are deployed (but don't force overwrite unless it's an update)
    builtins::deploy_all_builtins(&client, false).await?;

    if !output_format.is_structured() {
        println!(
            "{}",
            format!(
                "Generating workflow via built-in workflow '{WORKFLOW_GENERATOR_WORKFLOW_NAME}'..."
            )
            .cyan()
        );
    }

    let execution_id = client
        .run_workflow(
            WORKFLOW_GENERATOR_WORKFLOW_NAME,
            serde_json::json!({
                "input": input
            }),
            None,
            None,
            None,
        )
        .await
        .context("Failed to start workflow generation execution")?;

    if follow {
        client
            .stream_workflow_logs(
                execution_id,
                crate::daemon::client::WorkflowLogOptions {
                    transitions_only: false,
                    errors_only: false,
                    verbose: false,
                },
            )
            .await?;
    } else if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowGenerateOutput {
                workflow_name: WORKFLOW_GENERATOR_WORKFLOW_NAME,
                execution_id,
                follow,
                generated_workflows_root: generated_workflows_root.display().to_string(),
                generated_agents_root: generated_agents_root.display().to_string(),
            },
        );
    } else {
        println!(
            "{}",
            format!("✓ Workflow generation execution started: {execution_id}").green()
        );
        println!("Generated workflow manifests will be persisted under:");
        println!("  {}", generated_workflows_root.display());
        println!("Generated agent manifests from this flow will be persisted under:");
        println!("  {}", generated_agents_root.display());
        println!(
            "Follow generator workflow logs with:\n  aegis workflow logs {execution_id} --follow"
        );
    }

    Ok(())
}

/// List registered workflows
async fn list_workflows(
    long: bool,
    labels: Vec<String>,
    scope: Option<String>,
    visible: bool,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
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

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let workflows = client
        .list_workflows_with_scope(scope.as_deref(), visible)
        .await
        .context("Failed to list workflows")?;

    let label_filters: Vec<(&str, &str)> = labels
        .iter()
        .filter_map(|l| {
            let parts: Vec<&str> = l.splitn(2, '=').collect();
            (parts.len() == 2).then_some((parts[0], parts[1]))
        })
        .collect();

    let workflows = workflows
        .into_iter()
        .filter(|workflow| {
            label_filters.is_empty()
                || label_filters.iter().all(|(key, value)| {
                    workflow
                        .get("labels")
                        .and_then(|l| l.get(*key))
                        .and_then(|v| v.as_str())
                        .map(|v| v == *value)
                        .unwrap_or(false)
                })
        })
        .collect::<Vec<_>>();

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowListOutput {
                count: workflows.len(),
                workflows,
            },
        );
    }

    if workflows.is_empty() {
        println!("{}", "No workflows registered.".yellow());
        println!();
        println!("Deploy a workflow:");
        println!("  aegis workflow deploy <file>");
        return Ok(());
    }

    println!("{}", "📋 Registered Workflows".cyan().bold());
    println!();

    for workflow in workflows {
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
            if let Some(scope_val) = workflow.get("scope").and_then(|s| s.as_str()) {
                println!("  Scope:       {scope_val}");
            }
            if let Some(states) = workflow.get("state_count").and_then(|s| s.as_u64()) {
                println!("  States:      {states}");
            }
            println!();
        } else {
            let scope_tag = workflow
                .get("scope")
                .and_then(|s| s.as_str())
                .unwrap_or("tenant");
            println!("• {} [{}]", name.green(), scope_tag);
        }
    }

    Ok(())
}

/// Change workflow scope (promote or demote)
async fn change_workflow_scope(
    name_or_id: String,
    to: String,
    direction: &str,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Workflow scope changes require the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    if !output_format.is_structured() {
        let action = if direction == "promote" {
            "Promoting"
        } else {
            "Demoting"
        };
        println!(
            "{}",
            format!("🔄 {action} workflow '{name_or_id}' to scope '{to}'...").cyan()
        );
        println!();
    }

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let result = client
        .change_workflow_scope(&name_or_id, &to)
        .await
        .context("Failed to change workflow scope")?;

    if output_format.is_structured() {
        return render_serialized(output_format, &result);
    }

    let previous = result
        .get("previous_scope")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let new_scope = result
        .get("new_scope")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let action_past = if direction == "promote" {
        "promoted"
    } else {
        "demoted"
    };
    println!(
        "{}",
        format!("✓ Workflow '{name_or_id}' {action_past}: {previous} → {new_scope}")
            .green()
            .bold()
    );

    Ok(())
}

/// List recent workflow executions
async fn list_workflow_executions(
    limit: usize,
    workflow: Option<String>,
    long: bool,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Listing workflow executions requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let workflow_id = match workflow {
        Some(ref raw) if Uuid::parse_str(raw).is_ok() => Some(Uuid::parse_str(raw)?),
        Some(raw) => client
            .lookup_workflow(&raw)
            .await
            .with_context(|| format!("Failed to resolve workflow '{raw}'"))?,
        None => None,
    };
    let executions = client
        .list_workflow_executions(limit, workflow_id)
        .await
        .context("Failed to list workflow executions")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowExecutionsOutput {
                count: executions.len(),
                executions,
            },
        );
    }

    if executions.is_empty() {
        println!("{}", "No workflow executions found.".yellow());
        println!();
        println!("Run a workflow with:");
        println!("  aegis workflow run <name>");
        return Ok(());
    }

    println!("{}", "📋 Workflow Executions".cyan().bold());
    println!();

    for execution in executions {
        let execution_id = execution.execution_id;
        let status = execution.status.as_str();
        let status_display = match status {
            "running" => status.green(),
            "completed" => status.cyan(),
            "failed" => status.red(),
            "cancelled" => status.yellow(),
            _ => status.normal(),
        };

        if long {
            println!("{}", format!("• {execution_id}").bold());
            let workflow_name = execution.workflow_name.as_deref().unwrap_or("unknown");
            println!(
                "  Workflow:     {} ({})",
                workflow_name, execution.workflow_id
            );
            println!("  Status:       {status_display}");
            if let Some(state) = execution.current_state.as_deref() {
                println!("  State:        {state}");
            }
            if let Some(started) = execution.started_at.as_deref() {
                println!("  Started:      {started}");
            }
            if let Some(updated) = execution.last_transition_at.as_deref() {
                println!("  Last updated: {updated}");
            }
            println!();
        } else {
            let workflow_name = execution.workflow_name.as_deref().unwrap_or("unknown");
            println!(
                "• {}  [{}]  {}",
                execution_id.to_string().green(),
                status_display,
                workflow_name
            );
        }
    }

    Ok(())
}

/// Describe a workflow (show YAML definition)
async fn describe_workflow(
    name: String,
    output_format: OutputFormat,
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

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let value = client
        .describe_workflow(&name)
        .await
        .context("Failed to get workflow details")?;

    match output_format {
        OutputFormat::Json | OutputFormat::Yaml => render_serialized(output_format, &value),
        OutputFormat::Text | OutputFormat::Table => {
            print!(
                "{}",
                serde_json::to_string_pretty(&value)
                    .context("Failed to serialize workflow details")?
            );
            Ok(())
        }
    }
}

/// Stream workflow execution logs
async fn stream_workflow_logs(
    execution_id: Uuid,
    follow: bool,
    transitions_only: bool,
    verbose: bool,
    errors_only: bool,
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
    if errors_only {
        println!("   Filter:       Errors only");
    }
    if verbose {
        println!("   Mode:         Verbose");
    }
    println!();

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let options = crate::daemon::client::WorkflowLogOptions {
        transitions_only,
        errors_only,
        verbose,
    };

    if follow {
        client
            .stream_workflow_logs(execution_id, options)
            .await
            .context("Failed to stream logs")?;
    } else {
        let logs = client
            .get_workflow_logs(execution_id, options)
            .await
            .context("Failed to get logs")?;
        for event in logs {
            print!(
                "{}",
                crate::daemon::client::format_workflow_log_event(&event, verbose)
            );
        }
    }

    Ok(())
}

async fn get_workflow_execution(
    execution_id: Uuid,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        _ => {
            println!(
                "{}",
                "Workflow execution status requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    let execution = client
        .get_workflow_execution(execution_id)
        .await
        .context("Failed to get workflow execution")?;

    if output_format.is_structured() {
        return render_serialized(output_format, &execution);
    }

    println!("Workflow execution {execution_id}");
    println!(
        "  Workflow: {} ({})",
        execution.workflow_name.as_deref().unwrap_or("unknown"),
        execution.workflow_id
    );
    println!("  Status: {}", execution.status);
    if let Some(state) = execution.current_state.as_deref() {
        println!("  State: {state}");
    }
    if let Some(started_at) = execution.started_at.as_deref() {
        println!("  Started: {started_at}");
    }
    if let Some(updated_at) = execution.last_transition_at.as_deref() {
        println!("  Updated: {updated_at}");
    }

    Ok(())
}

async fn signal_workflow_execution(
    execution_id: Uuid,
    response: String,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    client
        .signal_workflow_execution(execution_id, &response)
        .await
        .context("Failed to signal workflow execution")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowExecutionMutationOutput {
                execution_id,
                status: "signalled",
            },
        );
    }

    println!(
        "{}",
        format!("✓ Workflow execution {execution_id} signalled").green()
    );
    Ok(())
}

async fn cancel_workflow_execution(
    execution_id: Uuid,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    client
        .cancel_workflow_execution(execution_id)
        .await
        .context("Failed to cancel workflow execution")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowExecutionMutationOutput {
                execution_id,
                status: "cancelled",
            },
        );
    }

    println!(
        "{}",
        format!("✓ Workflow execution {execution_id} cancelled").green()
    );
    Ok(())
}

async fn remove_workflow_execution(
    execution_id: Uuid,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    client
        .remove_workflow_execution(execution_id)
        .await
        .context("Failed to remove workflow execution")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowExecutionMutationOutput {
                execution_id,
                status: "removed",
            },
        );
    }

    println!(
        "{}",
        format!("✓ Workflow execution {execution_id} removed").green()
    );
    Ok(())
}

/// Delete a workflow from registry
async fn delete_workflow(
    name: String,
    skip_confirmation: bool,
    host: &str,
    port: u16,
    output_format: OutputFormat,
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
    if output_format.is_structured() && !skip_confirmation {
        anyhow::bail!("Use --yes when requesting structured output for `aegis workflow delete`");
    }

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
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &WorkflowDeleteOutput {
                        name,
                        status: "cancelled",
                    },
                );
            }
            println!("{}", "Cancelled.".yellow());
            return Ok(());
        }
    }

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);
    client
        .delete_workflow(&name)
        .await
        .context("Failed to delete workflow")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &WorkflowDeleteOutput {
                name,
                status: "deleted",
            },
        );
    }

    println!("{}", "✓ Workflow deleted successfully!".green().bold());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct WorkflowTestCli {
        #[command(subcommand)]
        command: WorkflowCommand,
    }

    #[test]
    fn deploy_command_parses_force_flag() {
        let cli = WorkflowTestCli::try_parse_from(["workflow", "deploy", "./test.yaml", "--force"])
            .expect("deploy command should parse");

        match cli.command {
            WorkflowCommand::Deploy { file, force, .. } => {
                assert_eq!(file, PathBuf::from("./test.yaml"));
                assert!(force);
            }
            _ => panic!("expected deploy command"),
        }
    }

    #[tokio::test]
    async fn parse_json_or_yaml_input_supports_yaml_files() {
        let path =
            std::env::temp_dir().join(format!("aegis-workflow-input-{}.yaml", Uuid::new_v4()));
        tokio::fs::write(&path, "branch: main\nretries: 2\n")
            .await
            .unwrap();

        let parsed = parse_json_or_yaml_input(&format!("@{}", path.display()))
            .await
            .unwrap();
        assert_eq!(parsed["branch"], serde_json::json!("main"));
        assert_eq!(parsed["retries"], serde_json::json!(2));

        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn parse_optional_object_input_rejects_scalar_values() {
        let err = parse_optional_object_input(Some("hello".to_string()), "workflow blackboard")
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("workflow blackboard must be a JSON/YAML object"));
    }
}
