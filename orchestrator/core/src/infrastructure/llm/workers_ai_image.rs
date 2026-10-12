// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Workers AI image adapter (AEGIS ADR-143 Update I1a, on ADR-046 D1)
//!
//! Implements [`VisionProvider`] for an image model served by Cloudflare
//! Workers AI. Workers AI's OpenAI-compatible API (`/ai/v1`) serves chat
//! completions, embeddings and responses only; an image model is run at
//! `/ai/run/<model>` and answers its image as base64 in `result.image`. Its
//! request is a JSON body for every model but those `MULTIPART_MODELS`
//! names, whose schema takes a multipart form. So an `openai-compatible`
//! image generation alias whose endpoint is a Workers AI `/ai/v1` base
//! ([`WorkersAiImageAdapter::serves`]) is bound to this adapter, and every
//! other endpoint keeps
//! [`super::openai::OpenAIAdapter`]'s `/images/generations`.
//!
//! A model `FIXED_SIZE_MODELS` names takes no size field and refuses a
//! request carrying one, so a size asked of it is accepted and not sent, and
//! the image comes at the model's own size; every other model is sent the
//! size asked.
//!
//! The provider's configured `headers` (ADR-124 D3) are sent beside the
//! bearer `Authorization`, as the OpenAI adapter sends them, but for the AI
//! Gateway's `cf-aig-*` headers, which [`WorkersAiImageAdapter::new`] drops.
//!
//! A model at capacity (HTTP 429) and a transient internal error (HTTP 503)
//! are each retried once, after one second, then two; every non-2xx is logged at
//! WARN with its attempt, status and error code, and a generation that
//! answers its images is logged once at INFO with the model, the count, the
//! content type, the bytes and the elapsed milliseconds, never the prompt.
//! The client is bounded by 10 s to connect and 120 s a request; a
//! request that outlasts either is a [`VisionError::Network`] naming the
//! time it waited.

use super::openai::image_content_type;
use crate::domain::secrets::SensitiveUrl;
use crate::domain::vision::{
    Image, ImageOptions, ImageResponse, Mask, VisionError, VisionProvider,
};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName};
use std::time::{Duration, Instant};

/// The host of the Cloudflare API, which serves Workers AI.
const WORKERS_AI_HOST: &str = "api.cloudflare.com";

/// The end of the path of a Workers AI OpenAI-compatible base.
const OPENAI_COMPATIBLE_SUFFIX: &str = "/ai/v1";

/// The Workers AI image models whose input schema is a multipart form
/// (`flux-2-dev`: "multipart object required"). Every other image model's
/// schema is a JSON object (`flux-1-schnell`: `prompt` required, `steps` at
/// most 8), and such a model refuses a multipart request with HTTP 400. The
/// form is chosen by the model's name, never by the alias that names it.
const MULTIPART_MODELS: &[&str] = &["@cf/black-forest-labs/flux-2-dev"];

/// The Workers AI image models whose input schema takes no size field
/// (`flux-1-schnell`: `prompt` required, `steps` at most 8, no `width` or
/// `height`). Such a model refuses a body carrying `width` or `height` with
/// HTTP 400 code 5006 ("Additional or unevaluated properties '/height,
/// /width' at '/' not allowed"), so a size asked of it is accepted and not
/// sent, and the image comes at the model's own size. Chosen by the model's
/// name, as [`MULTIPART_MODELS`] chooses the form.
const FIXED_SIZE_MODELS: &[&str] = &["@cf/black-forest-labs/flux-1-schnell"];

/// The prefix of every AI Gateway request header.
const AI_GATEWAY_HEADER_PREFIX: &str = "cf-aig-";

/// The Workers AI error code of a request body it refuses before the model
/// runs ("Invalid request body"), answered with HTTP 500.
const INVALID_REQUEST_BODY: u64 = 7000;

/// The bound on connecting to Workers AI.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The bound on one request, from its sending to the last byte of its answer.
/// `flux-1-schnell` answered a 1024x1024 image in 2.0 s and `flux-2-dev`
/// failed after 11 to 31 s by the direct requests of 2026-10-11; 120 s holds
/// the slowest with room, and a request that outlasts it is a network error.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The waits before the first and the second retry. A model at capacity
/// (HTTP 429, code 3040) and a transient internal error (HTTP 503, code
/// 3048) are each retried once; on 2026-10-12 a 429 then a 503 were followed
/// by an image on the third call, about six seconds apart.
const RETRY_WAITS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

/// A client bounded by `connect` and `request`. Building fails only where
/// `reqwest::Client::new` panics, when the TLS backend cannot start.
fn client(connect: Duration, request: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(request)
        .build()
        .expect("the TLS backend starts")
}

pub struct WorkersAiImageAdapter {
    client: reqwest::Client,
    /// The provider's endpoint, a Workers AI `/ai/v1` base.
    endpoint: String,
    api_key: String,
    /// The Workers AI model name, for example
    /// `@cf/black-forest-labs/flux-2-dev`.
    model: String,
    /// The provider's configured request headers but the AI Gateway's, sent
    /// on every request beside `Authorization`.
    headers: HeaderMap,
    /// The waits before the first and the second retry.
    retry_waits: [Duration; 2],
}

impl WorkersAiImageAdapter {
    /// Every header whose name starts with `cf-aig-` is dropped from
    /// `headers`, because the AI Gateway's OpenAI-compatible route does not
    /// carry an `/ai/run` request, and Workers AI answers such a
    /// request that names a gateway with code 7000, "Invalid request body",
    /// before the model runs.
    pub fn new(endpoint: String, api_key: String, model: String, mut headers: HeaderMap) -> Self {
        let gateway: Vec<HeaderName> = headers
            .keys()
            .filter(|name| name.as_str().starts_with(AI_GATEWAY_HEADER_PREFIX))
            .cloned()
            .collect();
        for name in gateway {
            headers.remove(&name);
        }
        Self {
            client: client(CONNECT_TIMEOUT, REQUEST_TIMEOUT),
            endpoint,
            api_key,
            model,
            headers,
            retry_waits: RETRY_WAITS,
        }
    }

    /// The adapter with its client bounded by `connect` to connect and by
    /// `request` for a whole request, in place of 10 s and 120 s.
    pub fn with_timeouts(mut self, connect: Duration, request: Duration) -> Self {
        self.client = client(connect, request);
        self
    }

    /// The adapter waiting `first` before its first retry and `second`
    /// before its second, in place of one second and two.
    pub fn with_retry_waits(mut self, first: Duration, second: Duration) -> Self {
        self.retry_waits = [first, second];
        self
    }

    /// Whether `endpoint` is a Workers AI OpenAI-compatible base: host
    /// `api.cloudflare.com`, path ending `/ai/v1` (a trailing `/` aside), as
    /// `https://api.cloudflare.com/client/v4/accounts/<account>/ai/v1`.
    pub fn serves(endpoint: &str) -> bool {
        let Ok(url) = reqwest::Url::parse(endpoint) else {
            return false;
        };
        url.host_str() == Some(WORKERS_AI_HOST)
            && url
                .path()
                .trim_end_matches('/')
                .ends_with(OPENAI_COMPATIBLE_SUFFIX)
    }

    /// `<endpoint with its trailing /v1 removed>/run/<model>`.
    fn run_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        format!("{base}/run/{}", self.model)
    }

    /// Whether the model's schema takes a multipart form rather than JSON.
    fn takes_multipart(&self) -> bool {
        MULTIPART_MODELS.contains(&self.model.as_str())
    }

    /// Whether the model's schema takes a size: every model but those
    /// [`FIXED_SIZE_MODELS`] names.
    fn takes_size(&self) -> bool {
        !FIXED_SIZE_MODELS.contains(&self.model.as_str())
    }

    /// One request: one image. A multipart form of `prompt`, `width` and
    /// `height` for a model [`MULTIPART_MODELS`] names; for every other, a
    /// JSON body `{"prompt", "width"?, "height"?}`, the two sizes as numbers.
    /// `dimensions` is `None` for a model [`FIXED_SIZE_MODELS`] names, so its
    /// request carries the prompt alone. No `steps` is sent: [`ImageOptions`]
    /// carries none, so the model's default holds.
    ///
    /// A 429 and a 503 are each retried once, after [`RETRY_WAITS`]' first
    /// wait for the first retry and its second for the second; the last
    /// failure is the answer. Every non-2xx is logged at WARN with its
    /// attempt, status and error code.
    async fn generate_one(
        &self,
        url: &str,
        prompt: &str,
        dimensions: Option<(u32, u32)>,
    ) -> Result<Image, VisionError> {
        let mut retried_429 = false;
        let mut retried_503 = false;
        let mut attempt: u8 = 1;
        loop {
            let failure = match self.attempt(url, prompt, dimensions, attempt).await {
                Ok(image) => return Ok(image),
                Err(failure) => failure,
            };
            let retry = match failure.status {
                Some(429) if !retried_429 => {
                    retried_429 = true;
                    true
                }
                Some(503) if !retried_503 => {
                    retried_503 = true;
                    true
                }
                _ => false,
            };
            if !retry {
                return Err(failure.error);
            }
            let wait = self.retry_waits[usize::from(attempt - 1).min(1)];
            tracing::info!(
                provider = "workers-ai",
                model = %self.model,
                attempt,
                wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                "image generation retrying"
            );
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// One request, answered with its image, or with its failure and the
    /// HTTP status it carried, if any.
    async fn attempt(
        &self,
        url: &str,
        prompt: &str,
        dimensions: Option<(u32, u32)>,
        attempt: u8,
    ) -> Result<Image, Failure> {
        let request = self
            .client
            .post(url)
            .headers(self.headers.clone())
            .header("Authorization", format!("Bearer {}", self.api_key));
        let request = if self.takes_multipart() {
            let mut form = reqwest::multipart::Form::new().text("prompt", prompt.to_string());
            if let Some((width, height)) = dimensions {
                form = form
                    .text("width", width.to_string())
                    .text("height", height.to_string());
            }
            request.multipart(form)
        } else {
            let mut body = serde_json::Map::new();
            body.insert("prompt".to_string(), prompt.into());
            if let Some((width, height)) = dimensions {
                body.insert("width".to_string(), width.into());
                body.insert("height".to_string(), height.into());
            }
            request.json(&body)
        };
        let started = Instant::now();
        let response = request
            .send()
            .await
            .map_err(|e| Failure::network(e, started.elapsed()))?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let error_text = response.text().await.unwrap_or_default();
            let excerpt: String = error_text.chars().take(512).collect();
            let code = first_error_code(&error_text);
            tracing::warn!(
                provider = "workers-ai",
                model = %self.model,
                attempt,
                status = status.as_u16(),
                code = %code.map_or_else(|| "none".to_string(), |c| c.to_string()),
                retry_after = %retry_after.as_deref().unwrap_or("none"),
                body_excerpt = %excerpt,
                "image generation upstream non-2xx"
            );
            let error = match status.as_u16() {
                400 => VisionError::InvalidInput(format!("HTTP {status}: {excerpt}")),
                500 if code == Some(INVALID_REQUEST_BODY) => {
                    VisionError::InvalidInput(format!("HTTP {status}: {excerpt}"))
                }
                401 | 403 => VisionError::Authentication(excerpt),
                429 => VisionError::RateLimit,
                _ => VisionError::Provider(format!("HTTP {status}: {excerpt}")),
            };
            return Err(Failure {
                status: Some(status.as_u16()),
                error,
            });
        }

        let answer: serde_json::Value = response.json().await.map_err(|e| {
            if e.is_timeout() {
                Failure::network(e, started.elapsed())
            } else {
                Failure::from(VisionError::Provider(format!(
                    "the Workers AI answer did not parse: {e}"
                )))
            }
        })?;
        let Some(b64) = answer
            .get("result")
            .and_then(|result| result.get("image"))
            .and_then(serde_json::Value::as_str)
        else {
            return Err(Failure::from(VisionError::Provider(
                "the Workers AI answer carries no result.image".to_string(),
            )));
        };
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| {
                Failure::from(VisionError::Provider(format!(
                    "the answer's result.image is not base64: {e}"
                )))
            })?;
        Ok(Image {
            content_type: image_content_type(&bytes),
            bytes,
        })
    }
}

/// A failed request: its error, and the HTTP status it was answered with,
/// if it was answered.
struct Failure {
    status: Option<u16>,
    error: VisionError,
}

impl From<VisionError> for Failure {
    fn from(error: VisionError) -> Self {
        Self {
            status: None,
            error,
        }
    }
}

impl Failure {
    /// A request that did not reach Workers AI or whose answer did not
    /// arrive; a timeout names the time it waited.
    fn network(e: reqwest::Error, elapsed: Duration) -> Self {
        let message = if e.is_timeout() {
            format!(
                "the image model did not answer: the request timed out after {:.1} s",
                elapsed.as_secs_f64()
            )
        } else {
            e.to_string()
        };
        Self::from(VisionError::Network(message))
    }
}

/// The first `errors[].code` of a Workers AI error body, when the body is
/// JSON and carries one.
fn first_error_code(body: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("errors")?
        .get(0)?
        .get("code")?
        .as_u64()
}

/// `<width>x<height>`, each a whole number of digits only, as its two parts.
fn dimensions(size: &str) -> Option<(u32, u32)> {
    let (width, height) = size.split_once('x')?;
    let whole = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    Some((whole(width)?, whole(height)?))
}

#[async_trait]
impl VisionProvider for WorkersAiImageAdapter {
    /// One request per image, in sequence: a Workers AI image model answers
    /// one image a run. A size not of the form `<width>x<height>` is refused
    /// for every model; a well-formed size is sent to every model but those
    /// `FIXED_SIZE_MODELS` names, which answer at their own size.
    async fn generate_image(
        &self,
        prompt: &str,
        options: &ImageOptions,
    ) -> Result<ImageResponse, VisionError> {
        let dimensions = match options.size.as_deref() {
            None => None,
            Some(size) => Some(dimensions(size).ok_or_else(|| {
                VisionError::InvalidInput(format!(
                    "size '{size}' is not <width>x<height>, for example 1024x1024"
                ))
            })?),
        };
        let dimensions = dimensions.filter(|_| self.takes_size());
        let url = self.run_url();
        let count = options.count.max(1);
        tracing::debug!(
            provider = "workers-ai",
            model = %self.model,
            endpoint_url = %SensitiveUrl::new(url.as_str()),
            count,
            "image generation HTTP request"
        );
        let started = Instant::now();
        let mut images = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            images.push(self.generate_one(&url, prompt, dimensions).await?);
        }
        let mut content_types: Vec<&str> = images.iter().map(|i| i.content_type.as_str()).collect();
        content_types.dedup();
        tracing::info!(
            provider = "workers-ai",
            model = %self.model,
            count = images.len(),
            content_type = %content_types.join(","),
            bytes = images.iter().map(|i| i.bytes.len()).sum::<usize>(),
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "image generated"
        );
        Ok(ImageResponse { images })
    }

    async fn edit_image(
        &self,
        _image: &Image,
        _prompt: &str,
        _mask: Option<&Mask>,
    ) -> Result<ImageResponse, VisionError> {
        Err(VisionError::Unsupported(
            "Workers AI image editing is not wired".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_a_workers_ai_openai_compatible_base_only() {
        for endpoint in [
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1",
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/",
        ] {
            assert!(WorkersAiImageAdapter::serves(endpoint), "{endpoint}");
        }
        for endpoint in [
            "https://api.openai.com/v1",
            "http://127.0.0.1:9/v1",
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/other",
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/run",
            "https://gateway.ai.cloudflare.com/v1/acct/gw/workers-ai/v1",
            "https://api.cloudflare.com.example.com/client/v4/accounts/acct/ai/v1",
            "not a url",
            "",
        ] {
            assert!(!WorkersAiImageAdapter::serves(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn the_run_url_replaces_the_trailing_v1_with_run_and_the_model() {
        for endpoint in [
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1",
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/",
        ] {
            let adapter = WorkersAiImageAdapter::new(
                endpoint.to_string(),
                String::new(),
                "@cf/black-forest-labs/flux-2-dev".to_string(),
                HeaderMap::new(),
            );
            assert_eq!(
                adapter.run_url(),
                "https://api.cloudflare.com/client/v4/accounts/acct/ai/run/@cf/black-forest-labs/flux-2-dev"
            );
        }
    }

    #[test]
    fn a_size_names_width_and_height() {
        assert_eq!(dimensions("1024x768"), Some((1024, 768)));
        for size in [
            "large",
            "1024",
            "x768",
            "1024x",
            "1024x768x2",
            "-1x5",
            "10 x 10",
            "+5x5",
            "99999999999x5",
        ] {
            assert_eq!(dimensions(size), None, "{size}");
        }
    }
}
