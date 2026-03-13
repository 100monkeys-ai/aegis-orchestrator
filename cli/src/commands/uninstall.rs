// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis uninstall` — stop all containers and remove the AEGIS data directory.
//!
//! Prompts for confirmation, runs `docker compose down --volumes` to tear down
//! the stack and wipe named volumes, then deletes the AEGIS working directory
//! (`~/.aegis` by default).
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Teardown and cleanup command

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;

/// Arguments for `aegis uninstall`
#[derive(Args)]
pub struct UninstallArgs {
    /// AEGIS working directory to remove (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,

    /// Skip the confirmation prompt (non-interactive / CI use)
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// Run `aegis uninstall`.
pub async fn run(args: UninstallArgs) -> Result<()> {
    let dir = expand_tilde(Path::new(&args.dir));

    if !dir.exists() {
        println!(
            "  {} {} does not exist — nothing to uninstall.",
            "ℹ".cyan(),
            dir.display()
        );
        return Ok(());
    }

    println!();
    println!("{}", "AEGIS Uninstall".bold().red());
    println!();
    println!("  This will:");
    println!(
        "    1. Run {} — stops all containers and removes their volumes",
        "docker compose down --volumes".bold()
    );
    println!(
        "    2. Permanently delete {}",
        dir.display().to_string().bold()
    );
    println!();

    if !args.yes {
        let confirmed = Confirm::new()
            .with_prompt(format!(
                "Delete {} and all its contents? This cannot be undone",
                dir.display()
            ))
            .default(false)
            .interact()?;

        if !confirmed {
            println!("  Aborted — nothing was changed.");
            return Ok(());
        }
    }

    // ─── Step 1: Bring the stack down (best-effort) ────────────────────────────
    let compose_file = dir.join("docker-compose.yml");
    if compose_file.exists() {
        println!();
        println!("{}", "Stopping stack and removing volumes...".bold());

        let status = std::process::Command::new("docker")
            .arg("compose")
            .args(["down", "--volumes"])
            .current_dir(&dir)
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("  {} Stack stopped and volumes removed.", "✓".green())
            }
            Ok(s) => println!(
                "  {} `docker compose down --volumes` exited {} — continuing with directory removal.",
                "⚠".yellow(),
                s.code().unwrap_or(-1)
            ),
            Err(e) => println!(
                "  {} Could not run `docker compose down`: {} — continuing with directory removal.",
                "⚠".yellow(),
                e
            ),
        }
    } else {
        println!(
            "  {} No docker-compose.yml found — skipping compose teardown.",
            "ℹ".cyan()
        );
    }

    // ─── Step 2: Remove the directory ─────────────────────────────────────────
    println!();
    println!("{}", format!("Removing {}...", dir.display()).bold());

    match std::fs::remove_dir_all(&dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            println!(
                "  {} Permission denied while removing {}. Attempting automatic permission repair...",
                "⚠".yellow(),
                dir.display()
            );
            repair_permissions_for_removal(&dir)?;
            std::fs::remove_dir_all(&dir).map_err(|retry_err| {
                anyhow::anyhow!(
                    "Failed to remove {} after permission repair: {}",
                    dir.display(),
                    retry_err
                )
            })?;
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to remove {}: {}", dir.display(), e));
        }
    }

    println!("  {} {} removed.", "✓".green(), dir.display());

    // ─── Done ─────────────────────────────────────────────────────────────────
    println!();
    println!("{}", "AEGIS has been uninstalled.".bold());
    println!("  Run {} to set it up again.", "aegis init".cyan());

    Ok(())
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

/// Best-effort permission repair for docker-created root-owned files.
/// Uses a root container to recursively chmod the target directory.
fn repair_permissions_for_removal(dir: &Path) -> Result<()> {
    let mount_arg = format!("{}:/target", dir.display());
    let status = std::process::Command::new("docker")
        .args([
            "run",
            "--rm",
            "-v",
            &mount_arg,
            "alpine:3.20",
            "sh",
            "-c",
            "chmod -R a+rwx /target",
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            println!(
                "  {} Repaired permissions in {}",
                "✓".green(),
                dir.display()
            );
            Ok(())
        }
        Ok(s) => Err(anyhow::anyhow!(
            "Permission repair container exited {}",
            s.code().unwrap_or(-1)
        )),
        Err(e) => Err(anyhow::anyhow!(
            "Failed to run docker permission repair: {e}"
        )),
    }
}
