// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Node configuration step of `aegis init`.
//!
//! Prompts the user for node name, working directory, and LLM API key (when a
//! cloud provider was selected). Renders the `aegis-config.yaml` and `.env`
//! from templates and writes them to disk.
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** configure step inside the `aegis init` wizard

use std::path::PathBuf;

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::{Confirm, Input, Password};
use uuid::Uuid;

use super::components::{LlmChoice, SelectedComponents};

/// The resolved configuration values collected during the wizard.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node_name: String,
    pub node_id: String,
    /// Ollama model alias (e.g. "llama3.2:latest")
    pub ollama_model: String,
    /// Cloud LLM API key (None when Ollama is selected)
    pub api_key: Option<String>,
    /// The working directory where stack files will be written
    pub working_dir: PathBuf,
}

/// Drives the interactive configuration step.
pub struct ConfigWizard {
    yes: bool,
    dir: PathBuf,
}

impl ConfigWizard {
    pub fn new(yes: bool, dir: PathBuf) -> Self {
        Self { yes, dir }
    }

    /// Run the configuration step.
    ///
    /// Returns the `NodeConfig` and also writes `aegis-config.yaml`, `.env`,
    /// and `runtime-registry.yaml` to the working directory.
    pub fn configure(
        &self,
        components: &SelectedComponents,
        compose_content: &str,
        runtime_registry_content: &str,
    ) -> Result<NodeConfig> {
        println!();
        println!("{}", "Configure your AEGIS node:".bold());

        let working_dir = expand_tilde(&self.dir);

        let node_name: String = if self.yes {
            "my-aegis-node".to_string()
        } else {
            Input::new()
                .with_prompt("Node name")
                .default("my-aegis-node".to_string())
                .interact_text()?
        };

        let node_id = Uuid::new_v4().to_string();

        let ollama_model: String = if components.ollama_llm {
            if self.yes {
                "llama3.2:latest".to_string()
            } else {
                Input::new()
                    .with_prompt("Ollama model")
                    .default("llama3.2:latest".to_string())
                    .interact_text()?
            }
        } else {
            String::new()
        };

        let api_key: Option<String> = match &components.llm {
            LlmChoice::Ollama => None,
            LlmChoice::OpenAI => {
                if self.yes {
                    println!(
                        "  {} No OpenAI API key provided in --yes mode. Set OPENAI_API_KEY in .env.",
                        "⚠".yellow()
                    );
                    None
                } else {
                    let key: String = Password::new()
                        .with_prompt("OpenAI API key (sk-...)")
                        .interact()?;
                    Some(key)
                }
            }
            LlmChoice::Anthropic => {
                if self.yes {
                    println!(
                        "  {} No Anthropic API key provided in --yes mode. Set ANTHROPIC_API_KEY in .env.",
                        "⚠".yellow()
                    );
                    None
                } else {
                    let key: String = Password::new()
                        .with_prompt("Anthropic API key (sk-ant-...)")
                        .interact()?;
                    Some(key)
                }
            }
        };

        let node_config = NodeConfig {
            node_name,
            node_id,
            ollama_model,
            api_key,
            working_dir: working_dir.clone(),
        };

        // Write files
        std::fs::create_dir_all(&working_dir)
            .with_context(|| format!("Failed to create directory {}", working_dir.display()))?;

        let aegis_config_content = self.render_aegis_config(&node_config, components);
        let env_content = self.render_env(&node_config, components);

        let config_path = working_dir.join("aegis-config.yaml");
        let env_path = working_dir.join(".env");
        let compose_path = working_dir.join("docker-compose.yml");
        let runtime_registry_path = working_dir.join("runtime-registry.yaml");

        std::fs::write(&config_path, &aegis_config_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
        std::fs::write(&env_path, &env_content)
            .with_context(|| format!("Failed to write {}", env_path.display()))?;
        std::fs::write(&compose_path, compose_content)
            .with_context(|| format!("Failed to write {}", compose_path.display()))?;
        std::fs::write(&runtime_registry_path, runtime_registry_content)
            .with_context(|| format!("Failed to write {}", runtime_registry_path.display()))?;

        let already_exists = config_path.exists();
        if already_exists && !self.yes {
            let overwrite = Confirm::new()
                .with_prompt(format!(
                    "{} already exists. Overwrite?",
                    config_path.display()
                ))
                .default(false)
                .interact()?;
            if !overwrite {
                println!("  Skipping file write — keeping existing configuration.");
                return Ok(node_config);
            }
        }

        println!("  {} {}", "✓".green(), config_path.display());
        println!("  {} {}", "✓".green(), env_path.display());
        println!("  {} {}", "✓".green(), compose_path.display());
        println!("  {} {}", "✓".green(), runtime_registry_path.display());

        Ok(node_config)
    }

    /// Render the `aegis-config.yaml` content from collected inputs.
    pub fn render_aegis_config(
        &self,
        config: &NodeConfig,
        components: &SelectedComponents,
    ) -> String {
        let llm_section = match &components.llm {
            LlmChoice::Ollama => format!(
                r#"  llm_providers:
    - name: "local"
      type: "ollama"
      endpoint: "http://localhost:11434"
      enabled: true
      models:
        - alias: "default"
          model: "{model}"
          capabilities: ["code", "reasoning"]
          context_window: 8192
          cost_per_1k_tokens: 0.0

  llm_selection:
    strategy: "prefer-local"
    default_provider: "local"
    max_retries: 3
    retry_delay_ms: 1000
"#,
                model = config.ollama_model
            ),
            LlmChoice::OpenAI => r#"  llm_providers:
    - name: "openai"
      type: "openai"
      endpoint: "https://api.openai.com/v1"
      enabled: true
      api_key: "env:OPENAI_API_KEY"
      models:
        - alias: "default"
          model: "gpt-4o"
          capabilities: ["code", "reasoning"]
          context_window: 128000
          cost_per_1k_tokens: 0.005

  llm_selection:
    strategy: "prefer-local"
    default_provider: "openai"
    max_retries: 3
    retry_delay_ms: 1000
"#
            .to_string(),
            LlmChoice::Anthropic => r#"  llm_providers:
    - name: "anthropic"
      type: "anthropic"
      endpoint: "https://api.anthropic.com"
      enabled: true
      api_key: "env:ANTHROPIC_API_KEY"
      models:
        - alias: "default"
          model: "claude-3-5-sonnet-20241022"
          capabilities: ["code", "reasoning"]
          context_window: 200000
          cost_per_1k_tokens: 0.003

  llm_selection:
    strategy: "prefer-local"
    default_provider: "anthropic"
    max_retries: 3
    retry_delay_ms: 1000
"#
            .to_string(),
        };

        let database_section = r#"
  database:
    url: "env:AEGIS_DATABASE_URL"
    max_connections: 5
    connect_timeout_seconds: 5
"#;

        let temporal_section = if components.temporal {
            r#"
  temporal:
    address: "temporal:7233"
    worker_http_endpoint: "http://localhost:3000"
    worker_secret: "env:TEMPORAL_WORKER_SECRET"
    namespace: "default"
    task_queue: "aegis-agents"
"#
        } else {
            ""
        };

        let storage_section = if components.storage {
            r#"
  storage:
    backend: "seaweedfs"
    seaweedfs:
      filer_url: "http://seaweedfs-filer:8888"
"#
        } else {
            r#"
  storage:
    backend: "local"
"#
        };

        format!(
            r#"# AEGIS Agent Host — node configuration
# Generated by `aegis init`
#
# apiVersion / kind follow the Kubernetes-style manifest convention (ADR-002).
apiVersion: 100monkeys.ai/v1
kind: NodeConfig

metadata:
  name: "{node_name}"
  version: "1.0.0"

spec:
  node:
    id: "{node_id}"
    type: "edge"

{llm_section}
  runtime:
    docker_network_mode: "env:AEGIS_DOCKER_NETWORK"
    orchestrator_url: "env:AEGIS_ORCHESTRATOR_URL"
    nfs_server_host: "env:AEGIS_NFS_HOST"

  network:
    bind_address: "0.0.0.0"
    port: 8088

  observability:
    logging:
      level: "info"
{database_section}{temporal_section}{storage_section}"#,
            node_name = config.node_name,
            node_id = config.node_id,
            llm_section = llm_section,
            database_section = database_section,
            temporal_section = temporal_section,
            storage_section = storage_section,
        )
    }

    /// Render the `.env` file content.
    pub fn render_env(&self, config: &NodeConfig, components: &SelectedComponents) -> String {
        let profiles = components.compose_profiles();

        let api_key_line = match &components.llm {
            LlmChoice::Ollama => {
                "# OPENAI_API_KEY=sk-...\n# ANTHROPIC_API_KEY=sk-ant-...".to_string()
            }
            LlmChoice::OpenAI => format!(
                "OPENAI_API_KEY={}",
                config.api_key.as_deref().unwrap_or("sk-...")
            ),
            LlmChoice::Anthropic => format!(
                "ANTHROPIC_API_KEY={}",
                config.api_key.as_deref().unwrap_or("sk-ant-...")
            ),
        };

        format!(
            r#"# AEGIS Local Stack — generated by `aegis init`
# Edit this file to customise your environment.

# ─── Compose Profiles ─────────────────────────────────────────────────────────
# Controls which optional services are started.
# Profiles: core (always), temporal, storage, iam, llm
COMPOSE_PROFILES={profiles}

# ─── LLM Provider ─────────────────────────────────────────────────────────────
{api_key_line}

# ─── Keycloak ─────────────────────────────────────────────────────────────────
KEYCLOAK_ADMIN_PASSWORD=admin

# ─── AEGIS Runtime Networking ─────────────────────────────────────────────────
AEGIS_DOCKER_NETWORK=aegis-network
AEGIS_ORCHESTRATOR_URL=http://aegis-runtime:8088

# NFS server host — set to the correct value for your platform:
#   Linux native / WSL2  → 127.0.0.1
#   Docker Desktop       → host.docker.internal
# AEGIS_NFS_HOST=127.0.0.1

# ─── Secrets Management (OpenBao) ─────────────────────────────────────────────
OPENBAO_SECRET_ID=test-secret-id

# ─── Database ─────────────────────────────────────────────────────────────────
AEGIS_DATABASE_URL=postgresql://aegis:aegis@postgres:5432/aegis

# ─── Runtime ──────────────────────────────────────────────────────────────────
AEGIS_KEEP_CONTAINER=false
"#,
            profiles = profiles,
            api_key_line = api_key_line,
        )
    }
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with('~') {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(s.trim_start_matches("~/").trim_start_matches('~'));
        }
    }
    path.clone()
}
