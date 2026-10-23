// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! `aegis update` — upgrade the AEGIS stack to the latest version.
//!
//! Performs four steps in order:
//!
//! 1. **Pull** — `docker compose pull` fetches the latest image tags.
//! 2. **Restart** — `docker compose up -d --wait` replaces running containers.
//! 3. **Confirm migrations** — the runtime applies pending schema migrations
//!    when it starts; this step reads `/health/ready` and requires its
//!    database to be ready. Nothing on the host connects to the database.
//! 4. **Sync built-ins** — re-deploys bundled agents and workflows.
//!
//! Individual steps can be skipped via `--skip-pull`, `--skip-restart`, or
//! `--skip-migrations`. Use `--dry-run` to preview without making changes.
//!
//! # Architecture
//!
//! - **Layer:** CLI/Presentation
//! - **Purpose:** Stack upgrade + confirmation of the runtime's schema migrations
//! - **Integration:** CLI → ComposeRunner → runtime `/health/ready`

use std::path::{Path, PathBuf};
use std::time::Duration;

use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;
use anyhow::{Context, Result};
use clap::Args;
use colored::Colorize;

use super::init::compose::ComposeRunner;
use super::init::download::fetch_stack;
use crate::output::{structured_output_unsupported, OutputFormat};

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

    /// Skip confirming that the runtime applied its database migrations
    #[arg(long)]
    pub skip_migrations: bool,

    /// Skip re-deploying built-in agents and workflows
    #[arg(long)]
    pub skip_builtins: bool,

    /// Preview what would happen without making any changes
    #[arg(long)]
    pub dry_run: bool,

    /// Image tag for AEGIS-owned Docker images.
    /// If not set, reads `spec.image_tag` from the node config.
    /// Falls back to the version of this binary if neither is present.
    #[arg(long)]
    pub tag: Option<String>,
}

pub async fn execute(
    cmd: UpdateCommand,
    config_path: Option<PathBuf>,
    output_format: OutputFormat,
) -> Result<()> {
    if output_format.is_structured() {
        return structured_output_unsupported("aegis update", output_format);
    }

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

    // ─── Resolve image tag ────────────────────────────────────────────────────
    let config_file_path = config_path
        .clone()
        .unwrap_or_else(|| dir.join("aegis-config.yaml"));
    let image_tag = resolve_image_tag(&config_file_path, cmd.tag.as_deref());
    println!();
    println!("  {} Targeting image tag: {}", "→".cyan(), image_tag.bold());

    // ─── Rewrite docker-compose.yml with resolved tag ─────────────────────────
    refresh_compose(&dir, &image_tag, cmd.dry_run).await?;

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
    // The runtime applies pending migrations when it starts, so a restart is
    // what migrates; this step only confirms it through the runtime's
    // readiness answer. The host never connects to the database: the stack's
    // spec.database.url names the compose service `postgres`.
    if cmd.skip_migrations {
        println!();
        println!("{}", "[3/4] Database migrations... skipped".dimmed());
    } else if cmd.skip_restart {
        println!();
        println!(
            "{}",
            "[3/4] Database migrations: the runtime applies them when it starts; \
             services were not restarted, so none were applied now"
                .dimmed()
        );
    } else {
        println!();
        println!(
            "{}",
            "[3/4] Confirming the runtime applied database migrations...".bold()
        );
        load_stack_env(&dir);
        let config = NodeConfigManifest::load_or_default(config_path.clone())?;
        let (host, port) = daemon_host_port(&config);
        confirm_database_migrations(&host, port, cmd.dry_run).await?;
    }

    // ─── Step 4: Built-in templates ───────────────────────────────────────────
    if !cmd.skip_builtins {
        println!();
        println!("{}", "[4/4] Re-deploying built-in templates...".bold());
        sync_builtins(&dir, config_path.clone(), cmd.dry_run, true).await?;
    } else {
        println!();
        println!(
            "{}",
            "[4/4] Re-deploying built-in templates... skipped".dimmed()
        );
    }

    // ─── Persist updated image_tag in node config ─────────────────────────────
    persist_image_tag(&config_file_path, &image_tag, cmd.dry_run)?;

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

pub(crate) fn resolve_image_tag(config_file_path: &Path, requested_tag: Option<&str>) -> String {
    requested_tag
        .map(str::to_string)
        .or_else(|| {
            NodeConfigManifest::load_or_default(Some(config_file_path.to_path_buf()))
                .ok()
                .and_then(|c| c.spec.image_tag)
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

/// The daemon's host and port as the node config names them, for a client on
/// the same machine as the stack.
pub(crate) fn daemon_host_port(config: &NodeConfigManifest) -> (String, u16) {
    let host = config
        .spec
        .network
        .as_ref()
        .map(|n| n.bind_address.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = config.spec.network.as_ref().map(|n| n.port).unwrap_or(8088);
    (host, port)
}

/// Whether the runtime's `/health/ready` answer says its database is
/// connected and migrated.
///
/// `daemon::server::connect_postgres` applies pending migrations before it
/// hands the pool to the repositories, and readiness reports
/// `dependencies.database` true only once that pool exists, so this answer is
/// the runtime's own word on its schema. The host never connects to the
/// database itself: the stack's `spec.database.url` names the compose service
/// `postgres`, which only the compose network resolves, and the previous
/// host-side migration failed on every `aegis init` with "failed to lookup
/// address information" (2026-10-23, 1.2.0-beta).
pub(crate) fn migrations_applied(readiness: &serde_json::Value) -> Result<()> {
    match readiness
        .pointer("/dependencies/database")
        .and_then(|v| v.as_bool())
    {
        Some(true) => Ok(()),
        Some(false) => anyhow::bail!(
            "the runtime reports its database as not ready (status {}): its migrations \
             were not applied; check `docker compose logs aegis-runtime`",
            readiness
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        ),
        None => anyhow::bail!(
            "the runtime's readiness answer carries no dependencies.database field \
             ({readiness}); check `docker compose logs aegis-runtime`"
        ),
    }
}

/// Read the runtime's readiness and require its database to be ready.
pub(crate) async fn confirm_database_migrations(
    host: &str,
    port: u16,
    dry_run: bool,
) -> Result<()> {
    let url = format!("http://{host}:{port}/health/ready");
    if dry_run {
        println!(
            "  {} would read {url} and require dependencies.database to be true",
            "→".dimmed()
        );
        return Ok(());
    }

    let readiness: serde_json::Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to build HTTP client")?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("Failed to reach the runtime at {url}"))?
        .json()
        .await
        .with_context(|| format!("The runtime's answer at {url} is not JSON"))?;
    migrations_applied(&readiness)?;
    println!(
        "  {} The runtime applied its database migrations",
        "✓".green()
    );
    Ok(())
}

pub(crate) async fn sync_builtins(
    dir: &Path,
    config_path: Option<PathBuf>,
    dry_run: bool,
    force: bool,
) -> Result<()> {
    load_stack_env(dir);

    let config = NodeConfigManifest::load_or_default(config_path)?;
    let (host, port) = daemon_host_port(&config);

    if dry_run {
        println!(
            "  {} would connect to daemon and sync built-in agents and workflows",
            "→".dimmed()
        );
        return Ok(());
    }

    let client = crate::daemon::DaemonClient::new(&host, port)?;
    super::builtins::deploy_all_builtins(&client, force).await?;
    println!("  {} Built-in agents and workflows are ready", "✓".green());

    Ok(())
}

fn load_stack_env(dir: &Path) {
    let env_file = dir.join(".env");
    if env_file.exists() {
        dotenvy::from_path(&env_file).ok();
    }
}

pub(crate) async fn refresh_compose(dir: &Path, image_tag: &str, dry_run: bool) -> Result<()> {
    let stack = fetch_stack(image_tag).await?;
    if dry_run {
        println!(
            "  {} would rewrite docker-compose.yml with tag: {}",
            "→".dimmed(),
            image_tag
        );
    } else {
        std::fs::write(dir.join("docker-compose.yml"), &stack.docker_compose)
            .context("Failed to rewrite docker-compose.yml")?;
        println!("  {} docker-compose.yml updated", "✓".green());
    }

    Ok(())
}

pub(crate) fn persist_image_tag(
    config_file_path: &Path,
    image_tag: &str,
    dry_run: bool,
) -> Result<()> {
    println!();
    if dry_run {
        println!(
            "  {} would update spec.image_tag to: {}",
            "→".dimmed(),
            image_tag
        );
    } else if config_file_path.exists() {
        let raw = std::fs::read_to_string(config_file_path)
            .context("Failed to read aegis-config.yaml")?;
        let mut value: serde_yaml::Value =
            serde_yaml::from_str(&raw).context("Failed to parse aegis-config.yaml")?;
        if let Some(spec) = value.get_mut("spec") {
            spec["image_tag"] = serde_yaml::Value::String(image_tag.to_string());
        }
        let updated =
            serde_yaml::to_string(&value).context("Failed to serialize updated config")?;
        std::fs::write(config_file_path, updated)
            .context("Failed to write updated aegis-config.yaml")?;
        println!("  {} spec.image_tag updated to: {}", "✓".green(), image_tag);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;

    // The runtime applies its own migrations at startup (daemon::server::connect_postgres)
    // and `/health/ready` reports `dependencies.database` true only once the
    // migrated pool exists. `aegis init` and `aegis update` confirm the schema
    // through that answer and never connect to the database from the host,
    // whose `.env` names the compose service `postgres` that the host cannot
    // resolve (Jeshua's `aegis init` on 1.2.0-beta, 2026-10-23:
    // "failed to lookup address information: Name or service not known").

    #[test]
    fn readiness_with_database_true_means_the_runtime_applied_its_migrations() {
        let ready = serde_json::json!({
            "status": "ready",
            "dependencies": { "database": true, "temporal": true }
        });
        migrations_applied(&ready).expect("a ready database is an applied schema");
    }

    #[test]
    fn readiness_with_database_false_is_refused_and_names_the_runtime_log() {
        let degraded = serde_json::json!({
            "status": "degraded",
            "dependencies": { "database": false, "temporal": true }
        });
        let err = migrations_applied(&degraded)
            .expect_err("a database that is not ready is not a migrated schema")
            .to_string();
        assert!(err.contains("database"), "{err}");
        assert!(err.contains("docker compose logs aegis-runtime"), "{err}");
    }

    #[test]
    fn readiness_without_a_dependencies_block_is_refused() {
        let unknown = serde_json::json!({ "status": "ready" });
        assert!(migrations_applied(&unknown).is_err());
    }
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        dir.push(format!("aegis-update-test-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn refresh_compose_and_persist_image_tag_updates_compose_and_config() {
        let dir = temp_dir();
        let config_path = dir.join("aegis-config.yaml");

        let mut manifest = NodeConfigManifest::default();
        manifest.spec.image_tag = Some("old-tag".to_string());
        manifest
            .to_yaml_file(&config_path)
            .expect("write initial config");

        refresh_compose(&dir, "new-tag", false)
            .await
            .expect("refresh compose");
        persist_image_tag(&config_path, "new-tag", false).expect("persist tag");

        let compose =
            fs::read_to_string(dir.join("docker-compose.yml")).expect("read refreshed compose");
        assert!(compose.contains("new-tag"));

        let updated =
            NodeConfigManifest::from_yaml_file(&config_path).expect("load updated config");
        assert_eq!(updated.spec.image_tag.as_deref(), Some("new-tag"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_image_tag_prefers_explicit_override() {
        let dir = temp_dir();
        let config_path = dir.join("aegis-config.yaml");

        let mut manifest = NodeConfigManifest::default();
        manifest.spec.image_tag = Some("latest".to_string());
        manifest
            .to_yaml_file(&config_path)
            .expect("write config with latest tag");

        let resolved = resolve_image_tag(&config_path, Some("v1.2.3"));
        assert_eq!(resolved, "v1.2.3");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn resolve_image_tag_uses_persisted_config_tag_when_override_missing() {
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
