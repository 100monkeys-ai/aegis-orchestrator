// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Args;
use serde::Deserialize;

use super::client::EdgeApiClient;
use crate::output::OutputFormat;

#[derive(Debug, Args)]
pub struct LsArgs {
    #[arg(long)]
    pub tag: Vec<String>,
    #[arg(long)]
    pub label: Vec<String>,
    #[arg(long)]
    pub connected: bool,
}

#[derive(Debug, Deserialize)]
struct EdgeHostView {
    id: String,
    name: String,
    tenant_id: String,
    status: String,
    tags: Vec<String>,
    os: String,
    arch: String,
}

pub async fn run(args: LsArgs, output: OutputFormat) -> Result<()> {
    let _ = (&args.label, &args.connected); // ADR-117: server-side filters TBD
    let client = EdgeApiClient::from_env()?;
    let hosts: Vec<EdgeHostView> = client.get("/v1/edge/hosts").await?;

    let filtered: Vec<&EdgeHostView> = hosts
        .iter()
        .filter(|h| args.tag.iter().all(|t| h.tags.contains(t)))
        .collect();

    match output {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&filtered)?),
        OutputFormat::Yaml => print!("{}", serde_yaml::to_string(&filtered)?),
        OutputFormat::Text | OutputFormat::Table => {
            println!(
                "{:<24} {:<40} {:<24} {:<12} {:<8} {:<8} TAGS",
                "NAME", "NODE_ID", "TENANT", "STATUS", "OS", "ARCH"
            );
            for h in &filtered {
                println!(
                    "{:<24} {:<40} {:<24} {:<12} {:<8} {:<8} {}",
                    h.name,
                    h.id,
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

// Manual Serialize so the filtered slice round-trips through the Json/Yaml
// arms above without requiring a derived impl on the deserialized struct.
impl serde::Serialize for EdgeHostView {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("EdgeHostView", 7)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("name", &self.name)?;
        st.serialize_field("tenant_id", &self.tenant_id)?;
        st.serialize_field("status", &self.status)?;
        st.serialize_field("os", &self.os)?;
        st.serialize_field("arch", &self.arch)?;
        st.serialize_field("tags", &self.tags)?;
        st.end()
    }
}
