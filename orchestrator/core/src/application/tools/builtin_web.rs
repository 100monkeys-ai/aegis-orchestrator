use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::smcp_session::SmcpSessionError;
use serde_json::Value;
use std::time::Duration;
use tracing::info;

#[derive(Debug, Clone, Default)]
pub enum SearchEngine {
    #[default]
    DuckDuckGo,
    Brave,
    Searx,
}

#[derive(Debug, Clone)]
pub enum TimeRange {
    Day,
    Week,
    Month,
    Year,
    Anytime,
}

pub async fn invoke_web_tool(
    tool_name: &str,
    args: &Value,
    _execution_id: ExecutionId,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    match tool_name {
        "web.search" => invoke_web_search(args).await,
        "web.fetch" => invoke_web_fetch(args).await,
        _ => Err(SmcpSessionError::SignatureVerificationFailed(format!(
            "Unknown web tool: {}",
            tool_name
        ))),
    }
}

async fn invoke_web_search(args: &Value) -> Result<ToolInvocationResult, SmcpSessionError> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as u32;

    // Default to DuckDuckGo for basic HTML scraping search if not specified
    let _engine = SearchEngine::DuckDuckGo;
    let search_url = build_duckduckgo_url(&query, max_results);

    match perform_search(&search_url).await {
        Ok(results) => Ok(ToolInvocationResult::Direct(serde_json::json!({
            "status": "success",
            "query": query,
            "engine": "DuckDuckGo",
            "result_count": results.len(),
            "results": results
        }))),
        Err(e) => Err(SmcpSessionError::SignatureVerificationFailed(format!(
            "Web search failed: {}",
            e
        ))),
    }
}

async fn invoke_web_fetch(args: &Value) -> Result<ToolInvocationResult, SmcpSessionError> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let to_markdown = args
        .get("to_markdown")
        .and_then(|v| v.as_bool())
        .unwrap_or(true); // Default formatting to markdown
    let follow_redirects = args
        .get("follow_redirects")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(SmcpSessionError::SignatureVerificationFailed(format!(
            "Invalid URL: {}. Must start with http:// or https://",
            url
        )));
    }

    match perform_fetch(&url, Duration::from_secs(timeout_secs), follow_redirects).await {
        Ok((status, is_html, body)) => {
            let mut content = body;

            if to_markdown && is_html {
                content = html2md::parse_html(&content);
            }

            Ok(ToolInvocationResult::Direct(serde_json::json!({
                "status": "success",
                "url": url,
                "http_status": status,
                "content_length": content.len(),
                "content": content
            })))
        }
        Err(e) => Err(SmcpSessionError::SignatureVerificationFailed(format!(
            "Web fetch failed: {}",
            e
        ))),
    }
}

fn build_duckduckgo_url(query: &str, _max_results: u32) -> String {
    format!(
        "https://duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    )
}

async fn perform_search(url: &str) -> Result<Vec<serde_json::Value>, String> {
    info!("Searching at: {}", url);

    let client = reqwest::Client::builder()
        .user_agent("AEGIS Orchestrator WebSearch/1.0")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("DuckDuckGo request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("DuckDuckGo returned status: {}", response.status()));
    }

    let html = response
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    let document = scraper::Html::parse_document(&html);
    let result_selector = scraper::Selector::parse(".result__body")
        .map_err(|e| format!("Failed to parse result body selector: {:?}", e))?;
    let title_selector = scraper::Selector::parse(".result__title a")
        .map_err(|e| format!("Failed to parse title selector: {:?}", e))?;
    let snippet_selector = scraper::Selector::parse(".result__snippet")
        .map_err(|e| format!("Failed to parse snippet selector: {:?}", e))?;

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

        // Sometimes DDG wraps links in a redirect proxy, let's keep it simple

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
    info!("Fetching: {}", url);

    let client = reqwest::Client::builder()
        .user_agent("AEGIS Orchestrator WebFetch/1.0")
        .timeout(timeout)
        .redirect(if follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        })
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

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
        .map_err(|e| format!("Failed to read response body: {}", e))?;

    Ok((status, is_html, body))
}
