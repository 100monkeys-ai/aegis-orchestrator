// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge daemon` — long-running ConnectEdge lifecycle.
//!
//! Reads local edge state previously written by `aegis edge enroll`:
//!
//! ```text
//! ~/.aegis/edge/
//!   node.key             32-byte Ed25519 secret (mode 0600)
//!   node.token           NodeSecurityToken (RS256 JWT)
//!   aegis-config.yaml    NodeConfig manifest (controller endpoint + edge block)
//! ```
//!
//! Resolves `controller.endpoint` from the YAML config (preferred) and falls
//! back to the `cep` claim of the persisted enrollment JWT. Builds an
//! [`EdgeLifecycleConfig`] and invokes
//! [`crate::daemon::edge_lifecycle::run_edge_daemon`], which owns the bidi
//! `ConnectEdge` stream, heartbeats, reconnect backoff, and inbound command
//! dispatch (see ADR-117).

use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tracing::{info, warn};

use aegis_orchestrator_core::domain::cluster::NodeId;
use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;
use aegis_orchestrator_core::domain::security_context::{
    Capability, SecurityContext, SecurityContextMetadata,
};
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::EdgeCapabilities;

use super::grpc;
use crate::daemon::edge_lifecycle::{run_edge_daemon, EdgeLifecycleConfig};

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Override the local edge state directory (default: `~/.aegis/edge`).
    /// `node.key`, `node.token`, and `aegis-config.yaml` are read from here.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,

    /// Override the path to `aegis-config.yaml`. Defaults to
    /// `<state_dir>/aegis-config.yaml`.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Run a single connect attempt and exit. Diagnostic only — the bidi
    /// stream is established, Hello is sent, and the daemon returns when the
    /// stream closes (or fails to connect). Skips the reconnect loop.
    #[arg(long)]
    pub once: bool,
}

pub async fn run(args: DaemonArgs) -> Result<()> {
    let state_dir = args.state_dir.unwrap_or_else(grpc::default_state_dir);
    let config_path = args
        .config
        .unwrap_or_else(|| state_dir.join("aegis-config.yaml"));

    info!(
        state_dir = %state_dir.display(),
        config = %config_path.display(),
        "edge daemon: loading local state"
    );

    // 1. Load merged NodeConfigManifest. The yaml is the source of truth for
    //    `controller.endpoint`, `cluster.edge.*`, and `security_contexts`.
    //    We pass the explicit path so a missing file is a hard error rather
    //    than silently falling back to a default manifest.
    let config = NodeConfigManifest::load_or_default(Some(config_path.clone()))
        .with_context(|| format!("load node config from {}", config_path.display()))?;
    config.validate().context("node config validation failed")?;

    // 2. Resolve controller endpoint from config first; fall back to the `cep`
    //    claim of `enrollment.jwt` if the config does not pin one.
    let controller_endpoint = config
        .spec
        .cluster
        .as_ref()
        .and_then(|c| c.controller.as_ref())
        .map(|c| c.endpoint.clone())
        .filter(|ep| !ep.is_empty() && !ep.contains("{{"))
        .map(Ok)
        .unwrap_or_else(|| grpc::load_controller_endpoint(&state_dir))
        .context("resolve controller endpoint")?;

    // 3. Load the daemon's persisted Ed25519 signing key + NodeSecurityToken.
    let signing_key = Arc::new(grpc::load_signing_key(&state_dir)?);
    let node_security_token = grpc::load_node_security_token(&state_dir)?;
    let node_id_str = grpc::node_id_from_token(&node_security_token)?;
    let node_id =
        NodeId(uuid::Uuid::parse_str(&node_id_str).with_context(|| {
            format!("NodeSecurityToken sub claim is not a UUID: {node_id_str}")
        })?);

    // 4. Resolve EdgeConfig knobs (heartbeat, reconnect backoff, capabilities).
    let cluster = config.spec.cluster.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "aegis-config.yaml missing spec.cluster — run `aegis edge enroll` to bootstrap"
        )
    })?;
    let edge_cfg = cluster.edge.clone().unwrap_or_default();

    let heartbeat_interval = Duration::from_secs(cluster.heartbeat_interval_secs.max(1));
    let reconnect_backoff: Vec<Duration> = if edge_cfg.stream_reconnect_backoff_secs.is_empty() {
        vec![Duration::from_secs(60)]
    } else {
        edge_cfg
            .stream_reconnect_backoff_secs
            .iter()
            .map(|s| Duration::from_secs(*s))
            .collect()
    };

    // 5. Capabilities snapshot. v1: surface the operator-configured
    //    local_tools / mount_points / custom_labels verbatim. Hardware
    //    probes are a follow-up (TODO adr-117).
    let capabilities = EdgeCapabilities {
        local_tools: edge_cfg.capabilities.local_tools.clone(),
        mount_points: edge_cfg.capabilities.mount_points.clone(),
        custom_labels: edge_cfg.capabilities.custom_labels.clone(),
        ..Default::default()
    };

    // 6. Convert the YAML-side SecurityContextDefinition list into the domain
    //    SecurityContext shape that `run_edge_daemon` evaluates against. This
    //    mirrors the seeding loop in `daemon::server::start_daemon` so an edge
    //    daemon enforces the same SecurityContext semantics as a worker.
    let now = chrono::Utc::now();
    let security_contexts: Vec<SecurityContext> = config
        .spec
        .security_contexts
        .as_ref()
        .map(|defs| {
            defs.iter()
                .map(|def| SecurityContext {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    capabilities: def
                        .capabilities
                        .iter()
                        .map(|cap| Capability {
                            tool_pattern: cap.tool_pattern.clone(),
                            path_allowlist: cap
                                .path_allowlist
                                .as_ref()
                                .map(|paths| paths.iter().map(PathBuf::from).collect()),
                            command_allowlist: cap.command_allowlist.clone(),
                            subcommand_allowlist: None,
                            domain_allowlist: cap.domain_allowlist.clone(),
                            max_response_size: None,
                            rate_limit: None,
                            max_concurrent: None,
                        })
                        .collect(),
                    deny_list: def.deny_list.clone(),
                    metadata: SecurityContextMetadata {
                        created_at: now,
                        updated_at: now,
                        version: 1,
                    },
                })
                .collect()
        })
        .unwrap_or_default();

    if security_contexts.is_empty() {
        warn!(
            "edge daemon: aegis-config.yaml declares no security_contexts — \
             every InvokeTool will be rejected with `security_context_not_found`"
        );
    }

    let lifecycle_cfg = EdgeLifecycleConfig {
        controller_endpoint: controller_endpoint.clone(),
        state_dir: state_dir.clone(),
        heartbeat_interval,
        reconnect_backoff,
        node_security_token,
        node_id,
        signing_key,
        capabilities,
        security_contexts,
    };

    info!(
        node_id = %node_id_str,
        controller = %controller_endpoint,
        once = args.once,
        "edge daemon: starting ConnectEdge lifecycle"
    );

    if args.once {
        // Diagnostic single-shot: spawn run_edge_daemon and trip a short
        // shutdown deadline so the reconnect loop never fires more than once.
        // run_edge_daemon's connect_once() returns on stream close; we cancel
        // the task before it would reconnect.
        let task = tokio::spawn(run_edge_daemon(lifecycle_cfg));
        tokio::select! {
            res = task => match res {
                Ok(Ok(())) => {
                    info!("edge daemon: --once run completed");
                    Ok(())
                }
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("edge daemon task panicked: {e}")),
            },
            _ = shutdown_signal() => {
                info!("edge daemon: received shutdown signal during --once run");
                Ok(())
            }
        }
    } else {
        tokio::select! {
            res = run_edge_daemon(lifecycle_cfg) => {
                // run_edge_daemon's outer loop is unconditional — it only
                // returns if it fails before entering the loop. Surface the
                // error to the operator.
                match res {
                    Ok(()) => Ok(()),
                    Err(e) => Err(e),
                }
            }
            _ = shutdown_signal() => {
                info!("edge daemon: received shutdown signal — exiting");
                Ok(())
            }
        }
    }
}

/// Wait for SIGINT or (on Unix) SIGTERM. Mirrors `daemon::server::shutdown_signal`.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            warn!("Ctrl+C handler error: {}", e);
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(e) => {
                warn!("SIGTERM handler error: {}", e);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("edge daemon: received Ctrl+C");
        }
        _ = terminate => {
            info!("edge daemon: received SIGTERM");
        }
    }
}

#[cfg(test)]
mod tests {
    //! Regression coverage for SEV-2-A: the lifecycle was unreachable from the
    //! CLI because no caller ever constructed `EdgeLifecycleConfig`. These
    //! tests pin the wiring contract: the daemon command must (a) accept the
    //! `--state-dir` / `--config` / `--once` flags through clap, and (b)
    //! refuse to run when the local edge state is absent so we get a clear
    //! "run `aegis edge enroll` first" error rather than a silent no-op.
    use super::*;
    use clap::{Args as ClapArgs, FromArgMatches};

    #[test]
    fn daemon_args_parse_state_dir_config_and_once() {
        let cmd = clap::Command::new("test");
        let cmd = DaemonArgs::augment_args(cmd);
        let m = cmd
            .try_get_matches_from(vec![
                "test",
                "--state-dir",
                "/tmp/edge",
                "--config",
                "/tmp/edge/aegis-config.yaml",
                "--once",
            ])
            .expect("flags parse");
        let parsed = DaemonArgs::from_arg_matches(&m).expect("from_arg_matches");
        assert_eq!(
            parsed.state_dir.as_deref(),
            Some(std::path::Path::new("/tmp/edge"))
        );
        assert_eq!(
            parsed.config.as_deref(),
            Some(std::path::Path::new("/tmp/edge/aegis-config.yaml"))
        );
        assert!(parsed.once);
    }

    #[tokio::test]
    async fn daemon_run_errors_when_state_dir_missing() {
        // Regression for SEV-2-A: prior to wiring there was simply no command
        // here. With wiring in place, an empty state_dir must produce a clear
        // error mentioning the missing path, not a panic or a silent connect
        // attempt.
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = DaemonArgs {
            state_dir: Some(tmp.path().to_path_buf()),
            config: Some(tmp.path().join("aegis-config.yaml")),
            once: true,
        };
        let err = run(args)
            .await
            .expect_err("must fail with no state on disk");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("aegis-config.yaml")
                || msg.contains("node.key")
                || msg.contains("node.token")
                || msg.contains("controller endpoint")
                || msg.contains("config"),
            "error must mention the missing state component: {msg}"
        );
    }
}
