// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::Result;
use clap::Subcommand;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::client::EdgeApiClient;
use super::selector;
use crate::output::OutputFormat;

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
    Runs {},
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

pub async fn run(cmd: FleetCommand, output: OutputFormat) -> Result<()> {
    let client = EdgeApiClient::from_env()?;
    match cmd {
        FleetCommand::Preview { target } => {
            let parsed = selector::parse(&target)?;
            let resp: PreviewResponse = client
                .post("/v1/edge/fleet/preview", &PreviewBody { target: parsed })
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
                user_security_token: require_user_token()?,
                mode: Some(mode),
                max_concurrency,
                failure_policy: Some(on_error),
                require_min_targets: require_min,
                deadline_secs: Some(deadline_secs),
            };
            let resp = client.post_streamed("/v1/edge/fleet/invoke", &body).await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let txt = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| String::from("<unreadable response body>"));
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
                    &format!("/v1/edge/fleet/{fleet_command_id}/cancel"),
                    &serde_json::json!({}),
                )
                .await?;
            if resp.status().is_success() {
                println!("cancelled");
            } else {
                let status = resp.status();
                let txt = resp
                    .text()
                    .await
                    .unwrap_or_else(|_| String::from("<unreadable response body>"));
                return Err(anyhow::anyhow!("{status}: {txt}"));
            }
        }
        FleetCommand::Runs {} => {
            let runs: serde_json::Value = client.get("/v1/edge/fleet/runs").await?;
            match output {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&runs)?),
                OutputFormat::Yaml => print!("{}", serde_yaml::to_string(&runs)?),
                OutputFormat::Text | OutputFormat::Table => println!("{runs}"),
            }
        }
    }
    Ok(())
}

/// Fetch the operator's user-security-token from the env, failing fast if
/// unset. Previously called via `unwrap_or_default()`, which silently sent an
/// empty bearer to the orchestrator. ADR-117 audit pass 3, SEV-3-G.
fn require_user_token() -> Result<String> {
    std::env::var("AEGIS_USER_TOKEN")
        .map_err(|_| anyhow::anyhow!("AEGIS_USER_TOKEN must be set; run `aegis auth login` first"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: ADR-117 audit pass 3 SEV-3-G — `aegis edge fleet run` must
    /// surface a clear error when AEGIS_USER_TOKEN is missing rather than
    /// silently sending an empty bearer to the orchestrator. Combined into
    /// one test (rather than two) to avoid races on the shared process env
    /// when cargo runs tests in parallel.
    #[test]
    fn require_user_token_unset_errors_then_set_returns_value() {
        let prior = std::env::var("AEGIS_USER_TOKEN").ok();

        // Phase 1: missing var produces the expected typed error.
        std::env::remove_var("AEGIS_USER_TOKEN");
        let err = require_user_token().expect_err("must error when unset");
        let msg = format!("{err}");
        assert!(
            msg.contains("AEGIS_USER_TOKEN must be set"),
            "unexpected error message: {msg}"
        );

        // Phase 2: present var returns the value verbatim.
        std::env::set_var("AEGIS_USER_TOKEN", "test-token-value");
        let v = require_user_token().expect("must succeed when set");
        assert_eq!(v, "test-token-value");

        match prior {
            Some(p) => std::env::set_var("AEGIS_USER_TOKEN", p),
            None => std::env::remove_var("AEGIS_USER_TOKEN"),
        }
    }
}
