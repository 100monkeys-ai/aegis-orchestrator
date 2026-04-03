// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Daemon lifecycle management commands
//!
//! Commands: start, stop, status, install, uninstall
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for daemon

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;
use tracing::{info, warn};

use crate::daemon::{DaemonStatus, check_daemon_running, stop_daemon};
use crate::output::{OutputFormat, render_serialized, structured_output_unsupported};
use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;

#[derive(Subcommand)]
pub enum DaemonCommand {
    /// Start the daemon (if not already running)
    Start,

    /// Stop the daemon gracefully
    Stop {
        /// Force kill if daemon doesn't stop gracefully
        #[arg(short, long)]
        force: bool,

        /// Timeout in seconds (default: 30)
        #[arg(short, long, default_value = "30")]
        timeout: u64,
    },

    /// Check daemon status
    Status,

    /// Install daemon as system service
    Install {
        /// Binary path (default: current executable)
        #[arg(long)]
        binary_path: Option<PathBuf>,

        /// User to run as (Unix only)
        #[arg(long)]
        user: Option<String>,
    },

    /// Uninstall system service
    Uninstall,
}

pub async fn handle_command(
    command: DaemonCommand,
    config_path: Option<PathBuf>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        DaemonCommand::Start => start(config_path, host, port, output_format).await,
        DaemonCommand::Stop { force, timeout } => {
            stop(force, timeout, host, port, output_format).await
        }
        DaemonCommand::Status => status(host, port, output_format).await,
        DaemonCommand::Install { binary_path, user } => {
            if output_format.is_structured() {
                structured_output_unsupported("aegis daemon install", output_format)
            } else {
                install(binary_path, user).await
            }
        }
        DaemonCommand::Uninstall => {
            if output_format.is_structured() {
                structured_output_unsupported("aegis daemon uninstall", output_format)
            } else {
                uninstall().await
            }
        }
    }
}

#[derive(Serialize)]
struct DaemonStartOutput {
    status: &'static str,
    pid: u32,
    stdout_log: String,
    stderr_log: String,
}

#[derive(Serialize)]
struct DaemonStopOutput {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    force: bool,
    timeout_seconds: u64,
}

#[derive(Serialize)]
struct DaemonStatusOutput {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn start(
    config_path: Option<PathBuf>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    // 1. Validation: Load config to check for existence and validity
    // logic in NodeConfigManifest::load_or_default handles explicit path check (errors if missing)
    let config = NodeConfigManifest::load_or_default(config_path.clone())
        .context("Failed to load configuration")?;

    // 2. Warning: Check for empty providers
    if config.spec.llm_providers.is_empty() {
        println!(
            "{}",
            "WARNING: Started with NO LLM providers configured."
                .yellow()
                .bold()
        );
        println!("{}", "         Agents will fail to generate text.".yellow());
        println!("         Please check your config file or use --config <path>.");
    }

    info!("Checking if daemon is already running...");

    match check_daemon_running(host, port).await {
        Ok(DaemonStatus::Running { pid, .. }) => {
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &DaemonStartOutput {
                        status: "already_running",
                        pid,
                        stdout_log: std::env::temp_dir().join("aegis.out").display().to_string(),
                        stderr_log: std::env::temp_dir().join("aegis.err").display().to_string(),
                    },
                );
            }
            println!(
                "{}",
                format!("✓ Daemon already running (PID: {pid})").green()
            );
            println!("Use 'aegis daemon stop' to stop it first.");
            return Ok(());
        }
        Ok(DaemonStatus::Stopped) => {
            info!("Daemon not running, starting...");
        }
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
            warn!(
                "Daemon PID {} exists but unhealthy (error: {}), stopping...",
                pid, error
            );
            stop_daemon(false, 10).await?;
        }
        Err(e) => {
            warn!("Failed to check daemon status: {}", e);
        }
    }

    // Re-exec self with --daemon flag
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;

    let mut cmd = std::process::Command::new(current_exe);
    cmd.arg("--daemon");
    cmd.arg("--host").arg(host);
    cmd.arg("--port").arg(port.to_string());

    if let Some(config) = config_path {
        cmd.arg("--config").arg(config);
    }

    // Spawn detached process
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let temp_dir = std::env::temp_dir();
    let stdout_path = temp_dir.join("aegis.out");
    let stderr_path = temp_dir.join("aegis.err");

    let stdout_file =
        std::fs::File::create(&stdout_path).context("Failed to create stdout log file")?;
    let stderr_file =
        std::fs::File::create(&stderr_path).context("Failed to create stderr log file")?;

    cmd.stdin(std::process::Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);

    let child = cmd.spawn().context("Failed to spawn daemon process")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &DaemonStartOutput {
                status: "starting",
                pid: child.id(),
                stdout_log: stdout_path.display().to_string(),
                stderr_log: stderr_path.display().to_string(),
            },
        );
    }

    println!("Redirecting logs to: {}", stdout_path.display());
    println!(
        "{}",
        format!("✓ Daemon starting (PID: {})", child.id()).green()
    );
    println!("Check status with: aegis daemon status");

    Ok(())
}

async fn stop(
    force: bool,
    timeout: u64,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    info!("Stopping daemon...");

    match check_daemon_running(host, port).await {
        Ok(DaemonStatus::Stopped) => {
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &DaemonStopOutput {
                        status: "not_running",
                        pid: None,
                        force,
                        timeout_seconds: timeout,
                    },
                );
            }
            println!("{}", "ℹ Daemon not running".yellow());
            return Ok(());
        }
        Ok(DaemonStatus::Running { pid, .. }) | Ok(DaemonStatus::Unhealthy { pid, .. }) => {
            if !output_format.is_structured() {
                println!("Stopping daemon (PID: {pid})...");
            }
            stop_daemon(force, timeout).await?;
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &DaemonStopOutput {
                        status: "stopped",
                        pid: Some(pid),
                        force,
                        timeout_seconds: timeout,
                    },
                );
            }
            println!("{}", "✓ Daemon stopped".green());
        }
        Err(e) => {
            println!("{}", format!("✗ Failed to check daemon: {e}").red());
            return Err(e);
        }
    }

    Ok(())
}

async fn status(host: &str, port: u16, output_format: OutputFormat) -> Result<()> {
    match check_daemon_running(host, port).await {
        Ok(DaemonStatus::Running { pid, uptime }) => {
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &DaemonStatusOutput {
                        status: "running",
                        pid: Some(pid),
                        uptime_seconds: uptime,
                        error: None,
                    },
                );
            }
            println!("{}", "✓ Daemon is running".green());
            println!("  PID: {pid}");
            if let Some(uptime) = uptime {
                println!("  Uptime: {}", format_duration(uptime));
            }
        }
        Ok(DaemonStatus::Stopped) => {
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &DaemonStatusOutput {
                        status: "stopped",
                        pid: None,
                        uptime_seconds: None,
                        error: None,
                    },
                );
            }
            println!("{}", "✗ Daemon is not running".red());
        }
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &DaemonStatusOutput {
                        status: "unhealthy",
                        pid: Some(pid),
                        uptime_seconds: None,
                        error: Some(error),
                    },
                );
            }
            println!("{}", format!("⚠ Daemon unhealthy (PID: {pid})").yellow());
            println!("  Process exists but HTTP API check failed: {error}");
            println!("  Check logs at /tmp/aegis.out and /tmp/aegis.err");
        }
        Err(e) => {
            println!("{}", format!("✗ Failed to check status: {e}").red());
            return Err(e);
        }
    }

    Ok(())
}

async fn install(_binary_path: Option<PathBuf>, _user: Option<String>) -> Result<()> {
    #[cfg(unix)]
    {
        crate::daemon::install::install_service(_binary_path, _user).await
    }

    #[cfg(windows)]
    {
        anyhow::bail!("Windows service installation not yet implemented")
    }
}

async fn uninstall() -> Result<()> {
    #[cfg(unix)]
    {
        crate::daemon::install::uninstall_service().await
    }

    #[cfg(windows)]
    {
        anyhow::bail!("Windows service uninstallation not yet implemented")
    }
}

fn format_duration(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;

    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}
