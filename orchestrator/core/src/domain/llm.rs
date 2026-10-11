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
        // Default: concatenate history into a single prompt and return as FinalText.
        // A single prompt has no image form.
        if carries_images(messages) {
            return Err(images_refused("a single-prompt provider"));
        }
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
    /// Message content: text parts and image parts, in order.
    pub content: MessageContent,
    /// Tool call ID (required when `role == "tool"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant (required when `role == "assistant"` and it generated tool calls).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ChatToolCall>>,
}

/// A message's content: its parts in order, text and images interleaved
/// (AEGIS ADR-046 D2). Content of one text part, the common case, is
/// serialised as a plain string; content with an image as the array of parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ContentWire", into = "ContentWire")]
pub struct MessageContent {
    parts: Vec<ContentPart>,
}

/// One part of a message's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Text.
    Text { text: String },
    /// An image, by URL or by its bytes, with its MIME type
    /// (`image/png`, `image/jpeg`, …).
    Image {
        source: ImageSource,
        mime_type: String,
    },
}

/// Where an image part's image is: a URL the provider fetches, or its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageSource {
    /// A URL the provider can fetch.
    Url(String),
    /// The image's bytes, serialised as base64.
    Bytes(#[serde(with = "base64_bytes")] Vec<u8>),
}

impl ImageSource {
    /// The URL a provider is sent: the URL itself, or a `data:` URL of the
    /// bytes with their MIME type.
    pub fn url_for(&self, mime_type: &str) -> String {
        use base64::Engine;
        match self {
            ImageSource::Url(url) => url.clone(),
            ImageSource::Bytes(bytes) => format!(
                "data:{mime_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ),
        }
    }
}

mod base64_bytes {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(serde::de::Error::custom)
    }
}

/// The serialised form of [`MessageContent`]: a string, or the parts.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ContentWire {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl From<ContentWire> for MessageContent {
    fn from(wire: ContentWire) -> Self {
        match wire {
            ContentWire::Text(text) => MessageContent::from(text),
            ContentWire::Parts(parts) => MessageContent::from_parts(parts),
        }
    }
}

impl From<MessageContent> for ContentWire {
    fn from(content: MessageContent) -> Self {
        if content.has_images() || content.parts.len() > 1 {
            ContentWire::Parts(content.parts)
        } else {
            ContentWire::Text(content.as_text())
        }
    }
}

impl MessageContent {
    /// Content of these parts, in order.
    pub fn from_parts(parts: Vec<ContentPart>) -> Self {
        Self { parts }
    }

    /// The parts, in order.
    pub fn parts(&self) -> &[ContentPart] {
        &self.parts
    }

    /// Add parts after the last.
    pub fn extend(&mut self, parts: impl IntoIterator<Item = ContentPart>) {
        self.parts.extend(parts);
    }

    /// The text parts, joined by a blank line; images are left out.
    pub fn as_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Whether any part is an image.
    pub fn has_images(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, ContentPart::Image { .. }))
    }

    /// Whether the content holds no image and no text.
    pub fn is_empty(&self) -> bool {
        !self.has_images() && self.as_text().is_empty()
    }
}

impl From<String> for MessageContent {
    fn from(text: String) -> Self {
        Self {
            parts: vec![ContentPart::Text { text }],
        }
    }
}

impl From<&str> for MessageContent {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

impl std::fmt::Display for MessageContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_text())
    }
}

/// The words a refusal of an image part carries after the alias or model
/// it names. The registry recognises an adapter's refusal by them and puts
/// in the alias, which only it knows.
pub const IMAGES_REFUSED: &str = "cannot read images: ";

/// The refusal of an adapter whose provider is sent text only, for a
/// request carrying an image part. Returned before any request is sent.
pub fn images_refused(provider: &str) -> LLMError {
    LLMError::InvalidInput(format!(
        "{IMAGES_REFUSED}its provider, {provider}, is sent text only"
    ))
}

/// Whether any of the messages carries an image part.
pub fn carries_images(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| m.content.has_images())
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
            max_tokens: Some(8192),
            temperature: Some(0.4),
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

    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),
}
