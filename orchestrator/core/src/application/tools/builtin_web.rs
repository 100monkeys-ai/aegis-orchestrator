use crate::application::ports::{ExternalWebToolPort, WebFetchRequest, WebSearchRequest};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::seal_session::SealSessionError;
use serde_json::Value;
use tracing::debug;

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
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as u32;

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
                .unwrap_or(30);

            web_port
                .fetch(WebFetchRequest {
                    url,
                    to_markdown,
                    follow_redirects,
                    timeout_secs,
                })
                .await
        }
        _ => Err(SealSessionError::InvalidArguments(format!(
            "Unknown web tool: {tool_name}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

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
}
