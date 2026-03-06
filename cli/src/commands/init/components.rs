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
    /// Keycloak (OIDC IAM) + OpenBao (secrets management)
    pub iam: bool,
    /// Local Ollama LLM runtime
    pub ollama_llm: bool,
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
        if self.iam {
            profiles.push("iam");
        }
        if self.ollama_llm {
            profiles.push("llm");
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

        // Default: temporal=true, storage=false, iam=false, llm=Ollama
        if self.yes {
            return Ok(SelectedComponents {
                temporal: false,
                storage: false,
                iam: false,
                ollama_llm: true,
                llm: LlmChoice::Ollama,
            });
        }

        let items = vec![
            "Temporal (workflow engine, UI, and worker)  [recommended for workflows]",
            "SeaweedFS (distributed storage for agent volumes)",
            "IAM (Keycloak OIDC + OpenBao secrets)       [needed for multi-user / Zaru]",
            "Ollama (local LLM runtime — no API key needed)",
        ];

        let defaults = vec![false, false, false, true];

        let selections = MultiSelect::new()
            .with_prompt("Use SPACE to toggle, ENTER to confirm")
            .items(&items)
            .defaults(&defaults)
            .interact()?;

        let temporal = selections.contains(&0);
        let storage = selections.contains(&1);
        let iam = selections.contains(&2);
        let ollama_llm = selections.contains(&3);

        // If Ollama was not selected, ask which cloud LLM to use
        let llm = if ollama_llm {
            LlmChoice::Ollama
        } else {
            println!();
            println!(
                "{}",
                "Ollama not selected — choose a cloud LLM provider:".bold()
            );
            let providers = vec!["OpenAI (GPT-4o, etc.)", "Anthropic (Claude 3.x)"];
            let idx = dialoguer::Select::new()
                .with_prompt("LLM provider")
                .items(&providers)
                .default(0)
                .interact()?;
            match idx {
                0 => LlmChoice::OpenAI,
                _ => LlmChoice::Anthropic,
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
            if iam {
                "  ✓ Keycloak + OpenBao"
            } else {
                "  · IAM (skipped)"
            },
            match &llm {
                LlmChoice::Ollama => "  ✓ Ollama (local)",
                LlmChoice::OpenAI => "  ✓ OpenAI",
                LlmChoice::Anthropic => "  ✓ Anthropic",
            },
        ];

        println!();
        println!("{}", "Selected components:".bold());
        for line in summary_lines {
            println!("{}", line);
        }

        Ok(SelectedComponents {
            temporal,
            storage,
            iam,
            ollama_llm,
            llm,
        })
    }
}
