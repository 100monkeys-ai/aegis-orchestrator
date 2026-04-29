// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Re-attest with the controller using the same Ed25519 keypair to get a
    /// fresh `NodeSecurityToken`. Reuses the `RotateEdgeKey` RPC with
    /// `new_public_key == old_public_key` per ADR-117.
    Refresh,
}

pub async fn run(cmd: TokenCommand) -> Result<()> {
    match cmd {
        TokenCommand::Refresh => {
            // TODO(adr-117): implement the same-key refresh path. Cross-binary
            // dependency: needs the daemon's signing-key store helpers + a
            // stable RotateEdgeKey client wrapper to be reusable from the CLI
            // binary; deferred until the daemon-side helpers are extracted.
            Err(anyhow::anyhow!(
                "aegis edge token refresh is not yet implemented locally; \
                 TODO(adr-117): wire RotateEdgeKey same-key refresh path."
            ))
        }
    }
}
