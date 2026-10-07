pub mod builtin_dispatch;
pub mod builtin_execution_file;
pub mod builtin_fsal;
pub mod builtin_mail;
pub mod builtin_schema;
pub mod builtin_web;

use crate::application::nfs_gateway::NfsVolumeRegistry;
pub use crate::application::ports::ExternalWebToolPort;
use crate::application::schema_registry::SchemaRegistry;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::AegisFSAL;
use crate::domain::seal_session::SealSessionError;
use serde_json::Value;
use std::sync::Arc;

pub enum BuiltinToolResult {
    Handled(ToolInvocationResult),
    NotBuiltin,
}

/// A mail tool's call: the node's mail tools, if configured, and who the
/// call acts for (AEGIS ADR-125 D4, its Update of 2026-10-07 clause 7).
pub struct MailCall<'a> {
    pub tools: Option<&'a builtin_mail::MailTools>,
    pub acting: builtin_mail::MailActing,
}

#[allow(clippy::too_many_arguments)]
pub async fn try_invoke_builtin(
    tool_name: &str,
    args: &Value,
    execution_id: ExecutionId,
    fsal: &Arc<AegisFSAL>,
    volume_registry: &NfsVolumeRegistry,
    web_tool_port: &Arc<dyn ExternalWebToolPort>,
    schema_registry: &Arc<SchemaRegistry>,
    mail: Option<MailCall<'_>>,
) -> Result<BuiltinToolResult, SealSessionError> {
    if builtin_mail::is_mail_tool(tool_name) {
        if let Some(call) = mail {
            let Some(tools) = call.tools else {
                return Err(builtin_mail::not_configured());
            };
            return tools
                .invoke(tool_name, args, &call.acting)
                .await
                .map(|value| BuiltinToolResult::Handled(ToolInvocationResult::Direct(value)));
        }
    }

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
        return builtin_web::invoke_web_tool(tool_name, args, execution_id, web_tool_port.as_ref())
            .await
            .map(BuiltinToolResult::Handled);
    }

    if tool_name == "aegis.schema.get" {
        return builtin_schema::invoke_schema_get(schema_registry, args)
            .map(BuiltinToolResult::Handled);
    }

    if tool_name == "aegis.schema.validate" {
        return builtin_schema::invoke_schema_validate(schema_registry, args)
            .map(BuiltinToolResult::Handled);
    }

    Ok(BuiltinToolResult::NotBuiltin)
}
