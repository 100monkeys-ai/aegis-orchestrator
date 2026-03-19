// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis up` — start (or install-then-start) the local AEGIS stack.
//!
//! If `~/.aegis/docker-compose.yml` already exists the command simply runs
//! `docker compose up -d --wait`, streaming output in real-time.
//!
//! If the working directory or compose file is missing the full `aegis init`
//! wizard is invoked automatically so the user never has to remember which
//! command to run first.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Idempotent stack-start command

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;

use super::init::{self, compose::ComposeRunner, InitArgs};

/// Arguments for `aegis up`
#[derive(Args)]
pub struct UpArgs {
    /// Directory where the AEGIS stack files live (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,

    /// Orchestrator host to poll for health after startup (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Orchestrator port to poll for health after startup (default: 8088)
    #[arg(long, default_value = "8088")]
    pub port: u16,

    /// Accept all defaults without interactive prompts when `aegis init` is
    /// triggered automatically (suitable for CI)
    #[arg(long)]
    pub yes: bool,

    /// Image tag for AEGIS-owned Docker images.
    /// Defaults to the version of this binary.
    /// Pass `latest` to track the most recent build, or a semver tag such as
    /// `v0.10.0` to pin to a specific release.
    #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
    pub tag: String,

    /// Start only services in this Docker Compose profile
    #[arg(long)]
    pub profile: Option<String>,
}

/// Run `aegis up`.
pub async fn run(args: UpArgs) -> Result<()> {
    let dir = expand_tilde(Path::new(&args.dir));

    if !dir.join("docker-compose.yml").exists() {
        // Stack has never been set up — run the full init wizard first.
        println!();
        println!(
            "  {} No AEGIS stack found in {} — running {} first...",
            "ℹ".cyan(),
            dir.display(),
            "aegis init".bold()
        );
        println!();

        let advanced_override = if args.yes {
            Some(false)
        } else {
            Some(
                Confirm::new()
                    .with_prompt("Run advanced configuration walkthrough?")
                    .default(false)
                    .interact()?,
            )
        };

        init::run(InitArgs {
            yes: args.yes,
            manual: false,
            dir: args.dir.clone(),
            host: args.host.clone(),
            port: args.port,
            tag: args.tag.clone(),
            advanced_override,
        })
        .await?;

        if args.profile.is_some() {
            println!(
                "  {} `--profile` is ignored during first-time setup (`aegis init` controls initial profile selection).",
                "ℹ".cyan()
            );
        }

        // init already starts the stack; nothing more to do.
        return Ok(());
    }

    // Stack already installed — just bring it up.
    println!();
    println!("{}", "Starting AEGIS stack...".bold());

    let runner = ComposeRunner::new(dir.clone());
    runner.up_with_profile(args.profile.as_deref()).await?;

    println!();
    println!("{}", "  ✓  AEGIS is up!".green().bold());
    println!("\n  {}   http://{}:{}", "API".bold(), args.host, args.port);
    println!(
        "\n  Run {} to view real-time logs.",
        format!(
            "docker compose -f {}/docker-compose.yml logs --follow",
            dir.display()
        )
        .cyan()
    );

    Ok(())
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();

    if s == "~" || s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            if s == "~" {
                return home;
            } else {
                // Safe to slice from index 2 because we've confirmed the prefix "~/".
                let rest = &s[2..];
                return home.join(rest);
            }
        }
    }

    path.to_path_buf()
}
