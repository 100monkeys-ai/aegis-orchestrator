// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Anthropic Adapter
//!
//! Implements the `LLMProvider` domain trait for Anthropic `claude-*` models.
//! Acts as an Anti-Corruption Layer (ACL): translates AEGIS domain types into
//! Anthropic Messages API payloads and back, including native `tool_use`
//! content blocks for function-calling.

use crate::domain::llm::{
    ChatMessage, ChatResponse, ChatToolCall, FinishReason, GenerationOptions, GenerationResponse,
    LLMError, LLMProvider, ToolSchema,
};
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
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    /// Plain string for user/assistant or content-block array for tool results.
    content: serde_json::Value,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    usage: AnthropicUsage,
    stop_reason: Option<String>,
}

/// Response content block — either `"text"` or `"tool_use"`.
#[derive(Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    input: serde_json::Value,
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

    fn map_stop_reason(r: Option<&str>) -> FinishReason {
        match r {
            Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
            Some("max_tokens") => FinishReason::Length,
            _ => FinishReason::Stop,
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
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }];
        match self.generate_chat(&messages, &[], options).await? {
            ChatResponse::FinalText(r) => Ok(r),
            ChatResponse::ToolCalls(_) => Err(LLMError::Provider(
                "Unexpected tool_use blocks from single-turn generate()".into(),
            )),
        }
    }

    async fn generate_chat(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSchema],
        options: &GenerationOptions,
    ) -> Result<ChatResponse, LLMError> {
        // Map domain ChatMessage → Anthropic message.
        // role="system" is extracted to the top-level `system` parameter; Anthropic's
        // Messages API rejects system role entries inside the messages array.
        // role="tool" becomes a "tool_result" content-block array (Anthropic pattern).
        let system_prompt: Option<String> = {
            let parts: Vec<&str> = messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        };

        let anthropic_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| {
                if m.role == "tool" {
                    let content = serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id,
                        "content": m.content,
                    }]);
                    AnthropicMessage {
                        role: "user".to_string(), // Anthropic requires user role for tool results
                        content,
                    }
                } else if m.role == "assistant" && m.tool_calls.is_some() {
                    let mut content_blocks = Vec::new();
                    if !m.content.is_empty() {
                        content_blocks.push(serde_json::json!({
                            "type": "text",
                            "text": m.content,
                        }));
                    }
                    for tc in m.tool_calls.as_ref().unwrap() {
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name.replace('.', "_"),
                            "input": tc.arguments,
                        }));
                    }
                    AnthropicMessage {
                        role: "assistant".to_string(),
                        content: serde_json::Value::Array(content_blocks),
                    }
                } else {
                    AnthropicMessage {
                        role: m.role.clone(),
                        content: serde_json::Value::String(m.content.clone()),
                    }
                }
            })
            .collect();

        // Anthropic strictly forbids `.` in tool names (`^[a-zA-Z0-9_-]{1,128}$`).
        // We map `.` to `_` outbound, and reverse (`_` → `.`) when receiving tool_use blocks.
        let anthropic_tools: Option<Vec<serde_json::Value>> = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name.replace('.', "_"),
                            "description": t.description,
                            "input_schema": t.parameters,
                        })
                    })
                    .collect(),
            )
        };

        let request = AnthropicRequest {
            model: self.model.clone(),
            messages: anthropic_messages,
            max_tokens: options.max_tokens.unwrap_or(4096),
            system: system_prompt,
            temperature: options.temperature,
            tools: anthropic_tools,
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

        let ar: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {}", e)))?;

        let tool_calls: Vec<ChatToolCall> = ar
            .content
            .iter()
            .filter(|b| b.block_type == "tool_use")
            .map(|b| ChatToolCall {
                id: b.id.clone(),
                name: b.name.replace('_', "."), // Reverse outbound sanitization
                arguments: b.input.clone(),
            })
            .collect();

        if !tool_calls.is_empty() {
            return Ok(ChatResponse::ToolCalls(tool_calls));
        }

        let text = ar
            .content
            .iter()
            .filter(|b| b.block_type == "text")
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("");

        Ok(ChatResponse::FinalText(GenerationResponse {
            text,
            usage: crate::domain::llm::TokenUsage {
                prompt_tokens: ar.usage.input_tokens,
                completion_tokens: ar.usage.output_tokens,
                total_tokens: ar.usage.input_tokens + ar.usage.output_tokens,
            },
            provider: "anthropic".to_string(),
            model: self.model.clone(),
            finish_reason: Self::map_stop_reason(ar.stop_reason.as_deref()),
        }))
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        let response = self
            .client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        if response.status() == 401 || response.status() == 403 {
            Err(LLMError::Authentication("Invalid Anthropic API key".into()))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::llm::{FinishReason, GenerationOptions};

    #[test]
    fn test_anthropic_adapter_creation() {
        let adapter =
            AnthropicAdapter::new("k".to_string(), "claude-3-5-sonnet-20241022".to_string());
        assert_eq!(adapter.api_key, "k");
        assert_eq!(adapter.model, "claude-3-5-sonnet-20241022");
    }

    #[test]
    fn test_stop_reason_mapping() {
        assert_eq!(
            AnthropicAdapter::map_stop_reason(Some("end_turn")),
            FinishReason::Stop
        );
        assert_eq!(
            AnthropicAdapter::map_stop_reason(Some("max_tokens")),
            FinishReason::Length
        );
        assert_eq!(
            AnthropicAdapter::map_stop_reason(Some("stop_sequence")),
            FinishReason::Stop
        );
        assert_eq!(AnthropicAdapter::map_stop_reason(None), FinishReason::Stop);
    }

    #[test]
    fn test_request_serialization() {
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("Hello".to_string()),
            }],
            max_tokens: 1024,
            system: None,
            temperature: Some(0.7),
            tools: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(json["messages"][0]["content"], "Hello");
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn test_content_block_deserialization() {
        let json = serde_json::json!({
            "content": [
                {"type": "text", "text": "Sure, let me search."},
                {"type": "tool_use", "id": "call_1", "name": "web_search", "input": {"query": "rust async"}}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 20},
            "stop_reason": "tool_use"
        });
        let ar: AnthropicResponse = serde_json::from_value(json).unwrap();
        assert_eq!(ar.content.len(), 2);
        assert_eq!(ar.content[0].block_type, "text");
        assert_eq!(ar.content[1].block_type, "tool_use");
        assert_eq!(ar.content[1].name, "web_search");
    }

    #[test]
    fn test_tool_result_message_format() {
        let content = serde_json::json!([{
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "Search results here",
        }]);
        let msg = AnthropicMessage {
            role: "user".to_string(),
            content,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "tool_result");
    }

    #[test]
    fn test_tool_schema_to_anthropic_format() {
        let tool = ToolSchema {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        };
        let anthropic = serde_json::json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": tool.parameters,
        });
        assert_eq!(anthropic["name"], "read_file");
        assert!(anthropic.get("input_schema").is_some());
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
    fn test_usage_calculation() {
        let usage = AnthropicUsage {
            input_tokens: 100,
            output_tokens: 200,
        };
        assert_eq!(usage.input_tokens + usage.output_tokens, 300);
    }
}
