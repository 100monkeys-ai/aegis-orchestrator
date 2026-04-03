// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Component selection step of `aegis init`.
//!
//! Presents a multi-select prompt for optional AEGIS services. The result is
//! used to build the `COMPOSE_PROFILES` string that docker-compose uses to
//! activate the appropriate services.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Component selection inside the `aegis init` wizard

use anyhow::Result;
use colored::Colorize;
use dialoguer::MultiSelect;

/// All optional AEGIS service groups that a user can toggle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedComponents {
    /// Temporal workflow engine + UI + worker
    pub temporal: bool,
    /// SeaweedFS distributed storage (master, volume, filer, webdav)
    pub storage: bool,
    /// Standalone SEAL tooling gateway (ADR-053)
    pub seal_gateway: bool,
    /// Local Ollama LLM runtime
    pub ollama_llm: bool,
    /// Enable OTLP observability stack (Jaeger)
    pub observability: bool,
    /// LLM backend choice
    pub llm: LlmChoice,
}

/// Which LLM backend the user wants to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmChoice {
    /// Local Ollama (free, requires more resources)
    Ollama,
    /// OpenAI API
    OpenAI,
    /// Anthropic API
    Anthropic,
    /// Gemini API (Google)
    Gemini,
    /// vLLM, LM Studio, or other OpenAI-compatible endpoints
    OpenAICompatible,
}

impl SelectedComponents {
    /// Build the `COMPOSE_PROFILES` env-var value for this selection.
    ///
    /// `core` is always included. Each selected optional group adds its profile
    /// name.
    pub fn compose_profiles(&self) -> String {
        let mut profiles = vec!["core"];
        if self.temporal {
            profiles.push("temporal");
        }
        if self.storage {
            profiles.push("storage");
        }
        if self.seal_gateway {
            profiles.push("seal-gateway");
        }
        if self.ollama_llm {
            profiles.push("llm");
        }
        if self.observability {
            profiles.push("observability");
        }
        profiles.join(",")
    }
}

/// Drives the interactive component selection step of the wizard.
pub struct ComponentSelector {
    yes: bool,
}

impl ComponentSelector {
    pub fn new(yes: bool) -> Self {
        Self { yes }
    }

    /// Run the component selection step.
    ///
    /// In `--yes` mode all defaults are accepted silently.
    pub fn select(&self) -> Result<SelectedComponents> {
        println!();
        println!("{}", "Select optional components to enable:".bold());
        println!(
            "  {}  Core services (PostgreSQL + AEGIS Runtime) are always included.",
            "ℹ".cyan()
        );
        println!();

        // Default: temporal=true, storage=false, iam=false, secrets=false, llm=Ollama
        if self.yes {
            return Ok(SelectedComponents {
                temporal: true,
                storage: false,
                seal_gateway: false,
                ollama_llm: true,
                observability: false,
                llm: LlmChoice::Ollama,
            });
        }

        let items = vec![
            "Temporal (workflow engine, UI, and worker)  [required for workflows]",
            "SeaweedFS (distributed storage for agent volumes)",
            "SEAL Gateway (external tooling gateway)      [enables ToolWorkflows & secure external tool access]",
            "Ollama (local LLM runtime — no API key needed)",
            "Observability (Jaeger OTLP collector + UI)   [recommended for logging/tracing]",
        ];

        let defaults = vec![true, false, false, false, false];

        let selections = MultiSelect::new()
            .with_prompt("Use SPACE to toggle, ENTER to confirm")
            .items(&items)
            .defaults(&defaults)
            .interact()?;

        let temporal = selections.contains(&0);
        let storage = selections.contains(&1);
        let seal_gateway = selections.contains(&2);
        let ollama_llm = selections.contains(&3);
        let observability = selections.contains(&4);

        // If Ollama was not selected, ask which cloud LLM to use
        let llm = if ollama_llm {
            LlmChoice::Ollama
        } else {
            println!();
            println!(
                "{}",
                "Ollama not selected — choose a primary LLM provider:".bold()
            );
            let providers = vec![
                "OpenAI (GPT-4o, etc.)",
                "Anthropic (Claude 3.x)",
                "Gemini (Google)",
                "vLLM / LM Studio (OpenAI-compatible)",
            ];
            let idx = dialoguer::Select::new()
                .with_prompt("LLM provider")
                .items(&providers)
                .default(0)
                .interact()?;
            match idx {
                0 => LlmChoice::OpenAI,
                1 => LlmChoice::Anthropic,
                2 => LlmChoice::Gemini,
                _ => LlmChoice::OpenAICompatible,
            }
        };

        let summary_lines: Vec<&str> = vec![
            if temporal {
                "  ✓ Temporal"
            } else {
                "  · Temporal (skipped)"
            },
            if storage {
                "  ✓ SeaweedFS"
            } else {
                "  · SeaweedFS (skipped)"
            },
            if seal_gateway {
                "  ✓ SEAL Gateway"
            } else {
                "  · SEAL Gateway (skipped)"
            },
            if observability {
                "  ✓ Jaeger (Observability)"
            } else {
                "  · Observability (skipped)"
            },
            match &llm {
                LlmChoice::Ollama => "  ✓ Ollama (local)",
                LlmChoice::OpenAI => "  ✓ OpenAI",
                LlmChoice::Anthropic => "  ✓ Anthropic",
                LlmChoice::Gemini => "  ✓ Gemini",
                LlmChoice::OpenAICompatible => "  ✓ vLLM / OpenAI-compatible",
            },
        ];

        println!();
        println!("{}", "Selected components:".bold());
        for line in summary_lines {
            println!("{line}");
        }

        Ok(SelectedComponents {
            temporal,
            storage,
            seal_gateway,
            ollama_llm,
            observability,
            llm,
        })
    }
}
