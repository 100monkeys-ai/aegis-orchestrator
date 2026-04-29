// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Args;

#[derive(Debug, Args)]
pub struct LsArgs {
    #[arg(long)]
    pub tag: Vec<String>,
    #[arg(long)]
    pub label: Vec<String>,
    #[arg(long)]
    pub connected: bool,
    #[arg(long, default_value = "table")]
    pub output: String,
}

pub async fn run(_args: LsArgs) -> anyhow::Result<()> {
    println!("aegis edge ls: invokes GET /api/edge/hosts via the local profile");
    Ok(())
}
