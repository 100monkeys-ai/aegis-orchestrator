// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis restart` — restart the local AEGIS Docker Compose services.
//!
//! Restarts all running services by default. When `--profile` is provided,
//! restarts only services in that compose profile.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Compose service restart command

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Args;

use super::init::compose::ComposeRunner;

/// Arguments for `aegis restart`
#[derive(Args)]
pub struct RestartArgs {
    /// Directory where the AEGIS stack files live (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,

    /// Restart only services in this Docker Compose profile
    #[arg(long)]
    pub profile: Option<String>,
}

/// Run `aegis restart`.
pub async fn run(args: RestartArgs) -> Result<()> {
    let dir = expand_tilde(Path::new(&args.dir));

    if !dir.join("docker-compose.yml").exists() {
        bail!(
            "No docker-compose.yml found in {}.\nHave you run `aegis init`?",
            dir.display()
        );
    }

    let runner = ComposeRunner::new(dir);
    runner.restart(args.profile.as_deref()).await
}

fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}
