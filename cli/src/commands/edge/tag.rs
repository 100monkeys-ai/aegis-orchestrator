// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};

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

/// Subset of the orchestrator's `EdgeHostView` response that this command
/// needs — only `tags` is consumed for the user-facing summary.
#[derive(Deserialize)]
struct EdgeHostResponse {
    tags: Vec<String>,
}

pub async fn run(cmd: TagCommand) -> Result<()> {
    let client = EdgeApiClient::from_env()?;
    match cmd {
        TagCommand::Add { node_id, tags } => {
            let body = PatchHost {
                add_tags: Some(tags),
                remove_tags: None,
            };
            let updated: EdgeHostResponse = client
                .patch(&format!("/v1/edge/hosts/{node_id}"), &body)
                .await?;
            println!("tags: {}", updated.tags.join(","));
        }
        TagCommand::Rm { node_id, tags } => {
            let body = PatchHost {
                add_tags: None,
                remove_tags: Some(tags),
            };
            let updated: EdgeHostResponse = client
                .patch(&format!("/v1/edge/hosts/{node_id}"), &body)
                .await?;
            println!("tags: {}", updated.tags.join(","));
        }
    }
    Ok(())
}
