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

/// Computes the exponential backoff delay (in milliseconds) before the next
/// Brave Search retry given the just-completed `attempt` (1-indexed).
///
/// Uses `saturating_pow` and `saturating_mul` so that an overly large
/// `attempt` value can never overflow — the result simply saturates at
/// `u64::MAX` rather than wrapping or panicking. A bit-shift `1u64 << N`
/// would be undefined behaviour for `N >= 64`; this form is safe for any
/// `u32` input.
pub(super) fn brave_retry_backoff_ms(attempt: u32) -> u64 {
    let exp = 2u64.saturating_pow(attempt.saturating_sub(1));
    BRAVE_RETRY_BACKOFF_MS.saturating_mul(exp)
}

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
                let backoff = brave_retry_backoff_ms(attempt);
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
            // Log details for operators, but surface a generic message to the
            // caller so internal struct field names / response shape don't
            // leak through SealSessionError into LLM-visible output.
            warn!(error = %e, "web.search response parse failed");
            SealSessionError::UpstreamUnavailable("web.search response parse failed".to_string())
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
                let (content, content_format) = if request.to_markdown && is_html {
                    html_to_markdown_or_fallback(&body)
                } else if is_html {
                    (body, "html")
                } else {
                    (body, "text")
                };

                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "status": "success",
                    "url": request.url,
                    "http_status": status,
                    "content_length": content.len(),
                    "content_format": content_format,
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
    let sanitized_url = sanitize_url(url);
    info!("Web fetch requested: {}", sanitized_url);
    debug!("Fetching: {}", sanitized_url);

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

/// Convert HTML to Markdown via `html2md`, falling back to the original HTML
/// when conversion produces empty output from non-empty input.
///
/// `html2md` silently emits an empty string for HTML it cannot meaningfully
/// represent (e.g. pages whose visible content is entirely inside `<script>`
/// or `<style>` blocks). Returning empty content to the caller hides the
/// fact that real bytes were fetched. When this happens we log a warning,
/// preserve the original HTML, and report `content_format: "html"` so
/// downstream consumers can adapt.
///
/// Returns `(content, content_format)` where `content_format` is one of
/// `"markdown"` or `"html"`.
pub(super) fn html_to_markdown_or_fallback(html: &str) -> (String, &'static str) {
    if html.trim().is_empty() {
        return (html.to_string(), "html");
    }
    let markdown = html2md::parse_html(html);
    if markdown.trim().is_empty() {
        warn!("HTML to Markdown conversion produced empty output; returning original HTML content");
        (html.to_string(), "html")
    } else {
        (markdown, "markdown")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test: prior implementation used `1u64 << (attempt - 1)`,
    /// which is undefined behaviour for `attempt - 1 >= 64` and would also
    /// underflow at `attempt == 0`. The current implementation must
    /// saturate gracefully at the upper edge and never panic for any
    /// `u32` input, including 0 and the full 64+ range.
    #[test]
    fn test_brave_retry_backoff_ms_does_not_overflow() {
        // attempt = 1 → base * 2^0 = base
        assert_eq!(brave_retry_backoff_ms(1), BRAVE_RETRY_BACKOFF_MS);
        // attempt = 2 → base * 2
        assert_eq!(brave_retry_backoff_ms(2), BRAVE_RETRY_BACKOFF_MS * 2);
        // attempt = 3 → base * 4
        assert_eq!(brave_retry_backoff_ms(3), BRAVE_RETRY_BACKOFF_MS * 4);

        // attempt = 0 must not panic from underflow on `attempt - 1`.
        // saturating_sub yields 0, so 2^0 = 1, returning the base value.
        assert_eq!(brave_retry_backoff_ms(0), BRAVE_RETRY_BACKOFF_MS);

        // Exercise the full 0..=64 range. The original `1u64 << N` form
        // is UB at N >= 64; this loop must complete without panicking
        // for every value.
        for attempt in 0u32..=64 {
            let _ = brave_retry_backoff_ms(attempt);
        }

        // At attempt = 64, 2^63 multiplied by 250 saturates to u64::MAX.
        assert_eq!(brave_retry_backoff_ms(64), u64::MAX);
        // And well beyond u32::MAX-style edges, still saturated.
        assert_eq!(brave_retry_backoff_ms(u32::MAX), u64::MAX);
    }

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

    // -----------------------------------------------------------------------
    // Regression: a malformed JSON body from Brave MUST surface as a generic
    // UpstreamUnavailable error whose message is exactly
    // "web.search response parse failed" — no serde field names, no byte
    // offsets, no struct shape. Detailed parse errors get logged at warn
    // level for operator debugging.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn web_search_parse_error_does_not_leak_serde_details() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_query(mockito::Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            // Malformed JSON — missing closing brace, wrong types.
            .with_body(r#"{"web": {"results": "not-an-array""#)
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
        assert!(result.is_err(), "malformed JSON must return Err");
        let err = result.unwrap_err();
        assert!(
            matches!(err, SealSessionError::UpstreamUnavailable(_)),
            "parse failure must be UpstreamUnavailable, got: {err:?}"
        );
        let err_str = match &err {
            SealSessionError::UpstreamUnavailable(s) => s.clone(),
            _ => unreachable!(),
        };
        assert_eq!(
            err_str, "web.search response parse failed",
            "error string must be exactly the generic message — no leaked serde details: got {err_str:?}"
        );
        // Defence-in-depth: ensure no common serde detail leaks even if the
        // generic string above were ever modified.
        for needle in [
            "line ",
            "column ",
            "expected",
            "BraveSearchResponse",
            "BraveWebResults",
            "BraveResult",
            "missing field",
            "invalid type",
        ] {
            assert!(
                !err_str.contains(needle),
                "error must not leak serde detail {needle:?}: {err_str}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Regression: html2md silently emits an empty string for some valid HTML
    // (e.g. content entirely inside <script> tags). The fetch path must
    // detect this case and fall back to the original HTML so callers do not
    // receive empty content for pages that returned real bytes.
    // -----------------------------------------------------------------------
    #[test]
    fn web_fetch_html_to_markdown_fallback_on_empty_conversion() {
        // Empty input → empty html (no fallback drama, format is "html").
        let (content, fmt) = html_to_markdown_or_fallback("");
        assert_eq!(content, "");
        assert_eq!(fmt, "html");

        // Whitespace-only input → treated as empty.
        let (content, fmt) = html_to_markdown_or_fallback("   \n  ");
        assert_eq!(fmt, "html", "whitespace-only must short-circuit to html");
        assert!(content.trim().is_empty());

        // Valid HTML with real content → markdown.
        let html = "<h1>Hello</h1><p>world</p>";
        let (content, fmt) = html_to_markdown_or_fallback(html);
        assert_eq!(fmt, "markdown");
        assert!(
            !content.trim().is_empty(),
            "valid HTML must produce non-empty markdown"
        );

        // HTML that html2md collapses to empty (script-only). The fallback
        // must preserve the original HTML and report content_format = "html".
        let script_only = "<script>var x = 1;</script>";
        let html2md_output = html2md::parse_html(script_only);
        // Sanity: confirm html2md actually emits empty for this input on the
        // current dependency version. If this assertion ever fails the test
        // becomes vacuous — adjust the script_only fixture.
        assert!(
            html2md_output.trim().is_empty(),
            "fixture invariant: html2md is expected to produce empty markdown for script-only HTML; got {html2md_output:?}"
        );
        let (content, fmt) = html_to_markdown_or_fallback(script_only);
        assert_eq!(
            fmt, "html",
            "fallback must report html when conversion is empty"
        );
        assert_eq!(
            content, script_only,
            "fallback must preserve the original HTML byte-for-byte"
        );
    }

    // -----------------------------------------------------------------------
    // Regression: the web.fetch JSON response MUST include a content_format
    // field so callers can distinguish HTML from converted Markdown.
    // -----------------------------------------------------------------------
    #[tokio::test]
    async fn web_fetch_response_includes_content_format() {
        use crate::application::ports::{ExternalWebToolPort, WebFetchRequest};

        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/page")
            .with_status(200)
            .with_header("content-type", "text/html; charset=utf-8")
            .with_body("<h1>Title</h1><p>body text</p>")
            .create_async()
            .await;

        let adapter = ReqwestWebToolAdapter::unconfigured();
        let url = format!("{}/page", server.url());
        let result = adapter
            .fetch(WebFetchRequest {
                url: url.clone(),
                timeout_secs: 5,
                follow_redirects: true,
                to_markdown: true,
            })
            .await;
        assert!(result.is_ok(), "fetch must succeed: {:?}", result);
        if let Ok(ToolInvocationResult::Direct(v)) = result {
            assert!(
                v.get("content_format").is_some(),
                "response must include content_format field: {v}"
            );
            assert_eq!(
                v["content_format"], "markdown",
                "HTML page with to_markdown=true must report markdown: {v}"
            );
        }
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
