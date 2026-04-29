// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;

use super::client::EdgeApiClient;

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
        KeysCommand::Rotate {
            keep_old: _,
            force: _,
        } => {
            // TODO(adr-117): implement local Ed25519 rotation. Steps per the
            // ADR §RotateEdgeKey:
            //   1. read existing key from `~/.aegis/edge/<node>.key`,
            //   2. generate new keypair,
            //   3. compute deterministic challenge
            //      sha256("aegis-edge-rotate" || node_id || new_pubkey || nonce),
            //   4. sign with both old and new private keys,
            //   5. call `NodeClusterServiceClient::rotate_edge_key`,
            //   6. atomically swap files on success and persist the new
            //      NodeSecurityToken returned by the controller.
            // Cross-binary dependency: needs the daemon's signing-key store
            // helpers to be reusable from the CLI binary; deferred until
            // those are extracted.
            Err(anyhow::anyhow!(
                "aegis edge keys rotate is not yet implemented locally; \
                 TODO(adr-117): wire RotateEdgeKey RPC client + dual-signature."
            ))
        }
        KeysCommand::RevokeRemote { node_id } => {
            let client = EdgeApiClient::from_env()?;
            client.delete(&format!("/api/edge/hosts/{node_id}")).await?;
            println!("revoked {node_id}");
            Ok(())
        }
    }
}
