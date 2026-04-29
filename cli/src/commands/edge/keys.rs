// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    Rotate {
        #[arg(long, default_value = "24h")]
        keep_old: String,
        #[arg(long)]
        force: bool,
    },
    RevokeRemote {
        node_id: String,
    },
}

pub async fn run(cmd: KeysCommand) -> anyhow::Result<()> {
    match cmd {
        KeysCommand::Rotate { .. } => println!("aegis edge keys rotate -> calls RotateEdgeKey RPC"),
        KeysCommand::RevokeRemote { node_id } => {
            println!("aegis edge keys revoke-remote '{node_id}' -> DELETE /api/edge/hosts/{{id}}")
        }
    }
    Ok(())
}
