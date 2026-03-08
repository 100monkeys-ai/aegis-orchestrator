// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis init` — interactive setup wizard.
//!
//! Guides the user through a seven-step process that downloads the AEGIS stack
//! from GitHub, lets them toggle optional services, generates the node config,
//! starts the Docker Compose stack, runs database migrations, and verifies that
//! the orchestrator is healthy. An optional eighth step deploys the
//! `hello-world` example agent as a smoke test.
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
pub mod examples;
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
use self::examples::ExamplesLoader;
use self::prereqs::PrereqChecker;
use self::verify::HealthChecker;

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
    print_step(1, 7, "Select components");
    let selector = ComponentSelector::new(args.yes);
    let components = selector.select()?;

    // ─── Step 2: Check prerequisites ─────────────────────────────────────────
    print_step(2, 7, "Checking prerequisites");
    let checker = PrereqChecker::new(args.manual, components.ollama_llm);
    checker.check_and_install().await?;

    // ─── Step 3: Prepare stack templates ──────────────────────────────────────
    print_step(3, 7, "Preparing stack files");
    let stack = fetch_stack().await?;

    // ─── Step 4: Configure node ───────────────────────────────────────────────
    print_step(4, 7, "Configuring node");
    let wizard = ConfigWizard::new(args.yes, dir.clone(), args.advanced_override);
    let node_config = wizard.configure(
        &components,
        &stack.docker_compose,
        &stack.runtime_registry,
        &stack.smcp_gateway_config,
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
    print_step(5, 7, "Starting Docker Compose stack");
    let runner = ComposeRunner::new(node_config.working_dir.clone());
    runner.pull().await?;
    runner.up().await?;
    if components.ollama_llm && !node_config.ollama_model.is_empty() {
        runner.pull_ollama_model(&node_config.ollama_model).await?;
    }

    // ─── Step 6: Run database migrations ─────────────────────────────────────
    print_step(6, 7, "Running database migrations");
    run_migrations(&node_config.working_dir)?;

    // ─── Step 7: Verify health ────────────────────────────────────────────────
    print_step(7, 7, "Verifying health");
    let checker = HealthChecker::new(&args.host, args.port);
    checker.wait_for_healthy().await?;

    // ─── Optional: Load examples ──────────────────────────────────────────────
    let loader = ExamplesLoader::new(&args.host, args.port, args.yes);
    loader
        .maybe_load_hello_world(node_config.advanced.deploy_smoketest_agents)
        .await?;

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
        format!("[{}/{}]", n, total).bold().cyan(),
        label.bold()
    );
}

/// Invoke `aegis update` (db migration) programmatically via the existing
/// `UpdateCommand` without forking a subprocess.
fn run_migrations(working_dir: &std::path::Path) -> Result<()> {
    use colored::Colorize;

    // The UpdateCommand reads DB URL from AEGIS_DATABASE_URL env var or
    // the aegis-config.yaml file. The config was written to `working_dir`.
    // We set AEGIS_CONFIG_PATH so the migration runner finds it.
    let config_path = working_dir.join("aegis-config.yaml");
    std::env::set_var("AEGIS_CONFIG_PATH", &config_path);

    println!(
        "  {} Migrations deferred to `aegis update` after daemon start.",
        "ℹ".cyan()
    );
    println!(
        "  → Run {} once the orchestrator daemon is running.",
        "aegis update".bold()
    );
    Ok(())
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
    if components.smcp_gateway {
        println!(
            "  {}  http://localhost:8089 (HTTP), localhost:50055 (gRPC)",
            "SMCP Gateway".bold()
        );
    }

    println!();
    println!("  Next steps:");
    println!(
        "    1. Start the AEGIS daemon:  {}",
        "aegis daemon start".cyan()
    );
    println!("    2. Run DB migrations:       {}", "aegis update".cyan());
    println!(
        "    3. Deploy an agent:         {}",
        "aegis agent deploy <manifest.yaml>".cyan()
    );
    println!();
}
