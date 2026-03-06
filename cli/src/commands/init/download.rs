// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! GitHub download step of `aegis init`.
//!
//! Fetches the latest docker-compose stack and runtime-registry from the
//! canonical AEGIS GitHub repositories. Files are fetched over HTTPS from
//! `raw.githubusercontent.com` so the user always gets the latest version
//! without a CLI upgrade.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** download step inside the `aegis init` wizard

use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

const EXAMPLES_REPO: &str = "100monkeys-ai/aegis-examples";
const ORCHESTRATOR_REPO: &str = "100monkeys-ai/aegis-orchestrator";
const BRANCH: &str = "main";

/// Raw file manifests to download
struct RemoteFile {
    repo: &'static str,
    path: &'static str,
    label: &'static str,
}

const FILES: &[RemoteFile] = &[
    RemoteFile {
        repo: EXAMPLES_REPO,
        path: "deploy/docker-compose.yml",
        label: "docker-compose.yml",
    },
    RemoteFile {
        repo: EXAMPLES_REPO,
        path: "deploy/init-multiple-dbs.sh",
        label: "init-multiple-dbs.sh",
    },
    RemoteFile {
        repo: ORCHESTRATOR_REPO,
        path: "runtime-registry.yaml",
        label: "runtime-registry.yaml",
    },
    RemoteFile {
        repo: EXAMPLES_REPO,
        path: "agents/hello-world/agent.yaml",
        label: "hello-world/agent.yaml",
    },
];

/// Bundle of all downloaded stack files.
pub struct StackFiles {
    pub docker_compose: String,
    pub init_db_script: String,
    pub runtime_registry: String,
    pub hello_world_agent: String,
}

/// Download the AEGIS stack from GitHub.
pub async fn fetch_stack() -> Result<StackFiles> {
    println!();
    println!("{}", "Downloading stack files from GitHub...".bold());

    let client = reqwest::Client::builder()
        .user_agent(concat!("aegis-init/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("Failed to build HTTP client")?;

    let bar = ProgressBar::new(FILES.len() as u64);
    bar.set_style(
        ProgressStyle::with_template("  [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut contents: Vec<String> = Vec::with_capacity(FILES.len());

    for file in FILES {
        bar.set_message(file.label);
        let url = format!(
            "https://raw.githubusercontent.com/{}/{}/{}",
            file.repo, BRANCH, file.path
        );
        let text = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?
            .error_for_status()
            .with_context(|| format!("HTTP error fetching {}", file.label))?
            .text()
            .await
            .with_context(|| format!("Failed to read body for {}", file.label))?;
        contents.push(text);
        bar.inc(1);
    }

    bar.finish_and_clear();
    println!("  {} All stack files downloaded", "✓".green());

    Ok(StackFiles {
        docker_compose: contents[0].clone(),
        init_db_script: contents[1].clone(),
        runtime_registry: contents[2].clone(),
        hello_world_agent: contents[3].clone(),
    })
}
