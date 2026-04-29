// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::client::EdgeApiClient;
use super::selector;

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
        /// JSON object of tool arguments (single string).
        #[arg(long, default_value = "{}")]
        args: String,
        #[arg(long)]
        security_context: String,
        #[arg(long, default_value = "sequential")]
        mode: String,
        #[arg(long)]
        max_concurrency: Option<usize>,
        #[arg(long, default_value = "fail-fast")]
        on_error: String,
        #[arg(long)]
        require_min: Option<usize>,
        #[arg(long, default_value_t = 60)]
        deadline_secs: u64,
    },
    Cancel {
        fleet_command_id: String,
    },
    Runs {
        #[arg(long, default_value = "table")]
        output: String,
    },
}

#[derive(Serialize)]
struct PreviewBody {
    target: aegis_orchestrator_core::domain::edge::EdgeTarget,
}

#[derive(Deserialize)]
struct PreviewResponse {
    resolved: Vec<String>,
}

#[derive(Serialize)]
struct InvokeBody {
    target: aegis_orchestrator_core::domain::edge::EdgeTarget,
    tool_name: String,
    args: serde_json::Value,
    security_context_name: String,
    user_security_token: String,
    mode: Option<String>,
    max_concurrency: Option<usize>,
    failure_policy: Option<String>,
    require_min_targets: Option<usize>,
    deadline_secs: Option<u64>,
}

pub async fn run(cmd: FleetCommand) -> Result<()> {
    let client = EdgeApiClient::from_env()?;
    match cmd {
        FleetCommand::Preview { target } => {
            let parsed = selector::parse(&target)?;
            let resp: PreviewResponse = client
                .post("/api/edge/fleet/preview", &PreviewBody { target: parsed })
                .await?;
            println!("{}", serde_json::to_string_pretty(&resp.resolved)?);
        }
        FleetCommand::Run {
            target,
            tool,
            args,
            security_context,
            mode,
            max_concurrency,
            on_error,
            require_min,
            deadline_secs,
        } => {
            let parsed = selector::parse(&target)?;
            let args_value: serde_json::Value =
                serde_json::from_str(&args).map_err(|e| anyhow::anyhow!("--args JSON: {e}"))?;
            let body = InvokeBody {
                target: parsed,
                tool_name: tool,
                args: args_value,
                security_context_name: security_context,
                user_security_token: std::env::var("AEGIS_USER_TOKEN").unwrap_or_default(),
                mode: Some(mode),
                max_concurrency,
                failure_policy: Some(on_error),
                require_min_targets: require_min,
                deadline_secs: Some(deadline_secs),
            };
            let resp = client
                .post_streamed("/api/edge/fleet/invoke", &body)
                .await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("{status}: {txt}"));
            }
            // Read SSE-style stream of `event:` / `data:` frames; print
            // every `data:` payload on its own line.
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk?;
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(idx) = buf.find("\n\n") {
                    let frame = buf[..idx].to_string();
                    buf = buf[idx + 2..].to_string();
                    for line in frame.lines() {
                        if let Some(rest) = line.strip_prefix("data:") {
                            println!("{}", rest.trim());
                        }
                    }
                }
            }
        }
        FleetCommand::Cancel { fleet_command_id } => {
            // Router exposes this as POST returning 204; we don't decode a body.
            let resp = client
                .post_streamed(
                    &format!("/api/edge/fleet/{fleet_command_id}/cancel"),
                    &serde_json::json!({}),
                )
                .await?;
            if resp.status().is_success() {
                println!("cancelled");
            } else {
                let status = resp.status();
                let txt = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("{status}: {txt}"));
            }
        }
        FleetCommand::Runs { output } => {
            let runs: serde_json::Value = client.get("/api/edge/fleet/runs").await?;
            match output.as_str() {
                "json" => println!("{}", serde_json::to_string_pretty(&runs)?),
                _ => println!("{runs}"),
            }
        }
    }
    Ok(())
}
