// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Secret management commands for the AEGIS CLI
//!
//! Provides CLI access to the OpenBao-backed secret store via the daemon API.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements `aegis secret` subcommands (create, get, list, delete, rotate)

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};
use crate::output::{render_serialized, OutputFormat};

#[derive(Subcommand)]
pub enum SecretCommand {
    /// Store a new secret at the given path
    Create {
        /// Secret path (e.g. providers/openai/key)
        #[arg(value_name = "PATH")]
        path: String,

        /// Secret data as a JSON object (e.g. '{"value":"sk-..."}')
        #[arg(long, value_name = "JSON")]
        data: String,
    },

    /// Retrieve a secret by path
    Get {
        /// Secret path
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// List all accessible secrets
    List,

    /// Delete a secret by path
    Delete {
        /// Secret path
        #[arg(value_name = "PATH")]
        path: String,
    },

    /// Rotate a secret (idempotent overwrite at the same path)
    Rotate {
        /// Secret path
        #[arg(value_name = "PATH")]
        path: String,

        /// New secret data as a JSON object
        #[arg(long, value_name = "JSON")]
        data: String,
    },
}

pub async fn handle_command(
    command: SecretCommand,
    _config_path: Option<PathBuf>,
    host: &str,
    port: u16,
    output_format: OutputFormat,
) -> Result<()> {
    let daemon_status = check_daemon_running(host, port).await;
    match daemon_status {
        Ok(DaemonStatus::Running { .. }) => {}
        Ok(DaemonStatus::Unhealthy { pid, error }) => {
            println!(
                "{}",
                format!("⚠ Daemon is running (PID: {pid}) but unhealthy: {error}").yellow()
            );
            println!("Run 'aegis daemon status' for more info.");
            return Ok(());
        }
        _ => {
            println!(
                "{}",
                "Secret management requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);

    match command {
        SecretCommand::Create { path, data } => {
            create_secret(&path, &data, &client, output_format).await
        }
        SecretCommand::Get { path } => get_secret(&path, &client, output_format).await,
        SecretCommand::List => list_secrets(&client, output_format).await,
        SecretCommand::Delete { path } => delete_secret(&path, &client, output_format).await,
        SecretCommand::Rotate { path, data } => {
            rotate_secret(&path, &data, &client, output_format).await
        }
    }
}

#[derive(Serialize)]
struct SecretMutationOutput {
    path: String,
    status: &'static str,
}

async fn create_secret(
    path: &str,
    data_json: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let data: serde_json::Value = serde_json::from_str(data_json)
        .map_err(|e| anyhow::anyhow!("--data must be valid JSON: {e}"))?;

    client.put_secret(path, data).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &SecretMutationOutput {
                path: path.to_string(),
                status: "created",
            },
        );
    }

    println!("{}", format!("✓ Secret created at '{path}'").green());
    Ok(())
}

async fn get_secret(path: &str, client: &DaemonClient, output_format: OutputFormat) -> Result<()> {
    let body = client.get_secret(path).await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
    Ok(())
}

async fn list_secrets(client: &DaemonClient, output_format: OutputFormat) -> Result<()> {
    match client.list_secrets().await? {
        None => {
            if output_format.is_structured() {
                return render_serialized(
                    output_format,
                    &serde_json::json!({
                        "error": "secret listing is not implemented on this server"
                    }),
                );
            }
            println!(
                "{}",
                "Secret listing is not supported by this daemon version.".yellow()
            );
        }
        Some(body) => {
            if output_format.is_structured() {
                return render_serialized(output_format, &body);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&body).unwrap_or_default()
            );
        }
    }
    Ok(())
}

async fn delete_secret(
    path: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    client.delete_secret(path).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &SecretMutationOutput {
                path: path.to_string(),
                status: "deleted",
            },
        );
    }

    println!("{}", format!("✓ Secret deleted at '{path}'").green());
    Ok(())
}

async fn rotate_secret(
    path: &str,
    data_json: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let data: serde_json::Value = serde_json::from_str(data_json)
        .map_err(|e| anyhow::anyhow!("--data must be valid JSON: {e}"))?;

    client.put_secret(path, data).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &SecretMutationOutput {
                path: path.to_string(),
                status: "rotated",
            },
        );
    }

    println!("{}", format!("✓ Secret rotated at '{path}'").green());
    Ok(())
}
