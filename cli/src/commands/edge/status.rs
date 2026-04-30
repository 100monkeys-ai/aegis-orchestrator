// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Args;
use serde::Serialize;

use crate::output::OutputFormat;

#[derive(Debug, Args)]
pub struct StatusArgs {}

#[derive(Serialize)]
struct EdgeStatusView {
    state_dir: String,
    config: String,
    enrolled: bool,
}

pub async fn run(_args: StatusArgs, output: OutputFormat) -> anyhow::Result<()> {
    let dir = dirs_next::home_dir()
        .map(|h| h.join(".aegis").join("edge"))
        .unwrap_or_else(|| std::path::PathBuf::from(".aegis/edge"));
    let token = dir.join("node.token");
    let cfg = dir.join("aegis-config.yaml");
    let view = EdgeStatusView {
        state_dir: dir.display().to_string(),
        config: cfg.display().to_string(),
        enrolled: token.exists(),
    };
    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&view)?),
        OutputFormat::Yaml => print!("{}", serde_yaml::to_string(&view)?),
        OutputFormat::Text | OutputFormat::Table => {
            println!("aegis edge status");
            println!("  state_dir       : {}", view.state_dir);
            println!("  config          : {}", view.config);
            println!(
                "  enrolled        : {}",
                if view.enrolled { "yes" } else { "no" }
            );
        }
    }
    Ok(())
}
