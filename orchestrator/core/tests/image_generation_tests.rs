// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An image generation model is an alias of the node's provider table with
//! the capability `image_generation` (AEGIS ADR-143 I1, on ADR-046 D1).
//!
//! Each test parses a provider block as an operator writes it in
//! `aegis-config.yaml`, builds the real `ProviderRegistry` from it, and, where
//! it calls the provider, calls a local stand-in of the OpenAI image
//! generation endpoint, or of Workers AI's `/ai/run` (ADR-143 Update I1a), on
//! a loopback port. No request leaves the loopback interface.

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
    log_capture::install();
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

/// The log capture of this binary's tests: one subscriber, the global
/// default, installed by every helper that builds an image adapter before it
/// builds one, so no adapter callsite registers before it exists.
///
/// `tracing` caches each callsite's interest once, on whichever thread first
/// reaches it, and while at most one dispatcher is registered it asks only
/// that thread's default. A per-test `set_default` left that answer to
/// whichever test reached the callsite first: another test's thread, with no
/// subscriber, cached the retry WARN as never, and the capturing test saw no
/// WARN line. Every thread's default is this subscriber, so the interest is
/// always its own, and a capture keeps only the events emitted inside its own
/// span, on whatever thread the instrumented future is polled.
mod log_capture {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    static INSTALLED: OnceLock<()> = OnceLock::new();
    static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);
    /// Each capture's span id and the lines captured inside it.
    static CAPTURES: Mutex<Vec<(u64, Arc<Mutex<String>>)>> = Mutex::new(Vec::new());

    thread_local! {
        /// The spans entered on this thread, innermost last.
        static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    }

    /// Install the subscriber as the binary's global default, once; every
    /// caller returns after it is installed.
    pub(crate) fn install() {
        INSTALLED.get_or_init(|| {
            tracing::subscriber::set_global_default(Global)
                .expect("no other global subscriber in this test binary");
        });
    }

    struct Global;

    struct Fields<'a>(&'a mut String);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
    }

    impl tracing::Subscriber for Global {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(NEXT_SPAN.fetch_add(1, Ordering::Relaxed))
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let Ok(Some(lines)) = ENTERED.try_with(|entered| {
                let captures = CAPTURES.lock().unwrap();
                entered.borrow().iter().rev().find_map(|span| {
                    captures
                        .iter()
                        .find(|(id, _)| id == span)
                        .map(|(_, lines)| lines.clone())
                })
            }) else {
                return;
            };
            let mut line = event.metadata().level().to_string();
            event.record(&mut Fields(&mut line));
            line.push('\n');
            lines.lock().unwrap().push_str(&line);
        }
        fn enter(&self, span: &tracing::span::Id) {
            let _ = ENTERED.try_with(|entered| entered.borrow_mut().push(span.into_u64()));
        }
        fn exit(&self, span: &tracing::span::Id) {
            let _ = ENTERED.try_with(|entered| {
                let mut entered = entered.borrow_mut();
                if let Some(at) = entered.iter().rposition(|id| *id == span.into_u64()) {
                    entered.remove(at);
                }
            });
        }
    }

    /// Every event logged inside one span, as `<LEVEL> <fields>` lines.
    pub(crate) struct Captured {
        span: tracing::Span,
        lines: Arc<Mutex<String>>,
    }

    impl Captured {
        pub(crate) fn new() -> Self {
            install();
            let span = tracing::info_span!("log_capture");
            let id = span.id().expect("the global subscriber enables the span");
            let lines = Arc::new(Mutex::new(String::new()));
            CAPTURES
                .lock()
                .unwrap()
                .push((id.into_u64(), lines.clone()));
            Self { span, lines }
        }

        /// The span whose events this capture keeps.
        pub(crate) fn span(&self) -> tracing::Span {
            self.span.clone()
        }

        pub(crate) fn lines(&self) -> String {
            self.lines.lock().unwrap().clone()
        }
    }
}

// ===========================================================================
// A Workers AI image model (AEGIS ADR-143 Update I1a)
// ===========================================================================

mod workers_ai {
    use super::{node, registry, PNG};
    use aegis_orchestrator_core::domain::vision::{Image, ImageOptions, VisionError};
    use aegis_orchestrator_core::infrastructure::llm::workers_ai_image::WorkersAiImageAdapter;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use axum::Router;
    use base64::Engine;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tracing::Instrument;

    /// Production's alias table names Workers AI in this form.
    const WORKERS_AI_ENDPOINT: &str = "https://api.cloudflare.com/client/v4/accounts/acct-1/ai/v1";

    /// The one model whose schema takes a multipart form.
    const FLUX_2_DEV: &str = "@cf/black-forest-labs/flux-2-dev";

    /// A model whose schema takes a JSON body, refuses multipart, and takes
    /// no size.
    const FLUX_1_SCHNELL: &str = "@cf/black-forest-labs/flux-1-schnell";

    /// A model whose schema takes a JSON body with `width` and `height`.
    const SDXL: &str = "@cf/stabilityai/stable-diffusion-xl-base-1.0";

    /// The first bytes of a JPEG file, then a few more.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIFfake-image";

    /// The sentences the two adapters answer an edit with, each as
    /// `Unsupported`. Telling the two adapters apart by them needs no
    /// request.
    const NOT_WIRED: &str = "Workers AI image editing is not wired";
    const OPENAI_NOT_OFFERED: &str = "image editing is not offered by this provider's adapter";

    /// One request the stand-in received.
    #[derive(Debug, Clone)]
    struct Request {
        method: Method,
        path: String,
        headers: HeaderMap,
        body: Bytes,
    }

    impl Request {
        fn content_type(&self) -> String {
            self.headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        }

        /// The JSON body, after asserting the request is one: its content
        /// type `application/json` and its body no multipart form.
        fn json(&self) -> serde_json::Map<String, serde_json::Value> {
            assert_eq!(
                self.content_type(),
                "application/json",
                "the request is not JSON"
            );
            let body = String::from_utf8(self.body.to_vec()).expect("a text body");
            assert!(
                !body.contains("Content-Disposition: form-data"),
                "the body is a multipart form: {body}"
            );
            match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(serde_json::Value::Object(map)) => map,
                other => panic!("the body is not a JSON object: {other:?}"),
            }
        }

        /// The multipart form's fields, `(name, value)`, in their order.
        fn fields(&self) -> Vec<(String, String)> {
            let content_type = self.content_type();
            let boundary = content_type
                .split("boundary=")
                .nth(1)
                .unwrap_or_else(|| panic!("no multipart boundary in '{content_type}'"))
                .trim_matches('"')
                .to_string();
            let body = String::from_utf8(self.body.to_vec()).expect("a text form");
            body.split(&format!("--{boundary}"))
                .filter_map(|part| {
                    let (head, value) = part.split_once("\r\n\r\n")?;
                    let name = head.split("name=\"").nth(1)?.split('"').next()?;
                    Some((
                        name.to_string(),
                        value.strip_suffix("\r\n").unwrap_or(value).to_string(),
                    ))
                })
                .collect()
        }
    }

    type Seen = Arc<Mutex<Vec<Request>>>;

    /// What the stand-in answers to every request.
    #[derive(Clone)]
    enum Answer {
        /// `{"result": {"image": <base64 of the bytes>}, "success": true}`.
        Image(&'static [u8]),
        /// `{"result": {}, "success": true}`: no image.
        NoImage,
        /// The status and the body, as given.
        Error(u16, &'static str),
        /// No answer: the request is held open for an hour.
        Hang,
    }

    /// Answer every request on a loopback port with `answer`, recording it.
    /// Returns a Workers AI `/ai/v1` base on that port and the record.
    async fn stand_in(answer: Answer) -> (String, Seen) {
        stand_in_answering(vec![answer]).await
    }

    /// Answer the n-th request on a loopback port with `answers[n]`, every
    /// request past the last with the last, recording each. Returns a
    /// Workers AI `/ai/v1` base on that port and the record.
    async fn stand_in_answering(answers: Vec<Answer>) -> (String, Seen) {
        assert!(!answers.is_empty(), "the stand-in needs an answer");
        let answers = Arc::new(answers);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let record = record.clone();
                let answers = answers.clone();
                async move {
                    let answer = {
                        let mut record = record.lock().unwrap();
                        let n = record.len();
                        record.push(Request {
                            method,
                            path: uri.path().to_string(),
                            headers,
                            body,
                        });
                        answers[n.min(answers.len() - 1)].clone()
                    };
                    match answer {
                        Answer::Image(bytes) => (
                            StatusCode::OK,
                            serde_json::json!({
                                "result": {
                                    "image": base64::engine::general_purpose::STANDARD.encode(bytes)
                                },
                                "success": true,
                                "errors": [],
                                "messages": []
                            })
                            .to_string(),
                        ),
                        Answer::NoImage => (
                            StatusCode::OK,
                            serde_json::json!({"result": {}, "success": true}).to_string(),
                        ),
                        Answer::Error(status, text) => {
                            (StatusCode::from_u16(status).unwrap(), text.to_string())
                        }
                        Answer::Hang => {
                            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                            (StatusCode::OK, String::new())
                        }
                    }
                }
            },
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve stand-in");
        });
        (
            format!("http://{addr}/client/v4/accounts/acct-1/ai/v1"),
            seen,
        )
    }

    /// The adapter against the stand-in, for `flux-2-dev`, with the
    /// provider's AI Gateway headers and one other header.
    fn adapter(endpoint: &str) -> WorkersAiImageAdapter {
        adapter_for(endpoint, FLUX_2_DEV)
    }

    /// The adapter against the stand-in, for `model`, with the provider's AI
    /// Gateway headers and one other header.
    fn adapter_for(endpoint: &str, model: &str) -> WorkersAiImageAdapter {
        super::log_capture::install();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "cf-aig-gateway-id",
            reqwest::header::HeaderValue::from_static("inference-test"),
        );
        headers.insert(
            "cf-aig-metadata",
            reqwest::header::HeaderValue::from_static("{\"team\":\"test\"}"),
        );
        headers.insert(
            "x-provider-header",
            reqwest::header::HeaderValue::from_static("kept"),
        );
        WorkersAiImageAdapter::new(
            endpoint.to_string(),
            "test-key".to_string(),
            model.to_string(),
            headers,
        )
        .with_retry_waits(Duration::ZERO, Duration::ZERO)
    }

    fn blank() -> Image {
        Image {
            bytes: PNG.to_vec(),
            content_type: "image/png".to_string(),
        }
    }

    use aegis_orchestrator_core::domain::vision::VisionProvider;

    #[tokio::test]
    async fn an_openai_compatible_alias_on_a_workers_ai_endpoint_binds_the_workers_ai_adapter() {
        for endpoint in [WORKERS_AI_ENDPOINT, &format!("{WORKERS_AI_ENDPOINT}/")] {
            let registry = registry(&node("openai-compatible", endpoint));
            let provider = registry
                .image_provider("artist")
                .unwrap_or_else(|| panic!("{endpoint}: no image provider"));
            match provider.edit_image(&blank(), "repaint it", None).await {
                Err(VisionError::Unsupported(message)) => {
                    assert_eq!(message, NOT_WIRED, "{endpoint}: not the Workers AI adapter")
                }
                other => panic!("{endpoint}: not the Workers AI adapter: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn any_other_endpoint_keeps_the_openai_adapter() {
        let cases = [
            ("openai-compatible", "http://127.0.0.1:9/v1"),
            ("openai-compatible", "https://api.openai.com/v1"),
            (
                "openai-compatible",
                "https://gateway.ai.cloudflare.com/v1/acct-1/inference/workers-ai/v1",
            ),
            (
                "openai-compatible",
                "https://api.cloudflare.com/client/v4/accounts/acct-1/ai/v1/other",
            ),
            ("openai", WORKERS_AI_ENDPOINT),
        ];
        for (provider_type, endpoint) in cases {
            let registry = registry(&node(provider_type, endpoint));
            let provider = registry
                .image_provider("artist")
                .unwrap_or_else(|| panic!("{provider_type} {endpoint}: no image provider"));
            match provider.edit_image(&blank(), "repaint it", None).await {
                Err(VisionError::Unsupported(message)) if message == OPENAI_NOT_OFFERED => {}
                other => panic!("{provider_type} {endpoint}: not the OpenAI adapter: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_adapter_posts_a_multipart_form_to_ai_run_and_decodes_result_image() {
        let (endpoint, seen) = stand_in(Answer::Image(PNG)).await;

        let answer = adapter(&endpoint)
            .generate_image(
                "a lighthouse at dusk",
                &ImageOptions {
                    size: Some("1024x768".to_string()),
                    count: 2,
                },
            )
            .await
            .expect("the provider answers");

        assert_eq!(answer.images.len(), 2, "one image per request");
        for image in &answer.images {
            assert_eq!(image.bytes, PNG);
            assert_eq!(image.content_type, "image/png");
        }
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "count 2 is two requests");
        for request in seen.iter() {
            assert_eq!(request.method, Method::POST);
            assert_eq!(
                request.path,
                "/client/v4/accounts/acct-1/ai/run/@cf/black-forest-labs/flux-2-dev"
            );
            assert!(
                request
                    .content_type()
                    .starts_with("multipart/form-data; boundary="),
                "the content type is {}",
                request.content_type()
            );
            assert_eq!(
                request
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer test-key")
            );
            assert_eq!(
                request.fields(),
                vec![
                    ("prompt".to_string(), "a lighthouse at dusk".to_string()),
                    ("width".to_string(), "1024".to_string()),
                    ("height".to_string(), "768".to_string()),
                ]
            );
        }
    }

    #[tokio::test]
    async fn the_run_request_carries_no_ai_gateway_header_and_keeps_the_others() {
        let (endpoint, seen) = stand_in(Answer::Image(PNG)).await;

        adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect("the provider answers");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "count 1 is one request");
        let gateway: Vec<&str> = seen[0]
            .headers
            .keys()
            .map(|name| name.as_str())
            .filter(|name| name.starts_with("cf-aig-"))
            .collect();
        assert!(
            gateway.is_empty(),
            "the /ai/run request carries AI Gateway headers: {gateway:?}"
        );
        assert_eq!(
            seen[0]
                .headers
                .get("x-provider-header")
                .and_then(|v| v.to_str().ok()),
            Some("kept"),
            "the provider's other header is not sent"
        );
        assert_eq!(
            seen[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer test-key")
        );
    }

    #[tokio::test]
    async fn with_no_size_the_form_carries_the_prompt_alone() {
        let (endpoint, seen) = stand_in(Answer::Image(PNG)).await;

        let answer = adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect("the provider answers");

        assert_eq!(answer.images.len(), 1);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "count 1 is one request");
        assert_eq!(
            seen[0].fields(),
            vec![("prompt".to_string(), "a red bicycle".to_string())]
        );
    }

    #[tokio::test]
    async fn the_content_type_is_read_from_the_bytes() {
        let (endpoint, _) = stand_in(Answer::Image(JPEG)).await;

        let answer = adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect("the provider answers");

        assert_eq!(answer.images[0].bytes, JPEG);
        assert_eq!(answer.images[0].content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn an_answer_without_result_image_is_a_provider_error_naming_it() {
        let (endpoint, _) = stand_in(Answer::NoImage).await;

        let error = adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect_err("no image is an error");

        match error {
            VisionError::Provider(message) => assert!(
                message.contains("result.image"),
                "the missing field is not named: {message}"
            ),
            other => panic!("not Provider: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_workers_ai_400_is_invalid_input_with_its_message() {
        let (endpoint, _) = stand_in(Answer::Error(
            400,
            r#"{"errors":[{"message":"width must be at most 1920"}],"success":false}"#,
        ))
        .await;

        let error = adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect_err("the provider refuses");

        match error {
            VisionError::InvalidInput(message) => assert!(
                message.starts_with("HTTP 400 Bad Request: ")
                    && message.contains("width must be at most 1920"),
                "the provider's message is lost: {message}"
            ),
            other => panic!("not InvalidInput: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_workers_ai_500_with_code_7000_is_invalid_input_with_its_message() {
        let (endpoint, _) = stand_in(Answer::Error(
            500,
            r#"{"errors":[{"message":"Invalid request body","code":7000}],"success":false,"result":{},"messages":[]}"#,
        ))
        .await;

        let error = adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect_err("the provider refuses");

        match error {
            VisionError::InvalidInput(message) => assert!(
                message.starts_with("HTTP 500 Internal Server Error: ")
                    && message.contains("Invalid request body"),
                "the provider's message is lost: {message}"
            ),
            other => panic!("not InvalidInput: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_workers_ai_500_with_any_other_code_is_a_provider_error() {
        let (endpoint, _) = stand_in(Answer::Error(
            500,
            r#"{"errors":[{"message":"AiError: AiError: Internal server error","code":3043}],"success":false,"result":{},"messages":[]}"#,
        ))
        .await;

        let error = adapter(&endpoint)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect_err("the provider fails");

        match error {
            VisionError::Provider(message) => assert!(
                message.starts_with("HTTP 500 Internal Server Error: ")
                    && message.contains("Internal server error"),
                "the provider's message is lost: {message}"
            ),
            other => panic!("not Provider: {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_other_status_maps_as_the_openai_adapter_maps_it() {
        for status in [401u16, 403, 429, 500] {
            let (endpoint, _) = stand_in(Answer::Error(status, "refused")).await;
            let error = adapter(&endpoint)
                .generate_image("a red bicycle", &ImageOptions::default())
                .await
                .expect_err("the provider refuses");
            match (status, &error) {
                (401 | 403, VisionError::Authentication(m)) if m == "refused" => {}
                (429, VisionError::RateLimit) => {}
                (500, VisionError::Provider(m)) if m.contains("HTTP 500") => {}
                _ => panic!("HTTP {status} mapped to {error:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_json_model_outside_the_fixed_size_list_is_sent_prompt_width_and_height() {
        let (endpoint, seen) = stand_in(Answer::Image(JPEG)).await;

        let answer = adapter_for(&endpoint, SDXL)
            .generate_image(
                "a lighthouse at dusk",
                &ImageOptions {
                    size: Some("1024x768".to_string()),
                    count: 1,
                },
            )
            .await
            .expect("the provider answers");

        assert_eq!(answer.images.len(), 1);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "count 1 is one request");
        let request = &seen[0];
        assert_eq!(request.method, Method::POST);
        assert_eq!(
            request.path,
            "/client/v4/accounts/acct-1/ai/run/@cf/stabilityai/stable-diffusion-xl-base-1.0"
        );
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer test-key")
        );
        let body = request.json();
        assert_eq!(
            serde_json::Value::Object(body.clone()),
            serde_json::json!({
                "prompt": "a lighthouse at dusk",
                "width": 1024,
                "height": 768,
            })
        );
        assert!(
            !body.contains_key("steps"),
            "steps is sent though none was given"
        );
    }

    #[tokio::test]
    async fn a_flux_1_schnell_request_asked_a_size_carries_the_prompt_alone_and_answers_the_image()
    {
        let (endpoint, seen) = stand_in(Answer::Image(JPEG)).await;

        let answer = adapter_for(&endpoint, FLUX_1_SCHNELL)
            .generate_image(
                "a small grey monkey under a tree",
                &ImageOptions {
                    size: Some("1024x1024".to_string()),
                    count: 1,
                },
            )
            .await
            .expect("the size is accepted and the provider answers");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "count 1 is one request");
        assert_eq!(
            seen[0].path,
            "/client/v4/accounts/acct-1/ai/run/@cf/black-forest-labs/flux-1-schnell"
        );
        let body = seen[0].json();
        assert!(
            !body.contains_key("width") && !body.contains_key("height"),
            "flux-1-schnell is sent a size it refuses: {body:?}"
        );
        assert_eq!(
            serde_json::Value::Object(body),
            serde_json::json!({"prompt": "a small grey monkey under a tree"})
        );
        assert_eq!(answer.images.len(), 1);
        assert_eq!(answer.images[0].bytes, JPEG);
        assert_eq!(answer.images[0].content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn a_flux_1_schnell_request_with_no_size_carries_the_prompt_alone() {
        let (endpoint, seen) = stand_in(Answer::Image(JPEG)).await;

        adapter_for(&endpoint, FLUX_1_SCHNELL)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect("the provider answers");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "count 1 is one request");
        assert_eq!(
            serde_json::Value::Object(seen[0].json()),
            serde_json::json!({"prompt": "a red bicycle"})
        );
    }

    #[tokio::test]
    async fn a_flux_2_dev_request_stays_a_multipart_form() {
        let (endpoint, seen) = stand_in(Answer::Image(PNG)).await;

        adapter_for(&endpoint, FLUX_2_DEV)
            .generate_image(
                "a red bicycle",
                &ImageOptions {
                    size: Some("512x512".to_string()),
                    count: 1,
                },
            )
            .await
            .expect("the provider answers");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "count 1 is one request");
        assert!(
            seen[0]
                .content_type()
                .starts_with("multipart/form-data; boundary="),
            "the content type is {}",
            seen[0].content_type()
        );
        assert_eq!(
            seen[0].fields(),
            vec![
                ("prompt".to_string(), "a red bicycle".to_string()),
                ("width".to_string(), "512".to_string()),
                ("height".to_string(), "512".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn a_json_models_base64_jpeg_answer_is_an_image_jpeg() {
        let (endpoint, seen) = stand_in(Answer::Image(JPEG)).await;

        let answer = adapter_for(&endpoint, FLUX_1_SCHNELL)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect("the provider answers");

        seen.lock().unwrap()[0].json();
        assert_eq!(answer.images.len(), 1);
        assert_eq!(answer.images[0].bytes, JPEG);
        assert_eq!(answer.images[0].content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn a_json_request_carries_no_ai_gateway_header_and_keeps_the_others() {
        let (endpoint, seen) = stand_in(Answer::Image(JPEG)).await;

        adapter_for(&endpoint, FLUX_1_SCHNELL)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect("the provider answers");

        let seen = seen.lock().unwrap();
        seen[0].json();
        let gateway: Vec<&str> = seen[0]
            .headers
            .keys()
            .map(|name| name.as_str())
            .filter(|name| name.starts_with("cf-aig-"))
            .collect();
        assert!(
            gateway.is_empty(),
            "the /ai/run request carries AI Gateway headers: {gateway:?}"
        );
        assert_eq!(
            seen[0]
                .headers
                .get("x-provider-header")
                .and_then(|v| v.to_str().ok()),
            Some("kept"),
            "the provider's other header is not sent"
        );
    }

    #[tokio::test]
    async fn a_json_request_answered_500_with_code_7000_is_invalid_input() {
        let (endpoint, seen) = stand_in(Answer::Error(
            500,
            r#"{"errors":[{"message":"Invalid request body","code":7000}],"success":false,"result":{},"messages":[]}"#,
        ))
        .await;

        let error = adapter_for(&endpoint, FLUX_1_SCHNELL)
            .generate_image("a red bicycle", &ImageOptions::default())
            .await
            .expect_err("the provider refuses");

        seen.lock().unwrap()[0].json();
        match error {
            VisionError::InvalidInput(message) => assert!(
                message.starts_with("HTTP 500 Internal Server Error: ")
                    && message.contains("Invalid request body"),
                "the provider's message is lost: {message}"
            ),
            other => panic!("not InvalidInput: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_size_not_of_the_form_width_x_height_is_refused_before_any_request() {
        let (endpoint, seen) = stand_in(Answer::Image(PNG)).await;

        let error = adapter(&endpoint)
            .generate_image(
                "a red bicycle",
                &ImageOptions {
                    size: Some("large".to_string()),
                    count: 1,
                },
            )
            .await
            .expect_err("the size is refused");

        match error {
            VisionError::InvalidInput(message) => assert_eq!(
                message,
                "size 'large' is not <width>x<height>, for example 1024x1024"
            ),
            other => panic!("not InvalidInput: {other:?}"),
        }
        assert!(seen.lock().unwrap().is_empty(), "a request was sent");
    }

    // --- a capacity refusal, a transient 503, a model that does not answer --

    /// Workers AI's answer to a model at capacity (read 2026-10-12, request
    /// `48d5797c`).
    const CAPACITY: &str = r#"{"errors":[{"message":"AiError: AiError: Capacity temporarily exceeded, please try again. (4db1b541-22cb-47f7-aa3a-7ea39fe97ee8)","code":3040}],"success":false,"result":{},"messages":[]}"#;

    /// Workers AI's transient 503 (read 2026-10-12, request `f4f10968`).
    const UNKNOWN_INTERNAL: &str = r#"{"errors":[{"message":"AiError: AiError: Unknown internal error (03a011fe-1633-48a6-be6a-a7fd92786d3d)","code":3048}],"success":false,"result":{},"messages":[]}"#;

    /// One image of `flux-1-schnell`, bounded so that an adapter that never
    /// answers fails the test rather than hanging it.
    async fn generate(adapter: &WorkersAiImageAdapter) -> Result<Vec<Image>, VisionError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            adapter.generate_image("a grey monkey reading", &ImageOptions::default()),
        )
        .await
        .expect("the adapter did not answer within 10 s")
        .map(|answer| answer.images)
    }

    #[tokio::test]
    async fn a_429_then_a_200_answers_the_image_after_one_retry() {
        let (endpoint, seen) =
            stand_in_answering(vec![Answer::Error(429, CAPACITY), Answer::Image(JPEG)]).await;

        let images = generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
            .await
            .expect("the retry answers the image");

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].bytes, JPEG);
        assert_eq!(seen.lock().unwrap().len(), 2, "one request, one retry");
    }

    #[tokio::test]
    async fn a_429_then_a_429_answers_the_rate_limit_refusal_after_one_retry() {
        let (endpoint, seen) = stand_in_answering(vec![Answer::Error(429, CAPACITY)]).await;

        let error = generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
            .await
            .expect_err("the model stays at capacity");

        assert!(matches!(error, VisionError::RateLimit), "{error:?}");
        assert_eq!(seen.lock().unwrap().len(), 2, "a 429 is retried once");
    }

    #[tokio::test]
    async fn a_503_then_a_200_answers_the_image_after_one_retry() {
        let (endpoint, seen) = stand_in_answering(vec![
            Answer::Error(503, UNKNOWN_INTERNAL),
            Answer::Image(JPEG),
        ])
        .await;

        let images = generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
            .await
            .expect("the retry answers the image");

        assert_eq!(images[0].bytes, JPEG);
        assert_eq!(seen.lock().unwrap().len(), 2, "one request, one retry");
    }

    /// The sequence production saw on 2026-10-12 at 03:42Z: a 429, a 503,
    /// then the image.
    #[tokio::test]
    async fn a_429_then_a_503_then_a_200_answers_the_image_after_two_retries() {
        let (endpoint, seen) = stand_in_answering(vec![
            Answer::Error(429, CAPACITY),
            Answer::Error(503, UNKNOWN_INTERNAL),
            Answer::Image(JPEG),
        ])
        .await;

        let images = generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
            .await
            .expect("the second retry answers the image");

        assert_eq!(images[0].bytes, JPEG);
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_503_then_a_503_answers_the_provider_error_after_one_retry() {
        let (endpoint, seen) = stand_in_answering(vec![Answer::Error(503, UNKNOWN_INTERNAL)]).await;

        let error = generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
            .await
            .expect_err("the model keeps failing");

        match error {
            VisionError::Provider(message) => {
                assert!(message.contains("HTTP 503"), "{message}")
            }
            other => panic!("not Provider: {other:?}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 2, "a 503 is retried once");
    }

    #[tokio::test]
    async fn any_other_failure_is_not_retried() {
        for (status, body) in [
            (400u16, "refused"),
            (401, "refused"),
            (500, UNKNOWN_INTERNAL),
            (502, "bad gateway"),
        ] {
            let (endpoint, seen) = stand_in_answering(vec![Answer::Error(status, body)]).await;
            generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
                .await
                .expect_err("the provider fails");
            assert_eq!(seen.lock().unwrap().len(), 1, "HTTP {status} was retried");
        }
    }

    #[tokio::test]
    async fn a_model_that_does_not_answer_within_the_request_timeout_is_a_network_error_naming_the_time(
    ) {
        let (endpoint, seen) = stand_in_answering(vec![Answer::Hang]).await;
        let adapter = adapter_for(&endpoint, FLUX_1_SCHNELL)
            .with_timeouts(Duration::from_secs(2), Duration::from_millis(300));

        let error = generate(&adapter)
            .await
            .expect_err("the model does not answer");

        match error {
            VisionError::Network(message) => assert!(
                message
                    .starts_with("the image model did not answer: the request timed out after 0.")
                    && message.ends_with(" s"),
                "{message}"
            ),
            other => panic!("not Network: {other:?}"),
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "a timeout is not retried");
    }

    #[tokio::test]
    async fn each_retried_attempt_logs_its_status_and_code_and_a_success_logs_one_info_line_without_the_prompt(
    ) {
        let captured = super::log_capture::Captured::new();
        // Another call on another thread first, as another test's would be:
        // it reaches the WARN callsite before this test's call does, and its
        // lines are not this capture's.
        std::thread::spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the other thread")
                .block_on(async {
                    let (endpoint, _) =
                        stand_in_answering(vec![Answer::Error(429, CAPACITY), Answer::Image(JPEG)])
                            .await;
                    generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
                        .await
                        .expect("the retry answers the image");
                })
        })
        .join()
        .expect("the other thread's call");
        let (endpoint, _) = stand_in_answering(vec![
            Answer::Error(429, CAPACITY),
            Answer::Error(503, UNKNOWN_INTERNAL),
            Answer::Image(JPEG),
        ])
        .await;

        generate(&adapter_for(&endpoint, FLUX_1_SCHNELL))
            .instrument(captured.span())
            .await
            .expect("the second retry answers the image");

        let logs = captured.lines();
        let warns: Vec<&str> = logs.lines().filter(|l| l.starts_with("WARN")).collect();
        assert_eq!(warns.len(), 2, "{logs}");
        for (warn, (attempt, status, code)) in warns.iter().zip([(1, 429, 3040), (2, 503, 3048)]) {
            for field in [
                format!(" attempt={attempt}"),
                format!(" status={status}"),
                format!(" code={code}"),
            ] {
                assert!(warn.contains(&field), "no{field} in: {warn}");
            }
        }
        let infos: Vec<&str> = logs
            .lines()
            .filter(|l| l.starts_with("INFO") && l.contains("image generated"))
            .collect();
        assert_eq!(infos.len(), 1, "{logs}");
        for field in [
            format!(" model={FLUX_1_SCHNELL}"),
            " count=1".to_string(),
            " content_type=image/jpeg".to_string(),
            format!(" bytes={}", JPEG.len()),
            " elapsed_ms=".to_string(),
        ] {
            assert!(infos[0].contains(&field), "no{field} in: {}", infos[0]);
        }
        assert!(
            !logs.contains("grey monkey"),
            "the prompt reached the log: {logs}"
        );
    }
}

// ===========================================================================
// `aegis.image.generate` (AEGIS ADR-143 I2)
// ===========================================================================

mod tool {
    use super::PNG;

    /// The first bytes of a JPEG file, then a few more.
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIFfake-image";
    use aegis_orchestrator_core::application::agent::AgentLifecycleService;
    use aegis_orchestrator_core::application::execution::ExecutionService;
    use aegis_orchestrator_core::application::file_operations_service::FileOperationsService;
    use aegis_orchestrator_core::application::nfs_gateway::{
        NfsVolumeRegistry, VolumeRegistration,
    };
    use aegis_orchestrator_core::application::tool_invocation_service::{
        GeneratedImageFile, ImageGenerateStep, ToolInvocationResult, ToolInvocationService,
    };
    use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
    use aegis_orchestrator_core::application::volume_manager::VolumeService;
    use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};
    use aegis_orchestrator_core::domain::events::{ExecutionEvent, StorageEvent};
    use aegis_orchestrator_core::domain::execution::ExecutionStatus;
    use aegis_orchestrator_core::domain::execution::{
        Execution, ExecutionId, ExecutionInput, Iteration,
    };
    use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher, FsalAccessPolicy};
    use aegis_orchestrator_core::domain::iam::ZaruTier;
    use aegis_orchestrator_core::domain::mcp::ToolInputContract;
    use aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository;
    use aegis_orchestrator_core::domain::repository::{
        AgentVersion, ExecutionRepository, VolumeRepository,
    };
    use aegis_orchestrator_core::domain::runtime::InstanceId;
    use aegis_orchestrator_core::domain::seal_session::{
        EnvelopeVerifier, SealSession, SealSessionError,
    };
    use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
    use aegis_orchestrator_core::domain::secrets::SensitiveString;
    use aegis_orchestrator_core::domain::security_context::capability::Capability;
    use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
    use aegis_orchestrator_core::domain::security_context::{
        SecurityContext, SecurityContextMetadata,
    };
    use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
    use aegis_orchestrator_core::domain::storage::{
        DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    use aegis_orchestrator_core::domain::vision::{
        Image, ImageGenerationModels, ImageOptions, ImageResponse, Mask, VisionError,
        VisionProvider,
    };
    use aegis_orchestrator_core::domain::volume::{
        AccessMode, StorageClass, StorageTierLimits, Volume, VolumeBackend, VolumeMount,
        VolumeOwnership,
    };
    use aegis_orchestrator_core::domain::workflow::WorkflowExecution;
    use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
    use aegis_orchestrator_core::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
        InMemoryWorkflowExecutionRepository,
    };
    use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
    use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
    use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
    use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
    use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
    use anyhow::Result;
    use async_trait::async_trait;
    use futures::Stream;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    const TOOL: &str = "aegis.image.generate";
    const PERSON: &str = "image-person";
    const CONTEXT: &str = "zaru-pro";

    // --- the provider, faked: no network -----------------------------------

    /// The node's image models: `artist` and `sketcher`, each answering
    /// `count` copies of [`PNG`] (of [`JPEG`] when `jpeg`) and recording the
    /// calls.
    #[derive(Default)]
    struct FakeImages {
        calls: Mutex<Vec<(String, String, ImageOptions)>>,
        none: bool,
        jpeg: bool,
        /// Answer every call as the provider's rate limit (HTTP 429).
        rate_limited: bool,
    }

    struct FakeProvider {
        alias: String,
        images: Arc<FakeImages>,
    }

    #[async_trait]
    impl VisionProvider for FakeProvider {
        async fn generate_image(
            &self,
            prompt: &str,
            options: &ImageOptions,
        ) -> Result<ImageResponse, VisionError> {
            self.images.calls.lock().unwrap().push((
                self.alias.clone(),
                prompt.to_string(),
                options.clone(),
            ));
            if self.images.rate_limited {
                return Err(VisionError::RateLimit);
            }
            let (bytes, content_type) = if self.images.jpeg {
                (JPEG, "image/jpeg")
            } else {
                (PNG, "image/png")
            };
            Ok(ImageResponse {
                images: (0..options.count)
                    .map(|_| Image {
                        bytes: bytes.to_vec(),
                        content_type: content_type.to_string(),
                    })
                    .collect(),
            })
        }
        async fn edit_image(
            &self,
            _: &Image,
            _: &str,
            _: Option<&Mask>,
        ) -> Result<ImageResponse, VisionError> {
            Err(VisionError::Unsupported("not exercised".to_string()))
        }
    }

    struct Models(Arc<FakeImages>);

    impl ImageGenerationModels for Models {
        fn image_generation_aliases(&self) -> Vec<String> {
            if self.0.none {
                return Vec::new();
            }
            vec!["artist".to_string(), "sketcher".to_string()]
        }
        fn image_provider(&self, alias: &str) -> Option<Arc<dyn VisionProvider>> {
            self.image_generation_aliases()
                .iter()
                .any(|a| a == alias)
                .then(|| {
                    Arc::new(FakeProvider {
                        alias: alias.to_string(),
                        images: self.0.clone(),
                    }) as Arc<dyn VisionProvider>
                })
        }
    }

    // --- the services the dispatch reads ----------------------------------

    struct Executions(HashMap<ExecutionId, Execution>);

    #[async_trait]
    impl ExecutionService for Executions {
        async fn start_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_execution_with_id(
            &self,
            execution_id: ExecutionId,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            Ok(execution_id)
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_for_tenant(
            &self,
            _: &TenantId,
            id: ExecutionId,
        ) -> Result<Execution> {
            self.get_execution_unscoped(id).await
        }
        async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
            self.0
                .get(&id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("execution not found"))
        }
        async fn get_iterations_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<Vec<Iteration>> {
            anyhow::bail!("not exercised")
        }
        async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
            anyhow::bail!("not exercised")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            anyhow::bail!("not exercised")
        }
        async fn list_executions_for_tenant(
            &self,
            _: &TenantId,
            _: Option<AgentId>,
            _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
            _: usize,
            _: usize,
        ) -> Result<Vec<Execution>> {
            anyhow::bail!("not exercised")
        }
        async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: aegis_orchestrator_core::domain::execution::LlmInteraction,
        ) -> Result<()> {
            Ok(())
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            Ok(())
        }
    }

    struct OneAgent(Agent);

    #[async_trait]
    impl AgentLifecycleService for OneAgent {
        async fn deploy_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentManifest,
            _: bool,
            _: aegis_orchestrator_core::domain::agent::AgentScope,
            _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
        ) -> Result<AgentId> {
            anyhow::bail!("not exercised")
        }
        async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
            Ok(self.0.clone())
        }
        async fn update_agent_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
            _: AgentManifest,
        ) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
            Ok(vec![self.0.clone()])
        }
        async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
            Ok(Some(self.0.id))
        }
        async fn lookup_agent_visible_for_tenant(
            &self,
            _: &TenantId,
            _: &str,
        ) -> Result<Option<AgentId>> {
            Ok(Some(self.0.id))
        }
        async fn lookup_agent_for_tenant_with_version(
            &self,
            _: &TenantId,
            _: &str,
            _: &str,
        ) -> Result<Option<AgentId>> {
            anyhow::bail!("not exercised")
        }
        async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
            Ok(vec![self.0.clone()])
        }
        async fn list_versions_for_tenant(
            &self,
            _: &TenantId,
            _: AgentId,
        ) -> Result<Vec<AgentVersion>> {
            Ok(vec![])
        }
    }

    /// The host directory storage, creating a directory that exists as the
    /// production store does (SeaweedFS's `/dir/` creates each ancestor
    /// whether or not it is there), where `LocalHostStorageProvider` refuses
    /// it: the FSAL creates a file's parent before every file, so a second
    /// file in one directory would be refused here and not in production.
    struct Storage(LocalHostStorageProvider);

    #[async_trait]
    impl StorageProvider for Storage {
        async fn create_directory(&self, path: &str) -> Result<(), StorageError> {
            match self.0.create_directory(path).await {
                Err(StorageError::AlreadyExists(_)) => Ok(()),
                other => other,
            }
        }
        async fn delete_directory(&self, path: &str) -> Result<(), StorageError> {
            self.0.delete_directory(path).await
        }
        async fn set_quota(&self, path: &str, bytes: u64) -> Result<(), StorageError> {
            self.0.set_quota(path, bytes).await
        }
        async fn get_usage(&self, path: &str) -> Result<u64, StorageError> {
            self.0.get_usage(path).await
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            self.0.health_check().await
        }
        async fn open_file(&self, path: &str, mode: OpenMode) -> Result<FileHandle, StorageError> {
            self.0.open_file(path, mode).await
        }
        async fn read_at(
            &self,
            handle: &FileHandle,
            offset: u64,
            length: usize,
        ) -> Result<Vec<u8>, StorageError> {
            self.0.read_at(handle, offset, length).await
        }
        async fn write_at(
            &self,
            handle: &FileHandle,
            offset: u64,
            data: &[u8],
        ) -> Result<usize, StorageError> {
            self.0.write_at(handle, offset, data).await
        }
        async fn close_file(&self, handle: &FileHandle) -> Result<(), StorageError> {
            self.0.close_file(handle).await
        }
        async fn stat(&self, path: &str) -> Result<FileAttributes, StorageError> {
            self.0.stat(path).await
        }
        async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, StorageError> {
            self.0.readdir(path).await
        }
        async fn create_file(&self, path: &str, mode: u32) -> Result<FileHandle, StorageError> {
            self.0.create_file(path, mode).await
        }
        async fn delete_file(&self, path: &str) -> Result<(), StorageError> {
            self.0.delete_file(path).await
        }
        async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
            self.0.rename(from, to).await
        }
    }

    struct NoStorageEvents;

    #[async_trait]
    impl EventPublisher for NoStorageEvents {
        async fn publish_storage_event(&self, _event: StorageEvent) {}
    }

    /// Volumes as the storage layer makes them: host directories under
    /// `/hosts/<id>` in the scratch root the file services serve.
    struct HostVolumes {
        repo: Arc<InMemoryVolumeRepository>,
    }

    #[async_trait]
    impl VolumeService for HostVolumes {
        async fn create_volume(
            &self,
            name: String,
            tenant_id: TenantId,
            storage_class: StorageClass,
            size_limit_mb: u64,
            ownership: VolumeOwnership,
        ) -> anyhow::Result<VolumeId> {
            let id = VolumeId::new();
            let mut volume = Volume::new(
                name,
                tenant_id,
                storage_class,
                VolumeBackend::HostPath {
                    path: PathBuf::from(format!("/hosts/{id}")),
                },
                size_limit_mb * 1024 * 1024,
                ownership,
            )?;
            volume.id = id;
            volume.mark_available()?;
            self.repo.save(&volume).await?;
            Ok(id)
        }
        async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
            self.repo
                .find_by_id(id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }
        async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_tenant(tenant_id).await?)
        }
        async fn list_volumes_by_ownership(
            &self,
            ownership: &VolumeOwnership,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(self.repo.find_by_ownership(ownership).await?)
        }
        async fn attach_volume(
            &self,
            _: VolumeId,
            _: InstanceId,
            _: PathBuf,
            _: AccessMode,
        ) -> anyhow::Result<VolumeMount> {
            unimplemented!()
        }
        async fn detach_volume(&self, _: VolumeId, _: InstanceId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_volume(&self, _: VolumeId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_volume_usage(&self, _: VolumeId) -> anyhow::Result<u64> {
            Ok(0)
        }
        async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn create_volumes_for_execution(
            &self,
            _: ExecutionId,
            _: TenantId,
            _: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
            _: &str,
        ) -> anyhow::Result<Vec<Volume>> {
            Ok(vec![])
        }
        async fn persist_external_volume(
            &self,
            _: VolumeId,
            _: String,
            _: TenantId,
            _: String,
            _: u64,
            _: VolumeOwnership,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A call as `POST /v1/seal/invoke` receives it.
    struct Envelope {
        token: SensitiveString,
        arguments: Value,
    }

    impl EnvelopeVerifier for Envelope {
        fn security_token(&self) -> &SensitiveString {
            &self.token
        }
        fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
            Ok(())
        }
        fn extract_tool_name(&self) -> Option<String> {
            Some(TOOL.to_string())
        }
        fn extract_arguments(&self) -> Option<Value> {
            Some(self.arguments.clone())
        }
        fn replay_nonce(&self) -> String {
            uuid::Uuid::new_v4().to_string()
        }
    }

    fn context() -> SecurityContext {
        SecurityContext {
            name: CONTEXT.to_string(),
            description: "image generation test".to_string(),
            capabilities: vec![Capability {
                tool_pattern: "aegis.*".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
                rate_limit: None,
                max_concurrent: None,
            }],
            deny_list: vec![],
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    fn agent() -> Agent {
        let manifest: AgentManifest = serde_yaml::from_str(
            r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: image-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["aegis.image.generate", "aegis.agent.create", "aegis.agent.update"]
"#,
        )
        .unwrap();
        Agent {
            id: AgentId::new(),
            tenant_id: TenantId::default(),
            scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
            name: manifest.metadata.name.clone(),
            manifest,
            status: AgentStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    struct Fixture {
        service: ToolInvocationService,
        images: Arc<FakeImages>,
        agent_id: AgentId,
        run: ExecutionId,
        run_volume: VolumeId,
        executions: Arc<InMemoryExecutionRepository>,
        files: Arc<FileOperationsService>,
        user_volumes: Arc<UserVolumeService>,
        fsal: Arc<AegisFSAL>,
        token: String,
        /// The workflow execution the agent's run is a step of, when the
        /// fixture is [`workflow_fixture`]'s; in [`fixture`]'s, a workflow
        /// execution with no agent execution and no volume.
        workflow: uuid::Uuid,
        /// The workflow executions' store, holding [`Self::workflow`]'s
        /// record, running.
        workflow_runs: Arc<InMemoryWorkflowExecutionRepository>,
        /// The volumes' store.
        volumes: Arc<InMemoryVolumeRepository>,
        /// The gateway's registry of mounted volumes.
        registry: NfsVolumeRegistry,
        _root: tempfile::TempDir,
    }

    /// A tool service with the node's image models (none when `no_models`),
    /// one agent run on its iteration 1 with a workspace volume at
    /// `/workspace`, and a conversation's session of [`PERSON`] with no
    /// execution record.
    async fn fixture(images: FakeImages, with_models: bool) -> Fixture {
        fixture_with(images, with_models, false).await
    }

    /// As [`fixture`], the agent's run a step of a workflow execution whose
    /// workspace volume, owned by that workflow execution, it mounts at
    /// `/workspace`, as an Agent state's run does.
    async fn workflow_fixture(images: FakeImages, with_models: bool) -> Fixture {
        fixture_with(images, with_models, true).await
    }

    async fn fixture_with(images: FakeImages, with_models: bool, in_workflow: bool) -> Fixture {
        let workflow = uuid::Uuid::new_v4();
        let root = tempfile::tempdir().unwrap();
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(Storage(LocalHostStorageProvider::new(root.path()).unwrap())),
            volumes.clone() as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoStorageEvents),
        ));
        let event_bus = Arc::new(EventBus::new(64));
        let tenant = TenantId::default();

        // Its workspace volume: the run's own, or the workflow execution's.
        let host = HostVolumes {
            repo: volumes.clone(),
        };
        let run_id = ExecutionId::new();
        let run_volume = host
            .create_volume(
                "workspace".to_string(),
                tenant.clone(),
                StorageClass::persistent(),
                64,
                if in_workflow {
                    VolumeOwnership::WorkflowExecution {
                        workflow_execution_id: workflow,
                    }
                } else {
                    VolumeOwnership::Execution {
                        execution_id: run_id,
                    }
                },
            )
            .await
            .unwrap();

        // The agent's run, on its first iteration.
        let agent = agent();
        let agent_id = agent.id;
        let mut run = Execution::new_with_id(
            run_id,
            agent_id,
            ExecutionInput {
                intent: None,
                input: json!({}),
                workspace_volume_id: in_workflow.then_some(run_volume),
                workspace_volume_mount_path: in_workflow.then(|| PathBuf::from("/workspace")),
                workspace_remote_path: None,
                workflow_execution_id: in_workflow.then_some(workflow),
                attachments: Vec::new(),
            },
            5,
            CONTEXT.to_string(),
        );
        run.tenant_id = tenant.clone();
        run.initiating_user_sub = Some(PERSON.to_string());
        run.start();
        run.start_iteration("act".to_string()).unwrap();
        let executions = Arc::new(InMemoryExecutionRepository::new());
        executions.save_for_tenant(&tenant, &run).await.unwrap();

        // The workspace volume, registered as the run's at /workspace.
        let mut volume = volumes.find_by_id(run_volume).await.unwrap().unwrap();
        volume.mark_attached().unwrap();
        volumes.save(&volume).await.unwrap();
        let registry = NfsVolumeRegistry::new();
        registry.register(VolumeRegistration {
            volume_id: run_volume,
            execution_id: run.id,
            workflow_execution_id: in_workflow.then_some(workflow),
            run_repository: None,
            container_uid: 1000,
            container_gid: 1000,
            policy: FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec!["/*".to_string()],
            },
            mount_point: PathBuf::from("/workspace"),
            remote_path: String::new(),
        });

        // The conversation's session: an execution id with no record.
        let sessions = Arc::new(InMemorySealSessionRepository::new());
        let token = format!("token-{}", uuid::Uuid::new_v4());
        let session = SealSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![],
            token.clone(),
            context(),
            tenant.clone(),
        )
        .with_principal_metadata(
            Some(PERSON.to_string()),
            Some(PERSON.to_string()),
            None,
            None,
        );
        sessions.save(session).await.unwrap();
        let contexts = Arc::new(InMemorySecurityContextRepository::new());
        contexts.save(context()).await.unwrap();

        // The workflow execution's record, running.
        let workflow_runs = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let mut workflow_run = WorkflowExecution::new(
            &WorkflowParser::parse_yaml(
                r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: draw
spec:
  initial_state: DRAW
  states:
    DRAW:
      kind: System
      command: "true"
      transitions: []
"#,
            )
            .unwrap(),
            ExecutionId(workflow),
            json!({}),
        );
        workflow_run.tenant_id = tenant.clone();
        workflow_run.initiating_user_sub = Some(PERSON.to_string());
        workflow_runs
            .save_for_tenant(&tenant, &workflow_run)
            .await
            .unwrap();

        let files = Arc::new(FileOperationsService::new(
            fsal.clone(),
            executions.clone() as Arc<dyn ExecutionRepository>,
            Arc::new(InMemoryAgentRepository::new()),
        ));
        let user_volumes = Arc::new(UserVolumeService::new(
            volumes.clone() as Arc<dyn VolumeRepository>,
            Arc::new(HostVolumes {
                repo: volumes.clone(),
            }),
            event_bus.clone(),
            StorageTierLimits::default(),
        ));
        let images = Arc::new(images);
        let mut service = ToolInvocationService::new(
            sessions,
            contexts,
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
            fsal.clone(),
            registry.clone(),
            Arc::new(OneAgent(agent)),
            Arc::new(Executions(HashMap::from([(run.id, run.clone())]))),
            Arc::new(
                aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(),
            ),
            event_bus,
            None,
        )
        .with_execution_repository(executions.clone())
        .with_workflow_execution_repo(workflow_runs.clone())
        .with_file_operations_service(files.clone())
        .with_user_volume_service(user_volumes.clone());
        if with_models {
            service = service.with_image_generation(Arc::new(Models(images.clone())));
        }
        Fixture {
            service,
            images,
            agent_id,
            run: run.id,
            run_volume,
            executions,
            files,
            user_volumes,
            fsal,
            token,
            workflow,
            workflow_runs,
            volumes,
            registry,
            _root: root,
        }
    }

    impl Fixture {
        /// The call inside the agent's run, on its iteration 1.
        async fn in_run(&self, args: Value) -> Result<Value, SealSessionError> {
            match self
                .service
                .invoke_tool_internal(
                    &self.agent_id,
                    self.run,
                    TenantId::default(),
                    1,
                    Vec::new(),
                    TOOL.to_string(),
                    args,
                )
                .await?
            {
                ToolInvocationResult::Direct(value) => Ok(value),
                other => panic!("the call was not answered directly: {other:?}"),
            }
        }

        /// A call of `tool` inside the agent's run, on its iteration 1.
        async fn call_in_run(&self, tool: &str, args: Value) -> Result<Value, SealSessionError> {
            match self
                .service
                .invoke_tool_internal(
                    &self.agent_id,
                    self.run,
                    TenantId::default(),
                    1,
                    Vec::new(),
                    tool.to_string(),
                    args,
                )
                .await?
            {
                ToolInvocationResult::Direct(value) => Ok(value),
                other => panic!("the call was not answered directly: {other:?}"),
            }
        }

        /// The call out of band, from the person's conversation.
        async fn in_chat(&self, args: Value) -> Result<Value, SealSessionError> {
            self.service
                .invoke_tool(&Envelope {
                    token: self.token.clone().into(),
                    arguments: args,
                })
                .await
        }
    }

    /// The `files` of an answer, wherever the invoke result carries it.
    fn files_of(answer: &Value) -> Vec<Value> {
        answer
            .get("files")
            .or_else(|| answer.get("result").and_then(|r| r.get("files")))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| panic!("the answer carries no files: {answer}"))
    }

    fn refusal(result: Result<Value, SealSessionError>) -> String {
        match result {
            Ok(answer) => format!("answered {answer}"),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn aegis_image_generate_is_listed_requiring_prompt_and_ungated() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
        let tools = router.list_tools().await.unwrap();
        let mut wrong = Vec::new();
        match tools.iter().find(|t| t.name == TOOL) {
            Some(tool) => {
                let schema = &tool.input_schema;
                if schema["required"] != json!(["prompt"]) {
                    wrong.push(format!("required: {}", schema["required"]));
                }
                for (property, kind) in [
                    ("prompt", "string"),
                    ("alias", "string"),
                    ("size", "string"),
                    ("count", "integer"),
                ] {
                    if schema["properties"][property]["type"] != kind {
                        wrong.push(format!("{property} is not a {kind}: {schema}"));
                    }
                }
                if schema["properties"]["count"]["minimum"] != 1
                    || schema["properties"]["count"]["maximum"] != 4
                {
                    wrong.push(format!("count is not 1 to 4: {schema}"));
                }
                let size = schema["properties"]["size"]["description"]
                    .as_str()
                    .unwrap_or_default();
                for needle in ["<width>x<height>", "flux-1-schnell", "not sent"] {
                    if !size.contains(needle) {
                        wrong.push(format!("the size description names no {needle}: {size}"));
                    }
                }
            }
            None => wrong.push(format!("{TOOL} is not listed")),
        }
        if ToolInputContract::required_fields(TOOL) != ["prompt"] {
            wrong.push(format!(
                "the input contract requires {:?}",
                ToolInputContract::required_fields(TOOL)
            ));
        }
        if router.requires_approval(TOOL) {
            wrong.push(format!("{TOOL} is gated"));
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Each file is named by its content type (`.jpg` for `image/jpeg`), so
    /// the description names no PNG and names each extension.
    #[tokio::test]
    async fn aegis_image_generate_description_names_each_file_by_its_type_and_no_png() {
        let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
        let tools = router.list_tools().await.unwrap();
        let description = tools
            .iter()
            .find(|t| t.name == TOOL)
            .map(|t| t.description.clone())
            .unwrap_or_default();
        let mut wrong = Vec::new();
        if description.to_lowercase().contains("png file") || description.contains("PNG") {
            wrong.push("the description names a PNG".to_string());
        }
        for needle in ["named by its type", ".jpg", ".png", ".webp"] {
            if !description.contains(needle) {
                wrong.push(format!("the description names no {needle}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}\n{description}");
    }

    #[tokio::test]
    async fn in_a_run_each_image_is_a_file_of_its_volume_and_a_produced_file_of_its_iteration() {
        let f = fixture(FakeImages::default(), true).await;
        let answer = f
            .in_run(json!({"prompt": "a lighthouse", "count": 2}))
            .await
            .expect("the run's call answers");
        let files = files_of(&answer);
        let expected: Vec<Value> = (1..=2)
            .map(|n| {
                json!({
                    "volume_id": f.run_volume.to_string(),
                    "path": format!("/workspace/generated/{n}.png"),
                    "content_type": "image/png",
                    "size_bytes": PNG.len(),
                })
            })
            .collect();
        assert_eq!(files, expected, "the answer's files");
        assert!(
            !answer.to_string().contains("http"),
            "the answer names a URL: {answer}"
        );

        // The bytes are in the run's volume.
        for n in 1..=2 {
            let handle = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(
                f.run,
                f.run_volume,
                "/",
            );
            let policy = FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec![],
            };
            let bytes = f
                .fsal
                .read(&handle, &format!("/generated/{n}.png"), &policy, 0, 1 << 20)
                .await
                .expect("the image is in the run's volume");
            assert_eq!(bytes, PNG, "image {n}'s bytes");
        }

        // Each is a produced file of iteration 1.
        let run = f
            .executions
            .find_by_id_for_tenant(&TenantId::default(), f.run)
            .await
            .unwrap()
            .unwrap();
        let produced: Vec<(String, u64, String)> = run.iterations()[0]
            .produced_files
            .iter()
            .map(|p| (p.path.clone(), p.size_bytes, p.content_type.clone()))
            .collect();
        assert_eq!(
            produced,
            (1..=2)
                .map(|n| (
                    format!("/workspace/generated/{n}.png"),
                    PNG.len() as u64,
                    "image/png".to_string()
                ))
                .collect::<Vec<_>>(),
            "the iteration's produced files"
        );

        // The default alias is the node's first; the prompt reaches it.
        let calls = f.images.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "artist");
        assert_eq!(calls[0].1, "a lighthouse");
        assert_eq!(calls[0].2.count, 2);
    }

    #[tokio::test]
    async fn out_of_band_each_image_is_a_file_under_generated_of_the_persons_attachments() {
        let f = fixture(FakeImages::default(), true).await;
        let answer = f
            .in_chat(json!({"prompt": "a red bicycle", "alias": "sketcher", "size": "512x512"}))
            .await
            .expect("the conversation's call answers");
        let files = files_of(&answer);
        assert_eq!(files.len(), 1, "one image: {answer}");
        let attachments = f
            .user_volumes
            .find_or_provision_chat_attachments(&TenantId::default(), PERSON, &ZaruTier::Free)
            .await
            .unwrap();
        let file = &files[0];
        assert_eq!(file["volume_id"], attachments.to_string());
        assert_eq!(file["content_type"], "image/png");
        assert_eq!(file["size_bytes"], PNG.len());
        let path = file["path"].as_str().expect("a path");
        assert!(path.starts_with("generated/"), "the path is {path}");
        assert!(
            !answer.to_string().contains("http"),
            "the answer names a URL: {answer}"
        );
        let read = f
            .files
            .read_file(&attachments, &TenantId::default(), PERSON, path)
            .await
            .expect("the image is in the person's attachments");
        assert_eq!(read.data, PNG);

        let calls = f.images.calls.lock().unwrap();
        assert_eq!(calls[0].0, "sketcher");
        assert_eq!(calls[0].2.size.as_deref(), Some("512x512"));
        assert_eq!(calls[0].2.count, 1);
    }

    #[tokio::test]
    async fn out_of_band_a_jpeg_is_named_jpg_and_a_png_png() {
        for (jpeg, bytes, content_type, extension) in [
            (true, JPEG, "image/jpeg", "jpg"),
            (false, PNG, "image/png", "png"),
        ] {
            let f = fixture(
                FakeImages {
                    jpeg,
                    ..Default::default()
                },
                true,
            )
            .await;
            let answer = f
                .in_chat(json!({"prompt": "a small grey monkey", "size": "1024x1024"}))
                .await
                .expect("the conversation's call answers");
            let files = files_of(&answer);
            assert_eq!(files.len(), 1, "one image: {answer}");
            let file = &files[0];
            assert_eq!(file["content_type"], content_type);
            let path = file["path"].as_str().expect("a path");
            let batch = path
                .strip_prefix("generated/")
                .and_then(|rest| rest.strip_suffix(&format!("/1.{extension}")))
                .unwrap_or_else(|| {
                    panic!("{content_type} is not saved as generated/<batch>/1.{extension}: {path}")
                });
            assert!(
                uuid::Uuid::parse_str(batch).is_ok(),
                "the batch is not a uuid: {path}"
            );
            let attachments = f
                .user_volumes
                .find_or_provision_chat_attachments(&TenantId::default(), PERSON, &ZaruTier::Free)
                .await
                .unwrap();
            let read = f
                .files
                .read_file(&attachments, &TenantId::default(), PERSON, path)
                .await
                .expect("the image is in the person's attachments");
            assert_eq!(read.data, bytes, "{content_type}'s bytes");
        }
    }

    #[tokio::test]
    async fn in_a_run_a_jpeg_is_a_jpg_file_and_a_jpg_produced_file() {
        let f = fixture(
            FakeImages {
                jpeg: true,
                ..Default::default()
            },
            true,
        )
        .await;
        let answer = f
            .in_run(json!({"prompt": "a lighthouse"}))
            .await
            .expect("the run's call answers");
        assert_eq!(
            files_of(&answer),
            vec![json!({
                "volume_id": f.run_volume.to_string(),
                "path": "/workspace/generated/1.jpg",
                "content_type": "image/jpeg",
                "size_bytes": JPEG.len(),
            })],
            "the answer's files"
        );
        let handle =
            aegis_orchestrator_core::domain::fsal::AegisFileHandle::new(f.run, f.run_volume, "/");
        let policy = FsalAccessPolicy {
            read: vec!["/*".to_string()],
            write: vec![],
        };
        let bytes = f
            .fsal
            .read(&handle, "/generated/1.jpg", &policy, 0, 1 << 20)
            .await
            .expect("the image is in the run's volume as 1.jpg");
        assert_eq!(bytes, JPEG);
        let run = f
            .executions
            .find_by_id_for_tenant(&TenantId::default(), f.run)
            .await
            .unwrap()
            .unwrap();
        let produced: Vec<(String, String)> = run.iterations()[0]
            .produced_files
            .iter()
            .map(|p| (p.path.clone(), p.content_type.clone()))
            .collect();
        assert_eq!(
            produced,
            vec![(
                "/workspace/generated/1.jpg".to_string(),
                "image/jpeg".to_string()
            )],
            "the iteration's produced file"
        );
    }

    #[tokio::test]
    async fn a_node_with_no_image_generation_model_answers_so() {
        for with_models in [false, true] {
            let f = fixture(
                FakeImages {
                    none: true,
                    ..Default::default()
                },
                with_models,
            )
            .await;
            let told = refusal(f.in_run(json!({"prompt": "a cat"})).await);
            assert!(
                told.contains("this node has no image generation model"),
                "with the models attached {with_models}: {told}"
            );
        }
    }

    /// A provider's rate limit reaches the caller in the person's words, in
    /// a run and out of band. The tool error codes have no rate-limit code
    /// of an upstream's own, so the code is the upstream failure's, whose
    /// answer carries `Retry-After`.
    #[tokio::test]
    async fn a_providers_rate_limit_is_answered_the_image_model_is_busy() {
        let f = fixture(
            FakeImages {
                rate_limited: true,
                ..Default::default()
            },
            true,
        )
        .await;
        for (place, result) in [
            ("in a run", f.in_run(json!({"prompt": "a cat"})).await),
            ("out of band", f.in_chat(json!({"prompt": "a cat"})).await),
        ] {
            let error = result.expect_err("the provider is at its rate limit");
            assert!(
                error
                    .to_string()
                    .ends_with("The image model is busy; try again in a moment."),
                "{place}: {error}"
            );
            let refusal = error.refusal();
            assert_eq!(
                (refusal.http_status, refusal.code),
                (503, "UPSTREAM_UNAVAILABLE"),
                "{place}"
            );
        }
    }

    #[tokio::test]
    async fn a_count_outside_1_to_4_and_an_alias_that_is_no_image_model_are_refused() {
        let f = fixture(FakeImages::default(), true).await;
        for count in [0, 5] {
            let told = refusal(f.in_run(json!({"prompt": "a cat", "count": count})).await);
            assert!(told.contains("'count' is 1 to 4"), "count {count}: {told}");
        }
        let told = refusal(f.in_run(json!({"prompt": "a cat", "alias": "smart"})).await);
        assert!(
            told.contains("'smart' is not an image generation model; the image generation models are artist, sketcher"),
            "{told}"
        );
        assert!(
            f.images.calls.lock().unwrap().is_empty(),
            "a refused call reached the provider"
        );
    }

    /// `aegis.agent.create` and `aegis.agent.update` refuse a manifest whose
    /// model is one of the node's image generation aliases, before anything
    /// is deployed, with I1's sentence.
    #[tokio::test]
    async fn agent_create_and_update_refuse_an_image_alias_as_the_agents_model() {
        let f = fixture(FakeImages::default(), true).await;
        let manifest = r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: painter
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    model: "artist"
  task:
    instruction: "Paint something"
"#;
        let sentence = "'artist' generates images; an agent's model must be a language model";
        let mut wrong = Vec::new();
        for tool in ["aegis.agent.create", "aegis.agent.update"] {
            let told = match f
                .call_in_run(tool, json!({"manifest_yaml": manifest}))
                .await
            {
                Ok(answer) => answer.to_string(),
                Err(e) => e.to_string(),
            };
            if !told.contains(sentence) {
                wrong.push(format!("{tool} answered: {told}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    // --- the workflow interpreter's step (AEGIS ADR-143 I5a) --------------

    impl Fixture {
        /// The workflow interpreter's image generation step on the fixture's
        /// workflow execution.
        async fn step(
            &self,
            step: ImageGenerateStep,
        ) -> aegis_orchestrator_core::application::tool_invocation_service::ImageGenerateAnswer
        {
            self.service
                .run_image_generate(&TenantId::default(), self.workflow, &step)
                .await
                .expect("the step answers")
        }

        /// The bytes at `path` of the workflow execution's volume.
        async fn read_in_run(&self, path: &str) -> Vec<u8> {
            let handle = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new_for_workflow(
                self.workflow,
                self.run_volume,
                "/",
            );
            let policy = FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec![],
            };
            self.fsal
                .read(&handle, path, &policy, 0, 1 << 20)
                .await
                .unwrap_or_else(|e| panic!("{path} is not in the run's volume: {e}"))
        }

        /// The produced files of the agent's run, by path.
        async fn produced(&self) -> Vec<(String, u64, String)> {
            let run = self
                .executions
                .find_by_id_for_tenant(&TenantId::default(), self.run)
                .await
                .unwrap()
                .unwrap();
            run.iterations()
                .iter()
                .flat_map(|i| i.produced_files.iter())
                .map(|p| (p.path.clone(), p.size_bytes, p.content_type.clone()))
                .collect()
        }
    }

    fn generated(volume: VolumeId, path: &str) -> GeneratedImageFile {
        GeneratedImageFile {
            volume_id: volume,
            path: path.to_string(),
            content_type: "image/png".to_string(),
            size_bytes: PNG.len() as u64,
        }
    }

    #[tokio::test]
    async fn a_step_writes_its_image_under_generated_of_the_runs_volume_and_answers_its_file() {
        let f = workflow_fixture(FakeImages::default(), true).await;
        let answer = f
            .step(ImageGenerateStep {
                prompt: "a lighthouse at dusk".to_string(),
                ..Default::default()
            })
            .await;
        assert_eq!(answer.sentence, None, "{answer:?}");
        assert_eq!(
            answer.files,
            vec![generated(f.run_volume, "generated/1.png")],
            "the answer's files"
        );
        assert_eq!(f.read_in_run("/generated/1.png").await, PNG);
        assert_eq!(
            f.produced().await,
            vec![(
                "/workspace/generated/1.png".to_string(),
                PNG.len() as u64,
                "image/png".to_string()
            )],
            "the run's produced files"
        );
        let calls = f.images.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (calls[0].0.as_str(), calls[0].1.as_str(), calls[0].2.count),
            ("artist", "a lighthouse at dusk", 1)
        );
    }

    #[tokio::test]
    async fn a_steps_path_is_where_its_images_go() {
        let f = workflow_fixture(FakeImages::default(), true).await;
        let answer = f
            .step(ImageGenerateStep {
                prompt: "a red bicycle".to_string(),
                alias: Some("sketcher".to_string()),
                size: Some("512x512".to_string()),
                count: Some(2),
                path: Some("pictures/bicycles/".to_string()),
            })
            .await;
        assert_eq!(answer.sentence, None, "{answer:?}");
        assert_eq!(
            answer.files,
            vec![
                generated(f.run_volume, "pictures/bicycles/1.png"),
                generated(f.run_volume, "pictures/bicycles/2.png"),
            ],
            "the answer's files"
        );
        for n in 1..=2 {
            assert_eq!(
                f.read_in_run(&format!("/pictures/bicycles/{n}.png")).await,
                PNG
            );
        }
        let calls = f.images.calls.lock().unwrap();
        assert_eq!(calls[0].0, "sketcher");
        assert_eq!(calls[0].2.size.as_deref(), Some("512x512"));
        assert_eq!(calls[0].2.count, 2);
    }

    #[tokio::test]
    async fn a_step_on_a_node_with_no_image_generation_model_answers_so_and_writes_nothing() {
        let f = workflow_fixture(FakeImages::default(), false).await;
        let answer = f
            .step(ImageGenerateStep {
                prompt: "a cat".to_string(),
                ..Default::default()
            })
            .await;
        assert_eq!(
            answer,
            aegis_orchestrator_core::application::tool_invocation_service::ImageGenerateAnswer {
                files: Vec::new(),
                sentence: Some("this node has no image generation model".to_string()),
            }
        );
        assert!(
            f.produced().await.is_empty(),
            "a refused step produced a file"
        );
        assert!(
            f.images.calls.lock().unwrap().is_empty(),
            "a refused step reached the provider"
        );
    }

    // --- a run with no agent execution (AEGIS ADR-143 I5a) ----------------

    impl Fixture {
        /// The volumes the workflow execution owns.
        async fn workflow_volumes(&self) -> Vec<Volume> {
            self.volumes
                .find_by_ownership(&VolumeOwnership::workflow(self.workflow))
                .await
                .unwrap()
        }

        /// The bytes at `path` of `volume`, read as the workflow execution.
        async fn read_as_run(&self, volume: VolumeId, path: &str) -> Vec<u8> {
            let handle = aegis_orchestrator_core::domain::fsal::AegisFileHandle::new_for_workflow(
                self.workflow,
                volume,
                "/",
            );
            let policy = FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec![],
            };
            self.fsal
                .read(&handle, path, &policy, 0, 1 << 20)
                .await
                .unwrap_or_else(|e| panic!("{path} is not in volume {volume}: {e}"))
        }
    }

    #[tokio::test]
    async fn a_step_in_a_run_with_no_agent_execution_writes_into_a_volume_the_run_owns() {
        // The workflow execution has no agent execution and no volume.
        let f = fixture(FakeImages::default(), true).await;
        assert!(f.workflow_volumes().await.is_empty());

        let answer = f
            .step(ImageGenerateStep {
                prompt: "a lighthouse at dusk".to_string(),
                ..Default::default()
            })
            .await;

        assert_eq!(answer.sentence, None, "{answer:?}");
        let owned = f.workflow_volumes().await;
        assert_eq!(owned.len(), 1, "the volumes the run owns: {owned:?}");
        let volume = owned[0].id;
        assert_eq!(
            answer.files,
            vec![generated(volume, "generated/1.png")],
            "the answer's files"
        );
        assert_eq!(f.read_as_run(volume, "/generated/1.png").await, PNG);
        let registered = f
            .registry
            .lookup(volume)
            .expect("the run's volume is registered with the gateway");
        assert_eq!(registered.workflow_execution_id, Some(f.workflow));
        assert!(
            f.produced().await.is_empty(),
            "a file of a run with no agent execution was recorded on another execution"
        );
    }

    #[tokio::test]
    async fn a_step_in_a_run_with_no_agent_execution_writes_into_the_volume_the_run_already_owns() {
        let f = fixture(FakeImages::default(), true).await;
        let host = HostVolumes {
            repo: f.volumes.clone(),
        };
        let existing = host
            .create_volume(
                format!("workspace-{}", f.workflow),
                TenantId::default(),
                StorageClass::ephemeral_hours(24),
                64,
                VolumeOwnership::workflow(f.workflow),
            )
            .await
            .unwrap();
        f.registry.register(VolumeRegistration {
            volume_id: existing,
            execution_id: ExecutionId(f.workflow),
            workflow_execution_id: Some(f.workflow),
            run_repository: None,
            container_uid: 1000,
            container_gid: 1000,
            policy: FsalAccessPolicy {
                read: vec!["/*".to_string()],
                write: vec!["/*".to_string()],
            },
            mount_point: PathBuf::from("/workspace"),
            remote_path: String::new(),
        });

        let first = f
            .step(ImageGenerateStep {
                prompt: "a lighthouse at dusk".to_string(),
                ..Default::default()
            })
            .await;
        let second = f
            .step(ImageGenerateStep {
                prompt: "a lighthouse at dawn".to_string(),
                ..Default::default()
            })
            .await;

        assert_eq!(first.sentence, None, "{first:?}");
        assert_eq!(first.files, vec![generated(existing, "generated/1.png")]);
        assert_eq!(second.sentence, None, "{second:?}");
        assert_eq!(second.files, vec![generated(existing, "generated/2.png")]);
        for n in 1..=2 {
            assert_eq!(
                f.read_as_run(existing, &format!("/generated/{n}.png"))
                    .await,
                PNG
            );
        }
        let owned: Vec<VolumeId> = f.workflow_volumes().await.iter().map(|v| v.id).collect();
        assert_eq!(owned, vec![existing], "the volumes the run owns");
    }

    #[tokio::test]
    async fn a_step_in_a_run_that_has_ended_answers_so_and_writes_nothing() {
        let f = fixture(FakeImages::default(), true).await;
        let mut record = f
            .workflow_runs
            .find_by_id_for_tenant(&TenantId::default(), ExecutionId(f.workflow))
            .await
            .unwrap()
            .unwrap();
        record.status = ExecutionStatus::Completed;
        f.workflow_runs
            .save_for_tenant(&TenantId::default(), &record)
            .await
            .unwrap();

        let answer = f
            .step(ImageGenerateStep {
                prompt: "a cat".to_string(),
                ..Default::default()
            })
            .await;

        assert_eq!(
            answer,
            aegis_orchestrator_core::application::tool_invocation_service::ImageGenerateAnswer {
                files: Vec::new(),
                sentence: Some("this run has ended".to_string()),
            }
        );
        assert!(
            f.workflow_volumes().await.is_empty(),
            "an ended run was made a volume"
        );
        assert!(
            f.images.calls.lock().unwrap().is_empty(),
            "an ended run's step reached the provider"
        );
    }

    #[tokio::test]
    async fn a_step_in_a_run_with_an_agent_execution_writes_into_its_volume_as_a_produced_file() {
        let f = workflow_fixture(FakeImages::default(), true).await;
        let answer = f
            .step(ImageGenerateStep {
                prompt: "a lighthouse at dusk".to_string(),
                ..Default::default()
            })
            .await;
        assert_eq!(answer.sentence, None, "{answer:?}");
        assert_eq!(
            answer.files,
            vec![generated(f.run_volume, "generated/1.png")]
        );
        assert_eq!(
            f.produced().await,
            vec![(
                "/workspace/generated/1.png".to_string(),
                PNG.len() as u64,
                "image/png".to_string()
            )],
            "the agent execution's produced files"
        );
        let owned: Vec<VolumeId> = f.workflow_volumes().await.iter().map(|v| v.id).collect();
        assert_eq!(
            owned,
            vec![f.run_volume],
            "a run with an agent execution was made a volume of its own"
        );
    }
}
