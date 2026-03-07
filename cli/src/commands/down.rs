// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis down` — stop the local AEGIS Docker Compose stack.
//!
//! Runs `docker compose down` inside the AEGIS working directory, streaming
//! output in real-time. Passes `--volumes` when requested to also wipe named
//! volumes (destructive — data loss).
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Compose teardown command

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use clap::Args;
use colored::Colorize;
use dialoguer::Confirm;
use indicatif::{ProgressBar, ProgressStyle};

/// Arguments for `aegis down`
#[derive(Args)]
pub struct DownArgs {
    /// Directory where the AEGIS stack files live (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,

    /// Stop only services in this Docker Compose profile
    #[arg(long)]
    pub profile: Option<String>,

    /// Also remove named volumes — destroys all persistent data (database, secrets, etc.)
    #[arg(long)]
    pub volumes: bool,

    /// Skip destructive-action confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// Run `aegis down`.
pub async fn run(args: DownArgs) -> Result<()> {
    let dir = expand_tilde(Path::new(&args.dir));

    if !dir.join("docker-compose.yml").exists() {
        bail!(
            "No docker-compose.yml found in {}.\nHave you run `aegis init`?",
            dir.display()
        );
    }

    println!();
    println!("{}", "Stopping AEGIS stack...".bold());

    let mut compose_args = vec!["down"];
    if args.volumes {
        compose_args.push("--volumes");
        println!(
            "  {} --volumes set: named volumes (database, secrets, etc.) will be removed",
            "⚠".yellow().bold()
        );
        if !args.yes {
            let confirmed = Confirm::new()
                .with_prompt("This will permanently destroy local AEGIS data volumes. Continue?")
                .default(false)
                .interact()?;
            if !confirmed {
                println!("  Aborted — no changes made.");
                return Ok(());
            }
        }
    }

    run_compose(&dir, &compose_args, args.profile.as_deref())?;

    println!();
    println!("  {} Stack stopped.", "✓".green().bold());

    if !args.volumes {
        println!(
            "\n  {}  Data volumes were preserved. To also remove them run:\n      {}",
            "tip:".cyan().bold(),
            "aegis down --volumes".bold()
        );
    }

    Ok(())
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn run_compose(dir: &Path, args: &[&str], profile: Option<&str>) -> Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg}")
            .unwrap()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
    );
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));

    let mut cmd = Command::new("docker");
    cmd.arg("compose");
    if let Some(profile) = profile {
        cmd.arg("--profile");
        cmd.arg(profile);
    }
    cmd.args(args);
    cmd.current_dir(dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .context("Failed to spawn `docker compose` — is Docker installed?")?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let last_stderr: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let spinner_stdout = spinner.clone();
    let stdout_thread = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let trimmed = line.trim().to_string();
            if !trimmed.is_empty() {
                spinner_stdout.set_message(trimmed);
            }
        }
    });

    let spinner_stderr = spinner.clone();
    let last_stderr_clone = Arc::clone(&last_stderr);
    let stderr_thread = thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let trimmed = line.trim().to_string();
            if !trimmed.is_empty() {
                spinner_stderr.set_message(trimmed.clone());
                *last_stderr_clone.lock().unwrap() = trimmed;
            }
        }
    });

    stdout_thread.join().ok();
    stderr_thread.join().ok();

    let status = child
        .wait()
        .context("Failed to wait for `docker compose`")?;
    spinner.finish_and_clear();

    if !status.success() {
        let msg = last_stderr.lock().unwrap().clone();
        let rendered_args = match profile {
            Some(profile) => format!("--profile {profile} {}", args.join(" ")),
            None => args.join(" "),
        };
        bail!(
            "`docker compose {}` failed (exit {}): {}",
            rendered_args,
            status.code().unwrap_or(-1),
            msg
        );
    }

    Ok(())
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}
