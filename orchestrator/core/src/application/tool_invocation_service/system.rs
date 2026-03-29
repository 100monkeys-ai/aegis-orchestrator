use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_system_info_tool(
        &self,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.system.info",
            "version": env!("CARGO_PKG_VERSION"),
            "status": "healthy",
            "capabilities": [
                "agent_lifecycle",
                "workflow_orchestration",
                "mcp_routing",
                "smcp_attestation"
            ]
        })))
    }

    pub(super) async fn invoke_aegis_tools_list(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let catalog = self.tool_catalog.as_ref().ok_or_else(|| {
            SmcpSessionError::InternalError("Tool catalog not configured on this node".to_string())
        })?;

        let permitted_tools: Vec<String> = security_context
            .capabilities
            .iter()
            .map(|c| c.tool_pattern.clone())
            .collect();

        let query = crate::application::tool_catalog::ToolListQuery {
            offset: args
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            limit: args.get("limit").and_then(|v| v.as_u64()).map(|v| v as u32),
            source: args
                .get("source")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_value(Value::String(s.to_string())).ok()),
            category: args
                .get("category")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_value(Value::String(s.to_string())).ok()),
        };

        let response = catalog.list_tools(&permitted_tools, query).await;
        Ok(ToolInvocationResult::Direct(
            serde_json::to_value(response).unwrap_or_else(
                |e| serde_json::json!({"error": format!("Serialization failed: {e}")}),
            ),
        ))
    }

    pub(super) async fn invoke_aegis_tools_search(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let catalog = self.tool_catalog.as_ref().ok_or_else(|| {
            SmcpSessionError::InternalError("Tool catalog not configured on this node".to_string())
        })?;

        let permitted_tools: Vec<String> = security_context
            .capabilities
            .iter()
            .map(|c| c.tool_pattern.clone())
            .collect();

        let query = crate::application::tool_catalog::ToolSearchQuery {
            keyword: args
                .get("keyword")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            name_pattern: args
                .get("name_pattern")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            source: args
                .get("source")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_value(Value::String(s.to_string())).ok()),
            category: args
                .get("category")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_value(Value::String(s.to_string())).ok()),
            tags: args.get("tags").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect()
            }),
        };

        let response = catalog.search_tools(&permitted_tools, query).await;
        Ok(ToolInvocationResult::Direct(
            serde_json::to_value(response).unwrap_or_else(
                |e| serde_json::json!({"error": format!("Serialization failed: {e}")}),
            ),
        ))
    }

    pub(super) async fn invoke_aegis_system_config_tool(
        &self,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let path = match &self.node_config_path {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.system.config",
                    "error": "Node configuration path not available"
                })));
            }
        };

        match std::fs::read_to_string(path) {
            Ok(content) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.system.config",
                "config_path": path.to_string_lossy(),
                "content_yaml": content
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.system.config",
                "error": format!("Failed to read node configuration: {e}")
            }))),
        }
    }
}
