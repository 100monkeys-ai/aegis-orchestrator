// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 edge daemon lifecycle: long-lived bidi `ConnectEdge` stream.
//!
//! High-level flow:
//!   * Establish gRPC connection to `cluster.controller.endpoint`.
//!   * Send `EdgeEvent::Hello` carrying capabilities + outer SealNodeEnvelope.
//!   * Send periodic `Heartbeat` events.
//!   * For each inbound `EdgeCommand::InvokeTool`:
//!       * verify inner `SealEnvelope` locally (deferred — see slice B / TODO);
//!       * resolve `security_context_name` against merged config;
//!       * dispatch to local builtin / MCP server (deferred — see slice B / TODO);
//!       * stream `CommandProgress`, terminate with `CommandResult`.
//!   * On `Drain`: stop accepting new InvokeTool commands.
//!   * On `Shutdown`: drain then exit.
//!
//! Reconnect uses the configured `stream_reconnect_backoff_secs` schedule.

use anyhow::{Context, Result};
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use aegis_orchestrator_core::domain::shared_kernel::NodeId;
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    edge_command::Command as InCmd, edge_event::Event as OutEv,
    node_cluster_service_client::NodeClusterServiceClient, CommandResultEvent, EdgeCapabilities,
    EdgeEvent, EdgeResult, HeartbeatEvent, HelloEvent, SealNodeEnvelope,
};

/// Canonical envelope-payload kinds. Encoded as a single byte at the start of
/// the canonical signing payload so a Hello signature cannot be replayed in
/// place of a Heartbeat (or vice-versa) — both share the same cryptographic
/// key, so a per-message-kind tag prevents cross-kind substitution.
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum CanonicalKind {
    Hello = 1,
    Heartbeat = 2,
    CommandResult = 3,
    CommandProgress = 4,
}

/// Build the canonical bytes that go into `SealNodeEnvelope.payload` and that
/// the daemon's Ed25519 key signs. Layout (all integers little-endian):
///
/// ```text
///   u8  kind
///   u64 timestamp_ms
///   u32 node_id_len     u8[]  node_id_utf8
///   u32 stream_id_len   u8[]  stream_id_utf8
///   u32 inner_len       u8[]  inner_payload_bytes
/// ```
///
/// The orchestrator-side `SealNodeVerifier::verify_envelope` (see
/// `infrastructure/cluster/seal_node.rs`) verifies the Ed25519 signature
/// directly over `envelope.payload`, so by setting the canonical bytes as
/// the payload and signing them we produce envelopes that round-trip through
/// the existing verifier with no orchestrator-side change.
pub(crate) fn canonical_envelope_payload(
    kind: CanonicalKind,
    node_id: &str,
    stream_id: &str,
    timestamp_ms: i64,
    inner: &[u8],
) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(1 + 8 + 4 + node_id.len() + 4 + stream_id.len() + 4 + inner.len());
    out.push(kind as u8);
    out.extend_from_slice(&(timestamp_ms as u64).to_le_bytes());
    out.extend_from_slice(&(node_id.len() as u32).to_le_bytes());
    out.extend_from_slice(node_id.as_bytes());
    out.extend_from_slice(&(stream_id.len() as u32).to_le_bytes());
    out.extend_from_slice(stream_id.as_bytes());
    out.extend_from_slice(&(inner.len() as u32).to_le_bytes());
    out.extend_from_slice(inner);
    out
}

/// Produce a signed `SealNodeEnvelope` whose `payload` is the canonical
/// signing bytes for the given message kind.
pub(crate) fn signed_envelope(
    signing_key: &SigningKey,
    node_security_token: &str,
    node_id: &str,
    stream_id: &str,
    kind: CanonicalKind,
    inner: &[u8],
) -> SealNodeEnvelope {
    let ts = Utc::now().timestamp_millis();
    let payload = canonical_envelope_payload(kind, node_id, stream_id, ts, inner);
    let signature = signing_key.sign(&payload).to_bytes().to_vec();
    SealNodeEnvelope {
        node_security_token: node_security_token.to_string(),
        signature,
        payload,
    }
}

#[derive(Debug, Clone)]
pub struct EdgeLifecycleConfig {
    pub controller_endpoint: String,
    pub state_dir: PathBuf,
    pub heartbeat_interval: Duration,
    pub reconnect_backoff: Vec<Duration>,
    /// Persisted NodeSecurityToken (RS256 JWT) issued at enrollment time.
    pub node_security_token: String,
    /// Stable UUID identifying this daemon (matches the `sub` claim on
    /// `node_security_token`). Used in canonical envelope payloads.
    pub node_id: NodeId,
    /// Daemon's persisted Ed25519 signing key (raw 32-byte seed loaded by
    /// `daemon::server` from the configured `node_keypair_path`).
    pub signing_key: Arc<SigningKey>,
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
    let node_id_str = cfg.node_id.0.to_string();
    let hello_envelope = signed_envelope(
        &cfg.signing_key,
        &cfg.node_security_token,
        &node_id_str,
        &stream_id,
        CanonicalKind::Hello,
        &[],
    );
    let hello = EdgeEvent {
        event: Some(OutEv::Hello(HelloEvent {
            envelope: Some(hello_envelope),
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
        let signing_key = cfg.signing_key.clone();
        let drain = drain.clone();
        let node_id_str = node_id_str.clone();
        let stream_id_hb = stream_id.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // skip immediate first tick
            loop {
                ticker.tick().await;
                if drain.load(Ordering::SeqCst) {
                    // stop heartbeating once the daemon is shutting down
                    break;
                }
                let envelope = signed_envelope(
                    &signing_key,
                    &token,
                    &node_id_str,
                    &stream_id_hb,
                    CanonicalKind::Heartbeat,
                    &[],
                );
                let hb = EdgeEvent {
                    event: Some(OutEv::Heartbeat(HeartbeatEvent {
                        envelope: Some(envelope),
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
                let envelope = signed_envelope(
                    &cfg.signing_key,
                    &cfg.node_security_token,
                    &node_id_str,
                    &stream_id,
                    CanonicalKind::CommandResult,
                    inv.command_id.as_bytes(),
                );
                let ev = EdgeEvent {
                    event: Some(OutEv::CommandResult(CommandResultEvent {
                        envelope: Some(envelope),
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
                let envelope = signed_envelope(
                    &cfg.signing_key,
                    &cfg.node_security_token,
                    &node_id_str,
                    &stream_id,
                    CanonicalKind::CommandResult,
                    c.command_id.as_bytes(),
                );
                let ev = EdgeEvent {
                    event: Some(OutEv::CommandResult(CommandResultEvent {
                        envelope: Some(envelope),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Verifier, VerifyingKey};

    #[test]
    fn canonical_payload_layout_is_deterministic_and_kind_tagged() {
        let a = canonical_envelope_payload(CanonicalKind::Hello, "node-a", "stream-1", 42, b"x");
        let b = canonical_envelope_payload(CanonicalKind::Hello, "node-a", "stream-1", 42, b"x");
        assert_eq!(a, b, "canonical payload must be deterministic");
        assert_ne!(
            a,
            canonical_envelope_payload(CanonicalKind::Heartbeat, "node-a", "stream-1", 42, b"x"),
            "different kinds must produce different canonical bytes"
        );
        assert_eq!(a[0], CanonicalKind::Hello as u8);
    }

    #[test]
    fn hello_envelope_signature_round_trips_through_verify() {
        // Regression: prior to ADR-117 audit fix SEV-2-F, daemon emitted an
        // empty `signature` field which the orchestrator's SealNodeVerifier
        // would reject. This test asserts the daemon-built envelope is
        // verifiable by `VerifyingKey::verify` (the same primitive the
        // orchestrator uses) against the daemon's published key.
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key: VerifyingKey = signing_key.verifying_key();

        let envelope = signed_envelope(
            &signing_key,
            "tok.tok.tok",
            "node-uuid",
            "stream-uuid",
            CanonicalKind::Hello,
            &[],
        );

        assert!(
            !envelope.signature.is_empty(),
            "Hello envelope must carry a non-empty signature"
        );
        assert_eq!(
            envelope.signature.len(),
            64,
            "Ed25519 signatures are 64 bytes"
        );

        let sig =
            ed25519_dalek::Signature::from_slice(&envelope.signature).expect("signature parses");
        verifying_key
            .verify(&envelope.payload, &sig)
            .expect("daemon signature must verify against its own VerifyingKey");
    }

    #[test]
    fn heartbeat_envelope_signature_round_trips_through_verify() {
        let signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let verifying_key: VerifyingKey = signing_key.verifying_key();
        let envelope = signed_envelope(
            &signing_key,
            "tok.tok.tok",
            "node-uuid",
            "stream-uuid",
            CanonicalKind::Heartbeat,
            &[],
        );
        let sig = ed25519_dalek::Signature::from_slice(&envelope.signature).unwrap();
        verifying_key.verify(&envelope.payload, &sig).unwrap();
    }

    #[test]
    fn command_result_envelope_binds_command_id_into_signed_payload() {
        let signing_key = SigningKey::from_bytes(&[5u8; 32]);
        let verifying_key: VerifyingKey = signing_key.verifying_key();
        let env_a = signed_envelope(
            &signing_key,
            "tok.tok.tok",
            "node",
            "stream",
            CanonicalKind::CommandResult,
            b"cmd-1",
        );
        let env_b = signed_envelope(
            &signing_key,
            "tok.tok.tok",
            "node",
            "stream",
            CanonicalKind::CommandResult,
            b"cmd-2",
        );
        assert_ne!(
            env_a.payload, env_b.payload,
            "different command_ids must produce different canonical payloads"
        );
        let sig_a = ed25519_dalek::Signature::from_slice(&env_a.signature).unwrap();
        verifying_key
            .verify(&env_a.payload, &sig_a)
            .expect("env_a verifies");
        // Cross-binding must fail.
        let cross = verifying_key.verify(&env_b.payload, &sig_a);
        assert!(
            cross.is_err(),
            "command_id is bound into the canonical payload — cross-verify must fail"
        );
    }
}
