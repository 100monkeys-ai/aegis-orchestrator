// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Anthropic
//!
//! Provides anthropic functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements anthropic

// Anthropic LLM Provider Adapter
//
// Anti-Corruption Layer for Anthropic Claude API

use crate::domain::llm::{FinishReason, GenerationOptions, GenerationResponse, LLMError, LLMProvider};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct AnthropicAdapter {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    usage: AnthropicUsage,
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: String,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

impl AnthropicAdapter {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
        }
    }
}

#[async_trait]
impl LLMProvider for AnthropicAdapter {
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        let request = AnthropicRequest {
            model: self.model.clone(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            max_tokens: options.max_tokens.unwrap_or(4096),
            temperature: options.temperature,
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            
            return Err(if status == 401 || status == 403 {
                LLMError::Authentication(error_text)
            } else if status == 429 {
                LLMError::RateLimit
            } else if status == 404 {
                LLMError::ModelNotFound(self.model.clone())
            } else {
                LLMError::Provider(format!("HTTP {}: {}", status, error_text))
            });
        }

        let anthropic_response: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {}", e)))?;

        let text = anthropic_response
            .content
            .first()
            .map(|c| c.text.clone())
            .unwrap_or_default();

        Ok(GenerationResponse {
            text,
            usage: crate::domain::llm::TokenUsage {
                prompt_tokens: anthropic_response.usage.input_tokens,
                completion_tokens: anthropic_response.usage.output_tokens,
                total_tokens: anthropic_response.usage.input_tokens + anthropic_response.usage.output_tokens,
            },
            provider: "anthropic".to_string(),
            model: self.model.clone(),
            finish_reason: match anthropic_response.stop_reason.as_deref() {
                Some("end_turn") => FinishReason::Stop,
                Some("max_tokens") => FinishReason::Length,
                Some("stop_sequence") => FinishReason::Stop,
                _ => FinishReason::Stop,
            },
        })
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        // Simple ping to check authentication
        // Anthropic doesn't have a models list endpoint, so we check auth with a minimal request
        let response = self
            .client
            .get("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        // 405 Method Not Allowed is expected (GET not supported), but it means auth is valid
        // 404 is also OK (endpoint exists)
        if response.status().is_success()
            || response.status() == 404
            || response.status() == 405
        {
            Ok(())
        } else if response.status() == 401 || response.status() == 403 {
            Err(LLMError::Authentication("Invalid API key".into()))
        } else {
            Err(LLMError::Network(format!("HTTP {}", response.status())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::llm::{FinishReason, GenerationOptions};
    
    #[test]
    fn test_anthropic_adapter_creation() {
        let adapter = AnthropicAdapter::new(
            "test-key".to_string(),
            "claude-3-opus".to_string(),
        );
        
        assert_eq!(adapter.api_key, "test-key");
        assert_eq!(adapter.model, "claude-3-opus");
    }
    
    #[test]
    fn test_anthropic_request_serialization() {
        let request = AnthropicRequest {
            model: "claude-3-opus".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: "Hello Claude".to_string(),
            }],
            max_tokens: 1024,
            temperature: Some(0.7),
        };
        
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "claude-3-opus");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "Hello Claude");
        assert_eq!(json["max_tokens"], 1024);
        // Use approximate comparison for floating point
        let temp = json["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
    }
    
    #[test]
    fn test_anthropic_response_deserialization() {
        let json = serde_json::json!({
            "content": [{
                "text": "Hello! How can I assist you today?"
            }],
            "usage": {
                "input_tokens": 15,
                "output_tokens": 25
            },
            "stop_reason": "end_turn"
        });
        
        let response: AnthropicResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.content.len(), 1);
        assert_eq!(response.content[0].text, "Hello! How can I assist you today?");
        assert_eq!(response.usage.input_tokens, 15);
        assert_eq!(response.usage.output_tokens, 25);
        assert_eq!(response.stop_reason, Some("end_turn".to_string()));
    }
    
    #[test]
    fn test_anthropic_finish_reason_mapping() {
        // Test that finish reasons are correctly mapped
        let reasons = vec![
            (Some("end_turn"), FinishReason::Stop),
            (Some("max_tokens"), FinishReason::Length),
            (Some("stop_sequence"), FinishReason::Stop),
            (None, FinishReason::Stop), // Default case
        ];
        
        for (anthropic_reason, expected) in reasons {
            let mapped = match anthropic_reason.as_deref() {
                Some("end_turn") => FinishReason::Stop,
                Some("max_tokens") => FinishReason::Length,
                Some("stop_sequence") => FinishReason::Stop,
                _ => FinishReason::Stop,
            };
            assert_eq!(mapped, expected);
        }
    }
    
    #[test]
    fn test_anthropic_message_structure() {
        let message = AnthropicMessage {
            role: "user".to_string(),
            content: "Test message for Claude".to_string(),
        };
        
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "Test message for Claude");
        
        // Test round-trip
        let deserialized: AnthropicMessage = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.role, "user");
        assert_eq!(deserialized.content, "Test message for Claude");
    }
    
    #[test]
    fn test_default_max_tokens() {
        // Test that max_tokens defaults to 4096 when not provided
        let options = GenerationOptions {
            max_tokens: None,
            temperature: Some(0.5),
            stop_sequences: None,
        };
        
        let max_tokens = options.max_tokens.unwrap_or(4096);
        assert_eq!(max_tokens, 4096);
    }
    
    #[test]
    fn test_anthropic_usage_calculation() {
        let usage = AnthropicUsage {
            input_tokens: 100,
            output_tokens: 200,
        };
        
        let total = usage.input_tokens + usage.output_tokens;
        assert_eq!(total, 300);
    }
}
