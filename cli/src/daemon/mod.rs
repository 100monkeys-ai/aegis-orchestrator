//! Daemon mode implementation
//!
//! Handles:
//! - Daemonization (background process)
//! - PID file management
//! - HTTP health checks
//! - Graceful shutdown

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

pub mod client;
#[cfg(unix)]
pub mod install;
mod server;

pub use client::{DaemonClient, DaemonStatus};
pub use server::start_daemon;

const PID_FILE: &str = "/var/run/aegis.pid";
const PID_FILE_FALLBACK: &str = "/tmp/aegis.pid";

/// Check if daemon is running via PID file + HTTP health check
pub async fn check_daemon_running() -> Result<DaemonStatus> {
    let pid_file = get_pid_file_path();

    // Read PID file
    let pid = match std::fs::read_to_string(&pid_file) {
        Ok(content) => content
            .trim()
            .parse::<u32>()
            .context("Invalid PID in PID file")?,
        Err(_) => return Ok(DaemonStatus::Stopped),
    };

    // Check if process exists
    if !process_exists(pid) {
        // Stale PID file
        let _ = std::fs::remove_file(&pid_file);
        return Ok(DaemonStatus::Stopped);
    }

    // Check HTTP health endpoint
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    match client.get("http://localhost:8080/health").send().await {
        Ok(resp) if resp.status().is_success() => {
            // Parse uptime from response
            let uptime = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v["uptime_seconds"].as_u64());

            Ok(DaemonStatus::Running { pid, uptime })
        }
        Ok(_) | Err(_) => Ok(DaemonStatus::Unhealthy { pid }),
    }
}

/// Stop the daemon gracefully
pub async fn stop_daemon(force: bool, timeout_secs: u64) -> Result<()> {
    let pid_file = get_pid_file_path();

    let pid = std::fs::read_to_string(&pid_file)
        .context("Failed to read PID file")?
        .trim()
        .parse::<u32>()
        .context("Invalid PID")?;

    info!("Sending SIGTERM to process {}", pid);

    #[cfg(unix)]
    {
        send_signal(pid, libc::SIGTERM)?;

        // Wait for graceful shutdown
        for _ in 0..timeout_secs {
            if !process_exists(pid) {
                info!("Daemon stopped gracefully");
                let _ = std::fs::remove_file(&pid_file);
                return Ok(());
            }
            sleep(Duration::from_secs(1)).await;
        }

        if force {
            warn!("Graceful shutdown timeout, sending SIGKILL");
            send_signal(pid, libc::SIGKILL)?;
            sleep(Duration::from_secs(1)).await;
        } else {
            anyhow::bail!("Daemon did not stop within timeout");
        }
    }

    #[cfg(windows)]
    {
        anyhow::bail!("Windows daemon stop not yet implemented");
    }

    let _ = std::fs::remove_file(&pid_file);
    Ok(())
}

fn get_pid_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        let pid_file = Path::new(PID_FILE);
        if pid_file.parent().unwrap().exists() {
            pid_file.to_path_buf()
        } else {
            PathBuf::from(PID_FILE_FALLBACK)
        }
    }

    #[cfg(windows)]
    {
        PathBuf::from("C:\\ProgramData\\aegis\\aegis.pid")
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(windows)]
    {
        // TODO: Implement Windows process check
        true
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) -> Result<()> {
    unsafe {
        if libc::kill(pid as i32, signal) != 0 {
            anyhow::bail!("Failed to send signal {} to process {}", signal, pid);
        }
    }
    Ok(())
}

/// Write PID file
pub fn write_pid_file(pid: u32) -> Result<()> {
    let pid_file = get_pid_file_path();
    std::fs::write(&pid_file, pid.to_string())
        .with_context(|| format!("Failed to write PID file: {:?}", pid_file))?;
    info!("Wrote PID file: {:?}", pid_file);
    Ok(())
}

/// Remove PID file
pub fn remove_pid_file() -> Result<()> {
    let pid_file = get_pid_file_path();
    if pid_file.exists() {
        std::fs::remove_file(&pid_file)
            .with_context(|| format!("Failed to remove PID file: {:?}", pid_file))?;
        info!("Removed PID file: {:?}", pid_file);
    }
    Ok(())
}
