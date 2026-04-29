// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Args;

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long, default_value = "table")]
    pub output: String,
}

pub async fn run(_args: StatusArgs) -> anyhow::Result<()> {
    let dir = dirs_next::home_dir()
        .map(|h| h.join(".aegis").join("edge"))
        .unwrap_or_else(|| std::path::PathBuf::from(".aegis/edge"));
    let token = dir.join("node.token");
    let cfg = dir.join("aegis-config.yaml");
    println!("aegis edge status");
    println!("  state_dir       : {}", dir.display());
    println!("  config          : {}", cfg.display());
    println!(
        "  enrolled        : {}",
        if token.exists() { "yes" } else { "no" }
    );
    Ok(())
}
