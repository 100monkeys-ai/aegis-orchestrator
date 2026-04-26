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

/// Indicates whether an API key is platform-managed or user-provided (BYOK).
///
/// Used by the inner loop to decide whether to enforce LLM rate limits:
/// BYOK users consume their own provider quota, so platform rate limits
/// for `LlmCall` and `LlmToken` are skipped (ADR-072).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// Key resolved from an environment variable or node config (platform-provided).
    Platform,
    /// Key provided directly by the user (Bring Your Own Key).
    User,
}

/// Maximum exponent for exponential backoff, preventing `u64` overflow in delay calculations.
/// For example, with `retry_delay_ms = 1000` (1 second), 2^16 × 1 s ≈ 18 hours; the
/// actual upper bound on backoff duration still depends on the configured base delay.
const MAX_BACKOFF_EXPONENT: u32 = 16;

/// Maximum backoff duration in milliseconds. Delays are clamped to this value regardless
/// of the configured base delay, ensuring retries always terminate in bounded time.
/// Matches the value produced by `retry_delay_ms = 1000` and `MAX_BACKOFF_EXPONENT = 16`.
const MAX_BACKOFF_MS: u64 = 65_536_000; // 2^16 * 1000 ms

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
    /// alias → raw `api_key` value from the provider config (before env resolution).
    /// Used to determine [`ApiKeySource`] for BYOK exemption (ADR-072).
    raw_api_keys: HashMap<String, Option<String>>,
    /// alias → per-model `max_output_tokens` override from config.
    /// When set, overrides `GenerationOptions::max_tokens` for calls on that alias.
    alias_max_output_tokens: HashMap<String, u32>,
    /// alias → per-model `temperature` override from config.
    /// When set, overrides `GenerationOptions::temperature` for calls on that alias.
    alias_temperatures: HashMap<String, f32>,
    max_retries: u32,
    retry_delay_ms: u64,
    /// Wall-clock budget for the full retry+fallback loop in `generate_chat` /
    /// `generate`. On elapsed: return `LLMError::Network("upstream timeout
    /// after Ns")`. Sourced from `LLMSelection::llm_overall_timeout_secs`.
    llm_overall_timeout_secs: u64,
}

/// Returns true for `LLMError` variants that are deterministic upstream
/// rejections — retrying or swapping to a fallback provider with the same
/// credentials/region won't change the answer.
fn is_non_retryable(e: &LLMError) -> bool {
    matches!(
        e,
        LLMError::Authentication(_)
            | LLMError::InvalidInput(_)
            | LLMError::ModelNotFound(_)
            | LLMError::ServiceUnavailable(_)
    )
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
        let mut raw_api_keys: HashMap<String, Option<String>> = HashMap::new();
        let mut alias_max_output_tokens: HashMap<String, u32> = HashMap::new();
        let mut alias_temperatures: HashMap<String, f32> = HashMap::new();

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
                                raw_api_keys.insert(alias.clone(), provider_config.api_key.clone());
                                if let Some(max_tokens) = model_config.max_output_tokens {
                                    info!(
                                        "Alias '{}' max_output_tokens override: {}",
                                        alias, max_tokens
                                    );
                                    alias_max_output_tokens.insert(alias.clone(), max_tokens);
                                }
                                if let Some(temp) = model_config.temperature {
                                    info!("Alias '{}' temperature override: {}", alias, temp);
                                    alias_temperatures.insert(alias.clone(), temp);
                                }
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
            raw_api_keys,
            alias_max_output_tokens,
            alias_temperatures,
            max_retries: config.spec.llm_selection.max_retries,
            retry_delay_ms: config.spec.llm_selection.retry_delay_ms,
            llm_overall_timeout_secs: config.spec.llm_selection.llm_overall_timeout_secs,
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

        let endpoint = if config.endpoint.is_empty() {
            match config.provider_type.as_str() {
                "openai" | "openai-compatible" => "https://api.openai.com/v1",
                "anthropic" => "https://api.anthropic.com/v1",
                "gemini" => "https://generativelanguage.googleapis.com/v1beta",
                "ollama" => "http://localhost:11434",
                _ => "",
            }
            .to_string()
        } else {
            config.endpoint.clone()
        };

        let provider: Arc<dyn LLMProvider> = match config.provider_type.as_str() {
            "openai" => Arc::new(OpenAIAdapter::new(endpoint, api_key, model.to_string())),
            "ollama" => Arc::new(OllamaAdapter::new(endpoint, model.to_string())),
            "anthropic" => Arc::new(AnthropicAdapter::new(endpoint, api_key, model.to_string())),
            "gemini" => Arc::new(GeminiAdapter::new(endpoint, api_key, model.to_string())),
            // OpenAI-compatible APIs (LM Studio, vLLM, etc.)
            "openai-compatible" => {
                Arc::new(OpenAIAdapter::new(endpoint, api_key, model.to_string()))
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

    /// Apply per-alias `max_output_tokens` override to a copy of the given options.
    /// If the alias has a configured override, `max_tokens` is replaced; otherwise
    /// the original options are returned unchanged.
    fn apply_alias_options(&self, alias: &str, options: &GenerationOptions) -> GenerationOptions {
        let mut opts = options.clone();
        if let Some(&max_tokens) = self.alias_max_output_tokens.get(alias) {
            opts.max_tokens = Some(max_tokens);
        }
        if let Some(&temp) = self.alias_temperatures.get(alias) {
            opts.temperature = Some(temp);
        }
        opts
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
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{alias}' not found")))?;

        info!("LLM inference: alias='{}', model='{}'", alias, model_name);

        let effective_options = self.apply_alias_options(alias, options);

        let overall_budget = tokio::time::Duration::from_secs(self.llm_overall_timeout_secs.max(1));

        let inner = async {
            let mut last_error: Option<LLMError> = None;

            for attempt in 0..self.max_retries {
                match provider
                    .generate_chat(messages, tools, &effective_options)
                    .await
                {
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

                        // Short-circuit on deterministic upstream rejections — retrying with
                        // the same credentials (or with the fallback that shares them) is futile.
                        if is_non_retryable(&e) {
                            if matches!(e, LLMError::ServiceUnavailable(_)) {
                                if let Some((fallback_model, fallback)) = &self.fallback_provider {
                                    info!(
                                        "Service unavailable, trying fallback provider (model='{}')",
                                        fallback_model
                                    );
                                    match fallback.generate_chat(messages, tools, options).await {
                                        Ok(r) => return Ok(r),
                                        Err(fe) => {
                                            // Fallback also short-circuits on non-retryable
                                            // errors (likely the same root cause).
                                            return Err(fe);
                                        }
                                    }
                                }
                            }
                            return Err(e);
                        }

                        last_error = Some(e);

                        if attempt == self.max_retries - 1 {
                            if let Some((fallback_model, fallback)) = &self.fallback_provider {
                                info!("Trying fallback provider (model='{}')", fallback_model);
                                match fallback.generate_chat(messages, tools, options).await {
                                    Ok(r) => return Ok(r),
                                    Err(fe) => {
                                        if is_non_retryable(&fe) {
                                            return Err(fe);
                                        }
                                        return Err(fe);
                                    }
                                }
                            }
                        }

                        tokio::time::sleep(tokio::time::Duration::from_millis({
                            let capped_attempt = attempt.min(MAX_BACKOFF_EXPONENT);
                            self.retry_delay_ms
                                .saturating_mul(2_u64.saturating_pow(capped_attempt))
                                .min(MAX_BACKOFF_MS)
                        }))
                        .await;
                    }
                }
            }

            Err(last_error.unwrap_or_else(|| LLMError::Provider("Unknown error".into())))
        };

        match tokio::time::timeout(overall_budget, inner).await {
            Ok(result) => result,
            Err(_) => {
                let secs = overall_budget.as_secs();
                warn!(
                    "generate_chat overall timeout: alias='{}', model='{}', budget={}s",
                    alias, model_name, secs
                );
                Err(LLMError::Network(format!("upstream timeout after {secs}s")))
            }
        }
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
            .ok_or_else(|| LLMError::ModelNotFound(format!("Model alias '{alias}' not found")))?;

        info!(
            "LLM text generation: alias='{}', model='{}'",
            alias, model_name
        );

        let effective_options = self.apply_alias_options(alias, options);
        let mut last_error = None;

        for attempt in 0..self.max_retries {
            match provider.generate(prompt, &effective_options).await {
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

                    // Exponential backoff with capped exponent to avoid overflow
                    let exp = std::cmp::min(attempt, MAX_BACKOFF_EXPONENT);
                    let backoff_factor = 2_u64.saturating_pow(exp);
                    let delay_ms = self
                        .retry_delay_ms
                        .saturating_mul(backoff_factor)
                        .min(MAX_BACKOFF_MS);

                    tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
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

    /// Determine whether the API key for a given model alias is platform-managed
    /// or user-provided (BYOK).
    ///
    /// Heuristic:
    /// - `None` (no key configured, e.g. local Ollama) → `Platform`
    /// - Starts with `"env:"` → `Platform` (resolved from an environment variable)
    /// - Any other literal value → `User` (directly configured BYOK)
    ///
    /// BYOK users are exempt from `LlmCall` and `LlmToken` rate limits (ADR-072)
    /// because they consume their own provider quota.
    pub fn key_source_for_alias(&self, alias: &str) -> ApiKeySource {
        match self.raw_api_keys.get(alias) {
            Some(Some(key)) if !key.starts_with("env:") => ApiKeySource::User,
            _ => ApiKeySource::Platform,
        }
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

impl ProviderRegistry {
    /// Test-only constructor that wires concrete primary + optional fallback
    /// adapters without going through `from_config`. Used to inject mock
    /// providers in retry-policy regression tests.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        primary: Arc<dyn LLMProvider>,
        fallback: Option<Arc<dyn LLMProvider>>,
        max_retries: u32,
        retry_delay_ms: u64,
        llm_overall_timeout_secs: u64,
    ) -> Self {
        let mut alias_map = HashMap::new();
        alias_map.insert(
            "default".to_string(),
            ("test-model".to_string(), primary.clone()),
        );
        let mut providers = HashMap::new();
        providers.insert("primary".to_string(), primary);
        let fallback_provider = fallback.map(|f| ("test-fallback-model".to_string(), f));

        Self {
            alias_map,
            providers,
            fallback_provider,
            raw_api_keys: HashMap::new(),
            alias_max_output_tokens: HashMap::new(),
            alias_temperatures: HashMap::new(),
            max_retries,
            retry_delay_ms,
            llm_overall_timeout_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::llm::{ChatResponse, FinishReason, GenerationResponse, TokenUsage};
    use crate::domain::node_config::{
        LLMSelection, ManifestMetadata, ModelConfig, NodeConfigManifest, NodeConfigSpec,
        NodeIdentity, NodeType,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Programmable mock provider. Returns one of the queued errors per call,
    /// or a success response when the queue is exhausted (configurable).
    /// Records the number of `generate_chat` calls received.
    struct MockProvider {
        /// Queue of responses to return, in order. When empty, returns the
        /// `default_response` if set, else `LLMError::Provider("queue empty")`.
        queue: Mutex<Vec<Result<ChatResponse, LLMError>>>,
        default_response: Mutex<Option<Result<ChatResponse, LLMError>>>,
        calls: AtomicUsize,
        /// When true, the mock panics if called — used to assert "this provider
        /// must NOT be invoked" in fallback short-circuit tests.
        panic_on_call: bool,
        /// When true, every call blocks forever (used for overall-timeout test).
        block_forever: bool,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                queue: Mutex::new(Vec::new()),
                default_response: Mutex::new(None),
                calls: AtomicUsize::new(0),
                panic_on_call: false,
                block_forever: false,
            }
        }

        fn with_responses(responses: Vec<Result<ChatResponse, LLMError>>) -> Arc<Self> {
            let m = Self::new();
            *m.queue.lock().unwrap() = responses;
            Arc::new(m)
        }

        fn panicking() -> Arc<Self> {
            let mut m = Self::new();
            m.panic_on_call = true;
            Arc::new(m)
        }

        fn blocking() -> Arc<Self> {
            let mut m = Self::new();
            m.block_forever = true;
            Arc::new(m)
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LLMProvider for MockProvider {
        async fn generate(
            &self,
            _prompt: &str,
            _options: &GenerationOptions,
        ) -> Result<GenerationResponse, LLMError> {
            unimplemented!("test mock: generate not used")
        }

        async fn generate_chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSchema],
            _options: &GenerationOptions,
        ) -> Result<ChatResponse, LLMError> {
            if self.panic_on_call {
                panic!("MockProvider configured to fail test if invoked");
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block_forever {
                futures::future::pending::<()>().await;
                unreachable!();
            }
            let mut q = self.queue.lock().unwrap();
            if !q.is_empty() {
                return q.remove(0);
            }
            drop(q);
            if let Some(r) = self.default_response.lock().unwrap().as_ref() {
                return match r {
                    Ok(c) => Ok(c.clone()),
                    Err(e) => Err(clone_llm_error(e)),
                };
            }
            Err(LLMError::Provider("queue empty".into()))
        }

        async fn health_check(&self) -> Result<(), LLMError> {
            Ok(())
        }
    }

    fn clone_llm_error(e: &LLMError) -> LLMError {
        match e {
            LLMError::Network(s) => LLMError::Network(s.clone()),
            LLMError::Authentication(s) => LLMError::Authentication(s.clone()),
            LLMError::RateLimit => LLMError::RateLimit,
            LLMError::ModelNotFound(s) => LLMError::ModelNotFound(s.clone()),
            LLMError::Provider(s) => LLMError::Provider(s.clone()),
            LLMError::InvalidInput(s) => LLMError::InvalidInput(s.clone()),
            LLMError::ServiceUnavailable(s) => LLMError::ServiceUnavailable(s.clone()),
        }
    }

    fn ok_response() -> ChatResponse {
        ChatResponse::FinalText(GenerationResponse {
            text: "ok".to_string(),
            usage: TokenUsage::default(),
            provider: "mock".to_string(),
            model: "test-model".to_string(),
            finish_reason: FinishReason::Stop,
        })
    }

    fn make_registry(
        primary: Arc<dyn LLMProvider>,
        fallback: Option<Arc<dyn LLMProvider>>,
        timeout_secs: u64,
    ) -> ProviderRegistry {
        ProviderRegistry::new_for_test(primary, fallback, 3, 1, timeout_secs)
    }

    #[tokio::test]
    async fn auth_error_short_circuits() {
        let primary = MockProvider::with_responses(vec![Err(LLMError::Authentication(
            "403 forbidden".into(),
        ))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback.clone() as Arc<dyn LLMProvider>),
            30,
        );

        let start = std::time::Instant::now();
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        let elapsed = start.elapsed();

        assert!(matches!(res, Err(LLMError::Authentication(_))));
        assert_eq!(
            primary.call_count(),
            1,
            "primary must be called exactly once"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "auth error must short-circuit fast (got {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn model_not_found_short_circuits() {
        let primary = MockProvider::with_responses(vec![Err(LLMError::ModelNotFound(
            "no-such-model".into(),
        ))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(matches!(res, Err(LLMError::ModelNotFound(_))));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn invalid_input_short_circuits() {
        let primary =
            MockProvider::with_responses(vec![Err(LLMError::InvalidInput("bad arg".into()))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(matches!(res, Err(LLMError::InvalidInput(_))));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn provider_error_retries_then_succeeds() {
        let primary = MockProvider::with_responses(vec![
            Err(LLMError::Provider("transient".into())),
            Err(LLMError::Provider("transient".into())),
            Ok(ok_response()),
        ]);
        let registry = make_registry(primary.clone() as Arc<dyn LLMProvider>, None, 30);

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(res.is_ok(), "expected success after retries: {res:?}");
        assert_eq!(primary.call_count(), 3);
    }

    #[tokio::test]
    async fn network_error_overall_timeout_fires() {
        let primary = MockProvider::blocking();
        let registry = make_registry(primary as Arc<dyn LLMProvider>, None, 1);

        let start = std::time::Instant::now();
        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        let elapsed = start.elapsed();

        assert!(
            matches!(&res, Err(LLMError::Network(msg)) if msg.contains("upstream timeout")),
            "expected upstream timeout, got {res:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "must return within budget (got {elapsed:?})"
        );
    }

    #[tokio::test]
    async fn fallback_short_circuits_on_auth() {
        // Primary returns Authentication on every attempt; fallback would
        // succeed if invoked, but must NOT be invoked.
        let primary = MockProvider::with_responses(vec![Err(LLMError::Authentication(
            "403 forbidden".into(),
        ))]);
        let fallback = MockProvider::panicking();
        let registry = make_registry(
            primary.clone() as Arc<dyn LLMProvider>,
            Some(fallback as Arc<dyn LLMProvider>),
            30,
        );

        let res = registry
            .generate_chat("default", &[], &[], &GenerationOptions::default())
            .await;
        assert!(matches!(res, Err(LLMError::Authentication(_))));
        assert_eq!(primary.call_count(), 1);
    }

    #[tokio::test]
    async fn llm_error_class_maps_correctly() {
        use crate::domain::events::LlmErrorClass;
        assert_eq!(
            LlmErrorClass::from(&LLMError::Authentication("x".into())),
            LlmErrorClass::Authentication
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::RateLimit),
            LlmErrorClass::RateLimit
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::ModelNotFound("m".into())),
            LlmErrorClass::ModelNotFound
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::InvalidInput("x".into())),
            LlmErrorClass::InvalidInput
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::ServiceUnavailable("x".into())),
            LlmErrorClass::ServiceUnavailable
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::Network("x".into())),
            LlmErrorClass::Network
        );
        assert_eq!(
            LlmErrorClass::from(&LLMError::Provider("x".into())),
            LlmErrorClass::Provider
        );
    }

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
                        max_output_tokens: None,
                        temperature: None,
                    }],
                }],
                llm_selection: LLMSelection::default(),
                runtime: crate::domain::node_config::RuntimeConfig::default(),
                network: None,
                observability: None,
                storage: None, // Optional storage configuration (ADR-032)
                mcp_servers: None,
                seal: None,
                security_contexts: None,
                database: None,
                temporal: None,
                cortex: None,
                secrets: None,
                builtin_dispatchers: None,
                cluster: None,
                registry_credentials: vec![],
                iam: None,
                grpc_auth: None,
                seal_gateway: None,
                image_tag: None,
                deploy_builtins: false,
                force_deploy_builtins: None,
                max_execution_list_limit: None,
                billing: None,
                zaru: None,
            },
        };

        let registry = ProviderRegistry::from_config(&config).unwrap();
        assert!(registry.has_alias("default"));
        assert_eq!(registry.available_aliases().len(), 1);
    }
}
