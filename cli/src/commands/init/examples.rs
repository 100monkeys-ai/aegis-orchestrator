// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Examples loading step of `aegis init`.
//!
//! Offers to deploy the `hello-world` example agent as a smoke-test after the
//! stack is running. Reuses the existing `agent deploy` HTTP client path so
//! there is no duplication.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** examples step inside the `aegis init` wizard

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::Confirm;

use crate::daemon::DaemonClient;

/// Offers to deploy the `hello-world` example agent.
pub struct ExamplesLoader {
    host: String,
    port: u16,
    yes: bool,
}

impl ExamplesLoader {
    pub fn new(host: impl Into<String>, port: u16, yes: bool) -> Self {
        Self {
            host: host.into(),
            port,
            yes,
        }
    }

    /// Prompt (if not `--yes`) and deploy `hello-world` agent.
    ///
    /// `agent_yaml` is the raw YAML string downloaded from `aegis-examples`.
    pub async fn maybe_load_hello_world(&self, agent_yaml: &str) -> Result<()> {
        println!();

        let should_load = if self.yes {
            false // In --yes mode skip the example to avoid side effects
        } else {
            Confirm::new()
                .with_prompt("Deploy the hello-world example agent as a smoke test?")
                .default(true)
                .interact()?
        };

        if !should_load {
            println!("  Skipping example agent.");
            return Ok(());
        }

        println!("{}", "Deploying hello-world example agent...".bold());

        let manifest: aegis_orchestrator_sdk::AgentManifest =
            serde_yaml::from_str(agent_yaml).context("Failed to parse hello-world agent.yaml")?;

        manifest
            .validate()
            .map_err(|e| anyhow::anyhow!("hello-world manifest validation failed: {}", e))?;

        let client = DaemonClient::new(&self.host, self.port)?;
        let agent_id = client.deploy_agent(manifest, false).await?;

        println!("  {} hello-world agent deployed: {}", "✓".green(), agent_id);
        println!();
        println!("  Run a task to test it:");
        println!(
            "    {}",
            format!("aegis task execute --agent {} 'Hello, AEGIS!'", agent_id).cyan()
        );

        Ok(())
    }
}
