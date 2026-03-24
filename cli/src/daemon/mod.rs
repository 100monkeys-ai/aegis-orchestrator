// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Daemon mode implementation
//!
//! Handles:
//! - Daemonization (background process)
//! - PID file management
//! - HTTP health checks
//! - Graceful shutdown
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for mod

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
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
pub mod operator_read_models;
pub mod server;

pub use client::DaemonClient;
pub use server::start_daemon;

#[derive(Debug, Clone)]
pub enum DaemonStatus {
    Running { pid: u32, uptime: Option<u64> },
    Stopped,
    Unhealthy { pid: u32, error: String },
}

#[derive(Debug, Clone)]
pub enum HealthEndpointStatus {
    Healthy { uptime: Option<u64> },
    Unhealthy { error: String },
}

/// Check if daemon is running via HTTP health check (primary) or PID file (secondary)
pub async fn check_daemon_running(host: &str, port: u16) -> Result<DaemonStatus> {
    // We check the PID file primarily to return the PID in the Running status if available locally.
    // If not available (remote), we return 0 or another indicator.
    let pid_file = get_pid_file_path();
    let local_pid = match tokio::fs::read_to_string(&pid_file).await {
        Ok(content) => content.trim().parse::<u32>().ok(),
        Err(_) => None,
    };

    match probe_health_endpoint(host, port).await {
        Ok(HealthEndpointStatus::Healthy { uptime }) => {
            let pid = local_pid.unwrap_or(0);
            Ok(DaemonStatus::Running { pid, uptime })
        }
        Ok(HealthEndpointStatus::Unhealthy { error }) => {
            if let Some(pid) = local_pid {
                Ok(DaemonStatus::Unhealthy { pid, error })
            } else {
                Ok(DaemonStatus::Unhealthy { pid: 0, error })
            }
        }
        Err(e) => {
            // HTTP failed. Check if local PID exists to determine if it SHOULD be running.
            if let Some(pid) = local_pid {
                if process_exists(pid) {
                    // PID exists, Process exists, but HTTP failed -> Unhealthy
                    Ok(DaemonStatus::Unhealthy {
                        pid,
                        error: e.to_string(),
                    })
                } else {
                    // Stale PID file
                    let _ = tokio::fs::remove_file(&pid_file).await;
                    Ok(DaemonStatus::Stopped)
                }
            } else {
                // No PID file, HTTP failed -> Stopped
                Ok(DaemonStatus::Stopped)
            }
        }
    }
}

pub async fn probe_health_endpoint(host: &str, port: u16) -> Result<HealthEndpointStatus> {
    let client = Client::builder()
        .timeout(Duration::from_millis(500))
        .build()?;

    let health_url = format!("{}/health", build_base_url(host, port));

    match client.get(&health_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let uptime = resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v["uptime_seconds"].as_u64());
            Ok(HealthEndpointStatus::Healthy { uptime })
        }
        Ok(resp) => Ok(HealthEndpointStatus::Unhealthy {
            error: format!("HTTP {}", resp.status()),
        }),
        Err(e) => Err(e.into()),
    }
}

fn build_base_url(host: &str, port: u16) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        format!("{host}:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Stop the daemon gracefully
pub async fn stop_daemon(_force: bool, _timeout_secs: u64) -> Result<()> {
    let pid_file = get_pid_file_path();

    let pid = tokio::fs::read_to_string(&pid_file)
        .await
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
                let _ = tokio::fs::remove_file(&pid_file).await;
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
        let output = tokio::process::Command::new("taskkill")
            .args(&["/PID", &pid.to_string(), "/F"])
            .output()
            .await
            .context("Failed to execute taskkill")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.contains("not found") {
                // Ignore if already gone
                anyhow::bail!("Failed to stop daemon: {}", stderr);
            }
        }
        info!("Daemon stopped (killed via taskkill)");
    }

    let _ = tokio::fs::remove_file(&pid_file).await;
    Ok(())
}

fn get_pid_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        // SAFETY: `geteuid()` is always safe to call — it has no preconditions,
        // never fails, and cannot cause undefined behaviour. A safe wrapper
        // (e.g. `nix::unistd::geteuid()`) could be used instead if the `nix`
        // crate is added as a dependency.
        let uid = unsafe { libc::geteuid() };

        if let Some(explicit_file) = std::env::var_os("AEGIS_PID_FILE") {
            return PathBuf::from(explicit_file);
        }

        let base_dir = if uid == 0 {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
        } else {
            std::env::temp_dir()
        };

        base_dir.join("aegis").join("aegis.pid")
    }

    #[cfg(windows)]
    {
        if let Some(explicit_file) = std::env::var_os("AEGIS_PID_FILE") {
            return PathBuf::from(explicit_file);
        }

        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("aegis").join("aegis.pid")
    }
}

fn process_exists(_pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `kill(pid, 0)` sends no signal and is the POSIX-standard way
        // to test whether a process exists. The only precondition is that `pid`
        // fits in `i32`; we guard against overflow with `try_from`.
        let Ok(pid_i32) = i32::try_from(_pid) else {
            return false;
        };
        unsafe { libc::kill(pid_i32, 0) == 0 }
    }

    #[cfg(windows)]
    {
        // Windows implementation can be added when daemon process management is required.
        true
    }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) -> Result<()> {
    let pid_i32 = i32::try_from(pid)
        .map_err(|_| anyhow::anyhow!("PID {pid} overflows i32; cannot send signal"))?;
    // SAFETY: `libc::kill` is safe when `pid_i32` is a valid process ID (which
    // we have just verified fits in `i32`) and `signal` is a valid signal
    // number supplied by the caller from named `libc::SIG*` constants.
    let rc = unsafe { libc::kill(pid_i32, signal) };
    if rc != 0 {
        anyhow::bail!("Failed to send signal {signal} to process {pid}");
    }
    Ok(())
}

/// Write PID file
pub fn write_pid_file(pid: u32) -> Result<()> {
    let pid_file = get_pid_file_path();
    if let Some(parent) = pid_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create PID directory: {parent:?}"))?;
    }
    std::fs::write(&pid_file, pid.to_string())
        .with_context(|| format!("Failed to write PID file: {pid_file:?}"))?;
    info!("Wrote PID file: {:?}", pid_file);
    Ok(())
}

/// Remove PID file
pub fn remove_pid_file() -> Result<()> {
    let pid_file = get_pid_file_path();
    if pid_file.exists() {
        std::fs::remove_file(&pid_file)
            .with_context(|| format!("Failed to remove PID file: {pid_file:?}"))?;
        info!("Removed PID file: {:?}", pid_file);
    }
    Ok(())
}
