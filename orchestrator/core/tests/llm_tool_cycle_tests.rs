// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The tool cycle as each LLM adapter sends it and reads it back.
//!
//! Two defects, each pinned by the tests below:
//!
//! 1. The OpenAI-compatible adapter sent an assistant message that carries
//!    tool calls and no text with no `content` key at all. Five Workers AI
//!    models (`gpt-oss-120b`, `gpt-oss-20b`, `llama-3.3-70b-instruct-fp8-fast`,
//!    `qwen3-30b-a3b-fp8`, `granite-4.0-h-micro`) refuse that body with
//!    HTTP 400, "required properties at '/messages/2' are 'role,content'", so
//!    every tool cycle's second request failed on them; with `content: ""`
//!    all seventeen models measured accept it.
//! 2. Every adapter mapped a returned tool name back by replacing each `_`
//!    with `.`, so a tool whose own name holds an underscore (`fs.create_dir`,
//!    `fs.multi_edit`) came back as a name that does not exist
//!    (`fs.create.dir`). The returned name is now looked up among the names
//!    the request sent; a name that was never sent comes back unchanged.
//!
//! Each test drives a real adapter against a local stand-in for the provider
//! that records every request body and answers with a canned response. No
//! request leaves the loopback interface.

use aegis_orchestrator_core::domain::llm::{
    ChatMessage, ChatResponse, ChatToolCall, GenerationOptions, LLMProvider, ToolSchema,
};
use aegis_orchestrator_core::infrastructure::llm::anthropic::AnthropicAdapter;
use aegis_orchestrator_core::infrastructure::llm::gemini::GeminiAdapter;
use aegis_orchestrator_core::infrastructure::llm::ollama::OllamaAdapter;
use aegis_orchestrator_core::infrastructure::llm::openai::OpenAIAdapter;
use axum::{Json, Router};
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The stand-in's record of every request body it answered.
type Bodies = Arc<Mutex<Vec<Value>>>;

/// Serve any path on a loopback port, record each request's JSON body, and
/// answer every request with `answer`. Returns `http://127.0.0.1:<port>` and
/// the record.
async fn stand_in(answer: Value) -> (String, Bodies) {
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let record = bodies.clone();
    let app = Router::new().fallback(move |Json(body): Json<Value>| {
        let record = record.clone();
        let answer = answer.clone();
        async move {
            record.lock().unwrap().push(body);
            Json(answer)
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve stand-in");
    });
    (format!("http://{addr}"), bodies)
}

fn message(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: content.to_string(),
        tool_call_id: None,
        tool_calls: None,
    }
}

fn openai(endpoint: &str) -> OpenAIAdapter {
    OpenAIAdapter::new(
        format!("{endpoint}/v1"),
        "test-key".to_string(),
        "@cf/openai/gpt-oss-120b".to_string(),
        HeaderMap::new(),
    )
}

fn openai_final_text() -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": "done"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

fn only_body(bodies: &Bodies) -> Value {
    let bodies = bodies.lock().unwrap();
    assert_eq!(
        bodies.len(),
        1,
        "the stand-in must have answered one request"
    );
    bodies[0].clone()
}

// ─── Defect 1: the assistant's tool-call message carries content "" ─────────

/// The cycle's second request: the user's ask, the assistant's tool call
/// with no text, and the tool's result.
fn tool_cycle_history() -> Vec<ChatMessage> {
    vec![
        message("user", "What port does the service listen on?"),
        ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(vec![ChatToolCall {
                id: "call_1".to_string(),
                name: "fs.read".to_string(),
                arguments: json!({"path": "/workspace/config.yaml"}),
            }]),
        },
        ChatMessage {
            role: "tool".to_string(),
            content: "port: 8443".to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_calls: None,
        },
    ]
}

#[tokio::test]
async fn an_assistant_tool_call_message_without_text_is_sent_with_empty_content() {
    let (endpoint, bodies) = stand_in(openai_final_text()).await;
    let response = openai(&endpoint)
        .generate_chat(&tool_cycle_history(), &[], &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");
    assert!(matches!(response, ChatResponse::FinalText(_)));

    let body = only_body(&bodies);
    let messages = body["messages"].as_array().expect("messages array");
    let assistant = messages[1].as_object().expect("assistant message object");

    let mut failures = Vec::new();
    match assistant.get("content") {
        Some(Value::String(s)) if s.is_empty() => {}
        other => failures.push(format!(
            "the assistant's tool-call message must carry the key content with \"\", got {other:?} in {}",
            messages[1]
        )),
    }
    if assistant.get("role") != Some(&json!("assistant")) {
        failures.push(format!(
            "messages[1] must be the assistant's, got {}",
            messages[1]
        ));
    }
    let calls = assistant.get("tool_calls").and_then(Value::as_array);
    match calls {
        Some(calls) if calls.len() == 1 => {
            if calls[0]["id"] != json!("call_1") || calls[0]["function"]["name"] != json!("fs_read")
            {
                failures.push(format!(
                    "the tool call must be sent as fs_read call_1, got {}",
                    calls[0]
                ));
            }
        }
        _ => failures.push(format!(
            "the assistant's tool_calls must be sent, got {}",
            messages[1]
        )),
    }
    if messages[2]["role"] != json!("tool")
        || messages[2]["tool_call_id"] != json!("call_1")
        || messages[2]["content"] != json!("port: 8443")
    {
        failures.push(format!(
            "the tool result must follow the call, got {}",
            messages[2]
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[tokio::test]
async fn an_assistant_message_with_text_keeps_it_and_other_roles_are_unchanged() {
    let (endpoint, bodies) = stand_in(openai_final_text()).await;
    let history = vec![
        message("system", "You are a careful agent."),
        message("user", "Read the config, then tell me the port."),
        ChatMessage {
            role: "assistant".to_string(),
            content: "Reading the config now.".to_string(),
            tool_call_id: None,
            tool_calls: Some(vec![ChatToolCall {
                id: "call_2".to_string(),
                name: "fs.read".to_string(),
                arguments: json!({"path": "/workspace/config.yaml"}),
            }]),
        },
        ChatMessage {
            role: "tool".to_string(),
            content: "port: 8443".to_string(),
            tool_call_id: Some("call_2".to_string()),
            tool_calls: None,
        },
        message("assistant", "The port is 8443."),
        message("user", "Thanks."),
    ];
    openai(&endpoint)
        .generate_chat(&history, &[], &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");

    let body = only_body(&bodies);
    let expected = json!([
        {"role": "system", "content": "You are a careful agent."},
        {"role": "user", "content": "Read the config, then tell me the port."},
        {"role": "assistant", "content": "Reading the config now.", "tool_calls": [
            {"id": "call_2", "type": "function",
             "function": {"name": "fs_read", "arguments": "{\"path\":\"/workspace/config.yaml\"}"}}
        ]},
        {"role": "tool", "content": "port: 8443", "tool_call_id": "call_2"},
        {"role": "assistant", "content": "The port is 8443."},
        {"role": "user", "content": "Thanks."}
    ]);
    assert_eq!(
        body["messages"], expected,
        "messages other than an assistant tool call without text must serialise as before"
    );
}

// ─── Defect 2: a returned tool name maps back exactly ───────────────────────

/// The tools the request sends: a name without an underscore, a registered
/// builtin whose own name holds one (`tool_router.rs`, `fs.create_dir`), and
/// a second such builtin.
fn sent_tools() -> Vec<ToolSchema> {
    ["fs.write", "fs.create_dir", "fs.multi_edit"]
        .iter()
        .map(|name| ToolSchema {
            name: name.to_string(),
            description: format!("the {name} tool"),
            parameters: json!({"type": "object", "properties": {}}),
        })
        .collect()
}

/// The names the stand-in answers with, in the sent form, and one name the
/// request never sent.
const RETURNED: [&str; 4] = [
    "fs_write",
    "fs_create_dir",
    "fs_multi_edit",
    "fs_not_a_tool",
];

/// What each returned name must come back as: the original name of each
/// sent tool, and the unsent name unchanged.
const EXPECTED: [&str; 4] = [
    "fs.write",
    "fs.create_dir",
    "fs.multi_edit",
    "fs_not_a_tool",
];

fn ask() -> Vec<ChatMessage> {
    vec![message(
        "user",
        "Write the file, make the directory, edit twice.",
    )]
}

fn returned_names(adapter: &str, response: ChatResponse) -> Vec<String> {
    match response {
        ChatResponse::ToolCalls(calls) => calls.into_iter().map(|c| c.name).collect(),
        ChatResponse::FinalText(r) => {
            panic!(
                "{adapter}: the stand-in's tool calls must come back as tool calls, got text {:?}",
                r.text
            )
        }
    }
}

fn assert_names(adapter: &str, got: Vec<String>) {
    let expected: Vec<String> = EXPECTED.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        got, expected,
        "{adapter}: each returned tool name must come back under the tool's original name, and a name never sent unchanged"
    );
}

#[tokio::test]
async fn openai_maps_returned_tool_names_back_exactly() {
    let calls: Vec<Value> = RETURNED
        .iter()
        .enumerate()
        .map(|(i, name)| {
            json!({"id": format!("call_{i}"), "type": "function",
                   "function": {"name": name, "arguments": "{}"}})
        })
        .collect();
    let answer = json!({
        "choices": [{
            "message": {"role": "assistant", "content": null, "tool_calls": calls},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let (endpoint, bodies) = stand_in(answer).await;
    let response = openai(&endpoint)
        .generate_chat(&ask(), &sent_tools(), &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");
    let sent: Vec<Value> = only_body(&bodies)["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["function"]["name"].clone())
        .collect();
    assert_eq!(
        sent,
        vec![
            json!("fs_write"),
            json!("fs_create_dir"),
            json!("fs_multi_edit")
        ],
        "the outgoing tool names keep their sent form"
    );
    assert_names("openai", returned_names("openai", response));
}

#[tokio::test]
async fn openai_raw_text_fallback_maps_returned_tool_names_back_exactly() {
    // A smaller model may write the tool-call array into its text instead of
    // `tool_calls`; the adapter's fallback reads an array that starts with
    // `[{"function":`.
    let raw: Vec<String> = RETURNED
        .iter()
        .enumerate()
        .map(|(i, name)| {
            format!("{{\"function\":{{\"name\":\"{name}\",\"arguments\":\"{{}}\"}},\"id\":\"call_{i}\"}}")
        })
        .collect();
    let raw_text = format!("I will call the tools. [{}]", raw.join(","));
    let answer = json!({
        "choices": [{
            "message": {"role": "assistant", "content": raw_text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let (endpoint, _bodies) = stand_in(answer).await;
    let response = openai(&endpoint)
        .generate_chat(&ask(), &sent_tools(), &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");
    assert_names(
        "openai raw-text fallback",
        returned_names("openai raw-text fallback", response),
    );
}

#[tokio::test]
async fn anthropic_maps_returned_tool_names_back_exactly() {
    let blocks: Vec<Value> = RETURNED
        .iter()
        .enumerate()
        .map(|(i, name)| json!({"type": "tool_use", "id": format!("toolu_{i}"), "name": name, "input": {}}))
        .collect();
    let answer = json!({
        "content": blocks,
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "stop_reason": "tool_use"
    });
    let (endpoint, bodies) = stand_in(answer).await;
    let adapter = AnthropicAdapter::new(
        format!("{endpoint}/v1"),
        "test-key".to_string(),
        "claude-test".to_string(),
    );
    let response = adapter
        .generate_chat(&ask(), &sent_tools(), &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");
    let sent: Vec<Value> = only_body(&bodies)["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(
        sent,
        vec![
            json!("fs_write"),
            json!("fs_create_dir"),
            json!("fs_multi_edit")
        ],
        "the outgoing tool names keep their sent form"
    );
    assert_names("anthropic", returned_names("anthropic", response));
}

#[tokio::test]
async fn gemini_maps_returned_tool_names_back_exactly() {
    let parts: Vec<Value> = RETURNED
        .iter()
        .map(|name| json!({"functionCall": {"name": name, "args": {}}}))
        .collect();
    let answer = json!({
        "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1, "totalTokenCount": 2}
    });
    let (endpoint, bodies) = stand_in(answer).await;
    let adapter = GeminiAdapter::new(
        format!("{endpoint}/v1beta"),
        "test-key".to_string(),
        "gemini-test".to_string(),
    );
    let response = adapter
        .generate_chat(&ask(), &sent_tools(), &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");
    let sent: Vec<Value> = only_body(&bodies)["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("functionDeclarations array")
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(
        sent,
        vec![
            json!("fs_write"),
            json!("fs_create_dir"),
            json!("fs_multi_edit")
        ],
        "the outgoing tool names keep their sent form"
    );
    assert_names("gemini", returned_names("gemini", response));
}

#[tokio::test]
async fn ollama_maps_returned_tool_names_back_exactly() {
    let calls: Vec<Value> = RETURNED
        .iter()
        .map(|name| json!({"function": {"name": name, "arguments": {}}}))
        .collect();
    let answer = json!({
        "message": {"role": "assistant", "content": "", "tool_calls": calls},
        "done": true
    });
    let (endpoint, bodies) = stand_in(answer).await;
    let adapter = OllamaAdapter::new(endpoint, "llama-test".to_string());
    let response = adapter
        .generate_chat(&ask(), &sent_tools(), &GenerationOptions::default())
        .await
        .expect("the stand-in answers 200");
    let sent: Vec<Value> = only_body(&bodies)["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["function"]["name"].clone())
        .collect();
    assert_eq!(
        sent,
        vec![
            json!("fs_write"),
            json!("fs_create_dir"),
            json!("fs_multi_edit")
        ],
        "the outgoing tool names keep their sent form"
    );
    assert_names("ollama", returned_names("ollama", response));
}
