// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # AEGIS Agent Host CLI
//!
//! The `aegis` binary is the Agent Host that enables an Agent Node.
//!
//! ## Architecture
//!
//! This CLI follows a **CLI-first** design with daemon capabilities:
//!
//! - **Default mode**: CLI commands delegate to daemon if running, else embed services
//! - **Daemon mode**: `aegis --daemon` runs as background service
//! - **Detection**: Check PID file + HTTP health check
//!
//! ## Commands
//!
//! - `aegis daemon start|stop|status|install|uninstall` - Manage daemon lifecycle
//! - `aegis task deploy|execute|status|logs` - Agent operations
//! - `aegis config show|validate|generate` - Configuration management
//! - `aegis init` - Interactive setup wizard
//! - `aegis up [--yes] [--tag <TAG>]` - Start the stack (runs init
//!   automatically if needed)
//! - `aegis down [--volumes]` - Stop the Docker Compose stack
//! - `aegis restart [--profile <name>]` - Restart the Docker Compose services
//! - `aegis uninstall [-y]` - Stop stack and remove the ~/.aegis directory
//!
//! See the architecture documentation for details.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for main

use aegis_orchestrator_core::domain::node_config::{LoggingConfig, OtlpProtocol};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use opentelemetry_otlp::{LogExporter, WithExportConfig, WithHttpConfig, WithTonicConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

mod auth;
mod commands;
mod daemon;
mod output;

use commands::auth::AuthCommand;
use commands::{
    AgentCommand, ConfigCommand, CredentialCommand, DaemonCommand, DownArgs, FuseDaemonCommand,
    InitArgs, NodeCommand, RestartArgs, SecretCommand, StatusArgs, TaskCommand, UninstallArgs,
    UpArgs, WorkflowCommand,
};
use output::{OutputFormat, structured_output_unsupported};

/// AEGIS Agent Host - Enable autonomous agent execution
#[derive(Parser)]
#[command(name = "aegis")]
#[command(version, about, long_about = None)]
struct Cli {
    /// Run as background daemon service
    #[arg(long)]
    daemon: bool,

    /// Path to configuration file (overrides discovery)
    #[arg(
        short,
        long,
        global = true,
        env = "AEGIS_CONFIG_PATH",
        value_name = "FILE"
    )]
    config: Option<PathBuf>,

    /// HTTP API port (default: 8088)
    #[arg(long, global = true, env = "AEGIS_PORT", default_value = "8088")]
    port: u16,

    /// HTTP API host (default: 127.0.0.1)
    #[arg(long, global = true, env = "AEGIS_HOST", default_value = "127.0.0.1")]
    host: String,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, global = true, env = "AEGIS_LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Output format for supported scriptable commands
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage daemon lifecycle
    #[command(name = "daemon")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Agent task operations
    #[command(name = "task")]
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },

    /// Cluster node operations
    #[command(name = "node")]
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },

    /// Configuration management
    #[command(name = "config")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Agent management
    #[command(name = "agent")]
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },

    /// Workflow management
    #[command(name = "workflow")]
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
    },
    /// Manage secrets in the OpenBao-backed secret store
    #[command(name = "secret")]
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },

    /// Manage provider credential bindings (API keys and OAuth tokens)
    #[command(name = "credential")]
    Credential {
        #[command(subcommand)]
        command: CredentialCommand,
    },

    /// Authenticate with an AEGIS environment.
    #[command(name = "auth")]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },

    /// Host-side FUSE daemon for out-of-process volume mounts (ADR-107)
    #[command(name = "fuse-daemon")]
    FuseDaemon {
        #[command(subcommand)]
        command: FuseDaemonCommand,
    },

    /// Update AEGIS database
    #[command(name = "update")]
    Update {
        #[command(flatten)]
        command: commands::UpdateCommand,
    },

    /// Interactive setup wizard — install and configure AEGIS from scratch
    #[command(name = "init")]
    Init {
        #[command(flatten)]
        args: InitArgs,
    },

    /// Stop the local AEGIS Docker Compose stack
    #[command(name = "down")]
    Down {
        #[command(flatten)]
        args: DownArgs,
    },

    /// Start the AEGIS stack (runs `aegis init` automatically if not set up)
    #[command(name = "up")]
    Up {
        #[command(flatten)]
        args: UpArgs,
    },

    /// Restart local AEGIS Docker Compose services
    #[command(name = "restart")]
    Restart {
        #[command(flatten)]
        args: RestartArgs,
    },

    /// Check AEGIS service health
    #[command(name = "status")]
    Status {
        #[command(flatten)]
        args: StatusArgs,
    },

    /// Stop the stack and permanently remove the AEGIS data directory
    #[command(name = "uninstall")]
    Uninstall {
        #[command(flatten)]
        args: UninstallArgs,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load config first to initialize logging properly
    let config =
        match aegis_orchestrator_core::domain::node_config::NodeConfigManifest::load_or_default(
            cli.config.clone(),
        ) {
            Ok(config) => config,
            Err(err) => {
                warn!(
                    error = %err,
                    config_path = ?cli.config,
                    "Failed to load configuration, falling back to defaults"
                );
                aegis_orchestrator_core::domain::node_config::NodeConfigManifest::default()
            }
        };

    // Initialize logging
    let log_provider = init_logging(
        &cli.log_level,
        config
            .spec
            .observability
            .as_ref()
            .and_then(|o| o.logging.as_ref()),
    )?;

    // Handle daemon mode (background service)
    if cli.daemon {
        info!("Starting AEGIS Agent Host in daemon mode");
        return daemon::start_daemon(cli.config, cli.port).await;
    }

    // Handle commands in CLI mode
    let res = match cli.command {
        Some(Commands::Daemon { command }) => {
            commands::daemon::handle_command(command, cli.config, &cli.host, cli.port, cli.output)
                .await
        }
        Some(Commands::Task { command }) => {
            commands::task::handle_command(command, &cli.host, cli.port, cli.output).await
        }
        Some(Commands::Node { command }) => {
            commands::node::handle_command(command, cli.config, &cli.host, cli.port, cli.output)
                .await
        }
        Some(Commands::Config { command }) => {
            commands::config::handle_command(command, cli.config, cli.output).await
        }
        Some(Commands::Agent { command }) => {
            commands::agent::handle_command(command, cli.config, &cli.host, cli.port, cli.output)
                .await
        }
        Some(Commands::Workflow { command }) => {
            commands::workflow::handle_command(command, cli.config, &cli.host, cli.port, cli.output)
                .await
        }
        Some(Commands::Secret { command }) => {
            commands::secret::handle_command(command, cli.config, &cli.host, cli.port, cli.output)
                .await
        }
        Some(Commands::Credential { command }) => {
            commands::credential::handle_command(
                command, cli.config, &cli.host, cli.port, cli.output,
            )
            .await
        }
        Some(Commands::Auth { command }) => {
            commands::auth::handle_command(command, cli.output).await
        }
        Some(Commands::FuseDaemon { command }) => {
            commands::fuse_daemon::handle_command(command, cli.output).await
        }
        Some(Commands::Update { command }) => {
            commands::update::execute(command, cli.config, cli.output).await
        }
        Some(Commands::Init { args }) => {
            if cli.output.is_structured() {
                structured_output_unsupported("aegis init", cli.output)
            } else {
                commands::init::run(args).await
            }
        }
        Some(Commands::Down { args }) => {
            if cli.output.is_structured() {
                structured_output_unsupported("aegis down", cli.output)
            } else {
                commands::down::run(args).await
            }
        }
        Some(Commands::Up { args }) => {
            if cli.output.is_structured() {
                structured_output_unsupported("aegis up", cli.output)
            } else {
                commands::up::run(args).await
            }
        }
        Some(Commands::Restart { args }) => {
            if cli.output.is_structured() {
                structured_output_unsupported("aegis restart", cli.output)
            } else {
                commands::restart::run(args).await
            }
        }
        Some(Commands::Status { args }) => {
            commands::status::run(args, cli.config, &cli.host, cli.port, cli.output).await
        }
        Some(Commands::Uninstall { args }) => {
            if cli.output.is_structured() {
                structured_output_unsupported("aegis uninstall", cli.output)
            } else {
                commands::uninstall::run(args).await
            }
        }
        None => {
            // No command provided - show help
            eprintln!("{}", "No command specified. Use --help for usage.".yellow());
            std::process::exit(1);
        }
    };

    if let Some(provider) = log_provider {
        let _ = provider.shutdown();
    }

    res
}

/// Initialize tracing subscriber for logging
fn init_logging(level: &str, config: Option<&LoggingConfig>) -> Result<Option<SdkLoggerProvider>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(level))
        .context("Failed to create log filter")?;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact();

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);

    if let Some(cfg) = config {
        if let Some(endpoint) = &cfg.otlp_endpoint {
            let exporter = match cfg.otlp_protocol {
                OtlpProtocol::Grpc => {
                    let mut exporter_builder = LogExporter::builder()
                        .with_tonic()
                        .with_endpoint(endpoint)
                        .with_timeout(Duration::from_millis(cfg.batch.export_timeout_ms));

                    let mut metadata = tonic::metadata::MetadataMap::new();
                    for (k, v) in &cfg.otlp_headers {
                        use std::str::FromStr;
                        if let (Ok(key), Ok(val)) = (
                            tonic::metadata::MetadataKey::from_str(k),
                            tonic::metadata::MetadataValue::from_str(v),
                        ) {
                            metadata.insert(key, val);
                        }
                    }
                    if !metadata.is_empty() {
                        exporter_builder = exporter_builder.with_metadata(metadata);
                    }

                    if let Some(ca) = &cfg.tls.ca_cert_path {
                        let pem = std::fs::read(ca).with_context(|| {
                            format!("Failed to read OTLP gRPC CA certificate from path: {ca}")
                        })?;
                        let cert = tonic::transport::Certificate::from_pem(pem);
                        let tls_config =
                            tonic::transport::ClientTlsConfig::new().ca_certificate(cert);
                        exporter_builder = exporter_builder.with_tls_config(tls_config);
                    }

                    exporter_builder
                        .build()
                        .context("Failed to build OTLP gRPC log exporter")?
                }
                OtlpProtocol::Http => {
                    let mut exporter_builder = LogExporter::builder()
                        .with_http()
                        .with_endpoint(endpoint)
                        .with_timeout(Duration::from_millis(cfg.batch.export_timeout_ms));

                    let mut headers = std::collections::HashMap::new();
                    for (k, v) in &cfg.otlp_headers {
                        headers.insert(k.clone(), v.clone());
                    }
                    if !headers.is_empty() {
                        exporter_builder = exporter_builder.with_headers(headers);
                    }

                    exporter_builder
                        .build()
                        .context("Failed to build OTLP HTTP log exporter")?
                }
            };

            let batch_config = opentelemetry_sdk::logs::BatchConfigBuilder::default()
                .with_max_queue_size(cfg.batch.max_queue_size)
                .with_scheduled_delay(Duration::from_millis(cfg.batch.scheduled_delay_ms))
                .with_max_export_batch_size(cfg.batch.max_export_batch_size)
                .build();

            let processor = opentelemetry_sdk::logs::BatchLogProcessor::builder(exporter)
                .with_batch_config(batch_config)
                .build();

            let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
                .with_log_processor(processor)
                .with_resource(
                    opentelemetry_sdk::Resource::builder_empty()
                        .with_attribute(opentelemetry::KeyValue::new(
                            "service.name",
                            cfg.service_name
                                .clone()
                                .unwrap_or_else(|| "aegis-orchestrator".to_string()),
                        ))
                        .build(),
                )
                .build();

            let otlp_layer =
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider);
            let min_level_filter = tracing_subscriber::EnvFilter::new(&cfg.min_level);

            subscriber
                .with(otlp_layer.with_filter(min_level_filter))
                .init();

            return Ok(Some(provider));
        }
    }

    subscriber.init();
    Ok(None)
}
