// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis edge token refresh` — degenerate same-key rotation per ADR-117
//! §"Key & token rotation". Reuses the `RotateEdgeKey` RPC with
//! `new_public_key == current_public_key` to obtain a fresh
//! `NodeSecurityToken` without rolling the underlying Ed25519 identity.
//!
//! The orchestrator-side application service explicitly permits same-key
//! rotations (see `application/edge/rotate_edge_key.rs`); the dual-signature
//! requirement is satisfied trivially by signing the challenge with the same
//! key that signs the outer envelope.

use anyhow::Result;
use chrono::Utc;
use clap::Subcommand;

use super::grpc;

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Re-attest with the controller using the same Ed25519 keypair to get a
    /// fresh `NodeSecurityToken`. Reuses the `RotateEdgeKey` RPC with
    /// `new_public_key == old_public_key` per ADR-117.
    Refresh,
}

pub async fn run(cmd: TokenCommand) -> Result<()> {
    match cmd {
        TokenCommand::Refresh => refresh().await,
    }
}

async fn refresh() -> Result<()> {
    let state_dir = grpc::default_state_dir();
    let key = grpc::load_signing_key(&state_dir)?;
    let token = grpc::load_node_security_token(&state_dir)?;
    let node_id = grpc::node_id_from_token(&token)?;
    let endpoint = grpc::load_controller_endpoint(&state_dir)?;

    // Same-key: pass the same SigningKey as both old and new — the orchestrator
    // accepts this as a token-only refresh.
    let req = grpc::build_rotate_request(&key, &key, &node_id)?;
    let resp = grpc::call_rotate(&endpoint, req).await?;

    grpc::atomic_write_secret(
        &state_dir.join("node.token"),
        resp.node_security_token.as_bytes(),
    )?;

    let exp = resp
        .expires_at
        .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, 0))
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    println!("Token refreshed. Expires at {exp}.");
    Ok(())
}
