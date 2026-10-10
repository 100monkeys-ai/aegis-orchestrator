// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A provider's configured request headers (AEGIS ADR-124 D3, clause 1).
//!
//! Production reaches Workers AI through the AI Gateway `inference-production`
//! at `https://api.cloudflare.com/client/v4/accounts/<account>/ai/v1`, which
//! routes a request only when it carries `cf-aig-gateway-id`. The
//! OpenAI-compatible adapter sent `Authorization` and `Content-Type` and
//! nothing else, so no configuration could name the gateway.
//!
//! Each test parses a provider block as an operator writes it in
//! `aegis-config.yaml`, builds the real `ProviderRegistry` from it, and sends
//! a chat request to a local stand-in that records every request's headers.
//! No request leaves the loopback interface.

use aegis_orchestrator_core::domain::llm::{ChatMessage, ChatResponse, GenerationOptions};
use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// The stand-in's record of every request it answered.
type Seen = Arc<Mutex<Vec<HeaderMap>>>;

/// Serve an OpenAI-compatible `/v1/chat/completions` on a loopback port that
/// records each request's headers and answers with a final text. Returns the
/// provider endpoint (`http://127.0.0.1:<port>/v1`) and the record.
async fn stand_in() -> (String, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |headers: HeaderMap| {
            let record = record.clone();
            async move {
                record.lock().unwrap().push(headers);
                Json(serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
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
    (format!("http://{addr}/v1"), seen)
}

/// A provider block as an operator writes it, with `extra` appended under it
/// at the provider's indentation (the `headers:` block, or nothing).
fn provider_yaml(endpoint: &str, extra: &str) -> String {
    format!(
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
{extra}"#
    )
}

fn parse_provider(yaml: &str) -> LLMProviderConfig {
    serde_yaml::from_str(yaml).expect("the provider block parses")
}

fn manifest_with(provider: LLMProviderConfig) -> NodeConfigManifest {
    let mut manifest = NodeConfigManifest::default();
    manifest.spec.llm_providers = vec![provider];
    manifest
}

async fn chat(registry: &ProviderRegistry) {
    let messages = [ChatMessage {
        role: "user".to_string(),
        content: "hello".to_string(),
        tool_call_id: None,
        tool_calls: None,
    }];
    match registry
        .generate_chat(
            "default",
            aegis_orchestrator_core::infrastructure::llm::registry::DataClass::Standard,
            &messages,
            &[],
            &GenerationOptions::default(),
        )
        .await
    {
        Ok(ChatResponse::FinalText(r)) => assert_eq!(r.text, "ok"),
        other => panic!("the stand-in's answer must come back as final text, got {other:?}"),
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn names(headers: &HeaderMap) -> BTreeSet<String> {
    headers.keys().map(|k| k.as_str().to_string()).collect()
}

const GATEWAY_HEADERS: &str = "headers:
  cf-aig-gateway-id: \"inference-production\"
  x-aegis-test: \"second-header\"
";

#[tokio::test]
async fn configured_headers_are_sent_on_every_chat_request_beside_authorization() {
    let (endpoint, seen) = stand_in().await;
    let manifest = manifest_with(parse_provider(&provider_yaml(&endpoint, GATEWAY_HEADERS)));
    manifest
        .validate()
        .expect("a provider with a gateway header is a valid configuration");
    let registry = ProviderRegistry::from_config(&manifest).expect("registry builds");

    chat(&registry).await;
    chat(&registry).await;

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "the stand-in must have answered both requests"
    );
    let mut failures = Vec::new();
    for (i, headers) in seen.iter().enumerate() {
        if header(headers, "cf-aig-gateway-id") != Some("inference-production") {
            failures.push(format!(
                "request {}: the configured header cf-aig-gateway-id was not sent (got {:?})",
                i + 1,
                header(headers, "cf-aig-gateway-id")
            ));
        }
        if header(headers, "x-aegis-test") != Some("second-header") {
            failures.push(format!(
                "request {}: the configured header x-aegis-test was not sent (got {:?})",
                i + 1,
                header(headers, "x-aegis-test")
            ));
        }
        if header(headers, "authorization") != Some("Bearer test-inference-key") {
            failures.push(format!(
                "request {}: Authorization must still carry the provider's key (got {:?})",
                i + 1,
                header(headers, "authorization")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[tokio::test]
async fn a_provider_without_headers_parses_and_sends_none() {
    let (endpoint, seen) = stand_in().await;

    let without = parse_provider(&provider_yaml(&endpoint, ""));
    let serialised = serde_yaml::to_string(&without).expect("the provider serialises");
    let with = parse_provider(&provider_yaml(&endpoint, GATEWAY_HEADERS));

    for provider in [without, with] {
        let manifest = manifest_with(provider);
        manifest.validate().expect("the configuration is valid");
        let registry = ProviderRegistry::from_config(&manifest).expect("registry builds");
        chat(&registry).await;
    }

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "the stand-in must have answered both requests"
    );
    let (plain, configured) = (&seen[0], &seen[1]);

    let mut failures = Vec::new();
    if serialised.contains("headers") {
        failures.push(format!(
            "a provider without headers must serialise without a headers key, got:\n{serialised}"
        ));
    }
    if header(plain, "authorization") != Some("Bearer test-inference-key") {
        failures.push(format!(
            "the provider without headers must still send its key (got {:?})",
            header(plain, "authorization")
        ));
    }
    // The controlled differential: the same provider with and without the
    // headers block. The configured request must carry exactly the plain
    // request's headers plus the two configured ones, and the plain request
    // none of the configured ones.
    let added: BTreeSet<String> = names(configured)
        .difference(&names(plain))
        .cloned()
        .collect();
    let expected: BTreeSet<String> = ["cf-aig-gateway-id", "x-aegis-test"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if added != expected {
        failures.push(format!(
            "the headers block must add exactly {expected:?} to the request, it added {added:?}"
        ));
    }
    let missing: BTreeSet<String> = names(plain)
        .difference(&names(configured))
        .cloned()
        .collect();
    if !missing.is_empty() {
        failures.push(format!(
            "the headers block must not remove a header the adapter sends, it removed {missing:?}"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn a_header_named_authorization_is_refused_at_validation() {
    let mut failures = Vec::new();
    for name in [
        "authorization",
        "Authorization",
        "AUTHORIZATION",
        "AuThOrIzAtIoN",
    ] {
        let extra = format!("headers:\n  {name}: \"Bearer another-key\"\n");
        let provider = parse_provider(&provider_yaml("https://example.invalid/v1", &extra));
        match manifest_with(provider).validate() {
            Ok(()) => failures.push(format!(
                "a header named {name} was accepted at validation; the key has its own field"
            )),
            Err(e) => {
                let msg = e.to_string();
                if !(msg.contains("workers-ai") && msg.to_lowercase().contains("authorization")) {
                    failures.push(format!(
                        "the refusal of {name} must name the provider and the header, got: {msg}"
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
