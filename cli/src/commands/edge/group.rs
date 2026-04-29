// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use super::client::EdgeApiClient;

#[derive(Debug, Subcommand)]
pub enum GroupCommand {
    Ls,
    Create {
        name: String,
        #[arg(long)]
        selector: String,
    },
    SetPinned {
        id: String,
        #[arg(long)]
        members: Vec<String>,
    },
    Rm {
        id: String,
    },
}

#[derive(Serialize)]
struct CreateGroupBody {
    name: String,
    selector: aegis_orchestrator_core::domain::edge::EdgeSelector,
    pinned_members: Vec<String>,
    created_by: String,
}

#[derive(Serialize)]
struct PatchGroupBody {
    pinned_members: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct GroupView {
    id: String,
    name: String,
    selector: aegis_orchestrator_core::domain::edge::EdgeSelector,
    pinned_members: Vec<String>,
    created_by: String,
    created_at: String,
}

pub async fn run(cmd: GroupCommand) -> Result<()> {
    let client = EdgeApiClient::from_env()?;
    match cmd {
        GroupCommand::Ls => {
            let groups: Vec<GroupView> = client.get("/v1/edge/groups").await?;
            println!("{}", serde_json::to_string_pretty(&groups)?);
        }
        GroupCommand::Create { name, selector } => {
            let target = super::selector::parse(&selector)?;
            let sel = match target {
                aegis_orchestrator_core::domain::edge::EdgeTarget::Selector(s) => s,
                _ => {
                    return Err(anyhow::anyhow!(
                        "create requires a selector expression (tags=, labels=, ...)"
                    ))
                }
            };
            let body = CreateGroupBody {
                name,
                selector: sel,
                pinned_members: vec![],
                created_by: std::env::var("AEGIS_OPERATOR_SUB").unwrap_or_else(|_| "cli".into()),
            };
            let group: GroupView = client.post("/v1/edge/groups", &body).await?;
            println!("{}", serde_json::to_string_pretty(&group)?);
        }
        GroupCommand::SetPinned { id, members } => {
            let body = PatchGroupBody {
                pinned_members: members,
            };
            let group: GroupView = client
                .patch(&format!("/v1/edge/groups/{id}"), &body)
                .await?;
            println!("{}", serde_json::to_string_pretty(&group)?);
        }
        GroupCommand::Rm { id } => {
            client.delete(&format!("/v1/edge/groups/{id}")).await?;
            println!("deleted");
        }
    }
    Ok(())
}
