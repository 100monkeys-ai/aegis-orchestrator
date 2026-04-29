// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Args;
use serde::Deserialize;

use super::client::EdgeApiClient;

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

#[derive(Debug, Deserialize)]
struct EdgeHostView {
    node_id: String,
    tenant_id: String,
    status: String,
    tags: Vec<String>,
    os: String,
    arch: String,
}

pub async fn run(args: LsArgs) -> Result<()> {
    let _ = (&args.label, &args.connected); // ADR-117: server-side filters TBD
    let client = EdgeApiClient::from_env()?;
    let hosts: Vec<EdgeHostView> = client.get("/v1/edge/hosts").await?;

    let filtered: Vec<&EdgeHostView> = hosts
        .iter()
        .filter(|h| args.tag.iter().all(|t| h.tags.contains(t)))
        .collect();

    match args.output.as_str() {
        "json" => println!("{}", serde_json::to_string_pretty(&filtered)?),
        _ => {
            println!(
                "{:<40} {:<24} {:<12} {:<8} {:<8} TAGS",
                "NODE_ID", "TENANT", "STATUS", "OS", "ARCH"
            );
            for h in &filtered {
                println!(
                    "{:<40} {:<24} {:<12} {:<8} {:<8} {}",
                    h.node_id,
                    h.tenant_id,
                    h.status,
                    h.os,
                    h.arch,
                    h.tags.join(",")
                );
            }
        }
    }
    Ok(())
}

// Permit the subset that can be derived for serializing filtered slice.
impl serde::Serialize for EdgeHostView {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("EdgeHostView", 6)?;
        st.serialize_field("node_id", &self.node_id)?;
        st.serialize_field("tenant_id", &self.tenant_id)?;
        st.serialize_field("status", &self.status)?;
        st.serialize_field("os", &self.os)?;
        st.serialize_field("arch", &self.arch)?;
        st.serialize_field("tags", &self.tags)?;
        st.end()
    }
}
