// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Database Update Command
//!
//! This module implements the `aegis update` command for applying database
//! migrations to keep the schema in sync with the application version.
//!
//! # Architecture
//!
//! - **Layer:** CLI/Presentation
//! - **Purpose:** Database schema migration management
//! - **Integration:** CLI → SQLx Migrator → PostgreSQL
//!
//! # Features
//!
//! - **Dry Run**: Preview pending migrations without applying them
//! - **Auto-Detection**: Automatically discovers and applies pending migrations
//! - **Status Display**: Shows applied vs. available migration counts
//! - **Safe Execution**: Uses SQLx's built-in migration tracking
//!
//! # Usage
//!
//! ```bash
//! # Apply all pending migrations
//! aegis update
//!
//! # Preview migrations without applying
//! aegis update --dry-run
//! ```
//!
//! # Environment
//!
//! Requires `AEGIS_DATABASE_URL` environment variable to be set.

use clap::Args;
use anyhow::{Result, Context};
use colored::Colorize;
use sqlx::postgres::PgPoolOptions;

#[derive(Args)]
pub struct UpdateCommand {
    /// Perform a dry run without applying changes
    #[arg(long)]
    dry_run: bool,
}

pub async fn execute(cmd: UpdateCommand) -> Result<()> {
    println!("{}", "AEGIS Update".bold().green());
    
    // Load config (we need database URL)
    // We can just check environment or load config.
    // server.rs logic checks env var first.
    let database_url = std::env::var("AEGIS_DATABASE_URL")
        .context("AEGIS_DATABASE_URL environment variable not set. Cannot run updates.")?;

    println!("Connecting to database...");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .context("Failed to connect to database")?;

    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

    // Check status
    let applied_result = sqlx::query("SELECT version FROM _sqlx_migrations")
        .fetch_all(&pool)
        .await;

    let applied_count = match applied_result {
        Ok(rows) => rows.len(),
        Err(_) => 0,
    };

    let total_migrations = MIGRATOR.iter().count();
    
    println!("Migration status: {} applied, {} total available.", applied_count, total_migrations);

    if applied_count < total_migrations {
        if cmd.dry_run {
            println!("Pending migrations found (Dry Run):");
             for migration in MIGRATOR.iter().skip(applied_count) {
                println!(" - {} {}", migration.version, migration.description);
            }
            println!("Skipping application due to --dry-run");
            return Ok(());
        }

        println!("Applying pending migrations...");
        MIGRATOR.run(&pool).await.context("Failed to apply migrations")?;
        println!("{}", "✓ Database updated successfully.".green());
    } else {
        println!("{}", "✓ Database is up to date.".green());
    }

    Ok(())
}
