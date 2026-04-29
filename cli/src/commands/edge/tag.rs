// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum TagCommand {
    Add { node_id: String, tags: Vec<String> },
    Rm { node_id: String, tags: Vec<String> },
}

pub async fn run(cmd: TagCommand) -> anyhow::Result<()> {
    match cmd {
        TagCommand::Add { node_id, tags } => {
            println!(
                "aegis edge tag add {} {:?} -> PATCH /api/edge/hosts/{{id}}",
                node_id, tags
            );
        }
        TagCommand::Rm { node_id, tags } => {
            println!(
                "aegis edge tag rm {} {:?} -> PATCH /api/edge/hosts/{{id}}",
                node_id, tags
            );
        }
    }
    Ok(())
}
