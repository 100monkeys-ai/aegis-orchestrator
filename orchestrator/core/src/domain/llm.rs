// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # LLM Provider Domain Interface
//!
//! Defines the `LLMProvider` domain trait (Anti-Corruption Layer) that insulates
//! the orchestrator's business logic from vendor-specific LLM APIs.
//!
//! ## Trait Methods
//!
//! | Method | Purpose |
//! |--------|---------|
//! | `generate` | Single-turn text completion (legacy / simple prompts) |
//! | `generate_chat` | Multi-turn conversation with optional tool-call support |
//! | `health_check` | Liveness probe |
//!
//! Implementations live in `crate::infrastructure::llm`:
//! - `openai.rs` — OpenAI `gpt-*` / Azure OpenAI
//! - `anthropic.rs` — Anthropic `claude-*`
//! - `ollama.rs` — Ollama local models (dev/offline)
//!
//! The active provider is selected at runtime by
//! `crate::infrastructure::llm::registry::ProviderRegistry` based on the
//! agent manifest's `spec.runtime.model` alias field.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Domain interface for LLM providers.
/// Anti-Corruption Layer that isolates business logic from vendor APIs.
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// Generate a single-turn completion from the LLM.
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError>;

    /// Generate a multi-turn chat response, optionally with tool-call support.
    ///
    /// The default implementation concatenates `messages` as `role: content` lines
    /// and delegates to `generate()`. Providers that support native function-calling
    /// (OpenAI, Anthropic, Ollama ≥ 0.2) should override this method.
    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        // Default: concatenate history into a single prompt and return as FinalText
        let prompt = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = tools; // not used in default fallback
        let response = self.generate(&prompt, options).await?;
        Ok(ChatResponse::FinalText(response))
    }

    /// Check if provider is healthy and accessible.
    async fn health_check(&self) -> Result<(), LLMError>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Chat / multi-turn types
// ──────────────────────────────────────────────────────────────────────────────

/// A single message in a multi-turn conversation.
///
/// Mirrors `ConversationMessage` in the application layer but lives in the domain
/// so adapters can work with it directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Role: `"system"`, `"user"`, `"assistant"`, or `"tool"`.
    pub role: String,
    /// Message content.
    pub content: String,
    /// Tool call ID (required when `role == "tool"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// JSON Schema description of a single tool available to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    /// Tool name (e.g. `"fs.read"`, `"web-search.search"`).
    pub name: String,
    /// Human-readable description shown to the model.
    pub description: String,
    /// JSON Schema object describing the tool's input parameters.
    pub parameters: serde_json::Value,
}

/// A tool invocation requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatToolCall {
    /// Opaque correlation ID returned by the provider.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as a JSON value.
    pub arguments: serde_json::Value,
}

/// Result of a [`LLMProvider::generate_chat`] call.
#[derive(Debug, Clone)]
pub enum ChatResponse {
    /// The model produced a final text answer — the conversation is complete.
    FinalText(GenerationResponse),
    /// The model requested one or more tool calls — the caller must execute
    /// them and append results before calling `generate_chat` again.
    ToolCalls(Vec<ChatToolCall>),
}

// ──────────────────────────────────────────────────────────────────────────────
// Generation options / response
// ──────────────────────────────────────────────────────────────────────────────

/// Options for LLM generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationOptions {
    /// Maximum tokens to generate
    pub max_tokens: Option<u32>,

    /// Sampling temperature (0.0 = deterministic, 1.0 = creative)
    pub temperature: Option<f32>,

    /// Sequences that stop generation
    pub stop_sequences: Option<Vec<String>>,
}

impl Default for GenerationOptions {
    fn default() -> Self {
        Self {
            max_tokens: Some(4096),
            temperature: Some(0.7),
            stop_sequences: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GenerationResponse {
    /// Generated text
    pub text: String,

    /// Token usage stats
    pub usage: TokenUsage,

    /// Usage provider name (e.g., "openai", "ollama")
    pub provider: String,

    /// Model used (e.g., "gpt-4o", "llama3.2")
    pub model: String,

    /// Why generation stopped
    pub finish_reason: FinishReason,
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Reason why generation stopped
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural completion (model decided to stop)
    Stop,

    /// Hit max_tokens limit
    Length,

    /// Blocked by content filter
    ContentFilter,
}

/// Errors that can occur during LLM operations
#[derive(Debug, thiserror::Error)]
pub enum LLMError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Authentication failed: {0}")]
    Authentication(String),

    #[error("Rate limit exceeded")]
    RateLimit,

    #[error("Model not found: {0}")]
    ModelNotFound(String),

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),
}
