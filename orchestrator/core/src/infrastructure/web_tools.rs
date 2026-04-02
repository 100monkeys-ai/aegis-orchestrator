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

#[derive(Debug, Clone, Default)]
pub struct ReqwestWebToolAdapter;

impl ReqwestWebToolAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ExternalWebToolPort for ReqwestWebToolAdapter {
    async fn search(
        &self,
        request: WebSearchRequest,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let search_url = build_duckduckgo_url(&request.query, request.max_results);

        match perform_search(&search_url).await {
            Ok(results) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "query": request.query,
                "engine": "DuckDuckGo",
                "result_count": results.len(),
                "results": results
            }))),
            Err(e) => Err(SealSessionError::SignatureVerificationFailed(format!(
                "Web search failed: {e}"
            ))),
        }
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

fn build_duckduckgo_url(query: &str, _max_results: u32) -> String {
    format!(
        "https://duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    )
}

async fn perform_search(url: &str) -> Result<Vec<serde_json::Value>, String> {
    info!("Web search requested");
    debug!("Searching at: {}", sanitize_url(url));

    let client = reqwest::Client::builder()
        .user_agent("AEGIS Orchestrator WebSearch/1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("DuckDuckGo returned status: {}", response.status()));
    }

    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {e}"))?;

    let document = scraper::Html::parse_document(&html);
    let result_selector = scraper::Selector::parse(".result__body")
        .map_err(|e| format!("Failed to parse result body selector: {e:?}"))?;
    let title_selector = scraper::Selector::parse(".result__title a")
        .map_err(|e| format!("Failed to parse title selector: {e:?}"))?;
    let snippet_selector = scraper::Selector::parse(".result__snippet")
        .map_err(|e| format!("Failed to parse snippet selector: {e:?}"))?;

    let mut results = Vec::new();
    for (idx, result) in document.select(&result_selector).enumerate() {
        if idx >= 10 {
            break;
        }

        let title = result
            .select(&title_selector)
            .next()
            .map(|el| el.text().collect::<String>())
            .unwrap_or_default();

        let url = result
            .select(&title_selector)
            .next()
            .and_then(|el| el.value().attr("href"))
            .unwrap_or_default()
            .to_string();

        let snippet = result
            .select(&snippet_selector)
            .next()
            .map(|el| el.text().collect::<String>())
            .unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(serde_json::json!({
                "title": title.trim(),
                "url": url.trim(),
                "snippet": snippet.trim(),
                "source": "DuckDuckGo"
            }));
        }
    }

    Ok(results)
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
