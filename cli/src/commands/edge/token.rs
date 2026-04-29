// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    Refresh,
}

pub async fn run(cmd: TokenCommand) -> anyhow::Result<()> {
    match cmd {
        TokenCommand::Refresh => println!("aegis edge token refresh"),
    }
    Ok(())
}
