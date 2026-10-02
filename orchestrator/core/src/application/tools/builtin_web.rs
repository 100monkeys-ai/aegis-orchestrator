use crate::application::ports::{ExternalWebToolPort, WebFetchRequest, WebSearchRequest};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::seal_session::SealSessionError;
use serde_json::Value;
use std::convert::TryFrom;
use tracing::debug;

const DEFAULT_MAX_RESULTS: u64 = 10;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// The most characters of a page one `web.fetch` returns (AEGIS ADR-124, the
/// measured Update of 2026-10-01: one fetched page of 595,266 characters went
/// whole into the conversation and, with two more, filled a 262,144-token
/// window). A longer page comes back cut, with a notice naming its whole
/// length and the `offset` that reads on. At about 0.28 tokens a character
/// (deepseek-v4-flash on the research requests) this is about 14,000 tokens,
/// so several sources fit in any window the aliases name; a typical article
/// (the evaluation's 13,714-character page) comes back whole. Stated in the
/// tool's schema and description in `infrastructure::tool_router`.
pub const WEB_FETCH_MAX_CHARS: usize = 50_000;

pub async fn invoke_web_tool(
    tool_name: &str,
    args: &Value,
    execution_id: ExecutionId,
    web_port: &dyn ExternalWebToolPort,
) -> Result<ToolInvocationResult, SealSessionError> {
    debug!(
        "invoke_web_tool execution_id={:?}, tool_name={}",
        execution_id, tool_name
    );
    match tool_name {
        "web.search" => {
            let query = match args.get("query").and_then(|v| v.as_str()) {
                Some(q) if !q.trim().is_empty() => q.to_string(),
                _ => {
                    return Err(SealSessionError::InvalidArguments(
                        "Missing or empty 'query' for web.search".to_string(),
                    ))
                }
            };
            let max_results_u64 = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_MAX_RESULTS);
            let max_results = u32::try_from(max_results_u64).map_err(|_| {
                SealSessionError::InvalidArguments(format!(
                    "'max_results' for web.search exceeds the maximum allowed value ({})",
                    u32::MAX
                ))
            })?;

            web_port
                .search(WebSearchRequest { query, max_results })
                .await
        }
        "web.fetch" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(url) if !url.trim().is_empty() => url.to_string(),
                _ => {
                    return Err(SealSessionError::InvalidArguments(
                        "Missing or empty 'url' parameter for web.fetch".to_string(),
                    ))
                }
            };
            let to_markdown = args
                .get("to_markdown")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let follow_redirects = args
                .get("follow_redirects")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let timeout_secs = args
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_SECS);
            let offset = match args.get("offset") {
                None | Some(Value::Null) => 0,
                Some(v) => v
                    .as_u64()
                    .and_then(|o| usize::try_from(o).ok())
                    .ok_or_else(|| {
                        SealSessionError::InvalidArguments(format!(
                            "'offset' for web.fetch must be a non-negative integer number of characters, got {v}"
                        ))
                    })?,
            };

            let result = web_port
                .fetch(WebFetchRequest {
                    url,
                    to_markdown,
                    follow_redirects,
                    timeout_secs,
                })
                .await?;
            bound_fetched_page(result, offset)
        }
        _ => Err(SealSessionError::InvalidArguments(format!(
            "Unknown web tool: {tool_name}"
        ))),
    }
}

/// Return at most `WEB_FETCH_MAX_CHARS` characters of the fetched page's
/// `content`, starting at character `offset`. A page that fits whole at
/// offset 0 comes back exactly as fetched. Otherwise the result carries the
/// part, its `offset`, the page's `total_chars`, `truncated`, and, while
/// more remains, `next_offset` and a `notice` saying how to read on.
fn bound_fetched_page(
    result: ToolInvocationResult,
    offset: usize,
) -> Result<ToolInvocationResult, SealSessionError> {
    let ToolInvocationResult::Direct(mut value) = result else {
        return Ok(result);
    };
    let Some(page) = value.get("content").and_then(|c| c.as_str()) else {
        return Ok(ToolInvocationResult::Direct(value));
    };
    let total_chars = page.chars().count();
    if offset == 0 && total_chars <= WEB_FETCH_MAX_CHARS {
        return Ok(ToolInvocationResult::Direct(value));
    }
    if offset >= total_chars {
        return Err(SealSessionError::InvalidArguments(format!(
            "'offset' {offset} for web.fetch is at or past the end of the page, which is {total_chars} characters long"
        )));
    }

    let end = (offset + WEB_FETCH_MAX_CHARS).min(total_chars);
    let byte_at = |chars: usize| {
        page.char_indices()
            .nth(chars)
            .map_or(page.len(), |(byte, _)| byte)
    };
    let part = page[byte_at(offset)..byte_at(end)].to_string();
    let truncated = end < total_chars;

    value["content_length"] = Value::from(part.len());
    value["content"] = Value::String(part);
    value["offset"] = Value::from(offset);
    value["total_chars"] = Value::from(total_chars);
    value["truncated"] = Value::Bool(truncated);
    if truncated {
        value["next_offset"] = Value::from(end);
        value["notice"] = Value::String(format!(
            "The page is {total_chars} characters long; this result is cut to characters {offset} to {end} (web.fetch returns at most {WEB_FETCH_MAX_CHARS} characters per call). To read on, call web.fetch again with the same url and offset {end}."
        ));
    }
    Ok(ToolInvocationResult::Direct(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    struct MockWebPort;

    #[async_trait]
    impl ExternalWebToolPort for MockWebPort {
        async fn search(
            &self,
            _request: WebSearchRequest,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            panic!("MockWebPort::search must not be called when arg validation fails");
        }

        async fn fetch(
            &self,
            _request: WebFetchRequest,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            panic!("MockWebPort::fetch must not be called when arg validation fails");
        }
    }

    struct RecordingWebPort {
        search_called: Arc<Mutex<bool>>,
        fetch_called: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl ExternalWebToolPort for RecordingWebPort {
        async fn search(
            &self,
            _request: WebSearchRequest,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            *self
                .search_called
                .lock()
                .expect("search_called mutex poisoned") = true;
            Ok(ToolInvocationResult::Direct(json!({"status": "search ok"})))
        }

        async fn fetch(
            &self,
            _request: WebFetchRequest,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            *self
                .fetch_called
                .lock()
                .expect("fetch_called mutex poisoned") = true;
            Ok(ToolInvocationResult::Direct(json!({"status": "fetch ok"})))
        }
    }

    #[tokio::test]
    async fn web_search_missing_query_is_invalid_arguments() {
        let port = MockWebPort;
        let result = invoke_web_tool("web.search", &json!({}), ExecutionId::new(), &port).await;
        assert!(matches!(result, Err(SealSessionError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn web_search_empty_query_is_invalid_arguments() {
        let port = MockWebPort;
        let result = invoke_web_tool(
            "web.search",
            &json!({"query": "   "}),
            ExecutionId::new(),
            &port,
        )
        .await;
        assert!(matches!(result, Err(SealSessionError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn web_fetch_missing_url_is_invalid_arguments() {
        let port = MockWebPort;
        let result = invoke_web_tool("web.fetch", &json!({}), ExecutionId::new(), &port).await;
        assert!(matches!(result, Err(SealSessionError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn web_fetch_empty_url_is_invalid_arguments() {
        let port = MockWebPort;
        let result =
            invoke_web_tool("web.fetch", &json!({"url": ""}), ExecutionId::new(), &port).await;
        assert!(matches!(result, Err(SealSessionError::InvalidArguments(_))));
    }

    #[tokio::test]
    async fn web_search_valid_query_invokes_port_successfully() {
        let search_called = Arc::new(Mutex::new(false));
        let fetch_called = Arc::new(Mutex::new(false));
        let port = RecordingWebPort {
            search_called: Arc::clone(&search_called),
            fetch_called: Arc::clone(&fetch_called),
        };
        let result = invoke_web_tool(
            "web.search",
            &json!({"query": "rust ddd"}),
            ExecutionId::new(),
            &port,
        )
        .await;
        assert!(matches!(result, Ok(ToolInvocationResult::Direct(_))));
        assert!(*search_called.lock().expect("search_called mutex poisoned"));
        assert!(!*fetch_called.lock().expect("fetch_called mutex poisoned"));
    }

    #[tokio::test]
    async fn web_fetch_valid_url_invokes_port_successfully() {
        let search_called = Arc::new(Mutex::new(false));
        let fetch_called = Arc::new(Mutex::new(false));
        let port = RecordingWebPort {
            search_called: Arc::clone(&search_called),
            fetch_called: Arc::clone(&fetch_called),
        };
        let result = invoke_web_tool(
            "web.fetch",
            &json!({"url": "https://example.com"}),
            ExecutionId::new(),
            &port,
        )
        .await;
        assert!(matches!(result, Ok(ToolInvocationResult::Direct(_))));
        assert!(*fetch_called.lock().expect("fetch_called mutex poisoned"));
        assert!(!*search_called.lock().expect("search_called mutex poisoned"));
    }

    #[tokio::test]
    async fn web_search_max_results_exceeding_u32_max_is_invalid_arguments() {
        let port = MockWebPort;
        let result = invoke_web_tool(
            "web.search",
            &json!({"query": "ok", "max_results": 4_294_967_296_u64}),
            ExecutionId::new(),
            &port,
        )
        .await;
        assert!(matches!(result, Err(SealSessionError::InvalidArguments(_))));
    }

    // ── web.fetch's bound (AEGIS ADR-124, the measured Update of 2026-10-01) ──
    //
    // One fetched page of 595,266 characters (R2-1, rfc9110) went whole into
    // the conversation and, with two more, filled a 262,144-token window.

    /// A port that answers every fetch as the reqwest adapter does, with the
    /// given page as its content.
    struct PagePort {
        page: String,
    }

    #[async_trait]
    impl ExternalWebToolPort for PagePort {
        async fn search(
            &self,
            _request: WebSearchRequest,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            panic!("PagePort::search is not used");
        }

        async fn fetch(
            &self,
            request: WebFetchRequest,
        ) -> Result<ToolInvocationResult, SealSessionError> {
            Ok(ToolInvocationResult::Direct(json!({
                "status": "success",
                "url": request.url,
                "http_status": 200,
                "content_length": self.page.len(),
                "content_format": "markdown",
                "content": self.page,
            })))
        }
    }

    /// A page of `chars` characters whose every 10-character block names its
    /// own position, so a wrong slice shows.
    fn numbered_page(chars: usize) -> String {
        let mut page = String::with_capacity(chars + 10);
        while page.len() < chars {
            page.push_str(&format!("{:>9}|", page.len()));
        }
        page.truncate(chars);
        page
    }

    async fn fetch(page: &str, args: Value) -> Result<Value, SealSessionError> {
        let port = PagePort {
            page: page.to_string(),
        };
        let mut args = args;
        args["url"] = json!("https://www.rfc-editor.org/rfc/rfc9110.html");
        match invoke_web_tool("web.fetch", &args, ExecutionId::new(), &port).await? {
            ToolInvocationResult::Direct(v) => Ok(v),
            _ => panic!("web.fetch answers directly"),
        }
    }

    #[tokio::test]
    async fn web_fetch_cuts_a_long_page_to_the_bound_and_says_how_to_read_on() {
        let page = numbered_page(120_000);
        let v = fetch(&page, json!({})).await.expect("the fetch succeeds");
        let content = v["content"].as_str().expect("content is a string");
        assert_eq!(
            content.chars().count(),
            50_000,
            "the content is cut to the bound of 50,000 characters"
        );
        assert_eq!(content, &page[..50_000], "the first part of the page");
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["total_chars"], json!(120_000));
        assert_eq!(v["offset"], json!(0));
        assert_eq!(v["next_offset"], json!(50_000));
        let notice = v["notice"].as_str().expect("a cut page carries a notice");
        assert!(
            notice.contains("120000") && notice.contains("offset") && notice.contains("50000"),
            "the notice names the page's whole length and how to read on: {notice}"
        );
    }

    #[tokio::test]
    async fn web_fetch_offset_reads_the_next_part() {
        let page = numbered_page(120_000);
        let v = fetch(&page, json!({"offset": 50_000}))
            .await
            .expect("the fetch succeeds");
        assert_eq!(v["content"].as_str().unwrap(), &page[50_000..100_000]);
        assert_eq!(v["offset"], json!(50_000));
        assert_eq!(v["next_offset"], json!(100_000));
        assert_eq!(v["truncated"], json!(true));

        let last = fetch(&page, json!({"offset": 100_000}))
            .await
            .expect("the fetch succeeds");
        assert_eq!(last["content"].as_str().unwrap(), &page[100_000..]);
        assert_eq!(last["truncated"], json!(false));
        assert_eq!(last["total_chars"], json!(120_000));
        assert!(
            last.get("next_offset").is_none(),
            "the last part names no next offset: {last}"
        );
    }

    #[tokio::test]
    async fn web_fetch_leaves_a_short_page_unchanged() {
        let page = numbered_page(13_714);
        let v = fetch(&page, json!({})).await.expect("the fetch succeeds");
        assert_eq!(
            v,
            json!({
                "status": "success",
                "url": "https://www.rfc-editor.org/rfc/rfc9110.html",
                "http_status": 200,
                "content_length": 13_714,
                "content_format": "markdown",
                "content": page,
            }),
            "a page within the bound comes back exactly as fetched"
        );
    }

    #[tokio::test]
    async fn web_fetch_cuts_on_characters_not_bytes() {
        let page = "é".repeat(60_000);
        let v = fetch(&page, json!({})).await.expect("the fetch succeeds");
        assert_eq!(v["content"].as_str().unwrap(), "é".repeat(50_000));
        assert_eq!(v["total_chars"], json!(60_000));
        assert_eq!(v["next_offset"], json!(50_000));
    }

    #[tokio::test]
    async fn web_fetch_refuses_an_offset_that_is_not_a_non_negative_integer() {
        for offset in [json!(-1), json!("10"), json!(1.5)] {
            let res = fetch(&numbered_page(10), json!({"offset": offset})).await;
            assert!(
                matches!(&res, Err(SealSessionError::InvalidArguments(m)) if m.contains("offset")),
                "offset {offset} must be refused, got {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn web_fetch_refuses_an_offset_past_the_end_naming_the_length() {
        let res = fetch(&numbered_page(1_000), json!({"offset": 1_000})).await;
        assert!(
            matches!(&res, Err(SealSessionError::InvalidArguments(m)) if m.contains("1000")),
            "an offset at or past the end must be refused naming the page's length, got {res:?}"
        );
    }
}
