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
            tokens_used: anthropic_response.usage.input_tokens
                + anthropic_response.usage.output_tokens,
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
