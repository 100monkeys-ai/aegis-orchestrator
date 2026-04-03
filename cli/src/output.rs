// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Shared output rendering helpers for scriptable CLI commands.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Text,
    Table,
    Json,
    Yaml,
}

impl OutputFormat {
    pub fn is_structured(self) -> bool {
        matches!(self, Self::Json | Self::Yaml)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Table => "table",
            Self::Json => "json",
            Self::Yaml => "yaml",
        }
    }
}

pub fn render_serialized<T: Serialize>(format: OutputFormat, value: &T) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).context("Failed to serialize JSON output")?
            );
            Ok(())
        }
        OutputFormat::Yaml => {
            print!(
                "{}",
                serde_yaml::to_string(value).context("Failed to serialize YAML output")?
            );
            Ok(())
        }
        OutputFormat::Text | OutputFormat::Table => {
            bail!("{} output requires a text/table renderer", format.as_str())
        }
    }
}

pub fn structured_output_unsupported(command: &str, format: OutputFormat) -> Result<()> {
    bail!(
        "`{command}` does not support --output {}. Use text/table output for this command.",
        format.as_str()
    )
}
