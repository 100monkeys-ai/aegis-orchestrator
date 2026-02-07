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
