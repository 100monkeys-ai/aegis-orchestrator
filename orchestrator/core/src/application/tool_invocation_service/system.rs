use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_system_info_tool(
        &self,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.system.info",
            "version": env!("CARGO_PKG_VERSION"),
            "status": "healthy",
            "capabilities": [
                "agent_lifecycle",
                "workflow_orchestration",
                "mcp_routing",
                "seal_attestation"
            ]
        })))
    }

    pub(super) async fn invoke_aegis_tools_list(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let catalog = self.tool_catalog.as_ref().ok_or_else(|| {
            SealSessionError::InternalError("Tool catalog not configured on this node".to_string())
                .answered(crate::domain::seal_session::CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Unavailable,
                ))
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
            fleet_capable: args.get("fleet_capable").and_then(|v| v.as_bool()),
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
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let catalog = self.tool_catalog.as_ref().ok_or_else(|| {
            SealSessionError::InternalError("Tool catalog not configured on this node".to_string())
                .answered(crate::domain::seal_session::CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Unavailable,
                ))
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
            fleet_capable: args.get("fleet_capable").and_then(|v| v.as_bool()),
        };

        let response = catalog.search_tools(&permitted_tools, query).await;
        Ok(ToolInvocationResult::Direct(
            serde_json::to_value(response).unwrap_or_else(
                |e| serde_json::json!({"error": format!("Serialization failed: {e}")}),
            ),
        ))
    }

    /// `aegis.system.config`: the node's configuration, as parsed from its
    /// file, in the redacted view. The answer reaches the agent and its
    /// model provider, so it never holds the file's text: every field of a
    /// secret type shows its reference, or `[REDACTED]` for a literal (see
    /// [`crate::domain::secrets::to_redacted_json`]).
    pub(super) async fn invoke_aegis_system_config_tool(
        &self,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let path = match &self.node_config_path {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.system.config",
                    "error": "Node configuration path not available"
                })));
            }
        };

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.system.config",
                    "error": format!("Failed to read node configuration: {}", e.kind())
                })));
            }
        };
        // The parser's own message can quote the value it could not read,
        // which may be a secret in the wrong field: only its place is given.
        let config: crate::domain::node_config::NodeConfigManifest = match serde_yaml::from_str(
            &text,
        ) {
            Ok(config) => config,
            Err(e) => {
                let place = e
                    .location()
                    .map(|l| format!(" at line {}, column {}", l.line(), l.column()))
                    .unwrap_or_default();
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.system.config",
                    "error": format!("The node configuration file is not a valid node configuration{place}")
                })));
            }
        };
        match crate::domain::secrets::to_redacted_json(&config) {
            Ok(shown) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.system.config",
                "config_path": path.to_string_lossy(),
                "config": shown
            }))),
            Err(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.system.config",
                "error": "The node configuration could not be shown"
            }))),
        }
    }
}
