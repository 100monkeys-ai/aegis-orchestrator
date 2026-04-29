// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum FleetCommand {
    Preview {
        #[arg(long)]
        target: String,
    },
    Run {
        #[arg(long)]
        target: String,
        #[arg(long)]
        tool: String,
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long, default_value = "sequential")]
        mode: String,
        #[arg(long)]
        max_concurrency: Option<usize>,
        #[arg(long, default_value = "fail-fast")]
        on_error: String,
        #[arg(long)]
        require_min: Option<usize>,
        #[arg(long, default_value = "60s")]
        deadline: String,
    },
    Cancel {
        fleet_command_id: String,
    },
    Runs {
        #[arg(long, default_value = "table")]
        output: String,
    },
}

pub async fn run(cmd: FleetCommand) -> anyhow::Result<()> {
    match cmd {
        FleetCommand::Preview { target } => {
            println!("aegis edge fleet preview --target '{target}'")
        }
        FleetCommand::Run { target, tool, .. } => {
            println!("aegis edge fleet run --target '{target}' --tool '{tool}'")
        }
        FleetCommand::Cancel { fleet_command_id } => {
            println!("aegis edge fleet cancel '{fleet_command_id}'")
        }
        FleetCommand::Runs { .. } => println!("aegis edge fleet runs"),
    }
    Ok(())
}
