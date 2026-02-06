//! Daemon lifecycle management commands
//!
//! Commands: start, stop, status, install, uninstall

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;
use tracing::{info, warn};

use crate::daemon::{check_daemon_running, stop_daemon, DaemonStatus};

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
    port: u16,
) -> Result<()> {
    match command {
        DaemonCommand::Start => start(config_path, port).await,
        DaemonCommand::Stop { force, timeout } => stop(force, timeout).await,
        DaemonCommand::Status => status().await,
        DaemonCommand::Install { binary_path, user } => install(binary_path, user).await,
        DaemonCommand::Uninstall => uninstall().await,
    }
}

async fn start(config_path: Option<PathBuf>, port: u16) -> Result<()> {
    info!("Checking if daemon is already running...");

    match check_daemon_running().await {
        Ok(DaemonStatus::Running { pid, .. }) => {
            println!("{}", format!("✓ Daemon already running (PID: {})", pid).green());
            println!("Use 'aegis daemon stop' to stop it first.");
            return Ok(());
        }
        Ok(DaemonStatus::Stopped) => {
            info!("Daemon not running, starting...");
        }
        Ok(DaemonStatus::Unhealthy { pid }) => {
            warn!("Daemon PID {} exists but unhealthy, stopping...", pid);
            stop_daemon(false, 10).await?;
        }
        Err(e) => {
            warn!("Failed to check daemon status: {}", e);
        }
    }

    // Re-exec self with --daemon flag
    let current_exe =
        std::env::current_exe().context("Failed to get current executable path")?;

    let mut cmd = std::process::Command::new(current_exe);
    cmd.arg("--daemon");
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

    let child = cmd.spawn().context("Failed to spawn daemon process")?;

    println!(
        "{}",
        format!("✓ Daemon starting (PID: {})", child.id()).green()
    );
    println!("Check status with: aegis daemon status");

    Ok(())
}

async fn stop(force: bool, timeout: u64) -> Result<()> {
    info!("Stopping daemon...");

    match check_daemon_running().await {
        Ok(DaemonStatus::Stopped) => {
            println!("{}", "ℹ Daemon not running".yellow());
            return Ok(());
        }
        Ok(DaemonStatus::Running { pid, .. }) | Ok(DaemonStatus::Unhealthy { pid }) => {
            println!("Stopping daemon (PID: {})...", pid);
            stop_daemon(force, timeout).await?;
            println!("{}", "✓ Daemon stopped".green());
        }
        Err(e) => {
            println!("{}", format!("✗ Failed to check daemon: {}", e).red());
            return Err(e);
        }
    }

    Ok(())
}

async fn status() -> Result<()> {
    match check_daemon_running().await {
        Ok(DaemonStatus::Running { pid, uptime }) => {
            println!("{}", "✓ Daemon is running".green());
            println!("  PID: {}", pid);
            if let Some(uptime) = uptime {
                println!("  Uptime: {}", format_duration(uptime));
            }
        }
        Ok(DaemonStatus::Stopped) => {
            println!("{}", "✗ Daemon is not running".red());
        }
        Ok(DaemonStatus::Unhealthy { pid }) => {
            println!(
                "{}",
                format!("⚠ Daemon unhealthy (PID: {})", pid).yellow()
            );
            println!("  Process exists but HTTP API not responding");
        }
        Err(e) => {
            println!("{}", format!("✗ Failed to check status: {}", e).red());
            return Err(e);
        }
    }

    Ok(())
}

async fn install(binary_path: Option<PathBuf>, user: Option<String>) -> Result<()> {
    #[cfg(unix)]
    {
        crate::daemon::install::install_service(binary_path, user).await
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
        format!("{}d {}h {}m", days, hours, minutes)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}
