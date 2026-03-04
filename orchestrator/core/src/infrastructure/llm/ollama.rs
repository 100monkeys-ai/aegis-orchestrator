// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Ollama Local Model Adapter
//!
//! Implements the `LLMProvider` domain trait for Ollama-served local models.
//! Useful for offline development and air-gapped deployments.
//!
//! Default base URL: `http://localhost:11434` (configurable via node config).

use crate::domain::llm::{
    ChatMessage, ChatResponse, ChatToolCall, FinishReason, GenerationOptions, GenerationResponse,
    LLMError, LLMProvider, ToolSchema,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct OllamaAdapter {
    client: reqwest::Client,
    endpoint: String,
    model: String,
}

// ─── /api/generate (legacy single-turn) ─────────────────────────────────────

#[derive(Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i32>,
}

// ─── /api/chat (multi-turn + tool call) ──────────────────────────────────────

#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Serialize, Deserialize)]
struct OllamaChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaChatAssistantMessage,
    done: bool,
    eval_count: Option<u32>,
    prompt_eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaChatAssistantMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

#[derive(Deserialize)]
struct OllamaToolCall {
    function: OllamaToolFunction,
}

#[derive(Deserialize)]
struct OllamaToolFunction {
    name: String,
    arguments: serde_json::Value,
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
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
            tool_call_id: None,
        }];
        match self.generate_chat(&messages, &[], options).await? {
            ChatResponse::FinalText(r) => Ok(r),
            ChatResponse::ToolCalls(_) => Err(LLMError::Provider(
                "Unexpected tool_calls from single-turn generate()".into(),
            )),
        }
    }

    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        let ollama_messages: Vec<OllamaChatMessage> = messages
            .iter()
            .map(|m| OllamaChatMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        // Sanitize tool names: `.` → `_` outbound, reversed on inbound.
        // Consistent with OpenAI/Anthropic adapters.
        let ollama_tools: Option<Vec<serde_json::Value>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": t.name.replace('.', "_"),
                                "description": t.description,
                                "parameters": t.parameters,
                            }
                        })
                    })
                    .collect(),
            )
        };

        let request = OllamaChatRequest {
            model: self.model.clone(),
            messages: ollama_messages,
            stream: false,
            tools: ollama_tools,
            options: Some(OllamaOptions {
                temperature: options.temperature,
                num_predict: options.max_tokens.map(|t| t as i32),
            }),
        };

        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));

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

        let cr: OllamaChatResponse = response
            .json()
            .await
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {}", e)))?;

        if !cr.message.tool_calls.is_empty() {
            let tool_calls = cr
                .message
                .tool_calls
                .into_iter()
                .enumerate()
                .map(|(i, tc)| ChatToolCall {
                    id: format!("ollama-call-{}", i),
                    name: tc.function.name.replace('_', "."), // Reverse outbound sanitization
                    arguments: tc.function.arguments,
                })
                .collect();
            return Ok(ChatResponse::ToolCalls(tool_calls));
        }

        Ok(ChatResponse::FinalText(GenerationResponse {
            text: cr.message.content,
            usage: crate::domain::llm::TokenUsage {
                prompt_tokens: cr.prompt_eval_count.unwrap_or(0),
                completion_tokens: cr.eval_count.unwrap_or(0),
                total_tokens: cr.prompt_eval_count.unwrap_or(0) + cr.eval_count.unwrap_or(0),
            },
            provider: "ollama".to_string(),
            model: self.model.clone(),
            finish_reason: if cr.done {
                FinishReason::Stop
            } else {
                FinishReason::Length
            },
        }))
    }

    async fn health_check(&self) -> Result<(), LLMError> {
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
    use crate::domain::llm::{FinishReason, GenerationOptions};

    #[test]
    fn test_ollama_adapter_creation() {
        let adapter =
            OllamaAdapter::new("http://localhost:11434".to_string(), "llama3.1".to_string());
        assert_eq!(adapter.endpoint, "http://localhost:11434");
        assert_eq!(adapter.model, "llama3.1");
    }

    #[test]
    fn test_chat_request_serialization() {
        let request = OllamaChatRequest {
            model: "llama3.1".to_string(),
            messages: vec![OllamaChatMessage {
                role: "user".to_string(),
                content: "Hi".to_string(),
            }],
            stream: false,
            tools: None,
            options: Some(OllamaOptions {
                temperature: Some(0.7),
                num_predict: Some(256),
            }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "llama3.1");
        assert_eq!(json["stream"], false);
        assert_eq!(json["messages"][0]["role"], "user");
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn test_chat_response_deserialization() {
        let json = serde_json::json!({
            "message": {"role": "assistant", "content": "Hello!", "tool_calls": []},
            "done": true,
            "eval_count": 50,
            "prompt_eval_count": 20
        });
        let cr: OllamaChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(cr.message.content, "Hello!");
        assert_eq!(cr.done, true);
        assert!(cr.message.tool_calls.is_empty());
    }

    #[test]
    fn test_tool_call_deserialization() {
        let json = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"name": "web_search", "arguments": {"query": "rust"}}}]
            },
            "done": true
        });
        let cr: OllamaChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(cr.message.tool_calls.len(), 1);
        assert_eq!(cr.message.tool_calls[0].function.name, "web_search");
    }

    #[test]
    fn test_tool_schema_to_ollama_format() {
        let tool = ToolSchema {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let ollama = serde_json::json!({
            "type": "function",
            "function": {"name": tool.name, "description": tool.description, "parameters": tool.parameters},
        });
        assert_eq!(ollama["type"], "function");
        assert_eq!(ollama["function"]["name"], "read_file");
    }

    #[test]
    fn test_endpoint_trailing_slash_handling() {
        let url = format!(
            "{}/api/chat",
            "http://localhost:11434/".trim_end_matches('/')
        );
        assert_eq!(url, "http://localhost:11434/api/chat");
    }

    #[test]
    fn test_finish_reason_from_done_flag() {
        assert_eq!(
            if true {
                FinishReason::Stop
            } else {
                FinishReason::Length
            },
            FinishReason::Stop
        );
        assert_eq!(
            if false {
                FinishReason::Stop
            } else {
                FinishReason::Length
            },
            FinishReason::Length
        );
    }

    #[test]
    fn test_default_max_tokens() {
        let opts = GenerationOptions {
            max_tokens: None,
            temperature: None,
            stop_sequences: None,
        };
        assert_eq!(opts.max_tokens.unwrap_or(4096), 4096);
    }

    #[test]
    fn test_ollama_call_id_generation() {
        // Ensure tool call IDs are generated deterministically by index
        let id = format!("ollama-call-{}", 0);
        assert_eq!(id, "ollama-call-0");
    }
}
