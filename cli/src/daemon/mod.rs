// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Daemon mode implementation
//!
//! Handles:
//! - Daemonization (background process)
//! - PID file management
//! - HTTP health checks
//! - Graceful shutdown

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
#[cfg(unix)]
use tokio::time::sleep;
use tracing::info;
#[cfg(unix)]
use tracing::warn;

pub mod client;
#[cfg(unix)]
pub mod install;
pub mod server;

pub use client::DaemonClient;
pub use server::start_daemon;

#[cfg(unix)]
const PID_FILE: &str = "/var/run/aegis/aegis.pid";
#[cfg(unix)]
const PID_FILE_FALLBACK: &str = "/tmp/aegis.pid";

#[derive(Debug, Clone)]
pub enum DaemonStatus {
    Running { pid: u32, uptime: Option<u64> },
    Stopped,
    Unhealthy { pid: u32, error: String },
}

/// Check if daemon is running via HTTP health check (primary) or PID file (secondary)
pub async fn check_daemon_running(host: &str, port: u16) -> Result<DaemonStatus> {
    // 1. Try HTTP health check first (works for local and remote/forwarded ports)
    // Use 127.0.0.1 to avoid ipv4/ipv6 ambiguity with localhost if host is explicitly localhost
    // otherwise use provided host.
    
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500)) // Fast timeout for local checks
        .build()?;

    let base_url = if host.starts_with("http://") || host.starts_with("https://") {
        format!("{}:{}", host, port)
    } else {
        format!("http://{}:{}", host, port)
    };
    
    let health_url = format!("{}/health", base_url);
    
    // We check the PID file primarily to return the PID in the Running status if available locally.
    // If not available (remote), we return 0 or another indicator.
    let pid_file = get_pid_file_path();
    let local_pid = match std::fs::read_to_string(&pid_file) {
        Ok(content) => content.trim().parse::<u32>().ok(),
        Err(_) => None,
    };

    match client.get(&health_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            // Parse uptime from response
            let uptime = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|v| v["uptime_seconds"].as_u64());

            // If we have a local PID, use it. Otherwise, we might be remote.
            // For now, if local PID is missing but HTTP works, we assume "Running" but maybe without a known PID?
            // Or we just don't return a PID in that case (DaemonStatus might need update or just use 0).
            // Existing `DaemonStatus::Running` requires `pid: u32`. Let's use 0 if unknown/remote.
            let pid = local_pid.unwrap_or(0);
            
            return Ok(DaemonStatus::Running { pid, uptime });
        }
        Ok(resp) => {
            // HTTP reached but returned error
            if let Some(pid) = local_pid {
                 return Ok(DaemonStatus::Unhealthy { 
                    pid, 
                    error: format!("HTTP {}", resp.status()) 
                });
            }
            // If no PID file and unhealthy HTTP, it's ambiguous but likely "Running but broken" or incompatible version?
            // Let's treat as Unhealthy with PID 0
            return Ok(DaemonStatus::Unhealthy { 
                pid: 0, 
                error: format!("HTTP {}", resp.status()) 
            });
        }
        Err(e) => {
             // HTTP failed. Check if local PID exists to determine if it SHOULD be running.
             if let Some(pid) = local_pid {
                 if process_exists(pid) {
                      // PID exists, Process exists, but HTTP failed -> Unhealthy
                      return Ok(DaemonStatus::Unhealthy { 
                        pid, 
                        error: e.to_string() 
                    });
                 } else {
                     // Stale PID file
                     let _ = std::fs::remove_file(&pid_file);
                     return Ok(DaemonStatus::Stopped);
                 }
             }
             
             // No PID file, HTTP failed -> Stopped
             return Ok(DaemonStatus::Stopped);
        }
    }
}

/// Stop the daemon gracefully
pub async fn stop_daemon(_force: bool, _timeout_secs: u64) -> Result<()> {
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
        for _ in 0.._timeout_secs {
            if !process_exists(pid) {
                info!("Daemon stopped gracefully");
                let _ = std::fs::remove_file(&pid_file);
                return Ok(());
            }
            sleep(Duration::from_secs(1)).await;
        }

        if _force {
            warn!("Graceful shutdown timeout, sending SIGKILL");
            send_signal(pid, libc::SIGKILL)?;
            sleep(Duration::from_secs(1)).await;
        } else {
            anyhow::bail!("Daemon did not stop within timeout");
        }
    }

    #[cfg(windows)]
    {
        // Use taskkill to kill the process by PID
        let output = std::process::Command::new("taskkill")
            .args(&["/PID", &pid.to_string(), "/F"])
            .output()
            .context("Failed to execute taskkill")?;

        if !output.status.success() {
             let stderr = String::from_utf8_lossy(&output.stderr);
             if !stderr.contains("not found") { // Ignore if already gone
                 anyhow::bail!("Failed to stop daemon: {}", stderr);
             }
        }
        info!("Daemon stopped (killed via taskkill)");
    }

    let _ = std::fs::remove_file(&pid_file);
    Ok(())
}

fn get_pid_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        let uid = unsafe { libc::geteuid() };
        if uid == 0 {
            PathBuf::from(PID_FILE)
        } else {
            PathBuf::from(PID_FILE_FALLBACK)
        }
    }

    #[cfg(windows)]
    {
        PathBuf::from("C:\\ProgramData\\aegis\\aegis.pid")
    }
}

fn process_exists(_pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(_pid as i32, 0) == 0 }
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
