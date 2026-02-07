// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use uuid::Uuid;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};

#[derive(Subcommand)]
pub enum AgentCommand {
    /// List deployed agents
    List,

    /// Deploy an agent from manifest file
    Deploy {
        /// Path to agent manifest YAML file
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
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
    },
}

pub async fn handle_command(
    command: AgentCommand,
    _config_path: Option<PathBuf>,
    port: u16,
) -> Result<()> {
    // Agents are currently only managed via Daemon.
    // Embedded mode might support it through direct repository access, 
    // but for now we'll focus on Daemon interaction as per architecture.
    
    let daemon_status = check_daemon_running().await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {},
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
             println!("{}", format!("⚠ Daemon is running (PID: {}) but unhealthy: {}", pid, error).yellow());
             println!("Run 'aegis daemon status' for more info.");
             return Ok(());
        }
        _ => {
            println!("{}", "Agent management requires the daemon to be running.".red());
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let client = DaemonClient::new(port)?;

    match command {
        AgentCommand::List => list_agents(client).await,
        AgentCommand::Deploy { manifest } => deploy_agent(manifest, client).await,
        AgentCommand::Show { agent_id } => show_agent(agent_id, client).await,
        AgentCommand::Remove { agent_id } => remove_agent(agent_id, client).await,
        AgentCommand::Logs { agent_id, follow, errors } => logs_agent(agent_id, follow, errors, client).await,
    }
}

async fn show_agent(agent_id: Uuid, client: DaemonClient) -> Result<()> {
    let manifest = client.get_agent(agent_id).await?;
    
    // Export as YAML to stdout
    let yaml = manifest.to_yaml_str()?;
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
    println!("{:<38} {:<20} {:<10} {}", "ID", "NAME", "VERSION", "STATUS");
    
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
    println!(
        "{}",
        format!("✓ Agent {} removed", agent_id).green()
    );
    Ok(())
}

async fn deploy_agent(manifest: PathBuf, client: DaemonClient) -> Result<()> {
    let manifest_content = std::fs::read_to_string(&manifest)
        .with_context(|| format!("Failed to read manifest: {:?}", manifest))?;

    let agent_manifest: aegis_sdk::manifest::AgentManifest =
        serde_yaml::from_str(&manifest_content).context("Failed to parse manifest YAML")?;

    println!("Deploying agent: {}", agent_manifest.agent.name.bold());

    let agent_id = client.deploy_agent(agent_manifest).await?;

    println!("{}", format!("✓ Agent deployed: {}", agent_id).green());

    Ok(())
}

async fn logs_agent(
    agent_id_str: String,
    follow: bool,
    errors_only: bool,
    client: DaemonClient,
) -> Result<()> {
    // Resolve ID if it's a name
    let agent_id = if let Ok(uuid) = Uuid::parse_str(&agent_id_str) {
        uuid
    } else {
        // Look up by name
        println!("{}", format!("Looking up agent '{}'...", agent_id_str).dimmed());
        match client.lookup_agent(&agent_id_str).await? {
            Some(id) => id,
            None => {
                anyhow::bail!("Agent '{}' not found", agent_id_str);
            }
        }
    };

    println!("{}", format!("Streaming logs for agent {}...", agent_id).dimmed());
    client.stream_agent_logs(agent_id, follow, errors_only).await?;

    Ok(())
}
