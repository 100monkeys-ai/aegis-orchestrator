// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum GroupCommand {
    Ls,
    Create {
        name: String,
        #[arg(long)]
        selector: String,
    },
    SetPinned {
        name: String,
        #[arg(long)]
        add: Vec<String>,
        #[arg(long)]
        rm: Vec<String>,
    },
    Rm {
        name: String,
    },
}

pub async fn run(cmd: GroupCommand) -> anyhow::Result<()> {
    match cmd {
        GroupCommand::Ls => println!("aegis edge group ls -> GET /api/edge/groups"),
        GroupCommand::Create { name, selector } => {
            println!("aegis edge group create '{name}' --selector '{selector}'")
        }
        GroupCommand::SetPinned { name, add, rm } => {
            println!("aegis edge group set-pinned '{name}' add={add:?} rm={rm:?}")
        }
        GroupCommand::Rm { name } => println!("aegis edge group rm '{name}'"),
    }
    Ok(())
}
