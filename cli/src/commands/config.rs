// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Configuration management commands
//!
//! Commands: show, validate, generate
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for config

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use serde::Serialize;
use std::path::PathBuf;

use aegis_orchestrator_core::domain::node_config::NodeConfigManifest;

use crate::output::{render_serialized, OutputFormat};

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show current configuration
    Show {
        /// Show config file paths checked
        #[arg(long)]
        paths: bool,
    },

    /// Validate configuration file
    Validate {
        /// Path to config file (default: discover)
        #[arg(value_name = "FILE")]
        file: Option<PathBuf>,
    },

    /// Generate sample configuration
    Generate {
        /// Destination path (default: ./aegis-config.yaml)
        #[arg(short = 'o', long = "out", default_value = "./aegis-config.yaml")]
        out: PathBuf,

        /// Include examples and comments
        #[arg(long)]
        examples: bool,
    },
}

pub async fn handle_command(
    command: ConfigCommand,
    config_override: Option<PathBuf>,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        ConfigCommand::Show { paths } => show(config_override, paths, output_format).await,
        ConfigCommand::Validate { file } => validate(file.or(config_override), output_format).await,
        ConfigCommand::Generate { out, examples } => generate(out, examples, output_format).await,
    }
}

#[derive(Serialize)]
struct ConfigShowOutput {
    config: NodeConfigManifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    discovery_paths: Option<Vec<String>>,
}

#[derive(Serialize)]
struct ConfigValidateOutput {
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Serialize)]
struct ConfigGenerateOutput {
    path: String,
    examples: bool,
}

async fn show(
    config_override: Option<PathBuf>,
    show_paths: bool,
    output_format: OutputFormat,
) -> Result<()> {
    let config = NodeConfigManifest::load_or_default(config_override.clone())
        .context("Failed to load configuration")?;

    if output_format.is_structured() {
        let discovery_paths = show_paths.then(|| {
            vec![
                config_override
                    .as_ref()
                    .map(|path| format!("--config: {}", path.display()))
                    .unwrap_or_else(|| "--config: (not set)".to_string()),
                format!(
                    "AEGIS_CONFIG_PATH: {}",
                    std::env::var("AEGIS_CONFIG_PATH").unwrap_or_else(|_| "(not set)".to_string())
                ),
                "./aegis-config.yaml".to_string(),
                "~/.aegis/config.yaml".to_string(),
                "/etc/aegis/config.yaml".to_string(),
            ]
        });

        return render_serialized(
            output_format,
            &ConfigShowOutput {
                config,
                discovery_paths,
            },
        );
    }

    if show_paths {
        println!("{}", "Configuration discovery paths:".bold());
        if let Some(path) = &config_override {
            println!("  1. --config flag: {}", path.display());
        } else {
            println!("  1. --config flag: {}", "(not set)".dimmed());
        }
        println!(
            "  2. AEGIS_CONFIG_PATH: {}",
            std::env::var("AEGIS_CONFIG_PATH")
                .unwrap_or_else(|_| "(not set)".to_string())
                .dimmed()
        );
        println!("  3. ./aegis-config.yaml");
        println!("  4. ~/.aegis/config.yaml");
        println!("  5. /etc/aegis/config.yaml");
        println!();
    }

    println!("{}", "Current configuration:".bold());
    println!();

    // Manifest info
    println!("{}", "Manifest:".bold());
    println!("  API Version: {}", config.api_version);
    println!("  Kind: {}", config.kind);
    println!();

    // Metadata
    println!("{}", "Metadata:".bold());
    println!("  Name: {}", config.metadata.name);
    if let Some(version) = &config.metadata.version {
        println!("  Version: {version}");
    }
    if let Some(labels) = &config.metadata.labels {
        if !labels.is_empty() {
            println!("  Labels:");
            for (key, value) in labels {
                println!("    {key}: {value}");
            }
        }
    }
    println!();

    // Node identity
    println!("{}", "Node Identity:".bold());
    println!("  ID: {}", config.spec.node.id);
    println!("  Type: {:?}", config.spec.node.node_type);
    if let Some(region) = &config.spec.node.region {
        println!("  Region: {region}");
    }
    if !config.spec.node.tags.is_empty() {
        println!("  Tags: {}", config.spec.node.tags.join(", "));
    }
    println!();

    // LLM providers
    println!("{}", "LLM Providers:".bold());
    for provider in &config.spec.llm_providers {
        println!("  {} ({})", provider.name.bold(), provider.provider_type);
        println!("    Endpoint: {}", provider.endpoint);
        println!("    Models: {}", provider.models.len());
        for model in &provider.models {
            println!("      - {} → {}", model.alias, model.model);
        }
    }
    println!();

    // LLM selection strategy
    println!("{}", "LLM Selection:".bold());
    println!("  Strategy: {:?}", config.spec.llm_selection.strategy);
    println!(
        "  Default provider: {}",
        config
            .spec
            .llm_selection
            .default_provider
            .as_deref()
            .unwrap_or("(none)")
    );
    if let Some(fallback) = &config.spec.llm_selection.fallback_provider {
        println!("  Fallback provider: {fallback}");
    }
    println!();

    Ok(())
}

async fn validate(config_path: Option<PathBuf>, output_format: OutputFormat) -> Result<()> {
    if !output_format.is_structured() {
        println!("Validating configuration...");
    }

    let config = NodeConfigManifest::load_or_default(config_path.clone())
        .context("Failed to load configuration")?;

    config
        .validate()
        .context("Configuration validation failed")?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &ConfigValidateOutput {
                valid: true,
                path: config_path.map(|path| path.display().to_string()),
            },
        );
    }

    println!("{}", "✓ Configuration is valid".green());

    Ok(())
}

async fn generate(output: PathBuf, with_examples: bool, output_format: OutputFormat) -> Result<()> {
    let sample = if with_examples {
        include_str!("../../templates/config-with-examples.yaml")
    } else {
        include_str!("../../templates/config-minimal.yaml")
    };

    std::fs::write(&output, sample)
        .with_context(|| format!("Failed to write config to {output:?}"))?;

    if output_format.is_structured() {
        return render_serialized(
            output_format,
            &ConfigGenerateOutput {
                path: output.display().to_string(),
                examples: with_examples,
            },
        );
    }

    println!(
        "{}",
        format!("✓ Configuration generated: {}", output.display()).green()
    );

    Ok(())
}
