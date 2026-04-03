// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis up` — start (or install-then-start) the local AEGIS stack.
//!
//! If `~/.aegis/docker-compose.yml` already exists the command refreshes the
//! stack to the requested image tag, updates `aegis-config.yaml`, and then
//! runs `docker compose up -d --wait`, streaming output in real-time.
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
use super::update::{persist_image_tag, refresh_compose, resolve_image_tag};

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
    /// Refreshes `docker-compose.yml` and `aegis-config.yaml` before startup
    /// when the stack already exists.
    /// For existing stacks, defaults to `spec.image_tag` from `aegis-config.yaml`.
    /// Falls back to the version of this binary when no persisted tag exists.
    /// Pass `latest` to track the most recent build, or a semver tag such as
    /// `v0.10.0` to pin to a specific release.
    #[arg(long)]
    pub tag: Option<String>,

    /// Start only services in this Docker Compose profile
    #[arg(long)]
    pub profile: Option<String>,
}

/// Run `aegis up`.
pub async fn run(args: UpArgs) -> Result<()> {
    let dir = expand_tilde(Path::new(&args.dir));
    let config_file_path = dir.join("aegis-config.yaml");

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

        let initial_tag = resolve_image_tag(&config_file_path, args.tag.as_deref());
        init::run(InitArgs {
            yes: args.yes,
            manual: false,
            dir: args.dir.clone(),
            host: args.host.clone(),
            port: args.port,
            tag: initial_tag,
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

    // Stack already installed — refresh the requested image tag, then bring it up.
    println!();
    println!("{}", "Starting AEGIS stack...".bold());

    let image_tag = resolve_image_tag(&config_file_path, args.tag.as_deref());
    refresh_compose(&dir, &image_tag, false).await?;
    persist_image_tag(&config_file_path, &image_tag, false)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        dir.push(format!("aegis-up-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn existing_stack_prefers_persisted_image_tag_when_override_missing() {
        let dir = temp_dir();
        let config_path = dir.join("aegis-config.yaml");

        let mut manifest = NodeConfigManifest::default();
        manifest.spec.image_tag = Some("latest".to_string());
        manifest
            .to_yaml_file(&config_path)
            .expect("write config with latest tag");

        let resolved = resolve_image_tag(&config_path, None);
        assert_eq!(resolved, "latest");

        let _ = fs::remove_dir_all(dir);
    }
}
