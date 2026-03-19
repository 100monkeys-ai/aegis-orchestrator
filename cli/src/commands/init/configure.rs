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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::{Confirm, Input, Password};
use rsa::pkcs1::{EncodeRsaPrivateKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;
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
    /// Extended init settings collected via advanced walkthrough
    pub advanced: AdvancedConfig,
}

/// Expanded node/env settings for advanced init walkthrough.
#[derive(Debug, Clone)]
pub struct AdvancedConfig {
    pub node_type: String,
    pub bind_address: String,
    pub api_port: u16,
    pub log_level: String,
    pub docker_network: String,
    pub orchestrator_url: String,
    pub nfs_host: String,
    pub keycloak_admin_password: String,
    pub openbao_secret_id: String,
    pub database_url: String,
    pub temporal_worker_secret: String,
    pub keep_container: bool,
    pub enable_lmstudio: bool,
    pub lmstudio_endpoint: String,
    pub lmstudio_smart_model: String,
    pub lmstudio_judge_model: String,
    pub enable_anthropic_extra: bool,
    pub anthropic_api_key: String,
    pub anthropic_smart_model: String,
    pub anthropic_judge_model: String,
    pub enable_gemini: bool,
    pub gemini_endpoint: String,
    pub gemini_api_key: String,
    pub gemini_smart_model: String,
    pub gemini_judge_model: String,
    pub deploy_smoketest_agents: Option<bool>,
    pub enable_otlp_logging: bool,
    pub otlp_endpoint: String,
    pub otlp_protocol: String,
    pub otlp_min_level: String,
    pub otlp_service_name: String,
}

/// Drives the interactive configuration step.
pub struct ConfigWizard {
    yes: bool,
    dir: PathBuf,
    advanced_override: Option<bool>,
}

impl ConfigWizard {
    pub fn new(yes: bool, dir: PathBuf, advanced_override: Option<bool>) -> Self {
        Self {
            yes,
            dir,
            advanced_override,
        }
    }

    /// Run the configuration step.
    ///
    /// Returns the `NodeConfig` and also writes `aegis-config.yaml`, `.env`,
    /// and `runtime-registry.yaml` to the working directory.
    pub fn configure(
        &self,
        tag: &str,
        components: &SelectedComponents,
        compose_content: &str,
        runtime_registry_content: &str,
        smcp_gateway_config_content: &str,
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

        let mut advanced = self.collect_advanced_config(components)?;
        self.collect_iam_config(components, &mut advanced)?;
        self.collect_secrets_config(components, &mut advanced)?;

        let node_config = NodeConfig {
            node_name,
            node_id,
            ollama_model,
            api_key,
            working_dir: working_dir.clone(),
            advanced,
        };

        let config_path = working_dir.join("aegis-config.yaml");
        let env_path = working_dir.join(".env");
        let compose_path = working_dir.join("docker-compose.yml");
        let runtime_registry_path = working_dir.join("runtime-registry.yaml");
        let smcp_gateway_config_path = working_dir.join("smcp-gateway-config.yaml");

        // Check for existing config *before* writing anything.
        if config_path.exists() && !self.yes {
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

        // Write files
        std::fs::create_dir_all(&working_dir)
            .with_context(|| format!("Failed to create directory {}", working_dir.display()))?;
        ensure_local_volumes_dir_permissions(&working_dir)?;

        let aegis_config_content = self.render_aegis_config(&node_config, components, tag);
        let env_content = self.render_env(&node_config, components)?;

        std::fs::write(&config_path, &aegis_config_content)
            .with_context(|| format!("Failed to write {}", config_path.display()))?;
        std::fs::write(&env_path, &env_content)
            .with_context(|| format!("Failed to write {}", env_path.display()))?;
        std::fs::write(&compose_path, compose_content)
            .with_context(|| format!("Failed to write {}", compose_path.display()))?;
        std::fs::write(&runtime_registry_path, runtime_registry_content)
            .with_context(|| format!("Failed to write {}", runtime_registry_path.display()))?;
        std::fs::write(&smcp_gateway_config_path, smcp_gateway_config_content)
            .with_context(|| format!("Failed to write {}", smcp_gateway_config_path.display()))?;

        println!("  {} {}", "✓".green(), config_path.display());
        println!("  {} {}", "✓".green(), env_path.display());
        println!("  {} {}", "✓".green(), compose_path.display());
        println!("  {} {}", "✓".green(), runtime_registry_path.display());
        println!("  {} {}", "✓".green(), smcp_gateway_config_path.display());

        Ok(node_config)
    }

    fn collect_advanced_config(&self, components: &SelectedComponents) -> Result<AdvancedConfig> {
        let defaults = AdvancedConfig {
            node_type: "hybrid".to_string(),
            bind_address: "0.0.0.0".to_string(),
            api_port: 8088,
            log_level: "info".to_string(),
            docker_network: "aegis-network".to_string(),
            orchestrator_url: "http://aegis-runtime:8088".to_string(),
            nfs_host: "127.0.0.1".to_string(),
            keycloak_admin_password: "admin".to_string(),
            openbao_secret_id: "test-secret-id".to_string(),
            database_url: "postgresql://aegis:aegis@postgres:5432/aegis".to_string(),
            temporal_worker_secret: "dev-temporal-secret".to_string(),
            keep_container: false,
            enable_lmstudio: false,
            lmstudio_endpoint: "http://host.docker.internal:1234/v1".to_string(),
            lmstudio_smart_model: "google/gemma-3-4b".to_string(),
            lmstudio_judge_model: "google/gemma-3-4b".to_string(),
            enable_anthropic_extra: false,
            anthropic_api_key: String::new(),
            anthropic_smart_model: "claude-sonnet-4-5".to_string(),
            anthropic_judge_model: "claude-sonnet-4-5".to_string(),
            enable_gemini: false,
            gemini_endpoint: "https://generativelanguage.googleapis.com/v1beta/openai".to_string(),
            gemini_api_key: String::new(),
            gemini_smart_model: "gemini-2.5-flash".to_string(),
            gemini_judge_model: "gemini-2.5-pro".to_string(),
            deploy_smoketest_agents: None,
            enable_otlp_logging: components.observability,
            otlp_endpoint: if components.observability {
                "http://jaeger:4317".to_string()
            } else {
                "http://localhost:4317".to_string()
            },
            otlp_protocol: "grpc".to_string(),
            otlp_min_level: "info".to_string(),
            otlp_service_name: "aegis-orchestrator".to_string(),
        };

        if self.yes {
            return Ok(defaults);
        }

        let advanced = match self.advanced_override {
            Some(value) => value,
            None => Confirm::new()
                .with_prompt("Run advanced configuration walkthrough?")
                .default(false)
                .interact()?,
        };
        if !advanced {
            return Ok(defaults);
        }

        println!();
        println!("{}", "Advanced configuration:".bold());

        let enable_lmstudio = Confirm::new()
            .with_prompt("Enable LM Studio provider?")
            .default(defaults.enable_lmstudio)
            .interact()?;
        let enable_anthropic_extra = Confirm::new()
            .with_prompt("Enable Anthropic provider in addition to base LLM choice?")
            .default(defaults.enable_anthropic_extra)
            .interact()?;
        let enable_gemini = Confirm::new()
            .with_prompt("Enable Gemini provider (OpenAI-compatible endpoint)?")
            .default(defaults.enable_gemini)
            .interact()?;
        let deploy_smoketest_agents = Confirm::new()
            .with_prompt("Install smoke-test agents during init?")
            .default(true)
            .interact()?;
        let enable_otlp_logging = Confirm::new()
            .with_prompt("Enable external OTLP logging (ADR-057)?")
            .default(defaults.enable_otlp_logging)
            .interact()?;

        Ok(AdvancedConfig {
            node_type: Input::new()
                .with_prompt("Node type")
                .default(defaults.node_type.clone())
                .interact_text()?,
            bind_address: Input::new()
                .with_prompt("API bind address")
                .default(defaults.bind_address.clone())
                .interact_text()?,
            api_port: Input::new()
                .with_prompt("API port")
                .default(defaults.api_port)
                .interact_text()?,
            log_level: Input::new()
                .with_prompt("Log level (trace/debug/info/warn/error)")
                .default(defaults.log_level.clone())
                .interact_text()?,
            docker_network: Input::new()
                .with_prompt("AEGIS_DOCKER_NETWORK")
                .default(defaults.docker_network.clone())
                .interact_text()?,
            orchestrator_url: Input::new()
                .with_prompt("AEGIS_ORCHESTRATOR_URL")
                .default(defaults.orchestrator_url.clone())
                .interact_text()?,
            nfs_host: Input::new()
                .with_prompt("AEGIS_NFS_HOST")
                .default(defaults.nfs_host.clone())
                .interact_text()?,
            keycloak_admin_password: defaults.keycloak_admin_password.clone(),
            openbao_secret_id: defaults.openbao_secret_id.clone(),
            database_url: Input::new()
                .with_prompt("AEGIS_DATABASE_URL")
                .default(defaults.database_url.clone())
                .interact_text()?,
            temporal_worker_secret: Password::new()
                .with_prompt("TEMPORAL_WORKER_SECRET")
                .with_confirmation("Confirm TEMPORAL_WORKER_SECRET", "Secrets mismatch")
                .allow_empty_password(true)
                .interact()?,
            keep_container: Confirm::new()
                .with_prompt("Set AEGIS_KEEP_CONTAINER=true for debugging?")
                .default(defaults.keep_container)
                .interact()?,
            enable_lmstudio,
            lmstudio_endpoint: if enable_lmstudio {
                Input::new()
                    .with_prompt("LM Studio endpoint")
                    .default(defaults.lmstudio_endpoint.clone())
                    .interact_text()?
            } else {
                defaults.lmstudio_endpoint.clone()
            },
            lmstudio_smart_model: if enable_lmstudio {
                Input::new()
                    .with_prompt("LM Studio smart model")
                    .default(defaults.lmstudio_smart_model.clone())
                    .interact_text()?
            } else {
                defaults.lmstudio_smart_model.clone()
            },
            lmstudio_judge_model: if enable_lmstudio {
                Input::new()
                    .with_prompt("LM Studio judge model")
                    .default(defaults.lmstudio_judge_model.clone())
                    .interact_text()?
            } else {
                defaults.lmstudio_judge_model.clone()
            },
            enable_anthropic_extra,
            anthropic_api_key: if enable_anthropic_extra {
                Password::new()
                    .with_prompt("ANTHROPIC_API_KEY (optional; blank to set later)")
                    .allow_empty_password(true)
                    .interact()?
            } else {
                String::new()
            },
            anthropic_smart_model: if enable_anthropic_extra {
                Input::new()
                    .with_prompt("Anthropic smart model")
                    .default(defaults.anthropic_smart_model.clone())
                    .interact_text()?
            } else {
                defaults.anthropic_smart_model.clone()
            },
            anthropic_judge_model: if enable_anthropic_extra {
                Input::new()
                    .with_prompt("Anthropic judge model")
                    .default(defaults.anthropic_judge_model.clone())
                    .interact_text()?
            } else {
                defaults.anthropic_judge_model.clone()
            },
            enable_gemini,
            gemini_endpoint: if enable_gemini {
                Input::new()
                    .with_prompt("Gemini endpoint")
                    .default(defaults.gemini_endpoint.clone())
                    .interact_text()?
            } else {
                defaults.gemini_endpoint.clone()
            },
            gemini_api_key: if enable_gemini {
                Password::new()
                    .with_prompt("GEMINI_API_KEY (optional; blank to set later)")
                    .allow_empty_password(true)
                    .interact()?
            } else {
                String::new()
            },
            gemini_smart_model: if enable_gemini {
                Input::new()
                    .with_prompt("Gemini smart model")
                    .default(defaults.gemini_smart_model.clone())
                    .interact_text()?
            } else {
                defaults.gemini_smart_model.clone()
            },
            gemini_judge_model: if enable_gemini {
                Input::new()
                    .with_prompt("Gemini judge model")
                    .default(defaults.gemini_judge_model.clone())
                    .interact_text()?
            } else {
                defaults.gemini_judge_model.clone()
            },
            deploy_smoketest_agents: Some(deploy_smoketest_agents),
            enable_otlp_logging,
            otlp_endpoint: if enable_otlp_logging {
                Input::new()
                    .with_prompt("OTLP collector endpoint")
                    .default(defaults.otlp_endpoint.clone())
                    .interact_text()?
            } else {
                defaults.otlp_endpoint.clone()
            },
            otlp_protocol: if enable_otlp_logging {
                Input::new()
                    .with_prompt("OTLP protocol (grpc/http)")
                    .default(defaults.otlp_protocol.clone())
                    .interact_text()?
            } else {
                defaults.otlp_protocol.clone()
            },
            otlp_min_level: if enable_otlp_logging {
                Input::new()
                    .with_prompt("Minimum OTLP log level")
                    .default(defaults.otlp_min_level.clone())
                    .interact_text()?
            } else {
                defaults.otlp_min_level.clone()
            },
            otlp_service_name: if enable_otlp_logging {
                Input::new()
                    .with_prompt("OTLP service name")
                    .default(defaults.otlp_service_name.clone())
                    .interact_text()?
            } else {
                defaults.otlp_service_name.clone()
            },
        })
    }

    /// Collect IAM-specific settings whenever IAM profile is enabled so users
    /// can configure Keycloak without needing advanced mode.
    fn collect_iam_config(
        &self,
        components: &SelectedComponents,
        advanced: &mut AdvancedConfig,
    ) -> Result<()> {
        if self.yes || !components.iam {
            return Ok(());
        }

        println!();
        println!("{}", "IAM configuration (Keycloak):".bold());

        let keycloak_password: String = Password::new()
            .with_prompt("KEYCLOAK_ADMIN_PASSWORD (blank to keep default 'admin')")
            .allow_empty_password(true)
            .interact()?;
        if !keycloak_password.is_empty() {
            advanced.keycloak_admin_password = keycloak_password;
        }

        Ok(())
    }

    /// Collect secrets-backend settings whenever OpenBao profile is enabled so
    /// users can configure secrets independently from IAM.
    fn collect_secrets_config(
        &self,
        components: &SelectedComponents,
        advanced: &mut AdvancedConfig,
    ) -> Result<()> {
        if self.yes || !components.secrets {
            return Ok(());
        }

        println!();
        println!("{}", "Secrets configuration (OpenBao):".bold());

        advanced.openbao_secret_id = Input::new()
            .with_prompt("OPENBAO_SECRET_ID")
            .default(advanced.openbao_secret_id.clone())
            .interact_text()?;

        Ok(())
    }

    /// Render the `aegis-config.yaml` content from collected inputs.
    pub fn render_aegis_config(
        &self,
        config: &NodeConfig,
        components: &SelectedComponents,
        tag: &str,
    ) -> String {
        let (base_provider_section, default_provider) = match &components.llm {
            LlmChoice::Ollama => (
                format!(
                    r#"    - name: "local"
      type: "ollama"
      endpoint: "http://ollama:11434"
      enabled: true
      models:
        - alias: "default"
          model: "{model}"
          capabilities: ["code", "reasoning"]
          context_window: 8192
          cost_per_1k_tokens: 0.0
        - alias: "smart"
          model: "{model}"
          capabilities: ["code", "reasoning"]
          context_window: 8192
          cost_per_1k_tokens: 0.0
        - alias: "judge"
          model: "{model}"
          capabilities: ["reasoning"]
          context_window: 8192
          cost_per_1k_tokens: 0.0
"#,
                    model = config.ollama_model
                ),
                "local",
            ),
            LlmChoice::OpenAI => (
                r#"    - name: "openai"
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
        - alias: "smart"
          model: "gpt-4o"
          capabilities: ["code", "reasoning"]
          context_window: 128000
          cost_per_1k_tokens: 0.005
        - alias: "judge"
          model: "gpt-4o"
          capabilities: ["reasoning"]
          context_window: 128000
          cost_per_1k_tokens: 0.005
"#
                .to_string(),
                "openai",
            ),
            LlmChoice::Anthropic => (
                r#"    - name: "anthropic"
      type: "anthropic"
      endpoint: "https://api.anthropic.com"
      enabled: true
      api_key: "env:ANTHROPIC_API_KEY"
      models:
        - alias: "default"
          model: "claude-haiku-4-5"
          capabilities: ["code", "reasoning"]
          context_window: 200000
          cost_per_1k_tokens: 0.0008
        - alias: "smart"
          model: "claude-sonnet-4-5"
          capabilities: ["code", "reasoning"]
          context_window: 200000
          cost_per_1k_tokens: 0.003
        - alias: "judge"
          model: "claude-sonnet-4-5"
          capabilities: ["reasoning"]
          context_window: 200000
          cost_per_1k_tokens: 0.003
"#
                .to_string(),
                "anthropic",
            ),
        };
        let extra_lmstudio_section = if config.advanced.enable_lmstudio {
            format!(
                r#"
    - name: "lmstudio"
      type: "openai-compatible"
      endpoint: "{endpoint}"
      enabled: true
      models:
        - alias: "smart"
          model: "{smart_model}"
          capabilities: ["code", "reasoning"]
          context_window: 8192
          cost_per_1k_tokens: 0.0
        - alias: "judge"
          model: "{judge_model}"
          capabilities: ["reasoning"]
          context_window: 8192
          cost_per_1k_tokens: 0.0
"#,
                endpoint = config.advanced.lmstudio_endpoint,
                smart_model = config.advanced.lmstudio_smart_model,
                judge_model = config.advanced.lmstudio_judge_model,
            )
        } else {
            String::new()
        };
        let extra_anthropic_section = if config.advanced.enable_anthropic_extra
            && !matches!(components.llm, LlmChoice::Anthropic)
        {
            format!(
                r#"
    - name: "anthropic-extra"
      type: "anthropic"
      endpoint: "https://api.anthropic.com"
      enabled: true
      api_key: "env:ANTHROPIC_API_KEY"
      models:
        - alias: "smart"
          model: "{smart_model}"
          capabilities: ["code", "reasoning"]
          context_window: 200000
          cost_per_1k_tokens: 0.003
        - alias: "judge"
          model: "{judge_model}"
          capabilities: ["reasoning"]
          context_window: 200000
          cost_per_1k_tokens: 0.003
"#,
                smart_model = config.advanced.anthropic_smart_model,
                judge_model = config.advanced.anthropic_judge_model,
            )
        } else {
            String::new()
        };
        let extra_gemini_section = if config.advanced.enable_gemini {
            format!(
                r#"
    - name: "gemini"
      type: "gemini"
      endpoint: "{endpoint}"
      enabled: true
      api_key: "env:GEMINI_API_KEY"
      models:
        - alias: "smart"
          model: "{smart_model}"
          capabilities: ["code", "reasoning"]
          context_window: 1048576
          cost_per_1k_tokens: 0.0
        - alias: "judge"
          model: "{judge_model}"
          capabilities: ["reasoning"]
          context_window: 1048576
          cost_per_1k_tokens: 0.0
"#,
                endpoint = config.advanced.gemini_endpoint,
                smart_model = config.advanced.gemini_smart_model,
                judge_model = config.advanced.gemini_judge_model,
            )
        } else {
            String::new()
        };
        let llm_section = format!(
            r#"  llm_providers:
{base_provider_section}{extra_lmstudio_section}{extra_anthropic_section}{extra_gemini_section}

  llm_selection:
    strategy: "prefer-cloud"
    default_provider: "{default_provider}"
    max_retries: 3
    retry_delay_ms: 1000
"#,
        );

        let database_section = r#"
  database:
    url: "env:AEGIS_DATABASE_URL"
    max_connections: 5
    connect_timeout_seconds: 5
"#;

        let builtin_dispatchers_section = r#"
  builtin_dispatchers:
    - name: "cmd.run"
      enabled: true
      description: "Executes a shell command inside the agent's ephemeral container environment."
      capabilities:
        - name: "cmd.run"
    - name: "fs.read"
      enabled: true
      description: "Read the contents of a file at the given POSIX path."
      capabilities:
        - name: "fs.read"
          skip_judge: true
    - name: "fs.write"
      enabled: true
      description: "Write content to a file at the given POSIX path."
      capabilities:
        - name: "fs.write"
    - name: "fs.list"
      enabled: true
      description: "List the contents of a directory."
      capabilities:
        - name: "fs.list"
          skip_judge: true
    - name: "fs.create_dir"
      enabled: true
      description: "Creates a new directory along with any necessary parent directories."
      capabilities:
        - name: "fs.create_dir"
    - name: "fs.delete"
      enabled: true
      description: "Deletes a file or directory."
      capabilities:
        - name: "fs.delete"
    - name: "fs.edit"
      enabled: true
      description: "Performs an exact string replacement in a file."
      capabilities:
        - name: "fs.edit"
    - name: "fs.multi_edit"
      enabled: true
      description: "Performs multiple sequential string replacements in a file."
      capabilities:
        - name: "fs.multi_edit"
    - name: "fs.grep"
      enabled: true
      description: "Recursively searches for a regex pattern within files in a given directory."
      capabilities:
        - name: "fs.grep"
          skip_judge: true
    - name: "fs.glob"
      enabled: true
      description: "Recursively matches files against a glob pattern."
      capabilities:
        - name: "fs.glob"
          skip_judge: true
    - name: "web.search"
      enabled: true
      description: "Performs an internet search query."
      capabilities:
        - name: "web.search"
          skip_judge: true
    - name: "web.fetch"
      enabled: true
      description: "Fetches content from a URL, optionally converting HTML to Markdown."
      capabilities:
        - name: "web.fetch"
          skip_judge: true
    - name: "aegis.schema.get"
      enabled: true
      description: "Returns the canonical JSON Schema for a manifest kind (agent or workflow)."
      capabilities:
        - name: "aegis.schema.get"
          skip_judge: true
    - name: "aegis.schema.validate"
      enabled: true
      description: "Validates a manifest YAML string against its canonical JSON Schema."
      capabilities:
        - name: "aegis.schema.validate"
          skip_judge: true
    - name: "aegis.agent.create"
      enabled: true
      description: "Parses, validates, and deploys an Agent manifest."
      capabilities:
        - name: "aegis.agent.create"
    - name: "aegis.agent.list"
      enabled: true
      description: "Lists currently deployed agents and metadata."
      capabilities:
        - name: "aegis.agent.list"
          skip_judge: true
    - name: "aegis.workflow.create"
      enabled: true
      description: "Performs strict deterministic + semantic workflow validation and registers the workflow on pass."
      capabilities:
        - name: "aegis.workflow.create"
    - name: "aegis.agent.update"
      enabled: true
      description: "Updates an existing Agent manifest in the registry."
      capabilities:
        - name: "aegis.agent.update"
    - name: "aegis.agent.export"
      enabled: true
      description: "Exports an Agent manifest by name."
      capabilities:
        - name: "aegis.agent.export"
          skip_judge: true
    - name: "aegis.agent.delete"
      enabled: true
      description: "Removes a deployed agent from the registry by UUID."
      capabilities:
        - name: "aegis.agent.delete"
    - name: "aegis.agent.generate"
      enabled: true
      description: "Generates an Agent manifest from a natural-language intent."
      capabilities:
        - name: "aegis.agent.generate"
    - name: "aegis.workflow.list"
      enabled: true
      description: "Lists currently registered workflows and metadata."
      capabilities:
        - name: "aegis.workflow.list"
          skip_judge: true
    - name: "aegis.workflow.update"
      enabled: true
      description: "Updates an existing Workflow manifest in the registry."
      capabilities:
        - name: "aegis.workflow.update"
    - name: "aegis.workflow.export"
      enabled: true
      description: "Exports a Workflow manifest by name."
      capabilities:
        - name: "aegis.workflow.export"
          skip_judge: true
    - name: "aegis.workflow.delete"
      enabled: true
      description: "Removes a registered workflow from the registry by name."
      capabilities:
        - name: "aegis.workflow.delete"
    - name: "aegis.workflow.run"
      enabled: true
      description: "Executes a registered workflow by name with optional input parameters."
      capabilities:
        - name: "aegis.workflow.run"
    - name: "aegis.workflow.generate"
      enabled: true
      description: "Generates a Workflow manifest from a natural-language objective."
      capabilities:
        - name: "aegis.workflow.generate"
    - name: "aegis.task.execute"
      enabled: true
      description: "Starts a new agent execution (task) by agent UUID or name."
      capabilities:
        - name: "aegis.task.execute"
    - name: "aegis.task.status"
      enabled: true
      description: "Returns the current status and output of an execution by UUID."
      capabilities:
        - name: "aegis.task.status"
          skip_judge: true
    - name: "aegis.task.list"
      enabled: true
      description: "Lists recent executions, optionally filtered by agent."
      capabilities:
        - name: "aegis.task.list"
          skip_judge: true
    - name: "aegis.task.cancel"
      enabled: true
      description: "Cancels an active agent execution by UUID."
      capabilities:
        - name: "aegis.task.cancel"
    - name: "aegis.task.remove"
      enabled: true
      description: "Removes a completed or failed execution record by UUID."
      capabilities:
        - name: "aegis.task.remove"
    - name: "aegis.system.info"
      enabled: true
      description: "Returns system version, status, and capabilities."
      capabilities:
        - name: "aegis.system.info"
          skip_judge: true
    - name: "aegis.system.config"
      enabled: true
      description: "Returns the current node configuration."
      capabilities:
        - name: "aegis.system.config"
          skip_judge: true
"#;

        let temporal_section = if components.temporal {
            r#"
  temporal:
    address: "temporal:7233"
    worker_http_endpoint: "http://temporal-worker:3000"
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
    backend: "local_host"
    local_host:
      mount_point: "/tmp/aegis-volumes"
"#
        };
        let smcp_gateway_section = if components.smcp_gateway {
            r#"
  smcp_gateway:
    url: "env:SMCP_GATEWAY_URL"
"#
        } else {
            ""
        };

        let otlp_section = if config.advanced.enable_otlp_logging {
            format!(
                r#"
      otlp_endpoint: "env:AEGIS_OTLP_ENDPOINT"
      otlp_protocol: "{protocol}"
      min_level: "{min_level}"
      service_name: "{service_name}"
      batch:
        max_queue_size: 2048
        scheduled_delay_ms: 5000
        max_export_batch_size: 512
        export_timeout_ms: 10000
      tls:
        verify: true"#,
                protocol = config.advanced.otlp_protocol,
                min_level = config.advanced.otlp_min_level,
                service_name = config.advanced.otlp_service_name,
            )
        } else {
            String::new()
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
    type: "{node_type}"

  image_tag: "{image_tag}"
{llm_section}
{builtin_dispatchers_section}
  runtime:
    docker_network_mode: "env:AEGIS_DOCKER_NETWORK"
    orchestrator_url: "env:AEGIS_ORCHESTRATOR_URL"
    nfs_server_host: "env:AEGIS_NFS_HOST"

  network:
    bind_address: "{bind_address}"
    port: {api_port}

  observability:
    logging:
      level: "{log_level}"{otlp_section}
{database_section}{temporal_section}{storage_section}{smcp_gateway_section}"#,
            node_name = config.node_name,
            node_id = config.node_id,
            node_type = config.advanced.node_type,
            bind_address = config.advanced.bind_address,
            api_port = config.advanced.api_port,
            log_level = config.advanced.log_level,
            image_tag = tag,
            llm_section = llm_section,
            builtin_dispatchers_section = builtin_dispatchers_section,
            otlp_section = otlp_section,
            database_section = database_section,
            temporal_section = temporal_section,
            storage_section = storage_section,
            smcp_gateway_section = smcp_gateway_section,
        )
    }

    /// Render the `.env` file content.
    pub fn render_env(
        &self,
        config: &NodeConfig,
        components: &SelectedComponents,
    ) -> Result<String> {
        let profiles = components.compose_profiles();
        let smcp_private_key = generate_smcp_private_key_env_value()?;
        let smcp_gateway_url_section = if components.smcp_gateway {
            "\n# ─── SMCP Tooling Gateway ─────────────────────────────────────────────────────\nSMCP_GATEWAY_URL=http://aegis-smcp-gateway:50055\n"
        } else {
            ""
        };

        let otlp_logging_section = if config.advanced.enable_otlp_logging {
            format!(
                "\n# ─── OTLP External Logging ───────────────────────────────────────────────────\nAEGIS_OTLP_ENDPOINT={}\n",
                config.advanced.otlp_endpoint
            )
        } else {
            String::new()
        };

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
        let anthropic_key_line = if config.advanced.enable_anthropic_extra
            && !matches!(components.llm, LlmChoice::Anthropic)
        {
            if config.advanced.anthropic_api_key.is_empty() {
                "ANTHROPIC_API_KEY=sk-ant-...".to_string()
            } else {
                format!("ANTHROPIC_API_KEY={}", config.advanced.anthropic_api_key)
            }
        } else {
            String::new()
        };
        let gemini_key_line = if config.advanced.enable_gemini {
            if config.advanced.gemini_api_key.is_empty() {
                "GEMINI_API_KEY=AIza...".to_string()
            } else {
                format!("GEMINI_API_KEY={}", config.advanced.gemini_api_key)
            }
        } else {
            String::new()
        };
        let additional_provider_env = [anthropic_key_line, gemini_key_line]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        Ok(format!(
            r#"# AEGIS Local Stack — generated by `aegis init`
# Edit this file to customise your environment.

# ─── Compose Profiles ─────────────────────────────────────────────────────────
# Controls which optional services are started.
# Profiles: core (always), temporal, storage, iam, secrets, llm
COMPOSE_PROFILES={profiles}

# ─── LLM Provider ─────────────────────────────────────────────────────────────
{api_key_line}
{additional_provider_env}

# ─── Keycloak ─────────────────────────────────────────────────────────────────
KEYCLOAK_ADMIN_PASSWORD={keycloak_admin_password}

# ─── AEGIS Runtime Networking ─────────────────────────────────────────────────
AEGIS_DOCKER_NETWORK={docker_network}
AEGIS_ORCHESTRATOR_URL={orchestrator_url}

# NFS server host — set to the correct value for your platform:
#   Linux native / WSL2  → 127.0.0.1
#   Docker Desktop       → host.docker.internal
AEGIS_NFS_HOST={nfs_host}

# ─── Secrets Management (OpenBao) ─────────────────────────────────────────────
OPENBAO_SECRET_ID={openbao_secret_id}

# ─── Database ─────────────────────────────────────────────────────────────────
AEGIS_DATABASE_URL={database_url}
TEMPORAL_WORKER_SECRET={temporal_worker_secret}

# ─── Runtime ──────────────────────────────────────────────────────────────────
AEGIS_KEEP_CONTAINER={keep_container}
AEGIS_SMCP_PRIVATE_KEY='{smcp_private_key}'
{smcp_gateway_url_section}{otlp_logging_section}"#,
            profiles = profiles,
            api_key_line = api_key_line,
            additional_provider_env = additional_provider_env,
            keycloak_admin_password = config.advanced.keycloak_admin_password,
            docker_network = config.advanced.docker_network,
            orchestrator_url = config.advanced.orchestrator_url,
            nfs_host = config.advanced.nfs_host,
            openbao_secret_id = config.advanced.openbao_secret_id,
            database_url = config.advanced.database_url,
            temporal_worker_secret = config.advanced.temporal_worker_secret,
            keep_container = if config.advanced.keep_container {
                "true"
            } else {
                "false"
            },
            smcp_private_key = smcp_private_key,
            smcp_gateway_url_section = smcp_gateway_url_section,
            otlp_logging_section = otlp_logging_section,
        ))
    }
}

/// Generate a 2048-bit RSA private key for SMCP token signing and encode it as
/// PEM for multi-line single-quoted `.env` storage.
fn generate_smcp_private_key_env_value() -> Result<String> {
    let mut rng = OsRng;
    let private_key =
        RsaPrivateKey::new(&mut rng, 2048).context("Failed to generate SMCP RSA private key")?;
    let pem = private_key
        .to_pkcs1_pem(LineEnding::LF)
        .context("Failed to encode SMCP RSA private key as PEM")?;
    Ok(pem.trim_end().to_string())
}

/// Ensure `./local-volumes` exists with permissions that allow the non-root
/// runtime container user to create execution volume directories.
fn ensure_local_volumes_dir_permissions(working_dir: &Path) -> Result<()> {
    let local_volumes_dir = working_dir.join("local-volumes");
    std::fs::create_dir_all(&local_volumes_dir).with_context(|| {
        format!(
            "Failed to create local volumes directory {}",
            local_volumes_dir.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&local_volumes_dir)
            .with_context(|| {
                format!(
                    "Failed to read metadata for {}",
                    local_volumes_dir.display()
                )
            })?
            .permissions();
        perms.set_mode(0o777);
        std::fs::set_permissions(&local_volumes_dir, perms).with_context(|| {
            format!(
                "Failed to set permissions on {}",
                local_volumes_dir.display()
            )
        })?;
    }

    Ok(())
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with('~') {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(s.trim_start_matches("~/").trim_start_matches('~'));
        }
    }
    path.to_path_buf()
}
