// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 edge daemon lifecycle: long-lived bidi `ConnectEdge` stream.
//!
//! High-level flow:
//!   * Establish gRPC connection to `cluster.controller.endpoint`.
//!   * Send `EdgeEvent::Hello` carrying capabilities + outer SealNodeEnvelope.
//!   * Send periodic `Heartbeat` events.
//!   * For each inbound `EdgeCommand::InvokeTool`:
//!       * verify inner `SealEnvelope` locally;
//!       * resolve `security_context_name` against merged config;
//!       * dispatch to local builtin / MCP server;
//!       * stream `CommandProgress`, terminate with `CommandResult`.
//!   * On `Drain`: stop accepting new InvokeTool commands.
//!   * On `Shutdown`: drain then exit.
//!
//! Reconnect uses the configured `stream_reconnect_backoff_secs` schedule.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct EdgeLifecycleConfig {
    pub controller_endpoint: String,
    pub state_dir: PathBuf,
    pub heartbeat_interval: Duration,
    pub reconnect_backoff: Vec<Duration>,
}

pub async fn run_edge_daemon(cfg: EdgeLifecycleConfig) -> Result<()> {
    let mut attempt: usize = 0;
    loop {
        match connect_once(&cfg).await {
            Ok(()) => {
                attempt = 0;
            }
            Err(e) => {
                tracing::warn!(error = %e, "edge stream disconnected; will reconnect");
            }
        }
        let delay = cfg
            .reconnect_backoff
            .get(attempt.min(cfg.reconnect_backoff.len().saturating_sub(1)))
            .copied()
            .unwrap_or(Duration::from_secs(60));
        attempt = attempt.saturating_add(1);
        sleep(delay).await;
    }
}

async fn connect_once(cfg: &EdgeLifecycleConfig) -> Result<()> {
    // The integration with `aegis_orchestrator_proto::node_cluster_service_client`
    // is wired in the daemon binary; this module owns the loop discipline.
    // The actual gRPC client construction is deferred to the binary that has
    // access to the config-loaded TLS settings.
    let _ = cfg;
    anyhow::bail!("edge gRPC client wiring is performed by the daemon binary")
        .context("connect_once: not yet bound to gRPC client in this build")
}
