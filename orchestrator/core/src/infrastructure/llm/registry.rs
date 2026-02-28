// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # LLM Provider Registry
//!
//! `ProviderRegistry` resolves model aliases (e.g. `"default"`, `"fast"`,
//! `"smart"`) to real `LLMProvider` adapters at runtime based on node config.
//! Includes retry-with-exponential-backoff and one-level fallback.

use crate::domain::llm::{
    ChatMessage, ChatResponse, GenerationOptions, GenerationResponse, LLMError, LLMProvider,
    ToolSchema,
};
use crate::domain::node_config::{
    resolve_env_value, LLMProviderConfig, LLMSelectionStrategy, ModelConfig, NodeConfigManifest,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use super::anthropic::AnthropicAdapter;
use super::ollama::OllamaAdapter;
use super::openai::OpenAIAdapter;

/// Builds the alias → (provider_name, ModelConfig) map respecting the configured
/// `LLMSelectionStrategy`.
///
/// When multiple enabled providers define the same alias, the strategy decides which
/// entry wins rather than the last-write-wins behaviour of a plain sequential insert:
///
/// | Strategy           | Wins when …                                                        |
/// |--------------------|---------------------------------------------------------------------|
/// | `PreferLocal`      | New provider `is_local()` and existing entry is not local          |
/// | `PreferCloud`      | New provider is not local and existing entry is local              |
/// | `CostOptimized`    | New model's `cost_per_1k_tokens` < existing model's cost           |
/// | `LatencyOptimized` | Same as `PreferLocal` — local inference always has lower latency    |
///
/// Providers that have been disabled via `enabled: false` are skipped entirely.
fn build_alias_map(
    provider_configs: &[LLMProviderConfig],
    strategy: &LLMSelectionStrategy,
) -> HashMap<String, (String, ModelConfig)> {
    // Intermediate map also tracks is_local for the current winner so we can
    // apply the strategy without re-looking up the provider config later.
    // (alias -> (provider_name, is_local, model_config))
    let mut working: HashMap<String, (String, bool, ModelConfig)> = HashMap::new();

    for provider_config in provider_configs {
        if !provider_config.enabled {
            continue;
        }

        let new_is_local = provider_config.is_local();

        for model_config in &provider_config.models {
            let alias = &model_config.alias;

            let should_insert = match working.get(alias) {
                None => true,
                Some((_, existing_is_local, existing_model)) => match strategy {
                    LLMSelectionStrategy::PreferLocal | LLMSelectionStrategy::LatencyOptimized => {
                        // Overwrite only if the new provider is local and the existing is not.
                        new_is_local && !existing_is_local
                    }
                    LLMSelectionStrategy::PreferCloud => {
                        // Overwrite only if the new provider is cloud and the existing is local.
                        !new_is_local && *existing_is_local
                    }
                    LLMSelectionStrategy::CostOptimized => {
                        model_config.cost_per_1k_tokens < existing_model.cost_per_1k_tokens
                    }
                },
            };

            if should_insert {
                info!(
                    "Mapping alias '{}' -> {} ({}) [strategy={:?}]",
                    alias, model_config.model, provider_config.name, strategy
                );
                working.insert(
                    alias.clone(),
                    (
                        provider_config.name.clone(),
                        new_is_local,
                        model_config.clone(),
                    ),
                );
            } else {
                info!(
                    "Alias '{}' already mapped to a preferred provider, skipping {} ({}) [strategy={:?}]",
                    alias, model_config.model, provider_config.name, strategy
                );
            }
        }
    }

    // Strip the is_local bookkeeping field — callers only need (provider_name, ModelConfig).
    working
        .into_iter()
        .map(|(alias, (provider_name, _, model_config))| (alias, (provider_name, model_config)))
        .collect()
}

/// Registry for managing LLM providers and resolving model aliases
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LLMProvider>>,
    alias_map: HashMap<String, (String, ModelConfig)>, // alias -> (provider_name, model_config)
    fallback_provider: Option<String>,
    max_retries: u32,
    retry_delay_ms: u64,
}

impl ProviderRegistry {
    /// Create provider registry from node configuration
    pub fn from_config(config: &NodeConfigManifest) -> anyhow::Result<Self> {
        let mut providers = HashMap::new();

        info!("Initializing LLM provider registry");

        // Initialize providers from config
        for provider_config in &config.spec.llm_providers {
            if !provider_config.enabled {
                info!("Provider '{}' disabled, skipping", provider_config.name);
                continue;
            }

            info!("Initializing provider: {}", provider_config.name);

            match Self::create_provider(provider_config) {
                Ok(provider) => {
                    providers.insert(provider_config.name.clone(), provider);
                }
                Err(e) => {
                    warn!(
                        "Failed to initialize provider '{}': {}",
                        provider_config.name, e
                    );
                    // Continue with other providers
                }
            }
        }

        if providers.is_empty() {
            warn!("No LLM providers configured - semantic validation will not be available");
        }

        let alias_map = build_alias_map(
            &config.spec.llm_providers,
            &config.spec.llm_selection.strategy,
        );

        Ok(Self {
            providers,
            alias_map,
            fallback_provider: config.spec.llm_selection.fallback_provider.clone(),
            max_retries: config.spec.llm_selection.max_retries,
            retry_delay_ms: config.spec.llm_selection.retry_delay_ms,
        })
    }

    /// Create a provider instance from configuration
    fn create_provider(config: &LLMProviderConfig) -> anyhow::Result<Arc<dyn LLMProvider>> {
        let api_key = Self::resolve_api_key(&config.api_key)?;

        let provider: Arc<dyn LLMProvider> = match config.provider_type.as_str() {
            "openai" => {
                let model = config
                    .models
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("No models configured"))?
                    .model
                    .clone();
                Arc::new(OpenAIAdapter::new(config.endpoint.clone(), api_key, model))
            }
            "ollama" => {
                let model = config
                    .models
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("No models configured"))?
                    .model
                    .clone();
                Arc::new(OllamaAdapter::new(config.endpoint.clone(), model))
            }
            "anthropic" => {
                let model = config
                    .models
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("No models configured"))?
                    .model
                    .clone();
                Arc::new(AnthropicAdapter::new(api_key, model))
            }
            "openai-compatible" => {
                // OpenAI-compatible APIs (LM Studio, vLLM, etc.)
                let model = config
                    .models
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("No models configured"))?
                    .model
                    .clone();
                Arc::new(OpenAIAdapter::new(config.endpoint.clone(), api_key, model))
            }
            _ => anyhow::bail!("Unsupported provider type: {}", config.provider_type),
        };

        Ok(provider)
    }

    /// Resolve API key from config (supports "env:VAR_NAME" syntax)
    /// Delegates to the centralized `resolve_env_value()` utility.
    fn resolve_api_key(key: &Option<String>) -> anyhow::Result<String> {
        match key {
            Some(k) => resolve_env_value(k),
            None => Ok(String::new()), // For local providers without auth
        }
    }

    /// Generate using a model alias, multi-turn messages, and optional tools.
    /// Includes retry logic and fallback to secondary provider.
    pub async fn generate_chat(
        &self,
        alias: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        let (provider_name, _model_config) = self
            .alias_map
            .get(alias)
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{}' not found", alias)))?;

        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| LLMError::Provider(format!("Provider '{}' not found", provider_name)))?;

        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match provider.generate_chat(messages, tools, options).await {
                Ok(response) => {
                    info!("generate_chat successful on attempt {}", attempt + 1);
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        "generate_chat failed (attempt {}/{}): {:?}",
                        attempt + 1,
                        self.max_retries,
                        e
                    );
                    last_error = Some(e);

                    if attempt == self.max_retries - 1 {
                        if let Some(fallback) = &self.fallback_provider {
                            if let Some(fb) = self.providers.get(fallback) {
                                info!("Trying fallback provider: {}", fallback);
                                return fb.generate_chat(messages, tools, options).await;
                            }
                        }
                    }

                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        self.retry_delay_ms * 2_u64.pow(attempt),
                    ))
                    .await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
    }

    /// Generate text using a model alias
    /// Includes retry logic and fallback to secondary provider
    pub async fn generate(
        &self,
        alias: &str,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        // Look up alias
        let (provider_name, _model_config) = self
            .alias_map
            .get(alias)
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{}' not found", alias)))?;

        // Get provider
        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| LLMError::Provider(format!("Provider '{}' not found", provider_name)))?;

        // Generate with retries
        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match provider.generate(prompt, options).await {
                Ok(response) => {
                    info!("Generation successful on attempt {}", attempt + 1);
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        "Generation failed (attempt {}/{}): {:?}",
                        attempt + 1,
                        self.max_retries,
                        e
                    );
                    last_error = Some(e);

                    // Try fallback provider on last attempt
                    if attempt == self.max_retries - 1 {
                        if let Some(fallback) = &self.fallback_provider {
                            if let Some(fallback_provider) = self.providers.get(fallback) {
                                info!("Trying fallback provider: {}", fallback);
                                return fallback_provider.generate(prompt, options).await;
                            }
                        }
                    }

                    // Exponential backoff
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        self.retry_delay_ms * 2_u64.pow(attempt),
                    ))
                    .await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
    }

    /// Check health of all providers
    pub async fn health_check_all(&self) -> HashMap<String, Result<(), LLMError>> {
        let mut results = HashMap::new();

        for (name, provider) in &self.providers {
            info!("Health checking provider: {}", name);
            results.insert(name.clone(), provider.health_check().await);
        }

        results
    }

    /// Get list of available model aliases
    pub fn available_aliases(&self) -> Vec<String> {
        self.alias_map.keys().cloned().collect()
    }

    /// Check if a model alias exists
    pub fn has_alias(&self, alias: &str) -> bool {
        self.alias_map.contains_key(alias)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::node_config::{
        LLMSelection, ManifestMetadata, NodeConfigManifest, NodeConfigSpec, NodeIdentity, NodeType,
    };

    #[test]
    fn test_registry_creation() {
        let config = NodeConfigManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "NodeConfig".to_string(),
            metadata: ManifestMetadata {
                name: "test-node".to_string(),
                version: Some("1.0.0".to_string()),
                labels: None,
            },
            spec: NodeConfigSpec {
                node: NodeIdentity {
                    id: "test".to_string(),
                    node_type: NodeType::Edge,
                    region: None,
                    tags: vec![],
                    resources: None,
                },
                llm_providers: vec![LLMProviderConfig {
                    name: "test-ollama".to_string(),
                    provider_type: "ollama".to_string(),
                    endpoint: "http://localhost:11434".to_string(),
                    api_key: None,
                    enabled: true,
                    models: vec![ModelConfig {
                        alias: "default".to_string(),
                        model: "llama3.2".to_string(),
                        capabilities: vec!["chat".to_string()],
                        context_window: 8192,
                        cost_per_1k_tokens: 0.0,
                    }],
                }],
                llm_selection: LLMSelection::default(),
                runtime: crate::domain::node_config::RuntimeConfig::default(),
                network: None,
                observability: None,
                storage: None, // Optional storage configuration (ADR-032)
                mcp_servers: None,
                smcp: None,
                security_contexts: None,
                database: None,
                temporal: None,
                cortex: None,
                registry_credentials: vec![],
            },
        };

        let registry = ProviderRegistry::from_config(&config).unwrap();
        assert!(registry.has_alias("default"));
        assert_eq!(registry.available_aliases().len(), 1);
    }
}

// Implement domain LLMProvider trait for infrastructure ProviderRegistry
// This allows the infrastructure to be used through domain interfaces
#[async_trait]
impl LLMProvider for ProviderRegistry {
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        self.generate("default", prompt, options).await
    }

    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        self.generate_chat("default", messages, tools, options)
            .await
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        let (provider_name, _) = self
            .alias_map
            .get("default")
            .ok_or_else(|| LLMError::Provider("Default model alias not configured".into()))?;

        let provider = self
            .providers
            .get(provider_name)
            .ok_or_else(|| LLMError::Provider(format!("Provider '{}' not found", provider_name)))?;

        provider.health_check().await
    }
}
