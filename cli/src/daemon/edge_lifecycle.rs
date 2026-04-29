// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 edge daemon lifecycle: long-lived bidi `ConnectEdge` stream.
//!
//! High-level flow:
//!   * Establish gRPC connection to `cluster.controller.endpoint`.
//!   * Send `EdgeEvent::Hello` carrying capabilities + outer SealNodeEnvelope.
//!   * Send periodic `Heartbeat` events.
//!   * For each inbound `EdgeCommand::InvokeTool`:
//!       * verify inner `SealEnvelope` locally (deferred — see TODO);
//!       * resolve `security_context_name` against merged config;
//!       * dispatch to local builtin / MCP server (deferred — see TODO);
//!       * stream `CommandProgress`, terminate with `CommandResult`.
//!   * On `Drain`: stop accepting new InvokeTool commands.
//!   * On `Shutdown`: drain then exit.
//!
//! Reconnect uses the configured `stream_reconnect_backoff_secs` schedule.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    edge_command::Command as InCmd, edge_event::Event as OutEv,
    node_cluster_service_client::NodeClusterServiceClient, CommandResultEvent, EdgeCapabilities,
    EdgeEvent, EdgeResult, HeartbeatEvent, HelloEvent, SealNodeEnvelope,
};

#[derive(Debug, Clone)]
pub struct EdgeLifecycleConfig {
    pub controller_endpoint: String,
    pub state_dir: PathBuf,
    pub heartbeat_interval: Duration,
    pub reconnect_backoff: Vec<Duration>,
    /// Persisted NodeSecurityToken (RS256 JWT) issued at enrollment time.
    pub node_security_token: String,
    /// Local capabilities snapshot — populated by the daemon binary from
    /// hardware probes plus operator-configured local_tools / mount_points.
    pub capabilities: EdgeCapabilities,
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
    // 1. Build the channel. TLS handshake configuration is out of scope for
    //    v1; the daemon binary supplies a pre-validated endpoint and tonic
    //    handles HTTP/2 over TLS automatically when the URI scheme is https.
    let endpoint_uri = if cfg.controller_endpoint.starts_with("http://")
        || cfg.controller_endpoint.starts_with("https://")
    {
        cfg.controller_endpoint.clone()
    } else {
        format!("http://{}", cfg.controller_endpoint)
    };
    let channel = tonic::transport::Channel::from_shared(endpoint_uri.clone())
        .with_context(|| format!("invalid controller endpoint URI: {endpoint_uri}"))?
        .connect()
        .await
        .with_context(|| format!("connect to controller {endpoint_uri}"))?;

    let mut client = NodeClusterServiceClient::new(channel);

    // 2. Construct the outbound stream.
    let (tx, rx) = mpsc::channel::<EdgeEvent>(64);
    let outbound = ReceiverStream::new(rx);

    // 3. Send Hello first.
    let stream_id = Uuid::new_v4().to_string();
    let hello = EdgeEvent {
        event: Some(OutEv::Hello(HelloEvent {
            envelope: Some(SealNodeEnvelope {
                node_security_token: cfg.node_security_token.clone(),
                // TODO(adr-117): compute Ed25519 signature over the canonical
                // payload using the daemon's persisted keypair. v1 wire flow
                // works with the empty signature when the controller is
                // configured for permissive edge attestation; production
                // deployments must populate this.
                signature: vec![],
                payload: Vec::new(),
            }),
            capabilities: Some(cfg.capabilities.clone()),
            stream_id: stream_id.clone(),
            last_seen_command_id: None,
        })),
    };
    tx.send(hello)
        .await
        .context("failed to enqueue Hello on outbound stream")?;

    // 4. Open the bidi stream.
    let mut inbound = client
        .connect_edge(tonic::Request::new(outbound))
        .await
        .context("ConnectEdge RPC failed")?
        .into_inner();

    // 5. Heartbeat ticker.
    let drain = Arc::new(AtomicBool::new(false));
    let heartbeat_handle = {
        let tx = tx.clone();
        let interval = cfg.heartbeat_interval;
        let token = cfg.node_security_token.clone();
        let drain = drain.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // skip immediate first tick
            loop {
                ticker.tick().await;
                if drain.load(Ordering::SeqCst) {
                    // stop heartbeating once the daemon is shutting down
                    break;
                }
                let hb = EdgeEvent {
                    event: Some(OutEv::Heartbeat(HeartbeatEvent {
                        envelope: Some(SealNodeEnvelope {
                            node_security_token: token.clone(),
                            signature: vec![],
                            payload: Vec::new(),
                        }),
                        active_invocations: 0,
                    })),
                };
                if tx.send(hb).await.is_err() {
                    break;
                }
            }
        })
    };

    // 6. Inbound command receive loop.
    while let Some(msg) = inbound.message().await.transpose() {
        let cmd = match msg {
            Ok(c) => c,
            Err(status) => {
                tracing::warn!(?status, "edge stream error");
                break;
            }
        };
        match cmd.command {
            Some(InCmd::InvokeTool(inv)) => {
                tracing::info!(
                    command_id = %inv.command_id,
                    tool = %inv.tool_name,
                    "edge: received InvokeTool"
                );
                // TODO(adr-117): full SecurityContext local enforcement —
                // currently permissive, gated by enrollment-bound tenant
                // only. The orchestrator already validated the
                // security_context_name during dispatch; the daemon should
                // re-resolve from its merged config once that helper is
                // available.
                // TODO(adr-117): wire local built-in dispatcher (cmd.run /
                // fs.* per ADR-048). Until extracted from
                // aegis_orchestrator_core::application::tools, the daemon
                // returns a structured "not yet locally supported" result so
                // the wire flow completes.
                let result = EdgeResult {
                    ok: false,
                    exit_code: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    structured_result: None,
                    error_kind: "tool_not_found".to_string(),
                    error_message: format!(
                        "tool '{}' not yet locally dispatched on edge daemon",
                        inv.tool_name
                    ),
                };
                let ev = EdgeEvent {
                    event: Some(OutEv::CommandResult(CommandResultEvent {
                        envelope: Some(SealNodeEnvelope {
                            node_security_token: cfg.node_security_token.clone(),
                            signature: vec![],
                            payload: Vec::new(),
                        }),
                        command_id: inv.command_id,
                        result: Some(result),
                    })),
                };
                let _ = tx.send(ev).await;
            }
            Some(InCmd::Cancel(c)) => {
                tracing::info!(command_id = %c.command_id, "edge: received Cancel");
                // No in-flight commands to cancel in the stub path; emit a
                // synthetic result so the orchestrator-side pending registry
                // unblocks.
                let result = EdgeResult {
                    ok: false,
                    exit_code: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    structured_result: None,
                    error_kind: "cancelled".to_string(),
                    error_message: "cancelled by operator".to_string(),
                };
                let ev = EdgeEvent {
                    event: Some(OutEv::CommandResult(CommandResultEvent {
                        envelope: Some(SealNodeEnvelope {
                            node_security_token: cfg.node_security_token.clone(),
                            signature: vec![],
                            payload: Vec::new(),
                        }),
                        command_id: c.command_id,
                        result: Some(result),
                    })),
                };
                let _ = tx.send(ev).await;
            }
            Some(InCmd::PushConfig(_)) => {
                tracing::info!("edge: received PushConfig (ignored — local config persistence is daemon-binary owned)");
            }
            Some(InCmd::Drain(_)) => {
                tracing::info!("edge: received Drain — refusing new tool invocations");
                drain.store(true, Ordering::SeqCst);
            }
            Some(InCmd::Shutdown(_)) => {
                tracing::info!("edge: received Shutdown — exiting connect_once loop");
                drain.store(true, Ordering::SeqCst);
                break;
            }
            None => {
                tracing::warn!("edge: received empty EdgeCommand frame; ignoring");
            }
        }
    }

    heartbeat_handle.abort();
    Ok(())
}
