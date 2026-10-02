// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! How one model call is bounded and when it is retried (AEGIS ADR-124, the
//! measured Update of 2026-10-01).
//!
//! Measured in production: six judgments failed because a stalled call ended
//! only at Workers AI's own 408 after about 235 s, leaving about 64 s of a
//! 300 s judgment for the retry, while the slowest successful answer of 539
//! took 188.4 s. And each context-length 400 from the provider was sent three
//! times, though no retry could change the answer.
//!
//! Each test parses an `llm_selection` block and a provider block as an
//! operator writes them in `aegis-config.yaml`, builds the real
//! `ProviderRegistry` from them, and sends a chat request to a local stand-in
//! that counts the requests it receives and answers each by a script: stall
//! for a time, then answer with a status and a body. No request leaves the
//! loopback interface.

use aegis_orchestrator_core::domain::llm::{
    ChatMessage, ChatResponse, GenerationOptions, LLMError,
};
use aegis_orchestrator_core::domain::node_config::{
    LLMProviderConfig, LLMSelection, NodeConfigManifest,
};
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One scripted answer of the stand-in: wait `stall`, then answer `status`
/// with `body`.
#[derive(Clone)]
struct Answer {
    stall: Duration,
    status: u16,
    body: String,
}

fn ok_answer(after: Duration) -> Answer {
    Answer {
        stall: after,
        status: 200,
        body: serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string(),
    }
}

fn error_answer(status: u16, body: &str) -> Answer {
    Answer {
        stall: Duration::ZERO,
        status,
        body: body.to_string(),
    }
}

/// Serve an OpenAI-compatible `/v1/chat/completions` on a loopback port. The
/// n-th request (from 0) gets `script[n]`, or the last entry once the script
/// is spent. Returns the provider endpoint and the request counter.
async fn stand_in(script: Vec<Answer>) -> (String, Arc<AtomicUsize>) {
    assert!(!script.is_empty(), "the stand-in needs at least one answer");
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let script = Arc::new(script);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = counter.clone();
            let script = script.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let answer = script[n.min(script.len() - 1)].clone();
                tokio::time::sleep(answer.stall).await;
                (
                    StatusCode::from_u16(answer.status).expect("a valid status"),
                    [("content-type", "application/json")],
                    answer.body,
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve stand-in");
    });
    (format!("http://{addr}/v1"), count)
}

/// The registry an operator's configuration builds: one openai-compatible
/// provider on the stand-in, and the `llm_selection` block as written.
fn registry(endpoint: &str, llm_selection_yaml: &str) -> ProviderRegistry {
    let provider: LLMProviderConfig = serde_yaml::from_str(&format!(
        r#"name: workers-ai
type: openai-compatible
endpoint: "{endpoint}"
api_key: "test-inference-key"
enabled: true
models:
  - alias: "default"
    model: "@cf/zai-org/glm-5.3-flash"
    capabilities: ["chat"]
    context_window: 8192
"#
    ))
    .expect("the provider block parses");
    let selection: LLMSelection =
        serde_yaml::from_str(llm_selection_yaml).expect("the llm_selection block parses");
    let mut manifest = NodeConfigManifest::default();
    manifest.spec.llm_providers = vec![provider];
    manifest.spec.llm_selection = selection;
    manifest
        .validate()
        .expect("the configuration under test is valid");
    ProviderRegistry::from_config(&manifest).expect("registry builds")
}

async fn chat(registry: &ProviderRegistry) -> Result<ChatResponse, LLMError> {
    let messages = [ChatMessage {
        role: "user".to_string(),
        content: "hello".to_string(),
        tool_call_id: None,
        tool_calls: None,
    }];
    registry
        .generate_chat("default", &messages, &[], &GenerationOptions::default())
        .await
}

fn assert_ok(res: &Result<ChatResponse, LLMError>) {
    match res {
        Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
        other => panic!("the stand-in's answer must come back as final text, got {other:?}"),
    }
}

// ── Item 1: one attempt is bounded by llm_attempt_timeout_secs ──────────────

/// The first attempt stalls for 8 s and then gets the provider's own 408, as
/// Workers AI answered after about 235 s; the second answers at once. With a
/// 1 s attempt bound the registry abandons the first attempt at 1 s and the
/// call succeeds on the second, well before the stand-in's own end.
#[tokio::test]
async fn a_stalled_attempt_is_abandoned_at_the_attempt_timeout_and_retried() {
    let (endpoint, count) = stand_in(vec![
        Answer {
            stall: Duration::from_secs(8),
            ..error_answer(
                408,
                r#"{"errors":[{"message":"AiError: 3046: Request timeout"}]}"#,
            )
        },
        ok_answer(Duration::ZERO),
    ])
    .await;
    let registry = registry(
        &endpoint,
        "max_retries: 3\nretry_delay_ms: 10\nllm_overall_timeout_secs: 30\nllm_attempt_timeout_secs: 1\n",
    );

    let start = Instant::now();
    let res = chat(&registry).await;
    let elapsed = start.elapsed();

    assert_ok(&res);
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the stalled attempt is abandoned and the call retried once"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the call must end at the attempt timeout (1 s) plus the retry, not at the stand-in's own end after 8 s; it took {elapsed:?}"
    );
}

/// Absent the key, an attempt is not bounded: a slow answer is waited for and
/// returned, from one request, as before the key existed.
#[tokio::test]
async fn without_the_key_a_slow_attempt_is_waited_for() {
    let (endpoint, count) = stand_in(vec![ok_answer(Duration::from_secs(3))]).await;
    let registry = registry(
        &endpoint,
        "max_retries: 3\nretry_delay_ms: 10\nllm_overall_timeout_secs: 30\n",
    );

    let start = Instant::now();
    let res = chat(&registry).await;
    let elapsed = start.elapsed();

    assert_ok(&res);
    assert_eq!(count.load(Ordering::SeqCst), 1, "one request, no retry");
    assert!(
        elapsed >= Duration::from_secs(3),
        "the slow answer is waited for: {elapsed:?}"
    );
}

/// The overall timeout still ends the call: every attempt stalls, each is
/// abandoned at 2 s and retried, and the 3 s overall budget ends the call
/// with the same error as before.
#[tokio::test]
async fn the_overall_timeout_still_ends_the_call() {
    let (endpoint, count) = stand_in(vec![ok_answer(Duration::from_secs(60))]).await;
    let registry = registry(
        &endpoint,
        "max_retries: 3\nretry_delay_ms: 10\nllm_overall_timeout_secs: 3\nllm_attempt_timeout_secs: 2\n",
    );

    let start = Instant::now();
    let res = chat(&registry).await;
    let elapsed = start.elapsed();

    assert!(
        matches!(&res, Err(LLMError::Network(msg)) if msg == "upstream timeout after 3s"),
        "the overall timeout ends the call, got {res:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(5),
        "the call ends at the overall budget: {elapsed:?}"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the first attempt was abandoned at 2 s and a second sent inside the budget"
    );
}

/// An elapsed attempt counts against max_retries like a 408: with every
/// attempt stalling and a budget that leaves room for all of them, the call
/// makes exactly max_retries attempts and ends with an error naming the
/// attempt timeout.
#[tokio::test]
async fn elapsed_attempts_count_against_max_retries() {
    let (endpoint, count) = stand_in(vec![ok_answer(Duration::from_secs(60))]).await;
    let registry = registry(
        &endpoint,
        "max_retries: 2\nretry_delay_ms: 10\nllm_overall_timeout_secs: 30\nllm_attempt_timeout_secs: 1\n",
    );

    let start = Instant::now();
    let res = chat(&registry).await;
    let elapsed = start.elapsed();

    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "max_retries attempts, no more"
    );
    assert!(
        matches!(&res, Err(LLMError::Network(msg)) if msg.contains("attempt timeout after 1s")),
        "the last error names the attempt timeout, got {res:?}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "two 1 s attempts and one short backoff: {elapsed:?}"
    );
}

// ── Item 2: a provider's 400 is not retried ─────────────────────────────────

/// Workers AI's refusal of a prompt longer than the model's window, as R2
/// received it 21 times.
const CONTEXT_LENGTH_400: &str = r#"{"errors":[{"message":"AiError: Bad input: maximum context length is 262144 tokens. However, you requested 16384 output tokens and your prompt contains at least 245761 input tokens."}],"success":false}"#;

/// A 400 cannot succeed on a retry: the stand-in answers every request with
/// the same 400, and the call fails after one request, with the provider's
/// message in the error.
#[tokio::test]
async fn a_provider_400_fails_at_once_with_the_providers_message() {
    let (endpoint, count) = stand_in(vec![error_answer(400, CONTEXT_LENGTH_400)]).await;
    let registry = registry(
        &endpoint,
        "max_retries: 3\nretry_delay_ms: 10\nllm_overall_timeout_secs: 30\n",
    );

    let res = chat(&registry).await;

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "a 400 is sent once, not retried; the call returned {res:?}"
    );
    let err = res.expect_err("a 400 fails the call");
    let msg = err.to_string();
    assert!(
        msg.contains("400") && msg.contains("maximum context length is 262144 tokens"),
        "the error carries the status and the provider's message, got: {msg}"
    );
}

/// The failures a retry can cure are still retried: a 408, a 429, a 500 and
/// a 503 each come back once and the call succeeds on the second request.
#[tokio::test]
async fn a_408_a_429_and_a_5xx_are_still_retried() {
    for status in [408_u16, 429, 500, 503] {
        let (endpoint, count) = stand_in(vec![
            error_answer(status, r#"{"errors":[{"message":"transient"}]}"#),
            ok_answer(Duration::ZERO),
        ])
        .await;
        let registry = registry(
            &endpoint,
            "max_retries: 3\nretry_delay_ms: 10\nllm_overall_timeout_secs: 30\n",
        );

        let res = chat(&registry).await;

        assert!(
            matches!(&res, Ok(ChatResponse::FinalText(r)) if r.text == "ok"),
            "HTTP {status} must be retried to the answer, got {res:?}"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "HTTP {status}: one failed request, one retry"
        );
    }
}
