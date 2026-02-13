// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! # AEGIS Agent Host CLI
//!
//! The `aegis` binary is the Agent Host that enables an Agent Node.
//!
//! ## Architecture
//!
//! This CLI follows a **CLI-first** design with daemon capabilities:
//!
//! - **Default mode**: CLI commands delegate to daemon if running, else embed services
//! - **Daemon mode**: `aegis --daemon` runs as background service
//! - **Detection**: Check PID file + HTTP health check
//!
//! ## Commands
//!
//! - `aegis daemon start|stop|status|install|uninstall` - Manage daemon lifecycle
//! - `aegis task deploy|execute|status|logs` - Agent operations
//! - `aegis config show|validate|generate` - Configuration management
//!
//! See ADR-008 for architecture details.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use std::path::PathBuf;
use tracing::info;

mod commands;
mod daemon;
mod embedded;

use commands::{ConfigCommand, DaemonCommand, TaskCommand, AgentCommand, WorkflowCommand};

/// AEGIS Agent Host - Enable autonomous agent execution
#[derive(Parser)]
#[command(name = "aegis")]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Run as background daemon service
    #[arg(long, global = true)]
    daemon: bool,

    /// Path to configuration file (overrides discovery)
    #[arg(
        short,
        long,
        global = true,
        env = "AEGIS_CONFIG_PATH",
        value_name = "FILE"
    )]
    config: Option<PathBuf>,

    /// HTTP API port (default: 8000)
    #[arg(long, global = true, env = "AEGIS_PORT", default_value = "8000")]
    port: u16,

    /// HTTP API host (default: 127.0.0.1)
    #[arg(long, global = true, env = "AEGIS_HOST", default_value = "127.0.0.1")]
    host: String,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, global = true, env = "AEGIS_LOG_LEVEL", default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage daemon lifecycle
    #[command(name = "daemon")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Agent task operations
    #[command(name = "task")]
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },

    /// Configuration management
    #[command(name = "config")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Agent management
    #[command(name = "agent")]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },

    /// Workflow management
    #[command(name = "workflow")]
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    /// Update AEGIS database
    #[command(name = "update")]
    Update {
        #[command(flatten)]
        command: commands::UpdateCommand,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    init_logging(&cli.log_level)?;

    // Handle daemon mode (background service)
    if cli.daemon {
        info!("Starting AEGIS Agent Host in daemon mode");
        return daemon::start_daemon(cli.config, cli.port).await;
    }

    // Handle commands in CLI mode
    match cli.command {
        Some(Commands::Daemon { command }) => {
            commands::daemon::handle_command(command, cli.config, &cli.host, cli.port).await
        }
        Some(Commands::Task { command }) => {
            commands::task::handle_command(command, cli.config, &cli.host, cli.port).await
        }
        Some(Commands::Config { command }) => {
            commands::config::handle_command(command, cli.config).await
        }
        Some(Commands::Agent { command }) => {
            commands::agent::handle_command(command, cli.config, &cli.host, cli.port).await
        }
        Some(Commands::Workflow { command }) => {
            commands::workflow::handle_command(command, cli.config, &cli.host, cli.port).await
        }
        Some(Commands::Update { command }) => {
            commands::update::execute(command).await
        }
        None => {
            // No command provided - show help
            eprintln!("{}", "No command specified. Use --help for usage.".yellow());
            std::process::exit(1);
        }
    }
}

/// Initialize tracing subscriber for logging
fn init_logging(level: &str) -> Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(level))
        .context("Failed to create log filter")?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact()
        .init();

    Ok(())
}
