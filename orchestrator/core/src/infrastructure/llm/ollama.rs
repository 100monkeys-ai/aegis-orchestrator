// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Ollama
//!
//! Provides ollama functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements ollama

// Ollama LLM Provider Adapter
//
// Anti-Corruption Layer for Ollama local models
// Supports air-gapped deployments with local LLMs

use crate::domain::llm::{FinishReason, GenerationOptions, GenerationResponse, LLMError, LLMProvider};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct OllamaAdapter {
    client: reqwest::Client,
    endpoint: String,
    model: String,
}

#[derive(Serialize)]
struct OllamaRequest {
    model: String,
    prompt: String,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i32>,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
    done: bool,
    eval_count: Option<u32>,
    prompt_eval_count: Option<u32>,
}

impl OllamaAdapter {
    pub fn new(endpoint: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            model,
        }
    }
}

#[async_trait]
impl LLMProvider for OllamaAdapter {
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        let request = OllamaRequest {
            model: self.model.clone(),
            prompt: prompt.to_string(),
            stream: false,
            options: Some(OllamaOptions {
                temperature: options.temperature,
                num_predict: options.max_tokens.map(|t| t as i32),
            }),
        };

        let url = format!("{}/api/generate", self.endpoint.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            
            return Err(if status == 404 {
                LLMError::ModelNotFound(self.model.clone())
            } else {
                LLMError::Provider(format!("HTTP {}: {}", status, error_text))
            });
        }

        let ollama_response: OllamaResponse = response
            .json()
            .await
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {}", e)))?;

        Ok(GenerationResponse {
            text: ollama_response.response,
            usage: crate::domain::llm::TokenUsage {
                prompt_tokens: ollama_response.prompt_eval_count.unwrap_or(0),
                completion_tokens: ollama_response.eval_count.unwrap_or(0),
                total_tokens: ollama_response.prompt_eval_count.unwrap_or(0) + ollama_response.eval_count.unwrap_or(0),
            },
            provider: "ollama".to_string(),
            model: self.model.clone(),
            finish_reason: if ollama_response.done {
                FinishReason::Stop
            } else {
                FinishReason::Length
            },
        })
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        // Check if Ollama server is running by listing models
        let url = format!("{}/api/tags", self.endpoint.trim_end_matches('/'));

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        if response.status().is_success() {
            Ok(())
        } else {
            Err(LLMError::Network(format!("HTTP {}", response.status())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::llm::FinishReason;
    
    #[test]
    fn test_ollama_adapter_creation() {
        let adapter = OllamaAdapter::new(
            "http://localhost:11434".to_string(),
            "llama2".to_string(),
        );
        
        assert_eq!(adapter.endpoint, "http://localhost:11434");
        assert_eq!(adapter.model, "llama2");
    }
    
    #[test]
    fn test_ollama_request_serialization() {
        let request = OllamaRequest {
            model: "llama2".to_string(),
            prompt: "Hello Ollama".to_string(),
            stream: false,
            options: Some(OllamaOptions {
                temperature: Some(0.7),
                num_predict: Some(100),
            }),
        };
        
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "llama2");
        assert_eq!(json["prompt"], "Hello Ollama");
        assert_eq!(json["stream"], false);
        // Use approximate comparison for floating point
        let temp = json["options"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
        assert_eq!(json["options"]["num_predict"], 100);
    }
    
    #[test]
    fn test_ollama_response_deserialization() {
        let json = serde_json::json!({
            "response": "Hello! I'm Llama.",
            "done": true,
            "eval_count": 50,
            "prompt_eval_count": 20
        });
        
        let response: OllamaResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.response, "Hello! I'm Llama.");
        assert_eq!(response.done, true);
        assert_eq!(response.eval_count, Some(50));
        assert_eq!(response.prompt_eval_count, Some(20));
    }
    
    #[test]
    fn test_ollama_finish_reason() {
        // Test done flag to finish reason mapping
        let done_response = OllamaResponse {
            response: "Test".to_string(),
            done: true,
            eval_count: Some(10),
            prompt_eval_count: Some(5),
        };
        
        let finish_reason = if done_response.done {
            FinishReason::Stop
        } else {
            FinishReason::Length
        };
        assert_eq!(finish_reason, FinishReason::Stop);
        
        // Test not done
        let incomplete_response = OllamaResponse {
            response: "Test".to_string(),
            done: false,
            eval_count: Some(10),
            prompt_eval_count: Some(5),
        };
        
        let finish_reason = if incomplete_response.done {
            FinishReason::Stop
        } else {
            FinishReason::Length
        };
        assert_eq!(finish_reason, FinishReason::Length);
    }
    
    #[test]
    fn test_ollama_token_counting() {
        let response = OllamaResponse {
            response: "Test response".to_string(),
            done: true,
            eval_count: Some(30),
            prompt_eval_count: Some(15),
        };
        
        let prompt_tokens = response.prompt_eval_count.unwrap_or(0);
        let completion_tokens = response.eval_count.unwrap_or(0);
        let total_tokens = prompt_tokens + completion_tokens;
        
        assert_eq!(prompt_tokens, 15);
        assert_eq!(completion_tokens, 30);
        assert_eq!(total_tokens, 45);
    }
    
    #[test]
    fn test_ollama_options_with_none() {
        let options = OllamaOptions {
            temperature: None,
            num_predict: None,
        };
        
        let json = serde_json::to_value(&options).unwrap();
        // Fields with None should be skipped in serialization
        assert!(!json.as_object().unwrap().contains_key("temperature"));
        assert!(!json.as_object().unwrap().contains_key("num_predict"));
    }
}
