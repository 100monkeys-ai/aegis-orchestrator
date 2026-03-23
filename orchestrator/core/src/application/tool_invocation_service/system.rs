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
