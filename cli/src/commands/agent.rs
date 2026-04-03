// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Agent
//!
//! Provides agent functionality for the system.
//! Includes list/deploy/show/remove/logs and generate operations.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements agent

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;
use uuid::Uuid;

use crate::commands::builtins;
use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};
use crate::output::{render_serialized, structured_output_unsupported, OutputFormat};

const AGENT_GENERATOR_NAME: &str = builtins::AGENT_GENERATOR_AGENT_NAME;

#[derive(Subcommand)]
pub enum AgentCommand {
    /// List deployed agents
    List,

    /// Deploy an agent from manifest file
    Deploy {
        /// Path to agent manifest YAML file
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,

        /// Validate manifest without deploying
        #[arg(long)]
        validate_only: bool,

        /// Overwrite an existing agent that has the same name and version.
        /// Without this flag the command fails if an agent with the same name
        /// and version is already deployed.
        #[arg(long)]
        force: bool,
    },

    /// Show agent configuration (YAML)
    Show {
        /// Agent ID
        #[arg(value_name = "AGENT_ID")]
        agent_id: Uuid,
    },

    /// Remove a deployed agent
    Remove {
        /// Agent ID
        #[arg(value_name = "AGENT_ID")]
        agent_id: Uuid,
    },

    /// Stream logs for an agent
    Logs {
        /// Agent ID or Name
        #[arg(value_name = "AGENT_ID")]
        agent_id: String,

        /// Follow log output
        #[arg(short, long)]
        follow: bool,

        /// Show errors only
        #[arg(short, long)]
        errors: bool,

        /// Show verbose output (e.g. LLM prompts)
        #[arg(short, long)]
        verbose: bool,
    },

    /// Generate an agent from natural-language input
    Generate {
        /// Natural-language intent for the agent to create
        #[arg(long, short = 'i', value_name = "INPUT")]
        input: String,

        /// Follow generator execution logs
        #[arg(short, long)]
        follow: bool,
    },
}

pub async fn handle_command(
    command: AgentCommand,
    config_path: Option<PathBuf>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    // Agents are currently managed via the daemon.
    // Embedded mode may later support direct repository access.

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
                "Agent management requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let client = DaemonClient::new(host, port)?;

    match command {
        AgentCommand::List => list_agents(client, output_format).await,
        AgentCommand::Deploy {
            manifest,
            validate_only,
            force,
        } => deploy_agent(manifest, validate_only, force, client, output_format).await,
        AgentCommand::Show { agent_id } => show_agent(agent_id, client, output_format).await,
        AgentCommand::Remove { agent_id } => remove_agent(agent_id, client, output_format).await,
        AgentCommand::Logs {
            agent_id,
            follow,
            errors,
            verbose,
        } => {
            if output_format.is_structured() {
                structured_output_unsupported("aegis agent logs", output_format)
            } else {
                logs_agent(agent_id, follow, errors, verbose, client).await
            }
        }
        AgentCommand::Generate { input, follow } => {
            generate_agent(input, follow, client, config_path.as_ref(), output_format).await
        }
    }
}

#[derive(Serialize)]
struct AgentListOutput {
    count: usize,
    agents: Vec<crate::daemon::client::AgentInfo>,
}

#[derive(Serialize)]
struct AgentDeployOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<Uuid>,
    name: String,
    version: String,
    validate_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
}

#[derive(Serialize)]
struct AgentMutationOutput {
    agent_id: Uuid,
    status: &'static str,
}

#[derive(Serialize)]
struct AgentGenerateOutput {
    generator_agent_id: Uuid,
    execution_id: Uuid,
    follow: bool,
    generated_agents_root: String,
}

async fn show_agent(
    agent_id: Uuid,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let manifest = client.get_agent(agent_id).await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &manifest);
    }

    let yaml = serde_yaml::to_string(&manifest).context("Failed to serialize manifest to YAML")?;
    print!("{yaml}");

    Ok(())
}

async fn list_agents(client: DaemonClient, output_format: OutputFormat) -> Result<()> {
    let agents = client.list_agents().await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &AgentListOutput {
                count: agents.len(),
                agents,
            },
        );
    }

    if agents.is_empty() {
        println!("{}", "No agents found".yellow());
        return Ok(());
    }

    println!("{} agents found:", agents.len());
    println!("{:<38} {:<20} {:<10} STATUS", "ID", "NAME", "VERSION");

    for agent in agents {
        println!(
            "{:<38} {:<20} {:<10} {}",
            agent.id,
            agent.name.bold(),
            agent.version,
            agent.status
        );
    }

    Ok(())
}

async fn remove_agent(
    agent_id: Uuid,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    client.delete_agent(agent_id).await?;
    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &AgentMutationOutput {
                agent_id,
                status: "removed",
            },
        );
    }
    println!("{}", format!("✓ Agent {agent_id} removed").green());
    Ok(())
}

async fn deploy_agent(
    manifest: PathBuf,
    validate_only: bool,
    force: bool,
    client: DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let manifest_content = tokio::fs::read_to_string(&manifest)
        .await
        .with_context(|| format!("Failed to read manifest: {manifest:?}"))?;

    // Parse with SDK types (now using core domain re-exports)
    let agent_manifest: aegis_orchestrator_sdk::AgentManifest =
        serde_yaml::from_str(&manifest_content).context("Failed to parse manifest YAML")?;

    // Use domain validation (comprehensive checks including DNS labels, timeouts, etc.)
    agent_manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("Manifest validation failed: {e}"))?;

    if validate_only {
        let runtime = format!(
            "{}:{}",
            agent_manifest
                .spec
                .runtime
                .language
                .as_deref()
                .unwrap_or("unknown"),
            agent_manifest
                .spec
                .runtime
                .version
                .as_deref()
                .unwrap_or("unknown")
        );
        if output_format.is_structured() {
            return render_serialized(
                output_format,
                &AgentDeployOutput {
                    agent_id: None,
                    name: agent_manifest.metadata.name.clone(),
                    version: agent_manifest.metadata.version.clone(),
                    validate_only: true,
                    runtime: Some(runtime.clone()),
                },
            );
        }

        println!(
            "{}",
            format!("✓ Manifest is valid: {}", agent_manifest.metadata.name).green()
        );
        println!("  API Version: {}", agent_manifest.api_version);
        println!("  Kind: {}", agent_manifest.kind);
        println!("  Name: {}", agent_manifest.metadata.name);
        println!("  Version: {}", agent_manifest.metadata.version);
        println!("  Runtime: {runtime}");
        return Ok(());
    }

    if !output_format.is_structured() {
        println!("Deploying agent: {}", agent_manifest.metadata.name.bold());
    }

    let name = agent_manifest.metadata.name.clone();
    let version = agent_manifest.metadata.version.clone();
    let runtime = Some(format!(
        "{}:{}",
        agent_manifest
            .spec
            .runtime
            .language
            .as_deref()
            .unwrap_or("unknown"),
        agent_manifest
            .spec
            .runtime
            .version
            .as_deref()
            .unwrap_or("unknown")
    ));
    let agent_id = client.deploy_agent(agent_manifest, force, None).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &AgentDeployOutput {
                agent_id: Some(agent_id),
                name,
                version,
                validate_only: false,
                runtime,
            },
        );
    }

    println!("{}", format!("✓ Agent deployed: {agent_id}").green());

    Ok(())
}

async fn logs_agent(
    agent_id_str: String,
    follow: bool,
    errors_only: bool,
    verbose: bool,
    client: DaemonClient,
) -> Result<()> {
    // Resolve ID if it's a name
    let agent_id = if let Ok(uuid) = Uuid::parse_str(&agent_id_str) {
        uuid
    } else {
        // Look up by name
        println!(
            "{}",
            format!("Looking up agent '{agent_id_str}'...").dimmed()
        );
        match client.lookup_agent(&agent_id_str).await? {
            Some(id) => id,
            None => {
                anyhow::bail!("Agent '{agent_id_str}' not found");
            }
        }
    };

    println!(
        "{}",
        format!("Streaming logs for agent {agent_id}...").dimmed()
    );
    client
        .stream_agent_logs(agent_id, follow, errors_only, verbose)
        .await?;

    Ok(())
}

async fn generate_agent(
    input: String,
    follow: bool,
    client: DaemonClient,
    config_path: Option<&PathBuf>,
    output_format: OutputFormat,
) -> Result<()> {
    let templates_root = builtins::resolve_templates_root(config_path);
    let generated_root = builtins::resolve_generated_root(config_path).join("agents");
    builtins::sync_generator_templates_to_disk(&templates_root)?;

    // Ensure built-ins are deployed (but don't force overwrite unless it's an update)
    builtins::deploy_all_builtins(&client, false).await?;

    if output_format.is_structured() && follow {
        return structured_output_unsupported("aegis agent generate --follow", output_format);
    }

    let generator_id = client
        .lookup_agent(AGENT_GENERATOR_NAME)
        .await?
        .context("Generator agent not found even after deployment attempt")?;

    if !output_format.is_structured() {
        println!(
            "{}",
            format!("Generating agent via '{AGENT_GENERATOR_NAME}' (id: {generator_id})...").cyan()
        );
    }
    let execution_id = client
        .execute_agent(generator_id, serde_json::Value::String(input), None, None)
        .await
        .context("Failed to start agent generation execution")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &AgentGenerateOutput {
                generator_agent_id: generator_id,
                execution_id,
                follow,
                generated_agents_root: generated_root.display().to_string(),
            },
        );
    }

    println!(
        "{}",
        format!("✓ Agent generation execution started: {execution_id}").green()
    );
    println!("Generated manifests will be persisted under:");
    println!("  {}", generated_root.display());

    if follow {
        client.stream_logs(execution_id, true, false, false).await?;
    } else {
        println!("Follow generator agent logs with:\n  aegis agent logs {execution_id} --follow");
    }

    Ok(())
}
