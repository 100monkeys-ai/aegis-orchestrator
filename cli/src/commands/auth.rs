// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Authentication commands for the AEGIS CLI
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements aegis auth login/logout/status/switch/token commands

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;

use crate::auth;
use crate::output::{render_serialized, OutputFormat};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Authenticate with an AEGIS environment using your browser.
    Login {
        /// Environment hostname (e.g. dev.100monkeys.ai).
        /// Auth endpoint is derived as `https://auth.<hostname>`, API as `https://api.<hostname>`.
        /// Precedence: --env flag > AEGIS_ENV env var > "dev.100monkeys.ai" default.
        #[arg(long)]
        env: Option<String>,
        /// Exit non-zero if not already authenticated (for CI/CD pipelines).
        #[arg(long)]
        non_interactive: bool,
    },
    /// Revoke the current session and clear local credentials.
    Logout,
    /// Show the current authentication status.
    Status,
    /// Switch to a different stored auth profile.
    Switch {
        /// Profile name to activate.
        profile: String,
    },
    /// Print the current access key to stdout (for scripting).
    Token,
}

pub async fn handle_command(command: AuthCommand, output_format: OutputFormat) -> Result<()> {
    match command {
        AuthCommand::Login {
            env,
            non_interactive,
        } => {
            let env = env
                .or_else(crate::auth::env::aegis_env)
                .unwrap_or_else(|| "dev.100monkeys.ai".to_string());
            login(&env, non_interactive).await
        }
        AuthCommand::Logout => logout().await,
        AuthCommand::Status => status(output_format).await,
        AuthCommand::Switch { profile } => switch_profile(&profile),
        AuthCommand::Token => print_token().await,
    }
}

async fn login(env: &str, non_interactive: bool) -> Result<()> {
    if non_interactive {
        match auth::require_key().await {
            Ok(_) => {
                println!("{} Already authenticated.", "✓".green());
                return Ok(());
            }
            Err(e) => anyhow::bail!("{e}"),
        }
    }

    println!("{} Authenticating with {}...", "»".dimmed(), env.cyan());
    let profile = auth::run_device_flow(env).await?;
    let profile_name = profile.name.clone();
    let roles = profile.roles.join(", ");

    let mut store = auth::load_store().unwrap_or_default();
    store.profiles.insert(profile_name.clone(), profile);
    store.active_profile = profile_name.clone();
    auth::save_store(&store)?;

    println!(
        "{} Authenticated. Profile: {} ({})",
        "✓".green(),
        profile_name.bold(),
        roles.cyan()
    );
    Ok(())
}

async fn logout() -> Result<()> {
    let mut store = auth::load_store()?;
    let profile_name = store.active_profile.clone();

    if profile_name.is_empty() || !store.profiles.contains_key(&profile_name) {
        println!("Not currently authenticated.");
        return Ok(());
    }

    let profile = store.profiles.remove(&profile_name).unwrap();
    store.active_profile = store.profiles.keys().next().cloned().unwrap_or_default();
    auth::save_store(&store)?;

    // Best-effort revocation
    let _ = revoke_session(&profile).await;

    println!("{} Logged out of {}.", "✓".green(), profile_name.cyan());
    Ok(())
}

async fn revoke_session(profile: &auth::AegisProfile) -> Result<()> {
    // Audit 002 §4.37.9 — bound the logout wait on a frozen IdP. Logout is
    // best-effort, so use a tight budget to avoid hanging the user's
    // terminal on a flaky auth host.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("reqwest client build: {e}"))?;
    let url = format!(
        "https://auth.{}/realms/aegis-system/protocol/openid-connect/logout",
        profile.env
    );
    client
        .post(&url)
        .form(&[
            ("client_id", "aegis-cli"),
            ("refresh_token", profile.refresh_key.as_str()),
        ])
        .send()
        .await?;
    Ok(())
}

#[derive(Serialize)]
struct StatusOutput {
    profile: String,
    env: String,
    roles: Vec<String>,
    expires_at: String,
    scope_count: usize,
}

async fn status(output_format: OutputFormat) -> Result<()> {
    let store = auth::load_store()?;

    if store.active_profile.is_empty() {
        println!("Not authenticated. Run 'aegis auth login'.");
        return Ok(());
    }

    let profile = store.profiles.get(&store.active_profile).ok_or_else(|| {
        anyhow::anyhow!(
            "Active profile '{}' not found in store.",
            store.active_profile
        )
    })?;

    let output = StatusOutput {
        profile: profile.name.clone(),
        env: profile.env.clone(),
        roles: profile.roles.clone(),
        expires_at: profile.expires_at.to_rfc3339(),
        scope_count: profile.scopes.len(),
    };

    if output_format.is_structured() {
        return render_serialized(output_format, &output);
    }

    println!("{:14} {}", "Profile:".dimmed(), output.profile.bold());
    println!("{:14} {}", "Environment:".dimmed(), output.env.cyan());
    println!("{:14} {}", "Roles:".dimmed(), output.roles.join(", "));
    println!("{:14} {}", "Expires:".dimmed(), output.expires_at);
    println!("{:14} {}", "Scopes:".dimmed(), output.scope_count);
    Ok(())
}

fn switch_profile(profile_name: &str) -> Result<()> {
    let mut store = auth::load_store()?;
    if !store.profiles.contains_key(profile_name) {
        let available: Vec<&String> = store.profiles.keys().collect();
        anyhow::bail!(
            "Profile '{}' not found. Available: {}",
            profile_name,
            available
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    store.active_profile = profile_name.to_string();
    auth::save_store(&store)?;
    println!(
        "{} Switched to profile '{}'.",
        "✓".green(),
        profile_name.cyan()
    );
    Ok(())
}

async fn print_token() -> Result<()> {
    let key = auth::require_key().await?;
    println!("{key}");
    Ok(())
}
