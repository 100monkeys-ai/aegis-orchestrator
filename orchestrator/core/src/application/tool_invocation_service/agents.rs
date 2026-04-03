use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_agent_create_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.agent.create requires 'manifest_yaml' string".to_string(),
                )
            })?;
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        let tenant_id = Self::resolve_tenant_arg(args)?;

        let manifest = match AgentManifestParser::parse_yaml(manifest_yaml) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.create",
                    "validated": false,
                    "deployed": false,
                    "errors": [format!("Agent manifest parse/validation failed: {}", e)]
                })));
            }
        };

        // ADR-087: Validate that all declared tools exist in the tool catalog.
        if let Some(catalog) = &self.tool_catalog {
            let declared_tools = &manifest.spec.tools;
            if !declared_tools.is_empty() {
                let available = catalog
                    .list_tools(
                        &["*".to_string()],
                        crate::application::tool_catalog::ToolListQuery {
                            source: None,
                            category: None,
                            limit: Some(1000),
                            offset: Some(0),
                        },
                    )
                    .await;
                let available_names: std::collections::HashSet<&str> =
                    available.tools.iter().map(|t| t.name.as_str()).collect();
                let unknown: Vec<&str> = declared_tools
                    .iter()
                    .filter(|t| !available_names.contains(t.as_str()))
                    .map(|t| t.as_str())
                    .collect();
                if !unknown.is_empty() {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.create",
                        "validated": false,
                        "deployed": false,
                        "errors": [format!(
                            "Agent manifest references tools not registered on this platform: [{}]. Call aegis.tools.list to see available tools.",
                            unknown.join(", ")
                        )]
                    })));
                }
            }
        }

        match self
            .agent_lifecycle
            .deploy_agent_for_tenant(
                &tenant_id,
                manifest.clone(),
                force,
                crate::domain::agent::AgentScope::Tenant,
            )
            .await
        {
            Ok(agent_id) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "agents",
                        &manifest.metadata.name,
                        &manifest.metadata.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SealSessionError::SignatureVerificationFailed(format!(
                            "Agent deployed but failed to persist manifest: {e}"
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.create",
                    "validated": true,
                    "deployed": true,
                    "agent_id": agent_id.0.to_string(),
                    "name": manifest.metadata.name,
                    "version": manifest.metadata.version,
                    "force": force,
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.create",
                "validated": true,
                "deployed": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "errors": [format!("Agent deployment failed: {}", e)]
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let agents = self
            .agent_lifecycle
            .list_agents_visible_for_tenant(&tenant_id, None)
            .await
            .map_err(|e| {
                SealSessionError::SignatureVerificationFailed(format!("Failed to list agents: {e}"))
            })?;

        let entries: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id.0.to_string(),
                    "name": a.name,
                    "version": a.manifest.metadata.version,
                    "status": format!("{:?}", a.status).to_lowercase(),
                    "description": a.manifest.metadata.description,
                    "labels": a.manifest.metadata.labels,
                    "tags": a.manifest.metadata.tags,
                    "input_schema": a.manifest.spec.input_schema,
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.agent.list",
            "count": entries.len(),
            "agents": entries
        })))
    }

    pub(super) async fn invoke_aegis_agent_delete_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let agent_id_str = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.agent.delete requires 'agent_id' string".to_string(),
                )
            })?;

        let tenant_id = Self::resolve_tenant_arg(args)?;
        let agent_id =
            crate::domain::agent::AgentId(uuid::Uuid::parse_str(agent_id_str).map_err(|e| {
                SealSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}"))
            })?);

        match self
            .agent_lifecycle
            .delete_agent_for_tenant(&tenant_id, agent_id)
            .await
        {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.delete",
                "deleted": true,
                "agent_id": agent_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.delete",
                "deleted": false,
                "error": format!("Failed to delete agent: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_generate_tool(
        &self,
        args: &Value,
        _security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let input = args.get("input").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::SignatureVerificationFailed(
                "aegis.agent.generate requires 'input' string".to_string(),
            )
        })?;

        let tenant_id = Self::resolve_tenant_arg(args)?;
        let agent_id = match self
            .agent_lifecycle
            .lookup_agent_visible_for_tenant(&tenant_id, None, "agent-creator-agent")
            .await
        {
            Ok(Some(id)) => id,
            _ => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.generate",
                    "error": "Generator agent 'agent-creator-agent' not found"
                })));
            }
        };

        let agent_tenant_id = match self
            .agent_lifecycle
            .get_agent_visible(&tenant_id, agent_id)
            .await
        {
            Ok(agent) => agent.tenant_id,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.generate",
                    "error": format!("Failed to resolve agent tenant: {e}")
                })));
            }
        };

        match self
            .execution_service
            .start_execution(
                agent_id,
                crate::domain::execution::ExecutionInput {
                    intent: Some(input.to_string()),
                    payload: serde_json::json!({
                        "tenant_id": agent_tenant_id.to_string()
                    }),
                },
                "agent-runtime".to_string(),
                None,
            )
            .await
        {
            Ok(exec_id) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.generate",
                "execution_id": exec_id.to_string(),
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.generate",
                "error": format!("Failed to start generation: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_update_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.agent.update requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let manifest = match AgentManifestParser::parse_yaml(manifest_yaml) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": false,
                    "updated": false,
                    "error": format!("Manifest parsing failed: {}", e)
                })));
            }
        };

        if let Err(e) = manifest.validate() {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.update",
                "validated": false,
                "updated": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "error": format!("Schema validation failed: {}", e)
            })));
        }

        let existing_agent = match self
            .agent_lifecycle
            .lookup_agent_for_tenant(&tenant_id, &manifest.metadata.name)
            .await
        {
            Ok(Some(id)) => self
                .agent_lifecycle
                .get_agent_for_tenant(&tenant_id, id)
                .await
                .ok(),
            _ => None,
        };

        let agent_id = match &existing_agent {
            Some(a) => a.id,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": true,
                    "updated": false,
                    "name": manifest.metadata.name,
                    "error": "Agent not found"
                })));
            }
        };

        // Version check if force=false
        if !force {
            if let Some(existing) = existing_agent {
                if existing.manifest.metadata.version == manifest.metadata.version {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.update",
                        "validated": true,
                        "updated": false,
                        "name": manifest.metadata.name,
                        "version": manifest.metadata.version,
                        "error": format!("Agent '{}' with version '{}' already exists. Create a new version or use 'force' to overwrite.", manifest.metadata.name, manifest.metadata.version)
                    })));
                }
            }
        }

        // ADR-087: Validate that all declared tools exist in the tool catalog.
        if let Some(catalog) = &self.tool_catalog {
            let declared_tools = &manifest.spec.tools;
            if !declared_tools.is_empty() {
                let available = catalog
                    .list_tools(
                        &["*".to_string()],
                        crate::application::tool_catalog::ToolListQuery {
                            source: None,
                            category: None,
                            limit: Some(1000),
                            offset: Some(0),
                        },
                    )
                    .await;
                let available_names: std::collections::HashSet<&str> =
                    available.tools.iter().map(|t| t.name.as_str()).collect();
                let unknown: Vec<&str> = declared_tools
                    .iter()
                    .filter(|t| !available_names.contains(t.as_str()))
                    .map(|t| t.as_str())
                    .collect();
                if !unknown.is_empty() {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.update",
                        "validated": false,
                        "updated": false,
                        "errors": [format!(
                            "Agent manifest references tools not registered on this platform: [{}]. Call aegis.tools.list to see available tools.",
                            unknown.join(", ")
                        )]
                    })));
                }
            }
        }

        match self
            .agent_lifecycle
            .update_agent_for_tenant(&tenant_id, agent_id, manifest.clone())
            .await
        {
            Ok(_) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "agents",
                        &manifest.metadata.name,
                        &manifest.metadata.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SealSessionError::SignatureVerificationFailed(format!(
                            "Agent updated but failed to persist manifest: {e}"
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.update",
                    "validated": true,
                    "updated": true,
                    "name": manifest.metadata.name,
                    "version": manifest.metadata.version,
                    "agent_id": agent_id.0.to_string(),
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path,
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.update",
                "validated": true,
                "updated": false,
                "name": manifest.metadata.name,
                "version": manifest.metadata.version,
                "errors": [format!("Agent update failed: {}", e)]
            }))),
        }
    }

    pub(super) async fn invoke_aegis_agent_export_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::SignatureVerificationFailed(
                "aegis.agent.export requires 'name' string".to_string(),
            )
        })?;

        let agent_id = match self
            .agent_lifecycle
            .lookup_agent_for_tenant(&tenant_id, name)
            .await
        {
            Ok(Some(id)) => id,
            _ => {
                if let Ok(uuid) = uuid::Uuid::parse_str(name) {
                    crate::domain::agent::AgentId(uuid)
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.agent.export",
                        "error": "Agent not found or invalid UUID"
                    })));
                }
            }
        };

        match self
            .agent_lifecycle
            .get_agent_for_tenant(&tenant_id, agent_id)
            .await
        {
            Ok(agent) => {
                let yaml = serde_yaml::to_string(&agent.manifest).unwrap_or_default();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.export",
                    "name": agent.name,
                    "manifest_yaml": yaml
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.export",
                "error": format!("Failed to get agent: {}", e)
            }))),
        }
    }

    /// Handler for `aegis.agent.logs` — retrieves agent-level activity log snapshot.
    pub(super) async fn invoke_aegis_agent_logs_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let agent_id_str = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.agent.logs requires 'agent_id' string".to_string(),
                )
            })?;

        let agent_id = uuid::Uuid::parse_str(agent_id_str).map_err(|e| {
            SealSessionError::InvalidArguments(format!(
                "aegis.agent.logs: invalid agent_id UUID: {e}"
            ))
        })?;

        let limit: usize = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(50);
        let offset: usize = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);

        let port = match &self.agent_activity {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.logs",
                    "error": "Agent activity port not configured"
                })));
            }
        };

        match port.agent_logs_snapshot(agent_id, limit, offset).await {
            Ok(events) => {
                let total = events.len();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.agent.logs",
                    "agent_id": agent_id_str,
                    "events": events,
                    "total": total,
                    "limit": limit,
                    "offset": offset,
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.agent.logs",
                "error": format!("Failed to fetch agent logs: {e}")
            }))),
        }
    }
}
