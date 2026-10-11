// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An image generation model is an alias of the node's provider table with
//! the capability `image_generation` (AEGIS ADR-143 I1, on ADR-046 D1).
//!
//! Each test parses a provider block as an operator writes it in
//! `aegis-config.yaml`, builds the real `ProviderRegistry` from it, and, where
//! it calls the provider, calls a local stand-in of the OpenAI image
//! generation endpoint on a loopback port. No request leaves the loopback
//! interface.

use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
use aegis_orchestrator_core::domain::vision::{ImageOptions, VisionError};
use aegis_orchestrator_core::infrastructure::agent_manifest_parser::AgentManifestParser;
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use std::sync::{Arc, Mutex};

/// The first bytes of a PNG file, then a few more: what a provider's image
/// decodes to in these tests.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDRfake-image";

/// The stand-in's record of every image request: its headers and its body.
type Seen = Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>;

/// What the stand-in answers to `/v1/images/generations`.
#[derive(Clone)]
enum Answer {
    /// `data[].b64_json`, one entry per requested image.
    Base64,
    /// `data[].url`, pointing at the stand-in's own `/files/<n>.png`.
    Url,
    /// The status and the body, as given.
    Error(u16, &'static str),
}

/// Serve `/v1/images/generations` and `/files/{n}` on a loopback port.
/// Returns the provider endpoint (`http://127.0.0.1:<port>/v1`) and the
/// record.
async fn stand_in(answer: Answer) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("listener address");
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = Router::new()
        .route(
            "/v1/images/generations",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let record = record.clone();
                    let answer = answer.clone();
                    async move {
                        let n = body["n"].as_u64().unwrap_or(1);
                        record.lock().unwrap().push((headers, body));
                        let data: Vec<serde_json::Value> = (0..n)
                        .map(|i| match answer {
                            Answer::Url => {
                                serde_json::json!({"url": format!("http://{addr}/files/{i}.png")})
                            }
                            _ => serde_json::json!({
                                "b64_json": base64::engine::general_purpose::STANDARD.encode(PNG)
                            }),
                        })
                        .collect();
                        match answer {
                            Answer::Error(status, text) => (
                                StatusCode::from_u16(status).unwrap(),
                                Json(serde_json::json!({"error": {"message": text}})),
                            ),
                            _ => (
                                StatusCode::OK,
                                Json(serde_json::json!({"created": 1, "data": data})),
                            ),
                        }
                    }
                },
            ),
        )
        .route("/files/{name}", get(|| async { PNG.to_vec() }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve stand-in");
    });
    (format!("http://{addr}/v1"), seen)
}

/// A node with one provider of `provider_type` at `endpoint`: the language
/// model `default` and the image generation model `artist`.
fn node(provider_type: &str, endpoint: &str) -> NodeConfigManifest {
    let providers: Vec<LLMProviderConfig> = serde_yaml::from_str(&format!(
        r#"- name: images
  type: {provider_type}
  endpoint: "{endpoint}"
  api_key: "test-key"
  models:
    - alias: "default"
      model: "m-chat"
      capabilities: ["chat"]
      context_window: 8192
    - alias: "artist"
      model: "m-image"
      capabilities: ["image_generation"]
      context_window: 0
"#
    ))
    .expect("the providers block parses");
    let mut manifest = NodeConfigManifest::default();
    manifest.spec.llm_providers = providers;
    manifest
}

fn registry(manifest: &NodeConfigManifest) -> ProviderRegistry {
    ProviderRegistry::from_config(manifest).expect("registry builds")
}

#[test]
fn an_image_generation_alias_binds_an_image_provider_and_no_language_model() {
    for provider_type in ["openai", "openai-compatible"] {
        let registry = registry(&node(provider_type, "http://127.0.0.1:9/v1"));
        assert_eq!(
            registry.image_generation_aliases(),
            vec!["artist".to_string()],
            "{provider_type}: the node's image generation aliases"
        );
        assert!(
            registry.image_provider("artist").is_some(),
            "{provider_type}: 'artist' has no image provider"
        );
        assert!(
            !registry.has_alias("artist"),
            "{provider_type}: 'artist' is bound as a language model"
        );
        assert!(
            registry.has_alias("default") && registry.image_provider("default").is_none(),
            "{provider_type}: 'default' is not a language model only"
        );
    }
}

#[test]
fn a_node_with_no_image_generation_alias_has_none() {
    let mut manifest = node("openai-compatible", "http://127.0.0.1:9/v1");
    manifest.spec.llm_providers[0].models.pop();
    let registry = registry(&manifest);
    assert!(registry.image_generation_aliases().is_empty());
    assert!(registry.image_provider("artist").is_none());
}

#[tokio::test]
async fn the_adapter_posts_to_images_generations_and_decodes_b64_json() {
    let (endpoint, seen) = stand_in(Answer::Base64).await;
    let registry = registry(&node("openai-compatible", &endpoint));
    let provider = registry
        .image_provider("artist")
        .expect("an image provider");

    let answer = provider
        .generate_image(
            "a lighthouse at dusk",
            &ImageOptions {
                size: Some("512x512".to_string()),
                count: 2,
            },
        )
        .await
        .expect("the provider answers");

    assert_eq!(answer.images.len(), 2);
    for image in &answer.images {
        assert_eq!(image.bytes, PNG);
        assert_eq!(image.content_type, "image/png");
    }
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one request");
    let (headers, body) = &seen[0];
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer test-key")
    );
    assert_eq!(body["model"], "m-image");
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert_eq!(body["n"], 2);
    assert_eq!(body["size"], "512x512");
}

#[tokio::test]
async fn an_answer_by_url_is_fetched_into_bytes() {
    let (endpoint, _) = stand_in(Answer::Url).await;
    let registry = registry(&node("openai", &endpoint));
    let provider = registry
        .image_provider("artist")
        .expect("an image provider");

    let answer = provider
        .generate_image("a red bicycle", &ImageOptions::default())
        .await
        .expect("the provider answers");

    assert_eq!(answer.images.len(), 1);
    assert_eq!(answer.images[0].bytes, PNG);
    assert_eq!(answer.images[0].content_type, "image/png");
}

#[tokio::test]
async fn a_provider_400_is_invalid_input_with_its_message() {
    let (endpoint, _) = stand_in(Answer::Error(400, "size is not supported")).await;
    let registry = registry(&node("openai-compatible", &endpoint));
    let provider = registry
        .image_provider("artist")
        .expect("an image provider");

    let error = provider
        .generate_image("a red bicycle", &ImageOptions::default())
        .await
        .expect_err("the provider refuses");

    match error {
        VisionError::InvalidInput(message) => assert!(
            message.contains("size is not supported"),
            "the provider's message is lost: {message}"
        ),
        other => panic!("not InvalidInput: {other:?}"),
    }
}

fn agent_yaml(model: &str) -> String {
    format!(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: painter
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    model: "{model}"
  task:
    instruction: "Paint something"
"#
    )
}

#[test]
fn an_agent_whose_model_generates_images_is_refused_at_parse() {
    let image_aliases =
        registry(&node("openai-compatible", "http://127.0.0.1:9/v1")).image_generation_aliases();

    let refusal = AgentManifestParser::parse_yaml_for_node(&agent_yaml("artist"), &image_aliases)
        .expect_err("the manifest is refused");
    assert!(
        refusal
            .to_string()
            .contains("'artist' generates images; an agent's model must be a language model"),
        "the refusal reads: {refusal}"
    );

    let manifest = AgentManifestParser::parse_yaml_for_node(&agent_yaml("default"), &image_aliases)
        .expect("a language model's manifest parses");
    assert_eq!(manifest.spec.runtime.model, "default");
}
