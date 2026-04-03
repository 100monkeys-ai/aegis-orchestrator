// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Gemini Adapter
//!
//! Native Google Gemini adapter using the Generative Language API.

use crate::domain::llm::{
    ChatMessage, ChatResponse, ChatToolCall, FinishReason, GenerationOptions, GenerationResponse,
    LLMError, LLMProvider, ToolSchema,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct GeminiAdapter {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

#[derive(Serialize)]
struct GeminiGenerateContentRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "toolConfig")]
    tool_config: Option<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_calling_config: GeminiFunctionCallingConfig,
}

#[derive(Serialize)]
struct GeminiFunctionCallingConfig {
    mode: String,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none", rename = "maxOutputTokens")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stopSequences")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thought: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerateContentResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
    #[serde(default)]
    total_token_count: u32,
}

impl GeminiAdapter {
    pub fn new(endpoint: String, api_key: String, model: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            api_key,
            model,
        }
    }

    fn map_finish_reason(reason: Option<&str>) -> FinishReason {
        match reason {
            Some("MAX_TOKENS") => FinishReason::Length,
            Some("SAFETY") => FinishReason::ContentFilter,
            _ => FinishReason::Stop,
        }
    }

    fn sanitize_tool_name(name: &str) -> String {
        name.replace('.', "_")
    }

    fn desanitize_tool_name(name: &str) -> String {
        name.replace('_', ".")
    }

    /// Recursively strip JSON Schema fields that Gemini's function calling API does not support.
    fn strip_unsupported_schema_fields(schema: &serde_json::Value) -> serde_json::Value {
        match schema {
            serde_json::Value::Object(map) => {
                let mut cleaned = serde_json::Map::new();
                for (key, value) in map {
                    if key == "additionalProperties" {
                        continue;
                    }
                    cleaned.insert(key.clone(), Self::strip_unsupported_schema_fields(value));
                }
                serde_json::Value::Object(cleaned)
            }
            serde_json::Value::Array(arr) => serde_json::Value::Array(
                arr.iter()
                    .map(Self::strip_unsupported_schema_fields)
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

#[async_trait]
impl LLMProvider for GeminiAdapter {
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
        let system_text = messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        let contents: Vec<GeminiContent> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| {
                let role = if m.role == "assistant" {
                    "model".to_string()
                } else {
                    "user".to_string()
                };

                if m.role == "assistant" && m.tool_calls.is_some() {
                    let mut parts = Vec::new();
                    if !m.content.is_empty() {
                        parts.push(GeminiPart {
                            text: Some(m.content.clone()),
                            function_call: None,
                            function_response: None,
                            thought: None,
                        });
                    }
                    for tc in m.tool_calls.as_ref().unwrap_or(&Vec::new()) {
                        parts.push(GeminiPart {
                            text: None,
                            function_call: Some(GeminiFunctionCall {
                                name: Self::sanitize_tool_name(&tc.name),
                                args: tc.arguments.clone(),
                            }),
                            function_response: None,
                            thought: None,
                        });
                    }
                    GeminiContent { role, parts }
                } else if m.role == "tool" {
                    GeminiContent {
                        role,
                        parts: vec![GeminiPart {
                            text: None,
                            function_call: None,
                            function_response: Some(GeminiFunctionResponse {
                                // We only have tool_call_id in domain message; keep this
                                // stable so Gemini can correlate conversationally.
                                name: m
                                    .tool_call_id
                                    .clone()
                                    .unwrap_or_else(|| "tool_result".to_string()),
                                response: serde_json::json!({
                                    "content": m.content,
                                }),
                            }),
                            thought: None,
                        }],
                    }
                } else {
                    GeminiContent {
                        role,
                        parts: vec![GeminiPart {
                            text: Some(m.content.clone()),
                            function_call: None,
                            function_response: None,
                            thought: None,
                        }],
                    }
                }
            })
            .collect();

        let request = GeminiGenerateContentRequest {
            contents,
            system_instruction: if system_text.is_empty() {
                None
            } else {
                Some(GeminiSystemInstruction {
                    parts: vec![GeminiPart {
                        text: Some(system_text),
                        function_call: None,
                        function_response: None,
                        thought: None,
                    }],
                })
            },
            tools: if tools.is_empty() {
                None
            } else {
                Some(vec![GeminiTool {
                    function_declarations: tools
                        .iter()
                        .map(|t| GeminiFunctionDeclaration {
                            name: Self::sanitize_tool_name(&t.name),
                            description: t.description.clone(),
                            parameters: Self::strip_unsupported_schema_fields(&t.parameters),
                        })
                        .collect(),
                }])
            },
            tool_config: if tools.is_empty() {
                None
            } else {
                Some(GeminiToolConfig {
                    function_calling_config: GeminiFunctionCallingConfig {
                        mode: "AUTO".to_string(),
                    },
                })
            },
            generation_config: Some(GeminiGenerationConfig {
                max_output_tokens: options.max_tokens,
                temperature: options.temperature,
                stop_sequences: options.stop_sequences.clone(),
            }),
        };

        let url = format!(
            "{}/models/{}:generateContent",
            self.endpoint.trim_end_matches('/'),
            self.model
        );

        let response = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
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
            } else if status == 503 {
                LLMError::ServiceUnavailable(error_text)
            } else {
                LLMError::Provider(format!("HTTP {status}: {error_text}"))
            });
        }

        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| LLMError::Network(format!("Failed to read response body: {e}")))?;

        tracing::debug!(
            body = %String::from_utf8_lossy(&body_bytes),
            "Gemini raw response body"
        );

        let gr: GeminiGenerateContentResponse = serde_json::from_slice(&body_bytes)
            .map_err(|e| LLMError::Provider(format!("Failed to parse response: {e}")))?;

        tracing::debug!(
            candidate_count = gr.candidates.len(),
            "Gemini raw response candidates"
        );
        for (i, c) in gr.candidates.iter().enumerate() {
            if let Some(ref content) = c.content {
                let thought_parts = content
                    .parts
                    .iter()
                    .filter(|p| p.thought.unwrap_or(false))
                    .count();
                let text_parts = content
                    .parts
                    .iter()
                    .filter(|p| p.text.is_some() && !p.thought.unwrap_or(false))
                    .count();
                let all_text: String = content
                    .parts
                    .iter()
                    .filter_map(|p| p.text.as_deref())
                    .collect::<Vec<_>>()
                    .join("|");
                tracing::debug!(
                    candidate_idx = i,
                    thought_parts,
                    text_parts,
                    part_count = content.parts.len(),
                    all_text_preview = %&all_text[..all_text.len().min(200)],
                    "Gemini candidate parts"
                );
            } else {
                tracing::debug!(candidate_idx = i, "Gemini candidate has no content");
            }
        }

        if gr.candidates.is_empty() {
            return Err(LLMError::Provider(
                "No response candidates from model".into(),
            ));
        }

        // Collect tool calls from all candidates, excluding thought parts
        let tool_calls: Vec<ChatToolCall> = gr
            .candidates
            .iter()
            .filter_map(|c| c.content.as_ref())
            .flat_map(|c| c.parts.iter())
            .filter(|p| !p.thought.unwrap_or(false))
            .filter_map(|p| {
                p.function_call.as_ref().map(|fc| ChatToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: Self::desanitize_tool_name(&fc.name),
                    arguments: fc.args.clone(),
                })
            })
            .collect();

        if !tool_calls.is_empty() {
            return Ok(ChatResponse::ToolCalls(tool_calls));
        }

        // Find the first candidate that yields non-empty text after filtering thought parts.
        // Gemini 2.5 Pro thinking mode places thought parts in early candidates; the actual
        // text response may appear in a later candidate.
        let text = gr
            .candidates
            .iter()
            .filter_map(|c| c.content.as_ref())
            .map(|c| {
                c.parts
                    .iter()
                    .filter(|p| !p.thought.unwrap_or(false))
                    .filter_map(|p| p.text.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .find(|t| !t.is_empty())
            .unwrap_or_default();

        // finish_reason from the first candidate (used for token counting etc.)
        let finish_reason = gr
            .candidates
            .first()
            .and_then(|c| c.finish_reason.as_deref());

        if finish_reason == Some("UNEXPECTED_TOOL_CALL") {
            return Err(LLMError::Provider(
                "Gemini rejected tool call generation (UNEXPECTED_TOOL_CALL) — check tool schemas"
                    .into(),
            ));
        }

        let usage = gr.usage_metadata.unwrap_or(GeminiUsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: 0,
            total_token_count: 0,
        });

        Ok(ChatResponse::FinalText(GenerationResponse {
            text,
            usage: crate::domain::llm::TokenUsage {
                prompt_tokens: usage.prompt_token_count,
                completion_tokens: usage.candidates_token_count,
                total_tokens: usage.total_token_count,
            },
            provider: "gemini".to_string(),
            model: self.model.clone(),
            finish_reason: Self::map_finish_reason(finish_reason),
        }))
    }

    async fn health_check(&self) -> Result<(), LLMError> {
        let url = format!("{}/models", self.endpoint.trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .header("x-goog-api-key", &self.api_key)
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        if response.status() == 401 || response.status() == 403 {
            Err(LLMError::Authentication("Invalid Gemini API key".into()))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gemini_adapter_creation() {
        let adapter = GeminiAdapter::new(
            "https://generativelanguage.googleapis.com/v1beta".to_string(),
            "k".to_string(),
            "gemini-2.5-flash".to_string(),
        );
        assert_eq!(adapter.api_key, "k");
        assert_eq!(adapter.model, "gemini-2.5-flash");
    }
}
