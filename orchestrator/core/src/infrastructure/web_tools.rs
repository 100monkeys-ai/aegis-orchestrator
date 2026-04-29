// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! External web tool adapter (Path 2: SEAL External).

use crate::application::ports::{ExternalWebToolPort, WebFetchRequest, WebSearchRequest};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::seal_session::SealSessionError;
use async_trait::async_trait;
use std::time::Duration;
use tracing::{debug, info, warn};

use super::log_sanitizer::sanitize_url;

const MAX_SEARCH_RESULTS: u32 = 20;

/// Maximum number of attempts for a Brave Search request when the upstream
/// returns a transient failure (HTTP 429 / 5xx). The first attempt counts
/// as attempt 1, so total attempts (initial + retries) equals this value.
const BRAVE_MAX_ATTEMPTS: u32 = 3;

/// Base backoff in milliseconds between Brave Search retries. The delay
/// before retry `n` (1-indexed) is `BRAVE_RETRY_BACKOFF_MS * 2^(n-1)`.
const BRAVE_RETRY_BACKOFF_MS: u64 = 250;

pub struct ReqwestWebToolAdapter {
    api_key: Option<String>,
    brave_base_url: String,
}

impl ReqwestWebToolAdapter {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            brave_base_url: "https://api.search.brave.com".to_string(),
        }
    }

    pub fn unconfigured() -> Self {
        Self::new(None)
    }

    #[cfg(test)]
    pub fn with_base_url(api_key: Option<String>, base_url: String) -> Self {
        Self {
            api_key,
            brave_base_url: base_url,
        }
    }
}

#[derive(serde::Deserialize)]
struct BraveSearchResponse {
    web: Option<BraveWebResults>,
}

#[derive(serde::Deserialize)]
struct BraveWebResults {
    results: Vec<BraveResult>,
}

#[derive(serde::Deserialize)]
struct BraveResult {
    title: String,
    url: String,
    description: Option<String>,
}

#[async_trait]
impl ExternalWebToolPort for ReqwestWebToolAdapter {
    async fn search(
        &self,
        request: WebSearchRequest,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let api_key = match &self.api_key {
            Some(k) => k.clone(),
            None => {
                return Err(SealSessionError::ConfigurationError(
                    "web.search is not configured: add api_key to the web.search entry in spec.builtin_dispatchers (e.g. api_key: \"env:BRAVE_SEARCH_API_KEY\")".into(),
                ))
            }
        };

        let count = request.max_results.min(MAX_SEARCH_RESULTS);
        let url = format!("{}/res/v1/web/search", self.brave_base_url);

        let client = reqwest::Client::new();

        // Retry transient upstream failures (HTTP 429, 5xx, transport errors)
        // with exponential backoff. Permanent failures (4xx other than 429)
        // are surfaced immediately. Per ADR-005 a transient external-service
        // error MUST be classified as recoverable so the inner loop can
        // continue (see `classify_seal_error`).
        let mut last_transient_err: Option<SealSessionError> = None;
        let mut response_opt = None;
        for attempt in 1..=BRAVE_MAX_ATTEMPTS {
            let send_result = client
                .get(&url)
                .query(&[("q", &request.query), ("count", &count.to_string())])
                .header("Accept", "application/json")
                .header("X-Subscription-Token", &api_key)
                .send()
                .await;

            match send_result {
                Err(e) => {
                    // Transport-level failure: retryable.
                    last_transient_err = Some(SealSessionError::UpstreamUnavailable(format!(
                        "web.search request failed (attempt {attempt}/{BRAVE_MAX_ATTEMPTS}): {e}"
                    )));
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        response_opt = Some(resp);
                        break;
                    }
                    if status.as_u16() == 429 || status.is_server_error() {
                        // Transient — eligible for retry.
                        last_transient_err = Some(SealSessionError::UpstreamUnavailable(format!(
                            "web.search Brave API returned {status} (attempt {attempt}/{BRAVE_MAX_ATTEMPTS})"
                        )));
                    } else {
                        // Permanent client error (e.g. 401 invalid key, 400
                        // bad request) — do not retry. Surface as
                        // UpstreamUnavailable so the LLM sees the failure
                        // class without it being mislabeled as a SEAL
                        // signature problem.
                        return Err(SealSessionError::UpstreamUnavailable(format!(
                            "web.search Brave API returned {status}"
                        )));
                    }
                }
            }

            if attempt < BRAVE_MAX_ATTEMPTS {
                let backoff = BRAVE_RETRY_BACKOFF_MS.saturating_mul(1u64 << (attempt - 1));
                warn!(
                    attempt,
                    backoff_ms = backoff,
                    "web.search transient failure — retrying"
                );
                tokio::time::sleep(Duration::from_millis(backoff)).await;
            }
        }

        let response = match response_opt {
            Some(r) => r,
            None => {
                return Err(last_transient_err.unwrap_or_else(|| {
                    SealSessionError::UpstreamUnavailable(
                        "web.search exhausted retries with no recorded error".to_string(),
                    )
                }));
            }
        };

        let brave_resp: BraveSearchResponse = response.json().await.map_err(|e| {
            SealSessionError::UpstreamUnavailable(format!("web.search response parse failed: {e}"))
        })?;

        let results: Vec<serde_json::Value> = brave_resp
            .web
            .map(|w| w.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "title": r.title,
                    "url": r.url,
                    "snippet": r.description.unwrap_or_default(),
                    "source": "Brave"
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "status": "success",
            "query": request.query,
            "engine": "Brave",
            "result_count": results.len(),
            "results": results
        })))
    }

    async fn fetch(
        &self,
        request: WebFetchRequest,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        if !request.url.starts_with("http://") && !request.url.starts_with("https://") {
            return Err(SealSessionError::InvalidArguments(format!(
                "Invalid URL: {}. Must start with http:// or https://",
                request.url
            )));
        }

        match perform_fetch(
            &request.url,
            Duration::from_secs(request.timeout_secs),
            request.follow_redirects,
        )
        .await
        {
            Ok((status, is_html, body)) => {
                let mut content = body;

                if request.to_markdown && is_html {
                    content = html2md::parse_html(&content);
                }

                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "status": "success",
                    "url": request.url,
                    "http_status": status,
                    "content_length": content.len(),
                    "content": content
                })))
            }
            Err(e) => Err(SealSessionError::UpstreamUnavailable(format!(
                "Web fetch failed: {e}"
            ))),
        }
    }
}

async fn perform_fetch(
    url: &str,
    timeout: Duration,
    follow_redirects: bool,
) -> Result<(u16, bool, String), String> {
    info!("Web fetch requested");
    debug!("Fetching: {}", sanitize_url(url));

    let client = reqwest::Client::builder()
        .user_agent("AEGIS Orchestrator WebFetch/1.0")
        .timeout(timeout)
        .redirect(if follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = response.status().as_u16();
    let is_html = response
        .headers()
        .get("content-type")
        .and_then(|ct| ct.to_str().ok())
        .map(|ct| ct.contains("text/html"))
        .unwrap_or(false);

    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    Ok((status, is_html, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_search_without_api_key_returns_error_not_empty_results() {
        let adapter = ReqwestWebToolAdapter::unconfigured();
        let result = adapter
            .search(WebSearchRequest {
                query: "rust async".to_string(),
                max_results: 5,
            })
            .await;
        assert!(
            result.is_err(),
            "search without API key must return Err, not Ok with empty results"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("web.search is not configured"),
            "error must guide the operator: {err}"
        );
    }

    #[tokio::test]
    async fn test_search_maps_brave_response_to_results() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"web":{"results":[
                {"title":"Rust Async Book","url":"https://rust-lang.github.io/async-book/","description":"The async book"},
                {"title":"Tokio Docs","url":"https://tokio.rs","description":"Tokio async runtime"}
            ]}}"#,
            )
            .create_async()
            .await;

        let adapter =
            ReqwestWebToolAdapter::with_base_url(Some("test-key".to_string()), server.url());
        let result = adapter
            .search(WebSearchRequest {
                query: "rust async".to_string(),
                max_results: 5,
            })
            .await;
        assert!(result.is_ok(), "expected Ok: {:?}", result);
        if let Ok(ToolInvocationResult::Direct(v)) = result {
            assert_eq!(v["result_count"], 2);
            assert_eq!(v["results"][0]["title"], "Rust Async Book");
            assert_eq!(
                v["results"][0]["url"],
                "https://rust-lang.github.io/async-book/"
            );
            assert_eq!(v["results"][0]["snippet"], "The async book");
            assert_eq!(v["results"][0]["source"], "Brave");
        }
    }

    #[tokio::test]
    async fn test_search_brave_api_error_returns_err() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(401)
            .with_body(r#"{"message":"Invalid API key"}"#)
            .create_async()
            .await;

        let adapter =
            ReqwestWebToolAdapter::with_base_url(Some("bad-key".to_string()), server.url());
        let result = adapter
            .search(WebSearchRequest {
                query: "test".to_string(),
                max_results: 5,
            })
            .await;
        assert!(result.is_err(), "non-2xx must return Err");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("401"), "error must include status code: {err}");
    }

    // -----------------------------------------------------------------------
    // Regression: Bug 1 — Brave HTTP 429 (or any upstream HTTP failure) MUST
    // NOT be reported as a SEAL "Signature verification failed" error.
    // Prior to the fix, web_tools.rs wrapped every upstream failure (transport
    // error, non-2xx, JSON parse error) in
    // `SealSessionError::SignatureVerificationFailed`, which both produced a
    // misleading error message AND caused the inner-loop classifier to mark
    // the failure as Fatal (terminating the inner loop).
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_search_brave_429_is_not_reported_as_signature_failure() {
        let mut server = mockito::Server::new_async().await;
        // 429 returned by every attempt — the retry path should also produce
        // an UpstreamUnavailable error, never a signature-verification one.
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(429)
            .with_body(r#"{"message":"Too Many Requests"}"#)
            .expect_at_least(1)
            .create_async()
            .await;

        let adapter =
            ReqwestWebToolAdapter::with_base_url(Some("test-key".to_string()), server.url());
        let result = adapter
            .search(WebSearchRequest {
                query: "Zion National Park Angels Landing hike".to_string(),
                max_results: 5,
            })
            .await;
        assert!(result.is_err(), "429 must surface as Err");
        let err = result.unwrap_err();
        // Bug 1 assertion: the displayed string MUST NOT contain
        // "Signature verification failed".
        let err_str = err.to_string();
        assert!(
            !err_str.contains("Signature verification"),
            "429 must not be reported as signature verification failure, got: {err_str}"
        );
        // The error variant MUST be UpstreamUnavailable so the inner-loop
        // classifier routes it to Recoverable (not Fatal).
        assert!(
            matches!(err, SealSessionError::UpstreamUnavailable(_)),
            "429 must be UpstreamUnavailable, got: {err:?}"
        );
        assert!(
            err_str.contains("429"),
            "error must include the upstream status: {err_str}"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: 429 from Brave should be retried with backoff before
    // surfacing as an error. If the upstream becomes available on a retry,
    // the call must succeed.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_search_brave_429_is_retried_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        // First call: 429. Second call: 200.
        let mock_429 = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(429)
            .with_body(r#"{"message":"Too Many Requests"}"#)
            .expect(1)
            .create_async()
            .await;
        let mock_ok = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"web":{"results":[{"title":"OK","url":"https://x","description":"d"}]}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let adapter =
            ReqwestWebToolAdapter::with_base_url(Some("test-key".to_string()), server.url());
        let result = adapter
            .search(WebSearchRequest {
                query: "test".to_string(),
                max_results: 5,
            })
            .await;
        assert!(
            result.is_ok(),
            "429 followed by 200 must succeed via retry: {:?}",
            result
        );
        mock_429.assert_async().await;
        mock_ok.assert_async().await;
    }

    // -----------------------------------------------------------------------
    // Regression: a transport-level failure (e.g. unreachable host) MUST
    // also surface as UpstreamUnavailable, never as a signature failure.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn test_search_transport_error_is_upstream_unavailable() {
        // Point at a port that's not listening. reqwest will fail with a
        // connection error after a short timeout.
        let adapter = ReqwestWebToolAdapter::with_base_url(
            Some("test-key".to_string()),
            // Reserved test port range, nothing listening.
            "http://127.0.0.1:1".to_string(),
        );
        let result = adapter
            .search(WebSearchRequest {
                query: "test".to_string(),
                max_results: 5,
            })
            .await;
        assert!(result.is_err(), "transport failure must return Err");
        let err = result.unwrap_err();
        assert!(
            matches!(err, SealSessionError::UpstreamUnavailable(_)),
            "transport failure must be UpstreamUnavailable, got: {err:?}"
        );
        assert!(
            !err.to_string().contains("Signature verification"),
            "transport failure must not mention signature verification: {err}"
        );
    }

    #[tokio::test]
    async fn test_search_brave_null_web_returns_empty_ok() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"web":null}"#)
            .create_async()
            .await;

        let adapter =
            ReqwestWebToolAdapter::with_base_url(Some("test-key".to_string()), server.url());
        let result = adapter
            .search(WebSearchRequest {
                query: "obscure query".to_string(),
                max_results: 5,
            })
            .await;
        assert!(result.is_ok(), "null web field must be Ok: {:?}", result);
        if let Ok(ToolInvocationResult::Direct(v)) = result {
            assert_eq!(v["result_count"], 0);
        }
    }
}
