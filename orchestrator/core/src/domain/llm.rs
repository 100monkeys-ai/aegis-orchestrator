// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Llm
//!
//! Provides llm functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements llm

// LLM Provider Domain Interface (Anti-Corruption Layer)
//
// Defines domain interface for LLM providers following DDD principles.
// Prevents vendor lock-in by abstracting external LLM APIs.
//
// Implementations in infrastructure/llm/ directory.
// See ADR-009: BYOLLM Provider System

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Domain interface for LLM providers
/// Anti-Corruption Layer that isolates business logic from vendor APIs
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// Generate a completion from the LLM
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError>;
    
    /// Check if provider is healthy and accessible
    async fn health_check(&self) -> Result<(), LLMError>;
}

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
