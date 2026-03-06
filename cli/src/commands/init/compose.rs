// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Docker Compose orchestration step of `aegis init`.
//!
//! Runs `docker compose pull` and `docker compose up -d` inside the working
//! directory where the downloaded stack files were written. Streams command
//! output with an `indicatif` spinner so the user is not left watching a blank
//! terminal.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** compose step inside the `aegis init` wizard

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{bail, Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

/// Drives the `docker compose` lifecycle during init.
pub struct ComposeRunner {
    dir: PathBuf,
}

impl ComposeRunner {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Pull all images listed in the compose file.
    pub async fn pull(&self) -> Result<()> {
        println!();
        println!(
            "{}",
            "Pulling Docker images (this may take a while)...".bold()
        );
        self.run_compose(&["pull"])?;
        println!("  {} Images pulled", "✓".green());
        Ok(())
    }

    /// Start all services in detached mode.
    pub async fn up(&self) -> Result<()> {
        println!();
        println!("{}", "Starting services...".bold());
        self.run_compose(&["up", "-d", "--wait"])?;
        println!("  {} Services started", "✓".green());
        Ok(())
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    /// Run a `docker compose` sub-command, streaming stdout/stderr lines to the
    /// terminal. Returns `Ok(())` if the command exits 0, otherwise an error
    /// containing the last non-empty stderr line.
    fn run_compose(&self, args: &[&str]) -> Result<()> {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("  {spinner:.cyan} {msg}")
                .unwrap()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
        );
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        let mut cmd = Command::new("docker");
        cmd.arg("compose");
        cmd.args(args);
        cmd.current_dir(&self.dir);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .context("Failed to spawn `docker compose` — is Docker installed?")?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        // Shared last-seen stderr line for the error message on failure.
        let last_stderr: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        // Drain stdout on a background thread, updating the spinner in real-time.
        let spinner_stdout = spinner.clone();
        let stdout_thread = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    spinner_stdout.set_message(trimmed);
                }
            }
        });

        // Drain stderr on a background thread — same real-time spinner updates
        // and capture of the last line for error reporting.
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

        // Wait for both reader threads before waiting on the child process,
        // so we never block with a full pipe buffer.
        stdout_thread.join().ok();
        stderr_thread.join().ok();

        let status = child
            .wait()
            .context("Failed to wait for `docker compose`")?;
        spinner.finish_and_clear();

        if !status.success() {
            let msg = last_stderr.lock().unwrap().clone();
            bail!(
                "`docker compose {}` failed (exit {}): {}",
                args.join(" "),
                status.code().unwrap_or(-1),
                msg
            );
        }

        Ok(())
    }
}
