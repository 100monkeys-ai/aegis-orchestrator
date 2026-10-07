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
//!
//! A provider's configured `headers` (AEGIS ADR-124 D3) are sent on every
//! request beside `Authorization`: Workers AI is reached through a Cloudflare
//! AI Gateway named by `cf-aig-gateway-id`.

use crate::domain::llm::{
    ChatMessage, ChatResponse, ChatToolCall, FinishReason, GenerationOptions, GenerationResponse,
    LLMError, LLMProvider, ToolSchema,
};
use crate::domain::secrets::SensitiveUrl;
use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The tool names one request sends, mapped back to each tool's own name.
///
/// OpenAI, Anthropic and Gemini forbid `.` in a tool name, so every adapter
/// sends `fs.read` as `fs_read`. Replacing each `_` with `.` on the way back
/// is not the inverse: `fs.create_dir` is sent as `fs_create_dir` and would
/// come back as `fs.create.dir`, a tool that does not exist. The way back is
/// a lookup among the names the request sent. A returned name that matches
/// none of them is passed through unchanged, with a warning, so the loop's
/// own unknown-tool handling answers it.
pub(super) struct SentToolNames {
    original_by_sent: HashMap<String, String>,
}

impl SentToolNames {
    /// The form in which a tool's name is sent to a provider.
    pub(super) fn sent_form(name: &str) -> String {
        name.replace('.', "_")
    }

    /// The map for a request that sends `tools`.
    pub(super) fn new(tools: &[ToolSchema]) -> Self {
        Self {
            original_by_sent: tools
                .iter()
                .map(|t| (Self::sent_form(&t.name), t.name.clone()))
                .collect(),
        }
    }

    /// The tool's own name for a name the provider returned.
    pub(super) fn original(&self, provider: &str, returned: &str) -> String {
        match self.original_by_sent.get(returned) {
            Some(original) => original.clone(),
            None => {
                tracing::warn!(
                    provider,
                    returned_tool_name = returned,
                    "the model returned a tool name this request did not send; passed through unchanged"
                );
                returned.to_string()
            }
        }
    }
}

/// Workers AI's error code for a generation it ended at its time limit
/// (`"AiError: AiError: Request timeout"`, HTTP 408).
const WORKERS_AI_TIME_LIMIT_CODE: i64 = 3046;

/// The start of the message of the error a provider's time limit becomes.
/// The registry recognises the error by it ([`is_provider_timeout`]) and adds
/// the alias, which only it knows.
pub(super) const PROVIDER_TIMEOUT_PREFIX: &str = "provider timeout: ";

/// True for Workers AI's answer that it ended the generation at its time
/// limit: HTTP 408 with an `errors` entry of code 3046.
///
/// The limit is the provider's, about 120 s on `@cf/nvidia/nemotron-3-120b-a12b`
/// whether the request is streamed or not (AEGIS ADR-124, the measurements of
/// 2026-10-03), so the identical request meets it again: in execution 18d8e0ca
/// each of three iterations sent it twice more and ended at the 300 s budget.
fn is_workers_ai_time_limit(status: u16, body: &str) -> bool {
    status == 408
        && serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("errors")?.as_array().map(|errors| {
                    errors.iter().any(|e| {
                        e.get("code").and_then(serde_json::Value::as_i64)
                            == Some(WORKERS_AI_TIME_LIMIT_CODE)
                    })
                })
            })
            .unwrap_or(false)
}

/// True for the error a provider's time limit becomes. The registry does not
/// send the identical request again for it.
pub(super) fn is_provider_timeout(e: &LLMError) -> bool {
    matches!(e, LLMError::Provider(msg) if msg.starts_with(PROVIDER_TIMEOUT_PREFIX))
}

/// The words before the seconds in a time-limit error's message.
const TIME_LIMIT_SECONDS_BEFORE: &str = "at its time limit after ";
/// The words after the seconds in a time-limit error's message.
const TIME_LIMIT_SECONDS_AFTER: &str = " s (HTTP 408";

/// The error a provider's time limit becomes: the model, the seconds the
/// provider took before it ended the generation, and the provider's answer.
/// The one place it is built; [`provider_time_limit_seconds`] reads it back.
pub(crate) fn provider_time_limit_error(model: &str, elapsed_ms: u64, excerpt: &str) -> LLMError {
    LLMError::Provider(format!(
        "{PROVIDER_TIMEOUT_PREFIX}model '{model}' was ended by the provider {TIME_LIMIT_SECONDS_BEFORE}{:.1}{TIME_LIMIT_SECONDS_AFTER}, code {WORKERS_AI_TIME_LIMIT_CODE}); the identical request is not sent again: {excerpt}",
        elapsed_ms as f64 / 1000.0,
    ))
}

/// `Some` when `e` is the error a provider's time limit becomes (Workers
/// AI's 408 with code 3046, [`provider_time_limit_error`]), with the alias or
/// a fallback's failure added to it or not; inside, the seconds the provider
/// took where the message states them. Any other error, an agent's own
/// `llm_timeout_seconds` among them, is `None`.
pub(crate) fn provider_time_limit_seconds(e: &LLMError) -> Option<Option<f64>> {
    let LLMError::Provider(msg) = e else {
        return None;
    };
    if !is_provider_timeout(e) {
        return None;
    }
    Some(
        msg.split_once(TIME_LIMIT_SECONDS_BEFORE)
            .and_then(|(_, rest)| rest.split_once(TIME_LIMIT_SECONDS_AFTER))
            .and_then(|(seconds, _)| seconds.parse::<f64>().ok()),
    )
}

pub struct OpenAIAdapter {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    /// The provider's configured request headers, sent on every request
    /// beside `Authorization`. Empty when the configuration has none.
    headers: HeaderMap,
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
    pub fn new(endpoint: String, api_key: String, model: String, headers: HeaderMap) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            api_key,
            model,
            headers,
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
        // Map domain ChatMessage → OpenAI message shape.
        //
        // An assistant message that carries tool calls and no text is sent
        // with `content: ""`, never without the key: Workers AI's
        // `gpt-oss-120b`, `gpt-oss-20b`, `llama-3.3-70b-instruct-fp8-fast`,
        // `qwen3-30b-a3b-fp8` and `granite-4.0-h-micro` refuse a message with
        // no `content` (HTTP 400, "required properties at '/messages/2' are
        // 'role,content'"), which failed every tool cycle's second request.
        let oai_messages: Vec<OpenAIMessage> = messages
            .iter()
            .map(|m| OpenAIMessage {
                role: m.role.clone(),
                content: if m.content.is_empty()
                    && m.role == "assistant"
                    && !m.tool_calls.as_ref().is_some_and(|tcs| !tcs.is_empty())
                {
                    None
                } else {
                    Some(m.content.clone())
                },
                tool_calls: m.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .map(|tc| OpenAIToolCall {
                            id: tc.id.clone(),
                            call_type: "function".to_string(),
                            function: OpenAIToolFunction {
                                name: SentToolNames::sent_form(&tc.name),
                                arguments: tc.arguments.to_string(),
                            },
                        })
                        .collect()
                }),
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect();

        // OpenAI strictly forbids `.` in tool names (`^[a-zA-Z0-9_-]{1,64}$`).
        // We map `.` to `_` outbound, and a returned name back to the tool's
        // own name by looking it up among the names sent (`SentToolNames`).
        let sent_names = SentToolNames::new(tools);
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
                                "name": SentToolNames::sent_form(&t.name),
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

        tracing::debug!(
            provider = "openai",
            model = %self.model,
            endpoint_url = %SensitiveUrl::new(url.as_str()),
            headers = ?self.headers.keys().map(|k| k.as_str()).collect::<Vec<_>>(),
            "LLM HTTP request"
        );
        let http_started_at = std::time::Instant::now();

        let response = self
            .client
            .post(&url)
            .headers(self.headers.clone())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|e| LLMError::Network(e.to_string()))?;

        let http_elapsed_ms = http_started_at.elapsed().as_millis() as u64;
        let status_code = response.status().as_u16();
        tracing::info!(
            provider = "openai",
            model = %self.model,
            status = status_code,
            elapsed_ms = http_elapsed_ms,
            "LLM HTTP response"
        );

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            let excerpt: String = error_text.chars().take(512).collect();
            tracing::warn!(
                provider = "openai",
                model = %self.model,
                status = status.as_u16(),
                body_excerpt = %excerpt,
                "LLM upstream non-2xx"
            );
            return Err(if is_workers_ai_time_limit(status.as_u16(), &error_text) {
                provider_time_limit_error(&self.model, http_elapsed_ms, &excerpt)
            } else if status == 401 || status == 403 {
                LLMError::Authentication(error_text)
            } else if status == 429 {
                LLMError::RateLimit
            } else if status == 404 {
                LLMError::ModelNotFound(self.model.clone())
            } else if status == 503 {
                LLMError::ServiceUnavailable(error_text)
            } else if status == 400 {
                // A 400 is the provider refusing this request as sent (a prompt
                // over the model's context window, among others): no retry can
                // change the answer, so it is InvalidInput, which the registry
                // does not retry, carrying the provider's message.
                LLMError::InvalidInput(format!("HTTP {status}: {error_text}"))
            } else {
                LLMError::Provider(format!("HTTP {status}: {error_text}"))
            });
        }

        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| LLMError::Network(format!("Failed to read response body: {e}")))?;

        let oai_response: OpenAIResponse = serde_json::from_slice(&body_bytes).map_err(|e| {
            let excerpt: String = String::from_utf8_lossy(&body_bytes)
                .chars()
                .take(512)
                .collect();
            tracing::error!(
                provider = "openai",
                model = %self.model,
                status = status_code,
                body_excerpt = %excerpt,
                parse_error = %e,
                "LLM response parse failure"
            );
            LLMError::Provider(format!("Failed to parse response: {e}"))
        })?;

        let choice = oai_response
            .choices
            .first()
            .ok_or_else(|| LLMError::Provider("No response choices from model".into()))?;

        // If the model requested tool calls natively, return them
        if let Some(tool_calls) = &choice.message.tool_calls {
            if !tool_calls.is_empty() {
                let calls: Vec<ChatToolCall> = tool_calls
                    .iter()
                    .map(|tc| ChatToolCall {
                        id: tc.id.clone(),
                        name: sent_names.original("openai", &tc.function.name),
                        arguments: serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Object(Default::default())),
                    })
                    .collect();
                return Ok(ChatResponse::ToolCalls(calls));
            }
        }

        let text = choice.message.content.clone().unwrap_or_default();

        // Fallback: If smaller models hallucinated the OpenAI JSON array inside raw text
        if let Some(start_idx) = text.find("[{\"function\":") {
            if let Some(end_offset) = text[start_idx..].find("}]") {
                let json_slice = &text[start_idx..start_idx + end_offset + 2];
                if let Ok(parsed_calls) = serde_json::from_str::<Vec<serde_json::Value>>(json_slice)
                {
                    let mut calls = Vec::new();
                    for c in parsed_calls {
                        if let (Some(func), Some(id)) =
                            (c.get("function"), c.get("id").and_then(|v| v.as_str()))
                        {
                            if let (Some(name), Some(args_str)) = (
                                func.get("name").and_then(|v| v.as_str()),
                                func.get("arguments").and_then(|v| v.as_str()),
                            ) {
                                calls.push(ChatToolCall {
                                    id: id.to_string(),
                                    name: sent_names.original("openai", name),
                                    arguments: serde_json::from_str(args_str)
                                        .unwrap_or(serde_json::Value::Object(Default::default())),
                                });
                            }
                        }
                    }
                    if !calls.is_empty() {
                        return Ok(ChatResponse::ToolCalls(calls));
                    }
                }
            }
        }

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
            .headers(self.headers.clone())
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
            HeaderMap::new(),
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
        let tools = [ToolSchema {
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
    fn workers_ai_time_limit_is_a_408_with_code_3046_only() {
        let body = r#"{"errors":[{"message":"AiError: AiError: Request timeout (df1159f0)","code":3046}],"success":false,"result":{},"messages":[]}"#;
        assert!(is_workers_ai_time_limit(408, body));
        assert!(!is_workers_ai_time_limit(504, body), "another status");
        assert!(
            !is_workers_ai_time_limit(408, r#"{"errors":[{"code":3040}]}"#),
            "another code"
        );
        assert!(!is_workers_ai_time_limit(408, "request timeout"), "no JSON");
    }

    /// The seconds a provider's time limit took go into its error and come
    /// back out of it, with the alias and a fallback's failure added after
    /// them as the registry adds them; any other error has no time limit.
    #[test]
    fn provider_time_limit_error_round_trips_its_seconds() {
        let e = provider_time_limit_error("@cf/nvidia/nemotron-3-120b-a12b", 120_500, "{}");
        assert_eq!(
            provider_time_limit_seconds(&e),
            Some(Some(120.5)),
            "the time-limit error must give back its 120.5 s: {e}"
        );
        let LLMError::Provider(msg) = &e else {
            panic!("a time limit is a provider error, got {e:?}");
        };
        assert_eq!(
            msg,
            "provider timeout: model '@cf/nvidia/nemotron-3-120b-a12b' was ended by the provider at its time limit after 120.5 s (HTTP 408, code 3046); the identical request is not sent again: {}",
            "the error's text is as it was"
        );
        let with_alias_and_fallback = LLMError::Provider(format!(
            "{PROVIDER_TIMEOUT_PREFIX}alias 'smart': {}; the fallback model 'm' was tried once and failed: x after 9.0 s (HTTP 408",
            &msg[PROVIDER_TIMEOUT_PREFIX.len()..]
        ));
        assert_eq!(
            provider_time_limit_seconds(&with_alias_and_fallback),
            Some(Some(120.5)),
            "the alias and a fallback's failure must not move the seconds"
        );
        assert_eq!(
            provider_time_limit_seconds(&LLMError::Provider(format!(
                "{PROVIDER_TIMEOUT_PREFIX}model 'm' gave no seconds"
            ))),
            Some(None),
            "a time limit whose message states no seconds is still a time limit"
        );
        assert_eq!(
            provider_time_limit_seconds(&LLMError::Provider("HTTP 500: boom".to_string())),
            None,
            "another provider error is no time limit"
        );
        assert_eq!(
            provider_time_limit_seconds(&LLMError::Network(
                "model call on alias 'smart' gave no answer within the agent's llm_timeout_seconds (120 s)".to_string()
            )),
            None,
            "the agent's own timeout is not the provider's limit"
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
