// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge keys rotate` — atomic dual-signature Ed25519 keypair rotation
//! per ADR-117 §"Key & token rotation". Generates a fresh keypair locally,
//! invokes the controller's `RotateEdgeKey` gRPC, and atomically swaps the
//! on-disk identity files on success. The controller's 60s overlap window
//! lets the in-flight `ConnectEdge` stream survive the swap.

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Subcommand;
use dialoguer::Confirm;
use ed25519_dalek::SigningKey;
use std::fs;
use std::path::Path;
use std::time::Duration;

use super::client::EdgeApiClient;
use super::grpc;

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Rotate the local edge daemon's Ed25519 keypair via the
    /// `RotateEdgeKey` gRPC RPC.
    Rotate {
        #[arg(long, default_value = "24h")]
        keep_old: String,
        #[arg(long)]
        force: bool,
    },
    /// Operator-side revoke: `DELETE /api/edge/hosts/{node_id}` which evicts
    /// the daemon and disables further dispatches.
    RevokeRemote { node_id: String },
}

pub async fn run(cmd: KeysCommand) -> Result<()> {
    match cmd {
        KeysCommand::Rotate { keep_old, force } => rotate(&keep_old, force).await,
        KeysCommand::RevokeRemote { node_id } => {
            let client = EdgeApiClient::from_env()?;
            client.delete(&format!("/api/edge/hosts/{node_id}")).await?;
            println!("revoked {node_id}");
            Ok(())
        }
    }
}

async fn rotate(keep_old: &str, force: bool) -> Result<()> {
    let keep_old_dur = grpc::parse_duration(keep_old)
        .with_context(|| format!("invalid --keep-old value: {keep_old}"))?;
    let state_dir = grpc::default_state_dir();

    let old_key = grpc::load_signing_key(&state_dir)?;
    let token = grpc::load_node_security_token(&state_dir)?;
    let node_id = grpc::node_id_from_token(&token)?;
    let endpoint = grpc::load_controller_endpoint(&state_dir)?;

    if grpc::edge_daemon_appears_running() && !force {
        eprintln!(
            "warning: aegis-edge.service appears active. The 60s controller \
             overlap window means the in-flight stream survives, but the \
             daemon process must reload its keypair on next reconnect."
        );
    }

    if !force {
        let proceed = Confirm::new()
            .with_prompt(
                "Rotate edge daemon keypair? This is destructive if the \
                 daemon is currently enrolled elsewhere with the old key.",
            )
            .default(false)
            .interact()
            .unwrap_or(false);
        if !proceed {
            anyhow::bail!("aborted");
        }
    }

    let new_key = grpc::generate_signing_key();
    let req = grpc::build_rotate_request(&old_key, &new_key, &node_id)?;
    let resp = grpc::call_rotate(&endpoint, req).await?;

    let archive_path = swap_keys_on_disk(&state_dir, &old_key, &new_key)?;
    grpc::atomic_write_secret(
        &state_dir.join("node.token"),
        resp.node_security_token.as_bytes(),
    )?;

    schedule_archive_pruning(&state_dir, keep_old_dur)?;

    let until = resp
        .overlap_until
        .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, 0))
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    println!(
        "Rotation succeeded. Old key archived to {}; will be deleted after {}. \
         Controller overlap window ends at {}.",
        archive_path.display(),
        humanize(keep_old_dur),
        until,
    );
    Ok(())
}

/// Atomic on-disk swap:
///   1. archive existing `node.key` + `node.key.pub` to
///      `archive/<rfc3339>.key{,.pub}` (rename — atomic on POSIX).
///   2. write new key to a tempfile in the same directory and rename onto
///      `node.key`. There is never a window where `node.key` is missing
///      because the rename in (1) preserves the inode in the archive and the
///      new key write in (2) is its own atomic rename.
fn swap_keys_on_disk(
    state_dir: &Path,
    _old_key: &SigningKey,
    new_key: &SigningKey,
) -> Result<std::path::PathBuf> {
    let archive_dir = state_dir.join("archive");
    fs::create_dir_all(&archive_dir).with_context(|| format!("mkdir {}", archive_dir.display()))?;
    grpc::set_file_perms(&archive_dir, 0o700)?;

    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let archived_key = archive_dir.join(format!("{stamp}.key"));
    let archived_pub = archive_dir.join(format!("{stamp}.key.pub"));

    let key_path = state_dir.join("node.key");
    let pub_path = state_dir.join("node.key.pub");

    if key_path.exists() {
        fs::rename(&key_path, &archived_key).with_context(|| {
            format!(
                "archive {} -> {}",
                key_path.display(),
                archived_key.display()
            )
        })?;
    }
    if pub_path.exists() {
        // Best-effort: pub file is informational; missing is fine.
        let _ = fs::rename(&pub_path, &archived_pub);
    }

    grpc::atomic_write_secret(&key_path, &new_key.to_bytes())?;
    let new_pub = new_key.verifying_key().to_bytes();
    grpc::atomic_write_secret(&pub_path, &new_pub)?;

    Ok(archived_key)
}

/// Garbage-collect any archive entries older than `keep_old`. Cheap to do
/// inline on every rotation — there will rarely be more than a handful of
/// archived keys.
fn schedule_archive_pruning(state_dir: &Path, keep_old: Duration) -> Result<()> {
    let archive_dir = state_dir.join("archive");
    if !archive_dir.exists() {
        return Ok(());
    }
    let now = std::time::SystemTime::now();
    for entry in fs::read_dir(&archive_dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let age = match now.duration_since(modified) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if age > keep_old {
            // Best-effort delete; an archived key past expiry is no longer
            // accepted by the controller anyway.
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

fn humanize(d: Duration) -> String {
    let secs = d.as_secs();
    if secs % 86_400 == 0 {
        format!("{}d", secs / 86_400)
    } else if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}
