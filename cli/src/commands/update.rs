// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis update` — upgrade the AEGIS stack to the latest version.
//!
//! Performs three steps in order:
//!
//! 1. **Pull** — `docker compose pull` fetches the latest image tags.
//! 2. **Restart** — `docker compose up -d --wait` replaces running containers.
//! 3. **Migrate** — applies any pending database schema migrations.
//!
//! Individual steps can be skipped via `--skip-pull`, `--skip-restart`, or
//! `--skip-migrations`. Use `--dry-run` to preview without making changes.
//!
//! # Architecture
//!
//! - **Layer:** CLI/Presentation
//! - **Purpose:** Stack upgrade + database schema migration management
//! - **Integration:** CLI → ComposeRunner → SQLx Migrator → PostgreSQL

use std::path::{Path, PathBuf};

use aegis_orchestrator_core::domain::node_config::{resolve_env_value, NodeConfigManifest};
use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;
use sqlx::postgres::PgPoolOptions;

use super::init::compose::ComposeRunner;

#[derive(Args)]
pub struct UpdateCommand {
    /// Directory where the AEGIS stack files live (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,

    /// Skip pulling new Docker images
    #[arg(long)]
    pub skip_pull: bool,

    /// Skip restarting services after pulling
    #[arg(long)]
    pub skip_restart: bool,

    /// Skip running database migrations
    #[arg(long)]
    pub skip_migrations: bool,

    /// Skip re-deploying built-in agents and workflows
    #[arg(long)]
    pub skip_builtins: bool,

    /// Preview what would happen without making any changes
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn execute(cmd: UpdateCommand, config_path: Option<PathBuf>) -> Result<()> {
    let dir = expand_tilde(Path::new(&cmd.dir));

    println!();
    println!("{}", "AEGIS Update".bold().green());

    if cmd.dry_run {
        println!("  {} dry-run mode — no changes will be made", "ℹ".cyan());
    }

    if !dir.join("docker-compose.yml").exists() {
        anyhow::bail!(
            "No docker-compose.yml found in {}.\nHave you run `aegis init`?",
            dir.display()
        );
    }

    // ─── Step 1: Pull latest images ───────────────────────────────────────────
    if !cmd.skip_pull {
        println!();
        println!("{}", "[1/4] Pulling latest images...".bold());
        if cmd.dry_run {
            println!("  {} would run: docker compose pull", "→".dimmed());
        } else {
            let runner = ComposeRunner::new(dir.clone());
            runner.pull().await?;
        }
    } else {
        println!();
        println!("{}", "[1/4] Pulling latest images... skipped".dimmed());
    }

    // ─── Step 2: Restart services ─────────────────────────────────────────────
    if !cmd.skip_restart {
        println!();
        println!("{}", "[2/4] Restarting services with new images...".bold());
        if cmd.dry_run {
            println!("  {} would run: docker compose up -d --wait", "→".dimmed());
        } else {
            let runner = ComposeRunner::new(dir.clone());
            runner.up().await?;
        }
    } else {
        println!();
        println!("{}", "[2/4] Restarting services... skipped".dimmed());
    }

    // ─── Step 3: DB migrations ────────────────────────────────────────────────
    if !cmd.skip_migrations {
        println!();
        println!("{}", "[3/4] Running database migrations...".bold());

        // Load the stack .env file so that `env:VAR` references in
        // aegis-config.yaml (e.g. `url: env:AEGIS_DATABASE_URL`) resolve
        // correctly when `aegis update` is run from outside the stack dir.
        let env_file = dir.join(".env");
        if env_file.exists() {
            dotenvy::from_path(&env_file).ok();
        }

        let config = NodeConfigManifest::load_or_default(config_path.clone())?;
        let db_config = config
            .spec
            .database
            .as_ref()
            .context("spec.database not configured in aegis-config.yaml")?;
        let database_url =
            resolve_env_value(&db_config.url).context("Failed to resolve spec.database.url")?;

        static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

        if cmd.dry_run {
            println!(
                "  {} would connect to database and run pending migrations",
                "→".dimmed()
            );
            println!(
                "  {} total migrations available: {}",
                "→".dimmed(),
                MIGRATOR.iter().count()
            );
        } else {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&database_url)
                .await
                .context("Failed to connect to database")?;

            let applied_count = sqlx::query("SELECT version FROM _sqlx_migrations")
                .fetch_all(&pool)
                .await
                .map(|r| r.len())
                .unwrap_or(0);

            let total = MIGRATOR.iter().count();
            let pending = total.saturating_sub(applied_count);

            if pending == 0 {
                println!(
                    "  {} Database schema is up to date ({} migrations applied)",
                    "✓".green(),
                    applied_count
                );
            } else {
                println!("  Applying {pending} pending migration(s)...");
                MIGRATOR
                    .run(&pool)
                    .await
                    .context("Failed to apply migrations")?;
                println!("  {} {} migration(s) applied", "✓".green(), pending);
            }
        }
    } else {
        println!();
        println!("{}", "[3/4] Database migrations... skipped".dimmed());
    }

    // ─── Step 4: Built-in templates ───────────────────────────────────────────
    if !cmd.skip_builtins {
        println!();
        println!("{}", "[4/4] Re-deploying built-in templates...".bold());

        // Load the stack .env file so that `env:VAR` references in
        // aegis-config.yaml resolve correctly.
        let env_file = dir.join(".env");
        if env_file.exists() {
            dotenvy::from_path(&env_file).ok();
        }

        let config = NodeConfigManifest::load_or_default(config_path.clone())?;
        let host = config
            .spec
            .network
            .as_ref()
            .map(|n| n.bind_address.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = config.spec.network.as_ref().map(|n| n.port).unwrap_or(8088);

        if cmd.dry_run {
            println!(
                "  {} would connect to daemon and re-deploy all built-in agents and workflows",
                "→".dimmed()
            );
        } else {
            let client = crate::daemon::DaemonClient::new(&host, port)?;
            // Force re-deployment of built-ins to ensure they are updated
            super::builtins::deploy_all_builtins(&client, true).await?;
            println!("  {} All built-in templates updated", "✓".green());
        }
    } else {
        println!();
        println!(
            "{}",
            "[4/4] Re-deploying built-in templates... skipped".dimmed()
        );
    }

    // ─── Done ─────────────────────────────────────────────────────────────────
    println!();
    if cmd.dry_run {
        println!(
            "{}",
            "  ✓  Dry run complete — no changes were made."
                .cyan()
                .bold()
        );
    } else {
        println!("{}", "  ✓  AEGIS updated successfully.".green().bold());
    }

    Ok(())
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}
