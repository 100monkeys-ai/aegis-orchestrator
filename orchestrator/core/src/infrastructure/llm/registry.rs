// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

// LLM Provider Registry - Model Alias Resolution and Provider Management
//
// Manages LLM providers and resolves model aliases to actual providers.
// Implements fallback and retry strategies per ADR-009.

use crate::domain::llm::{GenerationOptions, GenerationResponse, LLMError, LLMProvider};
use crate::domain::node_config::{LLMProviderConfig, LLMSelectionStrategy, ModelConfig, NodeConfig};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use super::anthropic::AnthropicAdapter;
use super::ollama::OllamaAdapter;
use super::openai::OpenAIAdapter;

/// Registry for managing LLM providers and resolving model aliases
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LLMProvider>>,
    alias_map: HashMap<String, (String, ModelConfig)>, // alias -> (provider_name, model_config)
    _selection_strategy: LLMSelectionStrategy,
    fallback_provider: Option<String>,
    max_retries: u32,
    retry_delay_ms: u64,
}

impl ProviderRegistry {
    /// Create provider registry from node configuration
    pub fn from_config(config: &NodeConfig) -> anyhow::Result<Self> {
        let mut providers = HashMap::new();
        let mut alias_map = HashMap::new();

        info!("Initializing LLM provider registry");

        // Initialize providers from config
        for provider_config in &config.llm_providers {
            if !provider_config.enabled {
                info!("Provider '{}' disabled, skipping", provider_config.name);
                continue;
            }

            info!("Initializing provider: {}", provider_config.name);

            match Self::create_provider(provider_config) {
                Ok(provider) => {
                    providers.insert(provider_config.name.clone(), provider);

                    // Build alias mapping
                    for model_config in &provider_config.models {
                        info!(
                            "Mapping alias '{}' -> {} ({})",
                            model_config.alias, model_config.model, provider_config.name
                        );
                        alias_map.insert(
                            model_config.alias.clone(),
                            (provider_config.name.clone(), model_config.clone()),
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to initialize provider '{}': {}", provider_config.name, e);
                    // Continue with other providers
                }
            }
        }

        if providers.is_empty() {
            warn!("No LLM providers configured - semantic validation will not be available");
        }

        Ok(Self {
            providers,
            alias_map,
            _selection_strategy: config.llm_selection.strategy.clone(),
            fallback_provider: config.llm_selection.fallback_provider.clone(),
            max_retries: config.llm_selection.max_retries,
            retry_delay_ms: config.llm_selection.retry_delay_ms,
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
    fn resolve_api_key(key: &Option<String>) -> anyhow::Result<String> {
        match key {
            Some(k) if k.starts_with("env:") => {
                let var_name = k.strip_prefix("env:").unwrap();
                std::env::var(var_name).map_err(|_| {
                    anyhow::anyhow!("Environment variable not set: {}", var_name)
                })
            }
            Some(k) => Ok(k.clone()),
            None => Ok(String::new()), // For local providers without auth
        }
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
        let provider = self.providers.get(provider_name).ok_or_else(|| {
            LLMError::Provider(format!("Provider '{}' not found", provider_name))
        })?;

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
    use crate::domain::node_config::{LLMSelection, NodeConfig, NodeIdentity, NodeType};

    #[test]
    fn test_registry_creation() {
        let config = NodeConfig {
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
        };

        let registry = ProviderRegistry::from_config(&config).unwrap();
        assert!(registry.has_alias("default"));
        assert_eq!(registry.available_aliases().len(), 1);
    }
}
