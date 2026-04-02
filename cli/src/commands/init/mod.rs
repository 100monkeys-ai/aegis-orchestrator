// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis init` — interactive setup wizard.
//!
//! Guides the user through an eight-step process that downloads the AEGIS stack
//! from GitHub, lets them toggle optional services, generates the node config,
//! starts the Docker Compose stack, applies database migrations, verifies that
//! the orchestrator is healthy, and syncs the built-in agents and workflows
//! needed for first-use flows.
//!
//! ## Flags
//!
//! | Flag | Behaviour |
//! |---|---|
//! | `--yes` | Accept all defaults; skip interactive prompts (CI/scripted use). |
//! | `--manual` | Skip auto-install of missing prerequisites; print instructions. |
//! | `--dir <PATH>` | Working directory for stack files (default: `~/.aegis`). |
//! | `--host` / `--port` | Orchestrator address for health-check (default: 127.0.0.1:8088). |
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Top-level orchestrator for `aegis init`

pub mod components;
pub mod compose;
pub mod configure;
pub mod download;
pub mod prereqs;
pub mod verify;

use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use colored::Colorize;

use self::components::ComponentSelector;
use self::compose::ComposeRunner;
use self::configure::ConfigWizard;
use self::download::fetch_stack;
use self::prereqs::PrereqChecker;
use self::verify::HealthChecker;
use super::update::{run_database_migrations, sync_builtins};

/// Arguments for `aegis init`
#[derive(Args)]
pub struct InitArgs {
    /// Accept all defaults without interactive prompts (suitable for CI)
    #[arg(long)]
    pub yes: bool,

    /// Skip auto-install of prerequisites; print instructions instead
    #[arg(long)]
    pub manual: bool,

    /// Directory in which stack files are written (default: ~/.aegis)
    #[arg(long, default_value = "~/.aegis")]
    pub dir: String,

    /// Orchestrator host to poll for health after startup (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Orchestrator port to poll for health after startup (default: 8088)
    #[arg(long, default_value = "8088")]
    pub port: u16,

    /// Image tag for AEGIS-owned Docker images.
    /// Defaults to the version of this binary.
    /// Pass `latest` to track the most recent build, or a semver tag such as
    /// `v0.10.0` to pin to a specific release.
    #[arg(long, default_value = env!("CARGO_PKG_VERSION"))]
    pub tag: String,

    /// Internal override for whether to run the advanced walkthrough.
    /// `None` keeps the normal interactive prompt behavior.
    #[arg(skip = None)]
    pub advanced_override: Option<bool>,
}

/// Run the `aegis init` wizard.
pub async fn run(args: InitArgs) -> Result<()> {
    print_banner();

    let dir = PathBuf::from(&args.dir);

    // ─── Step 1: Select components ────────────────────────────────────────────
    print_step(1, 8, "Select components");
    let selector = ComponentSelector::new(args.yes);
    let components = selector.select()?;

    // ─── Step 2: Check prerequisites ─────────────────────────────────────────
    print_step(2, 8, "Checking prerequisites");
    let checker = PrereqChecker::new(args.manual, components.ollama_llm);
    checker.check_and_install().await?;

    // ─── Step 3: Prepare stack templates ──────────────────────────────────────
    print_step(3, 8, "Preparing stack files");
    let stack = fetch_stack(&args.tag).await?;

    // ─── Step 4: Configure node ───────────────────────────────────────────────
    print_step(4, 8, "Configuring node");
    let wizard = ConfigWizard::new(args.yes, dir.clone(), args.advanced_override);
    let node_config = wizard.configure(
        &args.tag,
        &components,
        &stack.docker_compose,
        &stack.runtime_registry,
        &stack.seal_gateway_config,
    )?;

    // Also write the db-init script
    let db_script_path = node_config.working_dir.join("init-multiple-dbs.sh");
    std::fs::write(&db_script_path, &stack.init_db_script)?;
    // Make the script executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&db_script_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&db_script_path, perms)?;
    }

    // Write the Temporal dynamic config file that the compose volume mount expects:
    // ./temporal/ → /etc/temporal/config/dynamicconfig/ inside the container
    let temporal_dir = node_config.working_dir.join("temporal");
    std::fs::create_dir_all(&temporal_dir)?;
    std::fs::write(
        temporal_dir.join("development-sql.yaml"),
        &stack.temporal_dynamic_config,
    )?;

    // ─── Step 5: Pull images & start services ────────────────────────────────
    print_step(5, 8, "Starting Docker Compose stack");
    let runner = ComposeRunner::new(node_config.working_dir.clone());
    runner.pull().await?;
    runner.up().await?;
    if components.ollama_llm && !node_config.ollama_model.is_empty() {
        runner.pull_ollama_model(&node_config.ollama_model).await?;
    }

    // ─── Step 6: Apply database migrations ───────────────────────────────────
    print_step(6, 8, "Applying database migrations");
    let config_path = node_config.working_dir.join("aegis-config.yaml");
    run_database_migrations(&node_config.working_dir, Some(config_path.clone()), false).await?;

    // ─── Step 7: Verify health ────────────────────────────────────────────────
    print_step(7, 8, "Verifying health");
    let checker = HealthChecker::new(&args.host, args.port);
    checker.wait_for_healthy().await?;

    // ─── Step 8: Sync built-in agents and workflows ──────────────────────────
    print_step(8, 8, "Syncing built-in agents and workflows");
    sync_builtins(&node_config.working_dir, Some(config_path), false, true).await?;

    // ─── Done ─────────────────────────────────────────────────────────────────
    print_success(&args.host, args.port, &components);

    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn print_banner() {
    println!();
    println!("{}", "╔══════════════════════════════════════╗".cyan());
    println!("{}", "║     AEGIS Setup Wizard (aegis init)  ║".cyan());
    println!("{}", "╚══════════════════════════════════════╝".cyan());
    println!();
    println!("  This wizard sets up your local AEGIS stack from scratch.");
    println!("  Press {} at any time to abort.\n", "Ctrl+C".bold());
}

fn print_step(n: u8, total: u8, label: &str) {
    println!(
        "\n{} {}",
        format!("[{n}/{total}]").bold().cyan(),
        label.bold()
    );
}

fn print_success(host: &str, port: u16, components: &components::SelectedComponents) {
    println!();
    println!("{}", "═══════════════════════════════════════════".green());
    println!("{}", "  ✓  AEGIS is ready!".green().bold());
    println!("{}", "═══════════════════════════════════════════".green());
    println!();
    println!("  {}   http://{}:{}", "API".bold(), host, port);

    if components.temporal {
        println!("  {}   http://localhost:8233", "Temporal UI".bold());
    }
    if components.iam {
        println!("  {}  http://localhost:8180", "Keycloak".bold());
    }
    if components.secrets {
        println!("  {}    http://localhost:8200", "OpenBao".bold());
    }
    if components.storage {
        println!("  {}  http://localhost:9333", "SeaweedFS".bold());
    }
    if components.seal_gateway {
        println!(
            "  {}  http://localhost:8089 (HTTP), localhost:50055 (gRPC)",
            "SEAL Gateway".bold()
        );
    }

    println!();
    println!("  Next steps:");
    println!(
        "    1. Read the docs:           {}",
        "https://docs.100monkeys.ai".cyan()
    );
    println!("    2. Explore the CLI:         {}", "aegis --help".cyan());
    println!("    3. Check stack health:      {}", "aegis status".cyan());
    println!(
        "    4. Deploy your own agent:   {}",
        "aegis agent deploy <manifest.yaml>".cyan()
    );
    println!(
        "    5. Run a task:              {}",
        "aegis task execute hello-world --input \"Hello, AEGIS!\"".cyan()
    );
    println!("    6. Stop the stack:          {}", "aegis down".cyan());
    println!();
}
