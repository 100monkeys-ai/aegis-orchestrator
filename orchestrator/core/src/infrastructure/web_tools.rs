// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! External web tool adapter (Path 2: SEAL External).

use crate::application::ports::{ExternalWebToolPort, WebFetchRequest, WebSearchRequest};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::seal_session::SealSessionError;
use async_trait::async_trait;
use std::time::Duration;
use tracing::{debug, info};

use super::log_sanitizer::sanitize_url;

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
                return Err(SealSessionError::SignatureVerificationFailed(
                    "web.search is not configured: add api_key to the web.search entry in spec.builtin_dispatchers (e.g. api_key: \"env:BRAVE_SEARCH_API_KEY\")".into(),
                ))
            }
        };

        let count = request.max_results.min(20);
        let url = format!("{}/res/v1/web/search", self.brave_base_url);

        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .query(&[("q", &request.query), ("count", &count.to_string())])
            .header("Accept", "application/json")
            .header("X-Subscription-Token", &api_key)
            .send()
            .await
            .map_err(|e| {
                SealSessionError::SignatureVerificationFailed(format!(
                    "web.search request failed: {e}"
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(SealSessionError::SignatureVerificationFailed(format!(
                "web.search Brave API returned {status}"
            )));
        }

        let brave_resp: BraveSearchResponse = response.json().await.map_err(|e| {
            SealSessionError::SignatureVerificationFailed(format!(
                "web.search response parse failed: {e}"
            ))
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
            return Err(SealSessionError::SignatureVerificationFailed(format!(
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
            Err(e) => Err(SealSessionError::SignatureVerificationFailed(format!(
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

    #[tokio::test]
    async fn test_search_brave_null_web_returns_empty_ok() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/res/v1/web/search")
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
