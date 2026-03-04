use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::dispatch::DispatchAction;
use crate::domain::execution::ExecutionId;
use crate::domain::smcp_session::SmcpSessionError;
use serde_json::Value;

pub fn invoke_cmd_run(
    args: &Value,
    _execution_id: ExecutionId,
) -> Result<ToolInvocationResult, SmcpSessionError> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(ToolInvocationResult::DispatchRequired(
        DispatchAction::Exec {
            command,
            args: vec![],
            cwd: "/workspace".to_string(),
            env_additions: std::collections::HashMap::new(),
            timeout_secs: 300,
            max_output_bytes: 1048576,
        },
    ))
}
