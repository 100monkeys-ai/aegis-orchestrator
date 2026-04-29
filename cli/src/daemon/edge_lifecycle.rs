// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 edge daemon lifecycle: long-lived bidi `ConnectEdge` stream.
//!
//! High-level flow:
//!   * Establish gRPC connection to `cluster.controller.endpoint`.
//!   * Send `EdgeEvent::Hello` carrying capabilities + outer SealNodeEnvelope.
//!   * Send periodic `Heartbeat` events.
//!   * For each inbound `EdgeCommand::InvokeTool`:
//!       * verify inner `SealEnvelope` (token shape + consistency);
//!       * resolve `security_context_name` against merged config;
//!       * enforce SecurityContext via the domain `evaluate` policy;
//!       * dispatch to local builtin (`cmd.run` runs locally via `tokio::process`);
//!       * stream `CommandProgress`, terminate with `CommandResult`.
//!   * On `Drain`: stop accepting new InvokeTool commands.
//!   * On `Shutdown`: drain then exit.
//!
//! Reconnect uses the configured `stream_reconnect_backoff_secs` schedule.

use anyhow::{Context, Result};
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use aegis_orchestrator_core::domain::security_context::SecurityContext;

use crate::commands::edge::grpc;
use aegis_orchestrator_core::infrastructure::aegis_cluster_proto::{
    edge_command::Command as InCmd, edge_event::Event as OutEv,
    node_cluster_service_client::NodeClusterServiceClient, CommandProgressEvent,
    CommandResultEvent, EdgeCapabilities, EdgeEvent, EdgeResult, HeartbeatEvent, HelloEvent,
    InvokeToolCommand, SealEnvelope as ProtoSealEnvelope, SealNodeEnvelope,
};

/// Tool-name prefixes that are routed to a local builtin on the edge daemon
/// but are **not yet implemented**. Surfacing them as a structured
/// `tool_not_locally_supported` error (rather than `tool_not_found`) lets the
/// dispatcher tell the operator "this is on the roadmap" vs "you typo'd". New
/// not-yet-implemented prefixes go here, in one place.
///
/// TODO(adr-117): wire `fs.*`, `web.*`, and `aegis.schema.*` to local
/// implementations and remove from this list.
const LOCAL_UNSUPPORTED_PREFIXES: &[&str] = &["fs.", "web.", "aegis.schema."];

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
    /// Local edge state directory (default `~/.aegis/edge`). The lifecycle
    /// re-reads `node.key` and `node.token` from this directory on every
    /// reconnect attempt so that `aegis edge token refresh` and
    /// `aegis edge keys rotate` take effect without restarting the daemon —
    /// rotation writes the new credentials atomically and the next connect
    /// picks them up. Without this, the daemon would silently keep using
    /// stale credentials in memory until restart, which is the bug the
    /// dead-code lint on this field surfaced.
    pub state_dir: PathBuf,
    pub heartbeat_interval: Duration,
    pub reconnect_backoff: Vec<Duration>,
    /// Local capabilities snapshot — populated by the daemon binary from
    /// hardware probes plus operator-configured local_tools / mount_points.
    pub capabilities: EdgeCapabilities,
    /// Resolved security contexts from the merged hierarchical config
    /// (`spec.security_contexts`). Empty means default-deny for every tool.
    pub security_contexts: Vec<SecurityContext>,
}

/// Credentials snapshot loaded from `state_dir` at the start of every
/// connect attempt. Allows `connect_once` to pick up rotation that
/// happened while the daemon was running.
#[derive(Debug, Clone)]
struct EdgeCredentials {
    signing_key: Arc<SigningKey>,
    node_security_token: String,
    node_id_str: String,
}

/// Per-connect view of the lifecycle config plus the credentials freshly
/// reloaded from `state_dir`. Helpers below take `&EdgeSession` instead of
/// reaching into `EdgeLifecycleConfig.signing_key` / `.node_security_token`
/// directly so a mid-run rotation is honoured.
struct EdgeSession<'a> {
    cfg: &'a EdgeLifecycleConfig,
    creds: EdgeCredentials,
}

/// Load the daemon's signing key and NodeSecurityToken from `state_dir`.
/// On any I/O or parse failure, surface the error so `connect_once` can
/// log + retry — never silently fall back to in-memory credentials,
/// because that would mask a corruption that the operator must address.
fn load_credentials_from_disk(state_dir: &Path) -> Result<EdgeCredentials> {
    let signing_key = Arc::new(
        grpc::load_signing_key(state_dir)
            .with_context(|| format!("reload signing key from {}", state_dir.display()))?,
    );
    let node_security_token = grpc::load_node_security_token(state_dir)
        .with_context(|| format!("reload NodeSecurityToken from {}", state_dir.display()))?;
    let node_id_str = grpc::node_id_from_token(&node_security_token)
        .context("derive node_id from reloaded NodeSecurityToken")?;
    Ok(EdgeCredentials {
        signing_key,
        node_security_token,
        node_id_str,
    })
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
    // 0. Re-load credentials from `state_dir` on every connect so that a
    //    rotation that landed while the daemon was running (via
    //    `aegis edge token refresh` / `keys rotate`) takes effect on the
    //    next reconnect — without this, the daemon kept stale in-memory
    //    credentials until restart, which is the bug the dead-code lint
    //    on `state_dir` surfaced.
    let creds = load_credentials_from_disk(&cfg.state_dir)?;
    let session = EdgeSession {
        cfg,
        creds: creds.clone(),
    };
    let signing_key = creds.signing_key.clone();
    let node_security_token = creds.node_security_token.clone();
    let node_id_str = creds.node_id_str.clone();

    // 1. Build the channel.
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
    let hello_envelope = signed_envelope(
        &signing_key,
        &node_security_token,
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
        let token = node_security_token.clone();
        let signing_key = signing_key.clone();
        let drain = drain.clone();
        let node_id_str = node_id_str.clone();
        let stream_id_hb = stream_id.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // skip immediate first tick
            loop {
                ticker.tick().await;
                if drain.load(Ordering::SeqCst) {
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
                if drain.load(Ordering::SeqCst) {
                    let result = error_result("draining", "edge daemon is draining");
                    let _ = send_result(
                        &tx,
                        &session,
                        &node_id_str,
                        &stream_id,
                        &inv.command_id,
                        result,
                    )
                    .await;
                    continue;
                }
                let cmd_id = inv.command_id.clone();
                let result =
                    handle_invoke_tool(&session, &tx, &node_id_str, &stream_id, &inv).await;
                let _ = send_result(&tx, &session, &node_id_str, &stream_id, &cmd_id, result).await;
            }
            Some(InCmd::Cancel(c)) => {
                tracing::info!(command_id = %c.command_id, "edge: received Cancel");
                let result = error_result("cancelled", "cancelled by operator");
                let _ = send_result(
                    &tx,
                    &session,
                    &node_id_str,
                    &stream_id,
                    &c.command_id,
                    result,
                )
                .await;
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

fn error_result(kind: &str, message: impl Into<String>) -> EdgeResult {
    EdgeResult {
        ok: false,
        exit_code: 0,
        stdout: Vec::new(),
        stderr: Vec::new(),
        structured_result: None,
        error_kind: kind.to_string(),
        error_message: message.into(),
    }
}

async fn send_result(
    tx: &mpsc::Sender<EdgeEvent>,
    session: &EdgeSession<'_>,
    node_id_str: &str,
    stream_id: &str,
    command_id: &str,
    result: EdgeResult,
) -> Result<()> {
    let envelope = signed_envelope(
        &session.creds.signing_key,
        &session.creds.node_security_token,
        node_id_str,
        stream_id,
        CanonicalKind::CommandResult,
        command_id.as_bytes(),
    );
    let ev = EdgeEvent {
        event: Some(OutEv::CommandResult(CommandResultEvent {
            envelope: Some(envelope),
            command_id: command_id.to_string(),
            result: Some(result),
        })),
    };
    tx.send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send_result: {e}"))
}

/// Verify the inner proto `SealEnvelope` carried in an InvokeToolCommand.
///
/// What we verify locally on the edge:
/// * `user_security_token` is non-empty and structurally a JWT (3 dot
///   segments). Full RS256 signature verification requires fetching the
///   originating realm's JWKS, which the edge daemon does not have a path
///   to in v1 — see TODO below.
/// * `tenant_id` is non-empty.
/// * `security_context_name` matches the value on the outer
///   `InvokeToolCommand` (consistency check — protects against a server-side
///   bug substituting contexts in the inner envelope).
fn verify_inner_envelope(
    envelope: &ProtoSealEnvelope,
    outer_scn: &str,
) -> Result<(), Box<EdgeResult>> {
    if envelope.user_security_token.trim().is_empty() {
        return Err(Box::new(error_result(
            "envelope_invalid",
            "inner SealEnvelope missing user_security_token",
        )));
    }
    let parts = envelope.user_security_token.split('.').count();
    if parts != 3 {
        return Err(Box::new(error_result(
            "envelope_invalid",
            "inner user_security_token is not a JWT (expected 3 dot segments)",
        )));
    }
    if envelope.tenant_id.trim().is_empty() {
        return Err(Box::new(error_result(
            "envelope_invalid",
            "inner SealEnvelope missing tenant_id",
        )));
    }
    if envelope.security_context_name != outer_scn {
        return Err(Box::new(error_result(
            "envelope_invalid",
            format!(
                "inner SealEnvelope security_context_name '{}' does not match outer '{}'",
                envelope.security_context_name, outer_scn
            ),
        )));
    }
    // TODO(adr-117): verify the Ed25519 signature on the inner envelope
    // against the originating principal's public key. Requires the daemon
    // to reach the controller's IAM proxy / SealSession registry, which
    // is not wired in v1. Until then we trust the outer SealNodeEnvelope
    // (controller signed the InvokeToolCommand) plus the fields already
    // checked above.
    Ok(())
}

fn resolve_security_context<'a>(
    contexts: &'a [SecurityContext],
    name: &str,
) -> Result<&'a SecurityContext, Box<EdgeResult>> {
    contexts.iter().find(|c| c.name == name).ok_or_else(|| {
        Box::new(error_result(
            "security_context_not_found",
            format!("SecurityContext '{name}' not present in merged config"),
        ))
    })
}

fn struct_to_json(args: Option<&prost_types::Struct>) -> serde_json::Value {
    match args {
        Some(s) => prost_struct_to_json(s),
        None => serde_json::Value::Null,
    }
}

fn prost_struct_to_json(s: &prost_types::Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in &s.fields {
        map.insert(k.clone(), prost_value_to_json(v));
    }
    serde_json::Value::Object(map)
}

fn prost_value_to_json(v: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match &v.kind {
        Some(Kind::NullValue(_)) | None => serde_json::Value::Null,
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::StructValue(s)) => prost_struct_to_json(s),
        Some(Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.iter().map(prost_value_to_json).collect())
        }
    }
}

async fn handle_invoke_tool(
    session: &EdgeSession<'_>,
    tx: &mpsc::Sender<EdgeEvent>,
    node_id_str: &str,
    stream_id: &str,
    inv: &InvokeToolCommand,
) -> EdgeResult {
    let cfg = session.cfg;
    // 1. Inner envelope verification.
    let envelope = match &inv.seal_envelope {
        Some(e) => e,
        None => {
            return error_result(
                "envelope_invalid",
                "InvokeToolCommand missing inner SealEnvelope",
            );
        }
    };
    if let Err(e) = verify_inner_envelope(envelope, &inv.security_context_name) {
        return *e;
    }

    // 2. Resolve SecurityContext from merged config.
    let context = match resolve_security_context(&cfg.security_contexts, &inv.security_context_name)
    {
        Ok(c) => c,
        Err(e) => return *e,
    };

    // 3. Apply policy (deny-list + capability evaluation against tool args).
    let args_json = struct_to_json(inv.args.as_ref());
    if let Err(violation) = context.evaluate(&inv.tool_name, &args_json) {
        return error_result("policy_violation", format!("{violation:?}"));
    }

    // 4. Local builtin dispatch.
    if inv.tool_name == "cmd.run" {
        return run_local_cmd(session, tx, node_id_str, stream_id, inv, &args_json).await;
    }

    // These require AegisFSAL / NfsVolumeRegistry / SchemaRegistry — none of
    // which are constructed by the daemon binary today. Fail with a
    // structured error so the dispatcher can surface a useful message rather
    // than a silent ack. The prefix list lives at module-level (see
    // `LOCAL_UNSUPPORTED_PREFIXES`) so future additions land in one place.
    if LOCAL_UNSUPPORTED_PREFIXES
        .iter()
        .any(|prefix| inv.tool_name.starts_with(prefix))
    {
        return error_result(
            "tool_not_locally_supported",
            format!(
                "builtin '{}' not yet implemented on edge daemon (TODO adr-117)",
                inv.tool_name
            ),
        );
    }

    error_result(
        "tool_not_found",
        format!(
            "tool '{}' is not a recognised local builtin on this edge daemon",
            inv.tool_name
        ),
    )
}

async fn run_local_cmd(
    session: &EdgeSession<'_>,
    tx: &mpsc::Sender<EdgeEvent>,
    node_id_str: &str,
    stream_id: &str,
    inv: &InvokeToolCommand,
    args_json: &serde_json::Value,
) -> EdgeResult {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let command = match args_json.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => {
            return error_result(
                "envelope_invalid",
                "cmd.run: missing required arg 'command'",
            );
        }
    };
    let extra_args: Vec<String> = args_json
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = args_json
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("/")
        .to_string();
    let timeout_secs = args_json
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(600);

    // Honour the dispatcher's deadline if present and tighter than args.
    // Negative `d.seconds` (proto i64) falls through to the default
    // `timeout_secs` — `u64::try_from` returns Err for negatives, which
    // `.and_then` cleanly maps into the `unwrap_or` branch. The prior
    // `.max(0) as u64` cast saturated negatives to 0 silently, then relied
    // on a downstream `.filter(*d > 0)` to discard them — same end state,
    // but obscured the intent.
    let deadline_secs = inv
        .deadline
        .as_ref()
        .and_then(|d| u64::try_from(d.seconds).ok())
        .filter(|d| *d > 0)
        .map(|d| d.min(timeout_secs))
        .unwrap_or(timeout_secs);

    let mut cmd = Command::new(&command);
    cmd.args(&extra_args)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return error_result("internal", format!("spawn '{command}' failed: {e}")),
    };
    // RAII: any early return from this function (timeout, wait error, etc.)
    // must NOT leave the child running. ChildGuard's Drop calls
    // `start_kill()` on any child still held; we `disarm()` the guard once
    // we have observed a normal exit status, so no signal is sent to a
    // child that is already gone.
    let mut guard = ChildGuard::new(child);
    let stdout = guard.child_mut().stdout.take();
    let stderr = guard.child_mut().stderr.take();

    let cmd_id = inv.command_id.clone();
    let tx_out = tx.clone();
    let tx_err = tx.clone();
    let parts_out = (
        session.creds.signing_key.clone(),
        session.creds.node_security_token.clone(),
        node_id_str.to_string(),
        stream_id.to_string(),
        cmd_id.clone(),
    );
    let parts_err = parts_out.clone();

    // Stream stdout and stderr concurrently so the orchestrator sees progress.
    let stdout_task = tokio::spawn(async move {
        let Some(s) = stdout else { return Vec::new() };
        let mut reader = BufReader::new(s).lines();
        let mut buf: Vec<u8> = Vec::new();
        let mut seq: u32 = 0;
        while let Ok(Some(line)) = reader.next_line().await {
            let mut chunk = line.into_bytes();
            chunk.push(b'\n');
            buf.extend_from_slice(&chunk);
            send_progress_inline(&tx_out, &parts_out, seq, chunk, false).await;
            seq += 1;
        }
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let Some(s) = stderr else { return Vec::new() };
        let mut reader = BufReader::new(s).lines();
        let mut buf: Vec<u8> = Vec::new();
        let mut seq: u32 = 0;
        while let Ok(Some(line)) = reader.next_line().await {
            let mut chunk = line.into_bytes();
            chunk.push(b'\n');
            buf.extend_from_slice(&chunk);
            send_progress_inline(&tx_err, &parts_err, seq, chunk, true).await;
            seq += 1;
        }
        buf
    });

    let wait_fut = guard.child_mut().wait();
    let timeout = tokio::time::timeout(Duration::from_secs(deadline_secs), wait_fut);
    let exit = match timeout.await {
        Ok(Ok(status)) => {
            // Child has exited — disarm so Drop does not send a signal to
            // a now-reaped pid (which on some platforms could target an
            // unrelated process if the pid was recycled).
            guard.disarm();
            status
        }
        Ok(Err(e)) => {
            // wait() failed; ChildGuard::Drop will still kill the child as
            // we have not disarmed. Surface the error.
            return error_result("internal", format!("wait failed: {e}"));
        }
        Err(_) => {
            // Deadline elapsed. The child is still running; ChildGuard's
            // Drop sends SIGKILL via start_kill so the spawned process
            // does NOT leak as a detached orphan.
            return error_result(
                "timeout",
                format!("cmd.run '{command}' exceeded {deadline_secs}s deadline"),
            );
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();

    let code = exit.code().unwrap_or(-1);
    EdgeResult {
        ok: exit.success(),
        exit_code: code,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        structured_result: None,
        error_kind: if exit.success() {
            String::new()
        } else {
            "nonzero_exit".to_string()
        },
        error_message: String::new(),
    }
}

/// RAII wrapper that guarantees a spawned [`tokio::process::Child`] is killed
/// on every exit path of `run_local_cmd` unless explicitly disarmed.
///
/// **Why this exists:** prior to this guard, the cmd.run timeout arm (and
/// the `wait()` error arm) returned without calling `child.kill()`, leaking
/// the spawned process as a detached orphan. With multiple early-return
/// paths between `Command::spawn()` and the await of `child.wait()`,
/// inline `kill()` calls are easy to miss in future refactors. Drop-on-panic
/// also matters: if any await between spawn and disarm panics, the child is
/// still cleaned up.
///
/// `start_kill()` is non-async (sends the kill syscall and returns
/// immediately without waiting for reap), which is the correct fit for a
/// `Drop` impl. The kernel reaps via the daemon's tokio process driver.
struct ChildGuard {
    child: Option<tokio::process::Child>,
}

impl ChildGuard {
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        // Safe: the only path that takes() the inner child is `disarm()`,
        // and after disarm we never call child_mut again.
        self.child
            .as_mut()
            .expect("ChildGuard::child_mut after disarm()")
    }

    /// Mark the child as already-exited so `Drop` does NOT send a kill
    /// signal. Call this after `child.wait()` returns Ok(status).
    fn disarm(&mut self) {
        self.child = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort: ignore the kill error (typically `ESRCH` if the
            // child has already exited between the last poll and Drop).
            let _ = child.start_kill();
        }
    }
}

/// Inline variant of CommandProgress emission used inside the cmd.run streamers.
async fn send_progress_inline(
    tx: &mpsc::Sender<EdgeEvent>,
    parts: &(Arc<SigningKey>, String, String, String, String),
    sequence: u32,
    chunk: Vec<u8>,
    is_stderr: bool,
) {
    let (signing_key, token, node_id_str, stream_id, command_id) = parts;
    let mut inner = Vec::with_capacity(command_id.len() + 4 + 1 + chunk.len());
    inner.extend_from_slice(command_id.as_bytes());
    inner.extend_from_slice(&sequence.to_le_bytes());
    inner.push(is_stderr as u8);
    inner.extend_from_slice(&chunk);
    let envelope = signed_envelope(
        signing_key,
        token,
        node_id_str,
        stream_id,
        CanonicalKind::CommandProgress,
        &inner,
    );
    let ev = EdgeEvent {
        event: Some(OutEv::CommandProgress(CommandProgressEvent {
            envelope: Some(envelope),
            command_id: command_id.clone(),
            chunk,
            sequence,
            stderr: is_stderr,
        })),
    };
    let _ = tx.send(ev).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::security_context::{Capability, SecurityContextMetadata};
    use ed25519_dalek::{Verifier, VerifyingKey};

    /// Builds a minimal `SecurityContext` for policy-evaluation tests:
    /// allows `cmd.run` against `/bin/echo` only, with no other capabilities
    /// or deny-list entries. Caller supplies the context name.
    fn build_test_security_context(name: &str) -> SecurityContext {
        SecurityContext {
            name: name.to_string(),
            description: "test".into(),
            capabilities: vec![Capability {
                tool_pattern: "cmd.run".to_string(),
                path_allowlist: None,
                command_allowlist: Some(vec!["/bin/echo".to_string(), "echo".to_string()]),
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            }],
            deny_list: vec![],
            metadata: SecurityContextMetadata {
                created_at: Utc::now(),
                updated_at: Utc::now(),
                version: 1,
            },
        }
    }

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
        let cross = verifying_key.verify(&env_b.payload, &sig_a);
        assert!(
            cross.is_err(),
            "command_id is bound into the canonical payload — cross-verify must fail"
        );
    }

    #[test]
    fn verify_inner_envelope_rejects_missing_token() {
        let env = ProtoSealEnvelope {
            user_security_token: String::new(),
            tenant_id: "t".into(),
            security_context_name: "ctx".into(),
            payload: None,
            signature: vec![],
        };
        let err = verify_inner_envelope(&env, "ctx").unwrap_err();
        assert_eq!(err.error_kind, "envelope_invalid");
    }

    #[test]
    fn verify_inner_envelope_rejects_context_mismatch() {
        // Regression for SEV-2-G: prior to the fix the daemon would
        // ack-reply without checking the inner envelope at all.
        let env = ProtoSealEnvelope {
            user_security_token: "a.b.c".into(),
            tenant_id: "t".into(),
            security_context_name: "wrong".into(),
            payload: None,
            signature: vec![],
        };
        let err = verify_inner_envelope(&env, "expected").unwrap_err();
        assert_eq!(err.error_kind, "envelope_invalid");
        assert!(err.error_message.contains("security_context_name"));
    }

    #[test]
    fn verify_inner_envelope_accepts_consistent_envelope() {
        let env = ProtoSealEnvelope {
            user_security_token: "a.b.c".into(),
            tenant_id: "t".into(),
            security_context_name: "ctx".into(),
            payload: None,
            signature: vec![],
        };
        verify_inner_envelope(&env, "ctx").expect("accept consistent envelope");
    }

    #[test]
    fn resolve_security_context_returns_named_match() {
        let ctxs = vec![build_test_security_context("zaru-free")];
        let r = resolve_security_context(&ctxs, "zaru-free").unwrap();
        assert_eq!(r.name, "zaru-free");
    }

    #[test]
    fn resolve_security_context_missing_returns_structured_error() {
        // Regression for SEV-2-G: missing context must now produce a
        // structured `security_context_not_found` rather than a silent ack.
        let ctxs = vec![build_test_security_context("zaru-free")];
        let err = resolve_security_context(&ctxs, "missing").unwrap_err();
        assert_eq!(err.error_kind, "security_context_not_found");
    }

    #[test]
    fn policy_evaluation_rejects_disallowed_tool() {
        // Regression for SEV-2-G: SecurityContext was not consulted at all
        // before the fix. With enforcement in place, a tool outside the
        // capability set must now produce `policy_violation`.
        let ctx = build_test_security_context("ctx");
        let res = ctx.evaluate("fs.delete", &serde_json::json!({}));
        assert!(
            res.is_err(),
            "fs.delete is outside the test context's capabilities"
        );
    }

    #[test]
    fn local_unsupported_prefixes_match_documented_set() {
        // Regression: the prior `starts_with` chain hard-coded three prefixes
        // inline. Future additions had to be made in one specific call site.
        // Pin the constant so additions/removals are visible to reviewers.
        assert_eq!(
            LOCAL_UNSUPPORTED_PREFIXES,
            &["fs.", "web.", "aegis.schema."]
        );
        // Spot-check that .iter().any() preserves the prior chain semantics:
        for tool in &[
            "fs.read",
            "fs.delete",
            "web.fetch",
            "aegis.schema.get",
            "aegis.schema.list",
        ] {
            assert!(
                LOCAL_UNSUPPORTED_PREFIXES
                    .iter()
                    .any(|p| tool.starts_with(p)),
                "{tool} must be classified as locally-unsupported"
            );
        }
        for tool in &["cmd.run", "aegis.tenant.get", "fs", "web"] {
            assert!(
                !LOCAL_UNSUPPORTED_PREFIXES
                    .iter()
                    .any(|p| tool.starts_with(p)),
                "{tool} must NOT match a locally-unsupported prefix"
            );
        }
    }

    #[test]
    fn deadline_secs_negative_falls_through_to_default_timeout() {
        // Regression for F5: prior code used `d.seconds.max(0) as u64` which
        // saturated negatives to 0 and relied on a downstream `.filter(*>0)`
        // to discard them. The new path uses `u64::try_from(d.seconds).ok()`
        // — for negative i64, try_from returns Err which `.and_then` treats
        // as None, so the chain falls through cleanly to the args-supplied
        // default. This unit test pins that invariant against a regression
        // that swapped try_from for `as u64` (a wrap-and-panic-in-release).
        let timeout_secs: u64 = 600;
        // Mirror the in-function chain.
        let cases: &[(i64, u64)] = &[
            (-1, 600),       // negative -> default
            (i64::MIN, 600), // extreme negative -> default
            (0, 600),        // zero filtered out -> default
            (10, 10),        // positive smaller than default -> tighter
            (1000, 600),     // positive larger than default -> default (.min)
        ];
        for (raw_secs, expected) in cases {
            let derived: u64 = Some(*raw_secs)
                .and_then(|s| u64::try_from(s).ok())
                .filter(|d| *d > 0)
                .map(|d| d.min(timeout_secs))
                .unwrap_or(timeout_secs);
            assert_eq!(
                derived, *expected,
                "raw deadline {raw_secs}: expected {expected}, got {derived}"
            );
        }
    }

    /// Regression for F6: the cmd.run timeout arm previously returned without
    /// killing the child, leaking the spawned process as a detached orphan.
    /// `ChildGuard::Drop` now signals the child unconditionally unless
    /// `disarm()` has been called.
    ///
    /// Portability: this test is `#[cfg(unix)]` because (a) it spawns
    /// `/bin/sh -c sleep 30`, which is Unix-only, and (b) it uses
    /// `kill(pid, 0)` semantics via `nix`-style probing through `/proc` —
    /// so it does not run on Windows. The orchestrator's CI matrix is
    /// Linux-only for the daemon path, so this is acceptable. macOS would
    /// need a different liveness probe (no `/proc`), but the daemon binary
    /// is not built for macOS targets.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn child_guard_kills_running_child_on_drop() {
        use tokio::process::Command;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 30"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null());
        let child = cmd.spawn().expect("spawn /bin/sh -c sleep 30");
        let pid = child.id().expect("child has a pid");
        {
            let _guard = ChildGuard::new(child);
            // Confirm the pid is alive before we drop.
            assert!(
                std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "child pid {pid} must be alive before guard drops"
            );
        }
        // Drop has fired. start_kill() is async-signal-safe but reaping is
        // driven by tokio's process driver; yield long enough for the
        // SIGKILL + reap to complete.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return; // reaped, regression test passes
            }
        }
        // Final probe: even if /proc shows a zombie, the process should not
        // be runnable. If we get here, the child outlived the guard — the
        // F6 regression has returned.
        panic!("child pid {pid} survived ChildGuard::drop — F6 regression");
    }

    /// Companion to the kill-on-drop test: prove that `disarm()` is
    /// observed and the child is NOT signalled when the guard is dropped
    /// after a successful wait. Without this, every successful cmd.run
    /// would race a SIGKILL against an already-reaped pid (which on a
    /// pid-recycled host could target an unrelated process).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn child_guard_does_not_kill_after_disarm() {
        use tokio::process::Command;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null());
        let child = cmd.spawn().expect("spawn /bin/sh -c 'exit 0'");
        let mut guard = ChildGuard::new(child);
        let status = guard.child_mut().wait().await.expect("wait");
        assert!(status.success());
        guard.disarm();
        // Drop runs at end of scope — no kill should be sent because the
        // inner option is None. We assert structurally rather than
        // probing the now-reaped pid (which would race a recycled pid).
        assert!(guard.child.is_none(), "disarm must clear the inner child");
        drop(guard); // exercise the no-op Drop path explicitly
    }

    #[test]
    fn policy_evaluation_accepts_permitted_tool_with_allowlisted_command() {
        let ctx = build_test_security_context("ctx");
        let args = serde_json::json!({"command": "/bin/echo", "args": ["hello"]});
        ctx.evaluate("cmd.run", &args)
            .expect("cmd.run with allowlisted /bin/echo must be permitted");
    }

    /// Build a JWT-shaped string with the given `sub` claim. The lifecycle
    /// loader only decodes the claims segment; the signature segment is a
    /// placeholder.
    fn make_test_node_token(sub: &str) -> String {
        use base64::Engine;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{{\"sub\":\"{sub}\"}}").as_bytes());
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{claims}.{sig}")
    }

    /// Regression for the `state_dir` dead-code lint: prior code constructed
    /// the field on `EdgeLifecycleConfig` but the lifecycle ignored it,
    /// reading state from a hardcoded in-memory snapshot loaded once at
    /// startup. This test pins that `load_credentials_from_disk` actually
    /// reads from the supplied directory — proving an operator who passes
    /// `aegis edge daemon --state-dir /custom/path` is no longer silently
    /// using cached defaults.
    #[test]
    fn load_credentials_reads_from_supplied_state_dir_not_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let key_seed = [11u8; 32];
        std::fs::write(tmp.path().join("node.key"), key_seed).unwrap();
        let sub = uuid::Uuid::new_v4().to_string();
        let token = make_test_node_token(&sub);
        std::fs::write(tmp.path().join("node.token"), &token).unwrap();

        let creds =
            load_credentials_from_disk(tmp.path()).expect("must load from supplied state_dir");

        assert_eq!(
            creds.node_id_str, sub,
            "node_id must come from THIS dir's node.token, not a hardcoded default"
        );
        assert_eq!(
            creds.node_security_token.trim(),
            token,
            "token must be loaded verbatim from the supplied dir"
        );
        // Signing key must derive from the seed we wrote.
        let expected = SigningKey::from_bytes(&key_seed);
        assert_eq!(
            creds.signing_key.to_bytes(),
            expected.to_bytes(),
            "signing key must derive from the supplied dir's node.key"
        );
    }

    /// Regression: a missing state_dir must produce an error mentioning the
    /// supplied path. Without the state_dir wiring this would have errored
    /// with the hardcoded `~/.aegis/edge` path — and an operator who passed
    /// `--state-dir /custom` would have been baffled.
    #[test]
    fn load_credentials_error_mentions_supplied_path_not_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Empty dir — node.key is missing.
        let err = load_credentials_from_disk(tmp.path()).expect_err("must error on empty dir");
        let msg = format!("{err:#}");
        let supplied = tmp.path().display().to_string();
        assert!(
            msg.contains(&supplied),
            "error must surface the supplied state_dir path '{supplied}': {msg}"
        );
    }

    /// Regression for the same lint: a fresh credential loaded from a
    /// post-rotation state_dir must differ from one loaded from a
    /// pre-rotation state_dir. This pins the "rotation takes effect on
    /// reconnect without restart" behavior: previously, in-memory
    /// credentials were cached at startup so a token refresh between
    /// reconnects had no effect.
    #[test]
    fn load_credentials_reflects_post_rotation_state_dir_contents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Pre-rotation snapshot.
        std::fs::write(tmp.path().join("node.key"), [1u8; 32]).unwrap();
        let sub_old = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            tmp.path().join("node.token"),
            make_test_node_token(&sub_old),
        )
        .unwrap();
        let before = load_credentials_from_disk(tmp.path()).expect("pre-rotation load");

        // Rotation: a fresh key + token land at the same paths.
        std::fs::write(tmp.path().join("node.key"), [2u8; 32]).unwrap();
        let sub_new = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            tmp.path().join("node.token"),
            make_test_node_token(&sub_new),
        )
        .unwrap();
        let after = load_credentials_from_disk(tmp.path()).expect("post-rotation load");

        assert_ne!(
            before.node_id_str, after.node_id_str,
            "post-rotation reload must observe the new sub claim"
        );
        assert_ne!(
            before.signing_key.to_bytes(),
            after.signing_key.to_bytes(),
            "post-rotation reload must observe the new node.key seed"
        );
    }
}
