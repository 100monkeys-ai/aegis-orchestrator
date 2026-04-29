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

    let heartbeat_interval = validate_heartbeat_interval(cluster.heartbeat_interval_secs)?;
    let reconnect_backoff: Vec<Duration> =
        validate_reconnect_backoff(&edge_cfg.stream_reconnect_backoff_secs)?;

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
                res
            }
            _ = shutdown_signal() => {
                info!("edge daemon: received shutdown signal — exiting");
                Ok(())
            }
        }
    }
}

/// Lower bound (inclusive) for `spec.cluster.heartbeat_interval_secs`.
const MIN_HEARTBEAT_INTERVAL_SECS: u64 = 1;
/// Upper bound (inclusive) for `spec.cluster.heartbeat_interval_secs`. Guards
/// against an `u64::MAX` YAML value timing-overflowing the heartbeat ticker.
const MAX_HEARTBEAT_INTERVAL_SECS: u64 = 3600;
/// Inclusive range for each `spec.cluster.edge.stream_reconnect_backoff_secs[i]`.
const RECONNECT_BACKOFF_MIN_SECS: u64 = 1;
const RECONNECT_BACKOFF_MAX_SECS: u64 = 3600;

/// Validate the configured heartbeat interval and convert to a `Duration`.
/// Errors with a typed "exceeds maximum" message when over `MAX_HEARTBEAT_INTERVAL_SECS`;
/// silently clamps zero up to `MIN_HEARTBEAT_INTERVAL_SECS`.
fn validate_heartbeat_interval(raw: u64) -> Result<Duration> {
    if raw > MAX_HEARTBEAT_INTERVAL_SECS {
        return Err(anyhow::anyhow!(
            "aegis-config.yaml spec.cluster.heartbeat_interval_secs ({raw}) exceeds maximum allowed value ({MAX_HEARTBEAT_INTERVAL_SECS})"
        ));
    }
    Ok(Duration::from_secs(raw.max(MIN_HEARTBEAT_INTERVAL_SECS)))
}

/// Validate each entry of `stream_reconnect_backoff_secs` is in 1..=3600 and
/// convert to `Duration`. An empty input falls back to a single 60s backoff.
fn validate_reconnect_backoff(raw: &[u64]) -> Result<Vec<Duration>> {
    if raw.is_empty() {
        return Ok(vec![Duration::from_secs(60)]);
    }
    raw.iter()
        .enumerate()
        .map(|(idx, s)| {
            if !(RECONNECT_BACKOFF_MIN_SECS..=RECONNECT_BACKOFF_MAX_SECS).contains(s) {
                return Err(anyhow::anyhow!(
                    "invalid spec.cluster.edge.stream_reconnect_backoff_secs[{idx}]={s}; expected {RECONNECT_BACKOFF_MIN_SECS}..={RECONNECT_BACKOFF_MAX_SECS} seconds"
                ));
            }
            Ok(Duration::from_secs(*s))
        })
        .collect()
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

    #[test]
    fn heartbeat_interval_rejects_oversized_value() {
        // Regression: prior code only guarded the lower bound with `.max(1)`.
        // An `u64::MAX` from YAML compiled fine but timing-overflowed
        // downstream. The fix returns a typed "exceeds maximum" error.
        let err = validate_heartbeat_interval(u64::MAX).expect_err("u64::MAX must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exceeds maximum") && msg.contains("3600"),
            "error must mention the bound: {msg}"
        );
        // Boundary: MAX + 1 is rejected, MAX is accepted.
        validate_heartbeat_interval(MAX_HEARTBEAT_INTERVAL_SECS).expect("3600 accepted");
        validate_heartbeat_interval(MAX_HEARTBEAT_INTERVAL_SECS + 1).expect_err("3601 rejected");
        // Zero is silently clamped up to MIN.
        let dur = validate_heartbeat_interval(0).expect("0 clamps to MIN");
        assert_eq!(dur, Duration::from_secs(MIN_HEARTBEAT_INTERVAL_SECS));
    }

    #[test]
    fn reconnect_backoff_rejects_zero_with_index_zero() {
        let err = validate_reconnect_backoff(&[0]).expect_err("zero entry rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stream_reconnect_backoff_secs[0]=0"),
            "error must surface the zero-indexed bad entry: {msg}"
        );
    }

    #[test]
    fn reconnect_backoff_rejects_oversized_with_index_zero() {
        let err = validate_reconnect_backoff(&[3601]).expect_err("3601 rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stream_reconnect_backoff_secs[0]=3601"),
            "error must surface the bad entry: {msg}"
        );
    }

    #[test]
    fn reconnect_backoff_reports_correct_index_in_multi_element_list() {
        // Regression: the prior `.iter().map(...).collect()` accepted any u64
        // including 0. The new validator must surface the *index* of the bad
        // entry (idx=1 here) so operators can find it in long YAML lists.
        let err =
            validate_reconnect_backoff(&[1, 4000, 5]).expect_err("idx=1 out of range rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stream_reconnect_backoff_secs[1]=4000"),
            "error must surface the correct index (1): {msg}"
        );
    }

    #[test]
    fn reconnect_backoff_accepts_valid_list_and_falls_back_when_empty() {
        let v = validate_reconnect_backoff(&[1, 5, 60, 3600]).expect("valid list accepted");
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], Duration::from_secs(1));
        assert_eq!(v[3], Duration::from_secs(3600));
        let fallback = validate_reconnect_backoff(&[]).expect("empty list falls back");
        assert_eq!(fallback, vec![Duration::from_secs(60)]);
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
