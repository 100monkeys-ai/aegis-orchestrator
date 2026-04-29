// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;
use serde::Serialize;

use super::client::EdgeApiClient;

#[derive(Debug, Subcommand)]
pub enum TagCommand {
    Add { node_id: String, tags: Vec<String> },
    Rm { node_id: String, tags: Vec<String> },
}

#[derive(Serialize)]
struct PatchHost {
    #[serde(skip_serializing_if = "Option::is_none")]
    add_tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remove_tags: Option<Vec<String>>,
}

pub async fn run(cmd: TagCommand) -> Result<()> {
    let client = EdgeApiClient::from_env()?;
    match cmd {
        TagCommand::Add { node_id, tags } => {
            let body = PatchHost {
                add_tags: Some(tags),
                remove_tags: None,
            };
            let updated: Vec<String> = client
                .patch(&format!("/api/edge/hosts/{node_id}"), &body)
                .await?;
            println!("tags: {}", updated.join(","));
        }
        TagCommand::Rm { node_id, tags } => {
            let body = PatchHost {
                add_tags: None,
                remove_tags: Some(tags),
            };
            let updated: Vec<String> = client
                .patch(&format!("/api/edge/hosts/{node_id}"), &body)
                .await?;
            println!("tags: {}", updated.join(","));
        }
    }
    Ok(())
}
