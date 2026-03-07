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
    resolve_env_value, LLMProviderConfig, LLMSelectionStrategy, NodeConfigManifest,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use super::anthropic::AnthropicAdapter;
use super::gemini::GeminiAdapter;
use super::ollama::OllamaAdapter;
use super::openai::OpenAIAdapter;

/// Registry for managing LLM providers and resolving model aliases.
///
/// Each entry in `alias_map` is an `Arc<dyn LLMProvider>` that was constructed at
/// startup with the **exact model name** that won the selection-strategy evaluation.
/// There is no runtime model-override: the adapter *is* the model.
///
/// `providers` holds one health-check adapter per provider name (using `models.first()`).
pub struct ProviderRegistry {
    /// alias → pre-configured adapter for that exact model.
    alias_map: HashMap<String, (String, Arc<dyn LLMProvider>)>,
    /// provider_name → adapter used for health checks (built from models.first()).
    providers: HashMap<String, Arc<dyn LLMProvider>>,
    /// Fallback adapter resolved at construction time; used when primary exhausts retries.
    fallback_provider: Option<(String, Arc<dyn LLMProvider>)>,
    max_retries: u32,
    retry_delay_ms: u64,
}

impl ProviderRegistry {
    /// Create provider registry from node configuration.
    ///
    /// Applies the configured `LLMSelectionStrategy` to resolve each alias to its winner,
    /// then instantiates one `Arc<dyn LLMProvider>` per winning (provider, model) pair.
    /// Each adapter is pre-configured with the exact model name — no runtime override needed.
    ///
    /// | Strategy           | Wins when …                                                     |
    /// |--------------------|-----------------------------------------------------------------|
    /// | `PreferLocal`      | New provider `is_local()` and existing entry is not local       |
    /// | `PreferCloud`      | New provider is not local and existing entry is local           |
    /// | `CostOptimized`    | New model's `cost_per_1k_tokens` < existing model's cost        |
    /// | `LatencyOptimized` | Same as `PreferLocal` — local inference has lower latency       |
    pub fn from_config(config: &NodeConfigManifest) -> anyhow::Result<Self> {
        info!("Initializing LLM provider registry");

        let strategy = &config.spec.llm_selection.strategy;

        // ── Phase 1: health-check adapters (one per provider, models.first()) ────────────
        let mut providers: HashMap<String, Arc<dyn LLMProvider>> = HashMap::new();

        // ── Phase 1 + strategy selection pass ─────────────────────────────────────────
        // alias -> (provider_name, model_name, is_local, cost_per_1k_tokens)
        let mut alias_candidates: HashMap<String, (String, String, bool, f64)> = HashMap::new();

        for provider_config in &config.spec.llm_providers {
            if !provider_config.enabled {
                info!("Provider '{}' disabled, skipping", provider_config.name);
                continue;
            }

            let new_is_local = provider_config.is_local();

            // Health-check adapter uses the first model in the provider list.
            if let Some(first_model) = provider_config.models.first() {
                match Self::create_adapter(provider_config, &first_model.model) {
                    Ok(adapter) => {
                        providers.insert(provider_config.name.clone(), adapter);
                    }
                    Err(e) => {
                        warn!(
                            "Failed to initialize provider '{}': {}",
                            provider_config.name, e
                        );
                        continue;
                    }
                }
            }

            // Evaluate each alias under the selection strategy.
            for model_config in &provider_config.models {
                let alias = &model_config.alias;

                let should_insert = match alias_candidates.get(alias) {
                    None => true,
                    Some((_, _, existing_is_local, existing_cost)) => match strategy {
                        LLMSelectionStrategy::PreferLocal
                        | LLMSelectionStrategy::LatencyOptimized => {
                            new_is_local && !existing_is_local
                        }
                        LLMSelectionStrategy::PreferCloud => !new_is_local && *existing_is_local,
                        LLMSelectionStrategy::CostOptimized => {
                            model_config.cost_per_1k_tokens < *existing_cost
                        }
                    },
                };

                if should_insert {
                    info!(
                        "Mapping alias '{}' -> {} ({}) [strategy={:?}]",
                        alias, model_config.model, provider_config.name, strategy
                    );
                    alias_candidates.insert(
                        alias.clone(),
                        (
                            provider_config.name.clone(),
                            model_config.model.clone(),
                            new_is_local,
                            model_config.cost_per_1k_tokens,
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

        if providers.is_empty() {
            warn!("No LLM providers configured - semantic validation will not be available");
        }

        // ── Phase 2: build one per-model adapter per winning alias ─────────────────────
        let mut alias_map: HashMap<String, (String, Arc<dyn LLMProvider>)> = HashMap::new();

        for provider_config in &config.spec.llm_providers {
            if !provider_config.enabled {
                continue;
            }
            for model_config in &provider_config.models {
                let alias = &model_config.alias;
                if let Some((winner_provider, winner_model, _, _)) = alias_candidates.get(alias) {
                    if winner_provider == &provider_config.name
                        && winner_model == &model_config.model
                    {
                        match Self::create_adapter(provider_config, winner_model) {
                            Ok(adapter) => {
                                alias_map.insert(alias.clone(), (winner_model.clone(), adapter));
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to create adapter for alias '{}' ({}): {}",
                                    alias, winner_model, e
                                );
                            }
                        }
                    }
                }
            }
        }

        // Resolve fallback to a concrete adapter at construction time.
        // We look up the provider name and assume the first model was used for health checks.
        let fallback_provider = config
            .spec
            .llm_selection
            .fallback_provider
            .as_deref()
            .and_then(|name| {
                let adapter = providers.get(name)?.clone();
                let provider_config = config.spec.llm_providers.iter().find(|p| p.name == name)?;
                let first_model = provider_config.models.first()?.model.clone();
                Some((first_model, adapter))
            });

        Ok(Self {
            alias_map,
            providers,
            fallback_provider,
            max_retries: config.spec.llm_selection.max_retries,
            retry_delay_ms: config.spec.llm_selection.retry_delay_ms,
        })
    }

    /// Create an adapter for the given provider config initialized with a specific model name.
    ///
    /// Called twice per (provider, model) pair:
    /// 1. With `models.first()` to build the health-check adapter stored in `providers`.
    /// 2. With the strategy-selected model to build the per-alias adapter stored in `alias_map`.
    fn create_adapter(
        config: &LLMProviderConfig,
        model: &str,
    ) -> anyhow::Result<Arc<dyn LLMProvider>> {
        let api_key = Self::resolve_api_key(&config.api_key)?;

        let provider: Arc<dyn LLMProvider> = match config.provider_type.as_str() {
            "openai" => Arc::new(OpenAIAdapter::new(
                config.endpoint.clone(),
                api_key,
                model.to_string(),
            )),
            "ollama" => Arc::new(OllamaAdapter::new(
                config.endpoint.clone(),
                model.to_string(),
            )),
            "anthropic" => Arc::new(AnthropicAdapter::new(api_key, model.to_string())),
            "gemini" => Arc::new(GeminiAdapter::new(
                config.endpoint.clone(),
                api_key,
                model.to_string(),
            )),
            // OpenAI-compatible APIs (LM Studio, vLLM, etc.)
            "openai-compatible" => Arc::new(OpenAIAdapter::new(
                config.endpoint.clone(),
                api_key,
                model.to_string(),
            )),
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

    /// Generate a chat response for the given model alias.
    ///
    /// Resolves the alias directly to a pre-configured `Arc<dyn LLMProvider>` adapter;
    /// no model name override is needed at call time.
    /// Includes retry-with-exponential-backoff and one-level fallback.
    pub async fn generate_chat(
        &self,
        alias: &str,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        let (model_name, provider) = self
            .alias_map
            .get(alias)
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{}' not found", alias)))?;

        info!("LLM inference: alias='{}', model='{}'", alias, model_name);

        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match provider.generate_chat(messages, tools, options).await {
                Ok(response) => {
                    info!(
                        "generate_chat successful: alias='{}', model='{}', attempt={}",
                        alias,
                        model_name,
                        attempt + 1
                    );
                    return Ok(response);
                }
                Err(e) => {
                    warn!(
                        "generate_chat failed: alias='{}', attempt={}/{}: {:?}",
                        alias,
                        attempt + 1,
                        self.max_retries,
                        e
                    );
                    last_error = Some(e);

                    if attempt == self.max_retries - 1 {
                        if let Some((fallback_model, fallback)) = &self.fallback_provider {
                            info!("Trying fallback provider (model='{}')", fallback_model);
                            return fallback.generate_chat(messages, tools, options).await;
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

    /// Generate text for the given model alias.
    ///
    /// Resolves the alias directly to a pre-configured `Arc<dyn LLMProvider>` adapter;
    /// no model name override is needed at call time.
    /// Includes retry-with-exponential-backoff and one-level fallback.
    pub async fn generate(
        &self,
        alias: &str,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        let (model_name, provider) = self
            .alias_map
            .get(alias)
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{}' not found", alias)))?;

        info!(
            "LLM text generation: alias='{}', model='{}'",
            alias, model_name
        );

        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match provider.generate(prompt, options).await {
                Ok(response) => {
                    info!(
                        "Generation successful on attempt {} (model='{}')",
                        attempt + 1,
                        model_name
                    );
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

                    if attempt == self.max_retries - 1 {
                        if let Some((fallback_model, fallback)) = &self.fallback_provider {
                            info!("Trying fallback provider (model='{}')", fallback_model);
                            return fallback.generate(prompt, options).await;
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
        LLMSelection, ManifestMetadata, ModelConfig, NodeConfigManifest, NodeConfigSpec,
        NodeIdentity, NodeType,
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
                secrets: None,
                builtin_dispatchers: None,
                registry_credentials: vec![],
                iam: None,
                grpc_auth: None,
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
        let (_, provider) = self
            .alias_map
            .get("default")
            .ok_or_else(|| LLMError::Provider("Default model alias not configured".into()))?;

        provider.health_check().await
    }
}
