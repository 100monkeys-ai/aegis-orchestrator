pub mod builtin_dispatch;
pub mod builtin_fsal;
pub mod builtin_web;

use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::AegisFSAL;
use crate::domain::smcp_session::SmcpSessionError;
use serde_json::Value;
use std::sync::Arc;

pub enum BuiltinToolResult {
    Handled(ToolInvocationResult),
    NotBuiltin,
}

pub async fn try_invoke_builtin(
    tool_name: &str,
    args: &Value,
    execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    volume_registry: &NfsVolumeRegistry,
) -> Result<BuiltinToolResult, SmcpSessionError> {
    if tool_name == "cmd.run" {
        return builtin_dispatch::invoke_cmd_run(args, execution_id)
            .map(BuiltinToolResult::Handled);
    }

    if tool_name.starts_with("fs.") {
        return builtin_fsal::invoke_fs_tool(tool_name, args, execution_id, fsal, volume_registry)
            .await
            .map(BuiltinToolResult::Handled);
    }

    if tool_name.starts_with("web.") {
        return builtin_web::invoke_web_tool(tool_name, args, execution_id)
            .await
            .map(BuiltinToolResult::Handled);
    }

    Ok(BuiltinToolResult::NotBuiltin)
}
