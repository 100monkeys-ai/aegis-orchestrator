// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Credential management commands for the AEGIS CLI
//!
//! Manages provider credential bindings (API keys and OAuth tokens) through the
//! daemon API.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements `aegis credential` subcommands

use anyhow::Result;
use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;

use crate::daemon::{check_daemon_running, DaemonClient, DaemonStatus};
use crate::output::{render_serialized, OutputFormat};

/// Top-level `aegis credential` subcommands.
#[derive(Subcommand)]
pub enum CredentialCommand {
    /// Store a provider API key as a credential binding
    Store {
        /// Provider name (e.g. openai, anthropic)
        #[arg(long, value_name = "NAME")]
        provider: String,

        /// Human-readable label for this binding
        #[arg(long, value_name = "LABEL")]
        label: String,

        /// Scope: "personal" or "team:&lt;uuid&gt;"
        #[arg(long, value_name = "SCOPE", default_value = "personal")]
        scope: String,

        /// The raw API key value
        #[arg(long, value_name = "KEY")]
        value: String,
    },

    /// Initiate an OAuth authorization flow for a provider
    Connect {
        /// Provider name (e.g. github, google)
        #[arg(long, value_name = "NAME")]
        provider: String,

        /// OAuth redirect URI (must be registered with the provider)
        #[arg(long, value_name = "URI")]
        redirect_uri: String,
    },

    /// List all credential bindings accessible to the current identity
    List,

    /// Show details for a specific credential binding
    Get {
        /// Credential binding ID
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Remove an OAuth credential binding (alias for delete with OAuth context)
    Disconnect {
        /// Credential binding ID
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Delete a credential binding permanently
    Delete {
        /// Credential binding ID
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Manage access grants on a credential binding
    Grant {
        #[command(subcommand)]
        command: GrantCommand,
    },
}

/// Subcommands for `aegis credential grant`.
#[derive(Subcommand)]
pub enum GrantCommand {
    /// Grant a credential binding to an agent, workflow, or all agents
    Add {
        /// Credential binding ID to grant access on
        #[arg(value_name = "BINDING_ID")]
        binding_id: String,

        /// Grant target type: "agent", "workflow", or "all_agents"
        #[arg(long, value_name = "TYPE")]
        target_type: String,

        /// Target UUID (required for agent/workflow target types)
        #[arg(long, value_name = "UUID")]
        target_id: Option<String>,

        /// Human identifier for the granting principal (for audit purposes)
        #[arg(long, value_name = "STRING")]
        granted_by: Option<String>,
    },

    /// List grants on a credential binding
    List {
        /// Credential binding ID
        #[arg(value_name = "BINDING_ID")]
        binding_id: String,
    },

    /// Revoke a specific grant from a credential binding
    Revoke {
        /// Credential binding ID
        #[arg(value_name = "BINDING_ID")]
        binding_id: String,

        /// Grant ID to revoke
        #[arg(value_name = "GRANT_ID")]
        grant_id: String,
    },
}

pub async fn handle_command(
    command: CredentialCommand,
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
                "Credential management requires the daemon to be running.".red()
            );
            println!("Run 'aegis daemon start' to start the daemon.");
            return Ok(());
        }
    }

    let auth_key = crate::auth::require_key().await?;
    let client = DaemonClient::new(host, port)?.with_auth(auth_key);

    match command {
        CredentialCommand::Store {
            provider,
            label,
            scope,
            value,
        } => store_credential(&provider, &label, &scope, &value, &client, output_format).await,

        CredentialCommand::Connect {
            provider,
            redirect_uri,
        } => connect_oauth(&provider, &redirect_uri, &client, output_format).await,

        CredentialCommand::List => list_credentials(&client, output_format).await,

        CredentialCommand::Get { id } => get_credential(&id, &client, output_format).await,

        CredentialCommand::Disconnect { id } | CredentialCommand::Delete { id } => {
            delete_credential(&id, &client, output_format).await
        }

        CredentialCommand::Grant { command } => match command {
            GrantCommand::Add {
                binding_id,
                target_type,
                target_id,
                granted_by,
            } => {
                add_grant(
                    &binding_id,
                    &target_type,
                    target_id.as_deref(),
                    granted_by.as_deref(),
                    &client,
                    output_format,
                )
                .await
            }
            GrantCommand::List { binding_id } => {
                list_grants(&binding_id, &client, output_format).await
            }
            GrantCommand::Revoke {
                binding_id,
                grant_id,
            } => revoke_grant(&binding_id, &grant_id, &client, output_format).await,
        },
    }
}

// ── Output structs ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct CredentialMutationOutput {
    id: String,
    status: &'static str,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn store_credential(
    provider: &str,
    label: &str,
    scope: &str,
    value: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let body = client
        .store_api_key(provider, label, Some(scope), value)
        .await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    println!(
        "{}",
        format!("✓ Credential stored for provider '{provider}' (id: {id})").green()
    );
    Ok(())
}

async fn connect_oauth(
    provider: &str,
    redirect_uri: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let body = client.initiate_oauth(provider, redirect_uri).await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    let auth_url = body
        .get("authorization_url")
        .and_then(|v| v.as_str())
        .unwrap_or("<url not returned by server>");
    let state = body
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("<state not returned by server>");

    println!("{}", "OAuth flow initiated.".cyan());
    println!("{:16} {}", "Authorization URL:".dimmed(), auth_url.bold());
    println!("{:16} {}", "State:".dimmed(), state);
    println!();
    println!(
        "{}",
        "Visit the URL above to authorize access. The callback is handled by the web UI.".dimmed()
    );
    Ok(())
}

async fn list_credentials(client: &DaemonClient, output_format: OutputFormat) -> Result<()> {
    let body = client.list_credentials().await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
    Ok(())
}

async fn get_credential(
    id: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let body = client.get_credential(id).await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
    Ok(())
}

async fn delete_credential(
    id: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    client.delete_credential(id).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &CredentialMutationOutput {
                id: id.to_string(),
                status: "deleted",
            },
        );
    }

    println!("{}", format!("✓ Credential '{id}' deleted").green());
    Ok(())
}

async fn add_grant(
    binding_id: &str,
    target_type: &str,
    target_id: Option<&str>,
    granted_by: Option<&str>,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let body = client
        .add_credential_grant(binding_id, target_type, target_id, granted_by)
        .await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    let grant_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    println!(
        "{}",
        format!("✓ Grant '{grant_id}' added to credential '{binding_id}'").green()
    );
    Ok(())
}

async fn list_grants(
    binding_id: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    let body = client.list_credential_grants(binding_id).await?;

    if output_format.is_structured() {
        return render_serialized(output_format, &body);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    );
    Ok(())
}

async fn revoke_grant(
    binding_id: &str,
    grant_id: &str,
    client: &DaemonClient,
    output_format: OutputFormat,
) -> Result<()> {
    client.revoke_credential_grant(binding_id, grant_id).await?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &serde_json::json!({
                "binding_id": binding_id,
                "grant_id": grant_id,
                "status": "revoked"
            }),
        );
    }

    println!(
        "{}",
        format!("✓ Grant '{grant_id}' revoked from credential '{binding_id}'").green()
    );
    Ok(())
}
