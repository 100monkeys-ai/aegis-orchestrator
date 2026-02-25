// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # OpenAI / Azure OpenAI Adapter
//!
//! Implements the `LLMProvider` domain trait for OpenAI `gpt-*` models and
//! Azure OpenAI deployments. Acts as an **Anti-Corruption Layer** (ACL):
//! translates AEGIS domain types into OpenAI Chat Completions API payloads
//! and back, including native tool-call (function-calling) support.
//!
//! Also handles `openai-compatible` endpoints (LM Studio, vLLM, etc.) — pass
//! the custom `base_url` as `endpoint`.

use crate::domain::llm::{
    ChatMessage, ChatResponse, ChatToolCall, FinishReason, GenerationOptions, GenerationResponse,
    LLMError, LLMProvider, ToolSchema,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct OpenAIAdapter {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

// ─── Request types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// Present when role == "assistant" and the model requested tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    /// Present when role == "tool" (result of a tool call).
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String, // always "function"
    function: OpenAIToolFunction,
}

#[derive(Serialize, Deserialize, Clone)]
struct OpenAIToolFunction {
    name: String,
    arguments: String, // JSON string
}

// ─── Response types ───────────────────────────────────────────────────────────

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

// ─── Adapter ──────────────────────────────────────────────────────────────────

impl OpenAIAdapter {
    pub fn new(endpoint: String, api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            api_key,
            model,
        }
    }

    fn map_finish_reason(s: &str) -> FinishReason {
        match s {
            "stop" => FinishReason::Stop,
            "length" => FinishReason::Length,
            "content_filter" => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        }
    }

    fn build_token_usage(usage: &OpenAIUsage) -> crate::domain::llm::TokenUsage {
        crate::domain::llm::TokenUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
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
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
            tool_call_id: None,
        }];
        match self.generate_chat(&messages, &[], options).await? {
            ChatResponse::FinalText(r) => Ok(r),
            ChatResponse::ToolCalls(_) => Err(LLMError::Provider(
                "Unexpected tool calls from single-turn generate()".into(),
            )),
        }
    }

    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        // Map domain ChatMessage → OpenAI message shape
        let oai_messages: Vec<OpenAIMessage> = messages
            .iter()
            .map(|m| OpenAIMessage {
                role: m.role.clone(),
                content: Some(m.content.clone()),
                tool_calls: None,
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect();

        // Map ToolSchema → OpenAI function-calling schema
        let oai_tools: Option<Vec<serde_json::Value>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect(),
            )
        };

        let request = OpenAIRequest {
            model: self.model.clone(),
            messages: oai_messages,
            tools: oai_tools,
            max_tokens: options.max_tokens,
            temperature: options.temperature,
            stop: options.stop_sequences.clone(),
        };

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

        let oai_response: OpenAIResponse = response
            .json()
            .await
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {}", e)))?;

        let choice = oai_response
            .choices
            .first()
            .ok_or_else(|| LLMError::Provider("No response choices from model".into()))?;

        // If the model requested tool calls, return them
        if let Some(tool_calls) = &choice.message.tool_calls {
            if !tool_calls.is_empty() {
                let calls: Vec<ChatToolCall> = tool_calls
                    .iter()
                    .map(|tc| ChatToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Object(Default::default())),
                    })
                    .collect();
                return Ok(ChatResponse::ToolCalls(calls));
            }
        }

        // Otherwise it's a final text response
        let text = choice.message.content.clone().unwrap_or_default();

        Ok(ChatResponse::FinalText(GenerationResponse {
            text,
            usage: Self::build_token_usage(&oai_response.usage),
            provider: "openai".to_string(),
            model: self.model.clone(),
            finish_reason: Self::map_finish_reason(&choice.finish_reason),
        }))
    }

    async fn health_check(&self) -> Result<(), LLMError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::llm::GenerationOptions;

    #[test]
    fn test_openai_adapter_creation() {
        let adapter = OpenAIAdapter::new(
            "https://api.openai.com/v1".to_string(),
            "test-key".to_string(),
            "gpt-4o".to_string(),
        );
        assert_eq!(adapter.endpoint, "https://api.openai.com/v1");
        assert_eq!(adapter.model, "gpt-4o");
    }

    #[test]
    fn test_openai_request_serialization() {
        let request = OpenAIRequest {
            model: "gpt-4o".to_string(),
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: Some("Hello".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            max_tokens: Some(100),
            temperature: Some(0.7),
            stop: Some(vec!["STOP".to_string()]),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "gpt-4o");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "Hello");
        assert_eq!(json["max_tokens"], 100);
        let temp = json["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_tool_schema_mapping() {
        let tools = vec![ToolSchema {
            name: "fs.read".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let oai: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        assert_eq!(oai[0]["type"], "function");
        assert_eq!(oai[0]["function"]["name"], "fs.read");
    }

    #[test]
    fn test_finish_reason_mapping() {
        assert_eq!(OpenAIAdapter::map_finish_reason("stop"), FinishReason::Stop);
        assert_eq!(
            OpenAIAdapter::map_finish_reason("length"),
            FinishReason::Length
        );
        assert_eq!(
            OpenAIAdapter::map_finish_reason("content_filter"),
            FinishReason::ContentFilter
        );
        assert_eq!(
            OpenAIAdapter::map_finish_reason("tool_calls"),
            FinishReason::Stop
        );
    }

    #[test]
    fn test_generation_options() {
        let options = GenerationOptions {
            max_tokens: Some(500),
            temperature: Some(0.8),
            stop_sequences: Some(vec!["END".to_string()]),
        };
        assert_eq!(options.max_tokens, Some(500));
        assert_eq!(options.temperature, Some(0.8));
    }
}
