// OpenAI LLM Provider Adapter
//
// Anti-Corruption Layer for OpenAI API
// Also works with OpenAI-compatible APIs (LM Studio, vLLM, etc.)

use crate::domain::llm::{FinishReason, GenerationOptions, GenerationResponse, LLMError, LLMProvider};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct OpenAIAdapter {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
struct OpenAIMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
    usage: OpenAIUsage,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    message: OpenAIMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct OpenAIUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl OpenAIAdapter {
    pub fn new(endpoint: String, api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            api_key,
            model,
        }
    }
}

#[async_trait]
impl LLMProvider for OpenAIAdapter {
    async fn generate(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Result<GenerationResponse, LLMError> {
        // Translate our domain types to OpenAI's types
        let request = OpenAIRequest {
            model: self.model.clone(),
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: prompt.to_string(),
            }],
            max_tokens: options.max_tokens,
            temperature: options.temperature,
            stop: options.stop_sequences.clone(),
        };

        // Make API request
        let url = format!("{}/chat/completions", self.endpoint.trim_end_matches('/'));
        
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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

        let openai_response: OpenAIResponse = response
            .json()
            .await
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {}", e)))?;

        // Translate OpenAI's response to our domain types
        let choice = openai_response
            .choices
            .first()
            .ok_or_else(|| LLMError::Provider("No response from model".into()))?;

        Ok(GenerationResponse {
            text: choice.message.content.clone(),
            usage: crate::domain::llm::TokenUsage {
                prompt_tokens: openai_response.usage.prompt_tokens,
                completion_tokens: openai_response.usage.completion_tokens,
                total_tokens: openai_response.usage.total_tokens,
            },
            provider: "openai".to_string(), // Or use config name if passed/stored? 
            // Better to use "openai" as type, or pass name in constructor if we want "production-gpt4"
            // For now, hardcode provider type or store it?
            // The Registry stores provider *instances*. The instance doesn't know its registry alias.
            // But we can store "openai" or similar.
            // Let's use "openai" or the model name?
            // The request had 'provider' field we want to return.
            // Let's use the adapter type name or similar.
            // Wait, we can store "source" in adapter?
            // For now, "openai" is fine.
            model: self.model.clone(),
            finish_reason: match choice.finish_reason.as_str() {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                _ => FinishReason::Stop,
            },
        })
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        // Simple check - try to list models endpoint
        let url = format!("{}/models", self.endpoint.trim_end_matches('/'));
        
        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        if response.status().is_success() {
            Ok(())
        } else if response.status() == 401 || response.status() == 403 {
            Err(LLMError::Authentication("Invalid API key".into()))
        } else {
            Err(LLMError::Network(format!("HTTP {}", response.status())))
        }
    }
}
