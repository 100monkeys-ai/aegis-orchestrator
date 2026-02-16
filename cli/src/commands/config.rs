// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Configuration management commands
//!
//! Commands: show, validate, generate

use anyhow::{Context, Result};
use clap::Subcommand;
use colored::Colorize;
use std::path::PathBuf;

use aegis_core::domain::node_config::NodeConfigManifest;

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
        /// Output path (default: ./aegis-config.yaml)
        #[arg(short, long, default_value = "./aegis-config.yaml")]
        output: PathBuf,

        /// Include examples and comments
        #[arg(long)]
        examples: bool,
    },
}

pub async fn handle_command(
    command: ConfigCommand,
    config_override: Option<PathBuf>,
) -> Result<()> {
    match command {
        ConfigCommand::Show { paths } => show(config_override, paths).await,
        ConfigCommand::Validate { file } => validate(file.or(config_override)).await,
        ConfigCommand::Generate { output, examples } => generate(output, examples).await,
    }
}

async fn show(config_override: Option<PathBuf>, show_paths: bool) -> Result<()> {
    let config = NodeConfigManifest::load_or_default(config_override.clone())
        .context("Failed to load configuration")?;

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
        println!("  Version: {}", version);
    }
    if let Some(labels) = &config.metadata.labels {
        if !labels.is_empty() {
            println!("  Labels:");
            for (key, value) in labels {
                println!("    {}: {}", key, value);
            }
        }
    }
    println!();

    // Node identity
    println!("{}", "Node Identity:".bold());
    println!("  ID: {}", config.spec.node.id);
    println!("  Type: {:?}", config.spec.node.node_type);
    if let Some(region) = &config.spec.node.region {
        println!("  Region: {}", region);
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
        config.spec.llm_selection.default_provider.as_deref().unwrap_or("(none)")
    );
    if let Some(fallback) = &config.spec.llm_selection.fallback_provider {
        println!("  Fallback provider: {}", fallback);
    }
    println!();

    Ok(())
}

async fn validate(config_path: Option<PathBuf>) -> Result<()> {
    println!("Validating configuration...");

    let config = NodeConfigManifest::load_or_default(config_path)
        .context("Failed to load configuration")?;

    config
        .validate()
        .context("Configuration validation failed")?;

    println!("{}", "✓ Configuration is valid".green());

    Ok(())
}

async fn generate(output: PathBuf, with_examples: bool) -> Result<()> {
    let sample = if with_examples {
        include_str!("../../templates/config-with-examples.yaml")
    } else {
        include_str!("../../templates/config-minimal.yaml")
    };

    std::fs::write(&output, sample)
        .with_context(|| format!("Failed to write config to {:?}", output))?;

    println!(
        "{}",
        format!("✓ Configuration generated: {}", output.display()).green()
    );

    Ok(())
}
