use crate::application::ports::{ExternalWebToolPort, WebFetchRequest, WebSearchRequest};
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::smcp_session::SmcpSessionError;
use serde_json::Value;

pub async fn invoke_web_tool(
    tool_name: &str,
    args: &Value,
    _execution_id: ExecutionId,
    web_port: &dyn ExternalWebToolPort,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    match tool_name {
        "web.search" => {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as u32;

            web_port
                .search(WebSearchRequest { query, max_results })
                .await
        }
        "web.fetch" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
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
        _ => Err(SmcpSessionError::SignatureVerificationFailed(format!(
            "Unknown web tool: {tool_name}"
        ))),
    }
}
