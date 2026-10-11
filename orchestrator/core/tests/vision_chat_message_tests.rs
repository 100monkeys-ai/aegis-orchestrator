// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An image reaching a model: a chat message's content is text parts and
//! image parts, the openai-compatible adapter sends an image part in the
//! provider's form, an adapter with no image form refuses it naming the
//! alias, and an image attached to an agent's turn becomes an image part for
//! an alias tagged `vision` and a text note for one without.
//!
//! Each provider test builds the real `ProviderRegistry` from a provider
//! block as an operator writes it in `aegis-config.yaml` and sends a chat
//! request to a local stand-in that records every request body. No request
//! leaves the loopback interface.

use aegis_orchestrator_core::application::inner_loop_service::turn_attachment_parts;
use aegis_orchestrator_core::domain::execution::AttachmentRef;
use aegis_orchestrator_core::domain::llm::{
    ChatMessage, ChatResponse, ContentPart, GenerationOptions, ImageSource, LLMError,
    MessageContent,
};
use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
use aegis_orchestrator_core::domain::shared_kernel::VolumeId;
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The first bytes of a PNG file: enough for an image part's bytes.
const PNG_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];

type Bodies = Arc<Mutex<Vec<Value>>>;

/// Serve any path on a loopback port, record each request's JSON body, and
/// answer every request with one final text.
async fn stand_in() -> (String, Bodies) {
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let record = bodies.clone();
    let answer = json!({
        "choices": [{
            "message": {"role": "assistant", "content": "a cat"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
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

/// The registry one provider block builds, with its models as written.
fn registry(provider_type: &str, endpoint: &str, models_yaml: &str) -> ProviderRegistry {
    let provider: LLMProviderConfig = serde_yaml::from_str(&format!(
        r#"name: provider-under-test
type: {provider_type}
endpoint: "{endpoint}"
api_key: "test-inference-key"
enabled: true
models:
{models_yaml}"#
    ))
    .expect("the provider block parses");
    let mut manifest = NodeConfigManifest::default();
    manifest.spec.llm_providers = vec![provider];
    manifest.spec.llm_selection.max_retries = 1;
    manifest
        .validate()
        .expect("the configuration under test is valid");
    ProviderRegistry::from_config(&manifest).expect("registry builds")
}

const VISION_AND_TEXT_MODELS: &str = r#"  - alias: "seeing"
    model: "@cf/meta/llama-3.2-11b-vision-instruct"
    capabilities: ["chat", "vision"]
    context_window: 131072
  - alias: "default"
    model: "@cf/openai/gpt-oss-120b"
    capabilities: ["chat"]
    context_window: 131072
"#;

fn image_message() -> Vec<ChatMessage> {
    vec![ChatMessage {
        role: "user".to_string(),
        content: MessageContent::from_parts(vec![
            ContentPart::Text {
                text: "What is in this picture?".to_string(),
            },
            ContentPart::Image {
                source: ImageSource::Bytes(PNG_BYTES.to_vec()),
                mime_type: "image/png".to_string(),
            },
            ContentPart::Image {
                source: ImageSource::Url("https://example.com/cat.jpg".to_string()),
                mime_type: "image/jpeg".to_string(),
            },
        ]),
        tool_call_id: None,
        tool_calls: None,
    }]
}

fn attachment(name: &str, mime_type: &str) -> AttachmentRef {
    AttachmentRef {
        volume_id: VolumeId::new(),
        path: format!("/{name}"),
        name: name.to_string(),
        mime_type: mime_type.to_string(),
        size: PNG_BYTES.len() as u64,
        sha256: None,
    }
}

/// A message with an image part goes to an openai-compatible provider as
/// the Chat Completions content array: a `text` part, then an `image_url`
/// part whose URL is a data URL of the bytes with their MIME type, or the
/// image's own URL.
#[tokio::test]
async fn an_image_part_is_sent_to_an_openai_compatible_provider_as_an_image_url_part() {
    let (endpoint, bodies) = stand_in().await;
    let registry = registry(
        "openai-compatible",
        &format!("{endpoint}/v1"),
        VISION_AND_TEXT_MODELS,
    );

    let answer = registry
        .generate_chat(
            "seeing",
            &image_message(),
            &[],
            &GenerationOptions::default(),
        )
        .await
        .expect("the provider answers");
    assert!(matches!(answer, ChatResponse::FinalText(ref r) if r.text == "a cat"));

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1, "one request is sent");
    assert_eq!(
        bodies[0]["messages"][0]["content"],
        json!([
            {"type": "text", "text": "What is in this picture?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgoBAgM="}},
            {"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg"}},
        ])
    );
}

/// A message of text alone is still sent with `content` a string.
#[tokio::test]
async fn a_text_message_is_still_sent_with_string_content() {
    let (endpoint, bodies) = stand_in().await;
    let registry = registry(
        "openai-compatible",
        &format!("{endpoint}/v1"),
        VISION_AND_TEXT_MODELS,
    );
    let messages = [ChatMessage {
        role: "user".to_string(),
        content: MessageContent::from("hello"),
        tool_call_id: None,
        tool_calls: None,
    }];
    registry
        .generate_chat("default", &messages, &[], &GenerationOptions::default())
        .await
        .expect("the provider answers");
    assert_eq!(bodies.lock().unwrap()[0]["messages"][0]["content"], "hello");
}

/// An adapter with no image form refuses a message carrying an image part
/// before any request, with a sentence naming the alias.
#[tokio::test]
async fn an_adapter_without_an_image_form_refuses_an_image_part_naming_the_alias() {
    let (endpoint, bodies) = stand_in().await;
    let registry = registry(
        "anthropic",
        &format!("{endpoint}/v1"),
        r#"  - alias: "claude-eyes"
    model: "claude-sonnet"
    capabilities: ["chat", "vision"]
    context_window: 200000
"#,
    );

    let err = registry
        .generate_chat(
            "claude-eyes",
            &image_message(),
            &[],
            &GenerationOptions::default(),
        )
        .await
        .expect_err("the image part is refused");
    match err {
        LLMError::InvalidInput(sentence) => assert_eq!(
            sentence,
            "alias 'claude-eyes' cannot read images: its provider, anthropic, is sent text only"
        ),
        other => panic!("expected the refusal, got {other:?}"),
    }
    assert!(bodies.lock().unwrap().is_empty(), "no request is sent");
}

/// The capability is read where the alias is chosen: an alias tagged
/// `vision` reads images, one without the tag does not, nor an alias the
/// table does not hold.
#[test]
fn an_alias_reads_images_only_when_tagged_vision() {
    let registry = registry(
        "openai-compatible",
        "http://127.0.0.1:9/v1",
        VISION_AND_TEXT_MODELS,
    );
    assert!(registry.alias_reads_images("seeing"));
    assert!(!registry.alias_reads_images("default"));
    assert!(!registry.alias_reads_images("absent"));
}

/// An image attached to an agent's turn becomes an image part, with its
/// bytes and MIME type, for an alias tagged `vision`, and a text note naming
/// it for an alias without; an attachment that is not an image adds nothing.
#[test]
fn an_image_attachment_on_a_turn_is_an_image_part_for_a_vision_alias_and_a_note_otherwise() {
    let registry = registry(
        "openai-compatible",
        "http://127.0.0.1:9/v1",
        VISION_AND_TEXT_MODELS,
    );
    let photo = attachment("photo.png", "image/png");
    let notes = attachment("notes.txt", "text/plain");
    let read = |a: &AttachmentRef| -> Result<Vec<u8>, String> {
        assert_eq!(a.name, "photo.png", "only an image is read");
        Ok(PNG_BYTES.to_vec())
    };

    let seen = turn_attachment_parts(
        &[photo.clone(), notes.clone()],
        "seeing",
        registry.alias_reads_images("seeing"),
        read,
    );
    assert_eq!(
        seen,
        vec![
            ContentPart::Text {
                text: "The attached image photo.png:".to_string()
            },
            ContentPart::Image {
                source: ImageSource::Bytes(PNG_BYTES.to_vec()),
                mime_type: "image/png".to_string(),
            },
        ]
    );

    let unseen = turn_attachment_parts(
        &[photo, notes],
        "default",
        registry.alias_reads_images("default"),
        |_: &AttachmentRef| -> Result<Vec<u8>, String> {
            panic!("nothing is read for an alias that does not read images")
        },
    );
    assert_eq!(
        unseen,
        vec![ContentPart::Text {
            text: "The image photo.png (image/png) is attached and not shown to you: the model of alias 'default' does not read images.".to_string()
        }]
    );
}

/// An image the turn cannot read is a text note saying so, never a failed
/// call.
#[test]
fn an_image_attachment_that_cannot_be_read_is_a_note() {
    let parts = turn_attachment_parts(
        &[attachment("photo.png", "image/png")],
        "seeing",
        true,
        |_: &AttachmentRef| -> Result<Vec<u8>, String> { Err("file not found".to_string()) },
    );
    assert_eq!(
        parts,
        vec![ContentPart::Text {
            text:
                "The image photo.png (image/png) is attached and could not be read: file not found."
                    .to_string()
        }]
    );
}
