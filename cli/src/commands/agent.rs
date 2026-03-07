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
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};

const AGENT_GENERATOR_NAME: &str = "agent-creator-agent";
const AGENT_GENERATOR_TEMPLATE: &str =
    include_str!("../../templates/agents/agent-creator-agent.yaml");
const AGENT_GENERATOR_JUDGE_NAME: &str = "agent-generator-judge";
const AGENT_GENERATOR_JUDGE_TEMPLATE: &str =
    include_str!("../../templates/agents/agent-generator-judge.yaml");
const WORKFLOW_GENERATOR_JUDGE_NAME: &str = "workflow-generator-judge";
const WORKFLOW_GENERATOR_JUDGE_TEMPLATE: &str =
    include_str!("../../templates/agents/workflow-generator-judge.yaml");

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
) -> Result<()> {
    // Agents are currently managed via the daemon.
    // Embedded mode may later support direct repository access.

    let daemon_status = check_daemon_running(host, port).await;
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
                "Agent management requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let client = DaemonClient::new(host, port)?;

    match command {
        AgentCommand::List => list_agents(client).await,
        AgentCommand::Deploy {
            manifest,
            validate_only,
            force,
        } => deploy_agent(manifest, validate_only, force, client).await,
        AgentCommand::Show { agent_id } => show_agent(agent_id, client).await,
        AgentCommand::Remove { agent_id } => remove_agent(agent_id, client).await,
        AgentCommand::Logs {
            agent_id,
            follow,
            errors,
            verbose,
        } => logs_agent(agent_id, follow, errors, verbose, client).await,
        AgentCommand::Generate { input, follow } => {
            generate_agent(input, follow, client, config_path.as_ref()).await
        }
    }
}

async fn show_agent(agent_id: Uuid, client: DaemonClient) -> Result<()> {
    let manifest = client.get_agent(agent_id).await?;

    // Export as YAML to stdout
    let yaml = serde_yaml::to_string(&manifest).context("Failed to serialize manifest to YAML")?;
    println!("{}", yaml);

    Ok(())
}

async fn list_agents(client: DaemonClient) -> Result<()> {
    let agents = client.list_agents().await?;

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

async fn remove_agent(agent_id: Uuid, client: DaemonClient) -> Result<()> {
    client.delete_agent(agent_id).await?;
    println!("{}", format!("✓ Agent {} removed", agent_id).green());
    Ok(())
}

async fn deploy_agent(
    manifest: PathBuf,
    validate_only: bool,
    force: bool,
    client: DaemonClient,
) -> Result<()> {
    let manifest_content = tokio::fs::read_to_string(&manifest)
        .await
        .with_context(|| format!("Failed to read manifest: {:?}", manifest))?;

    // Parse with SDK types (now using core domain re-exports)
    let agent_manifest: aegis_orchestrator_sdk::AgentManifest =
        serde_yaml::from_str(&manifest_content).context("Failed to parse manifest YAML")?;

    // Use domain validation (comprehensive checks including DNS labels, timeouts, etc.)
    agent_manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("Manifest validation failed: {}", e))?;

    if validate_only {
        println!(
            "{}",
            format!("✓ Manifest is valid: {}", agent_manifest.metadata.name).green()
        );
        println!("  API Version: {}", agent_manifest.api_version);
        println!("  Kind: {}", agent_manifest.kind);
        println!("  Name: {}", agent_manifest.metadata.name);
        println!("  Version: {}", agent_manifest.metadata.version);
        println!(
            "  Runtime: {}:{}",
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
        return Ok(());
    }

    println!("Deploying agent: {}", agent_manifest.metadata.name.bold());

    let agent_id = client.deploy_agent(agent_manifest, force).await?;

    println!("{}", format!("✓ Agent deployed: {}", agent_id).green());

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
            format!("Looking up agent '{}'...", agent_id_str).dimmed()
        );
        match client.lookup_agent(&agent_id_str).await? {
            Some(id) => id,
            None => {
                anyhow::bail!("Agent '{}' not found", agent_id_str);
            }
        }
    };

    println!(
        "{}",
        format!("Streaming logs for agent {}...", agent_id).dimmed()
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
) -> Result<()> {
    let templates_root = resolve_templates_root(config_path);
    sync_generator_templates_to_disk(&templates_root)?;

    let _agent_judge_id = ensure_generator_agent_deployed(
        &client,
        AGENT_GENERATOR_JUDGE_NAME,
        AGENT_GENERATOR_JUDGE_TEMPLATE,
    )
    .await?;
    let _workflow_judge_id = ensure_generator_agent_deployed(
        &client,
        WORKFLOW_GENERATOR_JUDGE_NAME,
        WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
    )
    .await?;
    let generator_id =
        ensure_generator_agent_deployed(&client, AGENT_GENERATOR_NAME, AGENT_GENERATOR_TEMPLATE)
            .await?;

    println!(
        "{}",
        format!(
            "Generating agent via '{}' (id: {})...",
            AGENT_GENERATOR_NAME, generator_id
        )
        .cyan()
    );

    let execution_id = client
        .execute_agent(generator_id, serde_json::Value::String(input))
        .await
        .context("Failed to start agent generation execution")?;

    println!(
        "{}",
        format!("✓ Agent generation execution started: {}", execution_id).green()
    );

    if follow {
        client.stream_logs(execution_id, true, false, false).await?;
    } else {
        wait_for_execution_completion(execution_id, &client).await?;
    }

    Ok(())
}

fn resolve_templates_root(config_path: Option<&PathBuf>) -> PathBuf {
    let base_dir = config_path
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .or_else(|| dirs_next::home_dir().map(|h| h.join(".aegis")))
        .unwrap_or_else(|| PathBuf::from(".aegis"));
    base_dir.join("templates")
}

fn sync_generator_templates_to_disk(templates_root: &Path) -> Result<()> {
    persist_template(
        templates_root,
        "agents",
        "agent-creator-agent.yaml",
        AGENT_GENERATOR_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "agent-generator-judge.yaml",
        AGENT_GENERATOR_JUDGE_TEMPLATE,
    )?;
    persist_template(
        templates_root,
        "agents",
        "workflow-generator-judge.yaml",
        WORKFLOW_GENERATOR_JUDGE_TEMPLATE,
    )?;
    Ok(())
}

fn persist_template(
    templates_root: &Path,
    category: &str,
    file_name: &str,
    content: &str,
) -> Result<()> {
    let dir = templates_root.join(category);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create template directory {}", dir.display()))?;
    let file_path = dir.join(file_name);
    std::fs::write(&file_path, content)
        .with_context(|| format!("Failed to write bundled template {}", file_path.display()))?;
    Ok(())
}

async fn ensure_generator_agent_deployed(
    client: &DaemonClient,
    name: &str,
    template_yaml: &str,
) -> Result<Uuid> {
    if let Some(id) = client.lookup_agent(name).await? {
        return Ok(id);
    }

    println!(
        "{}",
        format!(
            "Generator agent '{}' not found. Deploying template...",
            name
        )
        .yellow()
    );

    let manifest: aegis_orchestrator_sdk::AgentManifest =
        serde_yaml::from_str(template_yaml).context("Failed to parse generator template YAML")?;
    manifest
        .validate()
        .map_err(|e| anyhow::anyhow!("Generator template validation failed: {}", e))?;

    client
        .deploy_agent(manifest, false)
        .await
        .context("Failed to deploy generator template")
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
                "Generation execution {} finished with status: {}",
                execution.id, execution.status
            );
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    anyhow::bail!(
        "Timed out waiting for generation execution {} to finish",
        execution_id
    );
}
