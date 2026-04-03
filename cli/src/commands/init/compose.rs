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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
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
        self.up_with_profile(None).await
    }

    /// Start services in detached mode, optionally limited to a compose profile.
    pub async fn up_with_profile(&self, profile: Option<&str>) -> Result<()> {
        println!();
        match profile {
            Some(profile) => println!(
                "{}",
                format!("Starting services in profile `{profile}`...").bold()
            ),
            None => println!("{}", "Starting services...".bold()),
        }

        if let Err(e) = self.run_compose_with_profile(&["up", "-d", "--wait"], profile) {
            // Print container logs so the user can see *why* a service crashed
            // without having to manually run `docker compose logs`.
            eprintln!();
            eprintln!(
                "{}",
                "── Container logs ──────────────────────────────".dimmed()
            );
            let logs = self.collect_logs();
            if logs.trim().is_empty() {
                eprintln!("{}", "  (no container logs captured)".dimmed());
            } else {
                for line in logs.lines() {
                    eprintln!("  {line}");
                }
            }
            eprintln!(
                "{}",
                "────────────────────────────────────────────────".dimmed()
            );
            eprintln!();
            eprintln!("{}  To inspect logs at any time run:", "tip:".cyan().bold());
            eprintln!(
                "      docker compose -f {} logs --follow",
                self.dir.join("docker-compose.yml").display()
            );
            eprintln!();
            return Err(e);
        }

        println!("  {} Services started", "✓".green());
        Ok(())
    }

    /// Pull an Ollama model inside the running `ollama` service container.
    pub async fn pull_ollama_model(&self, model: &str) -> Result<()> {
        println!();
        println!("{} {}", "Pulling Ollama model:".bold(), model.bold().cyan());

        // Run quietly so Ollama's animated progress output does not corrupt
        // subsequent wizard prompts and step output.
        let status = Command::new("docker")
            .arg("compose")
            .args(["exec", "-T", "ollama", "ollama", "pull", model])
            .current_dir(&self.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("Failed to run `docker compose exec ollama ollama pull`")?;

        if !status.success() {
            bail!(
                "Failed to pull Ollama model '{}' (exit {})",
                model,
                status.code().unwrap_or(-1)
            );
        }

        // Ensure model is actually available before continuing.
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let show_status = Command::new("docker")
                .arg("compose")
                .args(["exec", "-T", "ollama", "ollama", "show", model])
                .current_dir(&self.dir)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .context("Failed to verify pulled Ollama model")?;

            if show_status.success() {
                break;
            }

            if Instant::now() >= deadline {
                bail!("Timed out waiting for Ollama model '{model}' to become available");
            }
            thread::sleep(Duration::from_secs(2));
        }

        println!("  {} Ollama model ready: {}", "✓".green(), model);
        Ok(())
    }

    /// Restart running services. When `profile` is set, only services in that
    /// compose profile are restarted.
    pub async fn restart(&self, profile: Option<&str>) -> Result<()> {
        println!();
        match profile {
            Some(profile) => {
                println!(
                    "{}",
                    format!("Restarting services in profile `{profile}`...").bold()
                );
                self.run_compose_with_profile(&["down"], Some(profile))?;
                self.run_compose_with_profile(&["up", "-d", "--wait"], Some(profile))?;
            }
            None => {
                println!("{}", "Restarting all services...".bold());
                self.run_compose_with_profile(&["down"], None)?;
                self.run_compose_with_profile(&["up", "-d", "--wait"], None)?;
            }
        }

        println!("  {} Services restarted", "✓".green());
        Ok(())
    }

    /// Collect recent logs from all services for post-failure diagnostics.
    /// Returns an empty string if the command fails (best-effort).
    fn collect_logs(&self) -> String {
        let output = Command::new("docker")
            .arg("compose")
            .args(["logs", "--tail=60", "--no-color"])
            .current_dir(&self.dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                format!("{stdout}{stderr}")
            }
            Err(_) => String::new(),
        }
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    /// Run a `docker compose` sub-command, streaming stdout/stderr lines to the
    /// terminal. Returns `Ok(())` if the command exits 0, otherwise an error
    /// containing the last non-empty stderr line.
    fn run_compose(&self, args: &[&str]) -> Result<()> {
        self.run_compose_with_profile(args, None)
    }

    /// Run a `docker compose` sub-command, optionally scoping enabled services
    /// using a compose profile.
    fn run_compose_with_profile(&self, args: &[&str], profile: Option<&str>) -> Result<()> {
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
}
