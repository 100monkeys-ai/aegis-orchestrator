// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Health verification step of `aegis init`.
//!
//! Polls the AEGIS HTTP `/health` endpoint until the orchestrator reports
//! healthy, then prints a success banner with the key service URLs.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** verify step inside the `aegis init` wizard

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_WAIT: Duration = Duration::from_secs(120);

/// Polls the orchestrator `/health` endpoint until it responds.
pub struct HealthChecker {
    host: String,
    port: u16,
}

impl HealthChecker {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// Wait until the orchestrator is healthy or the timeout expires.
    pub async fn wait_for_healthy(&self) -> Result<()> {
        println!();
        println!("{}", "Waiting for AEGIS to become healthy...".bold());

        let url = format!("http://{}:{}/health", self.host, self.port);

        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
        );
        spinner.enable_steady_tick(Duration::from_millis(80));
        spinner.set_message(format!("Polling {url}..."));

        let start = Instant::now();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .context("Failed to build HTTP client")?;

        loop {
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    spinner.finish_and_clear();
                    println!("  {} AEGIS is healthy", "✓".green());
                    return Ok(());
                }
                Ok(resp) => {
                    spinner.set_message(format!("HTTP {} — retrying...", resp.status()));
                }
                Err(e) => {
                    spinner.set_message(format!("Not reachable yet ({e}) — retrying..."));
                }
            }

            if start.elapsed() > MAX_WAIT {
                spinner.finish_and_clear();
                bail!(
                    "AEGIS did not become healthy within {}s. \
                     Check `docker compose logs aegis-runtime` for errors.",
                    MAX_WAIT.as_secs()
                );
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
