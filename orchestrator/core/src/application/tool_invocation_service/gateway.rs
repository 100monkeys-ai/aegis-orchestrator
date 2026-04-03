use super::*;

impl ToolInvocationService {
    pub async fn get_available_tools(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let mut tools =
            self.tool_router.list_tools().await.map_err(|e| {
                SealSessionError::InternalError(format!("Failed to list tools: {e}"))
            })?;

        if self.seal_gateway_url.is_some() {
            if let Ok(gateway_tools) = self.fetch_gateway_tools_grpc().await {
                tools.extend(gateway_tools);
            }
        }

        Ok(tools)
    }

    pub async fn get_available_tools_for_agent(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let agent = self
            .agent_lifecycle
            .get_agent_visible(tenant_id, agent_id)
            .await
            .map_err(|e| {
                SealSessionError::InternalError(format!(
                    "Failed to load agent for tool scoping: {e}"
                ))
            })?;

        let declared_tools = agent.manifest.spec.tools;
        if declared_tools.is_empty() {
            return Ok(Vec::new());
        }

        let tools = self.get_available_tools().await?;
        Ok(tools
            .into_iter()
            .filter(|tool| declared_tools.iter().any(|name| name == &tool.name))
            .collect())
    }

    pub async fn get_available_tools_for_agent_in_context(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
        security_context_name: &str,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let agent = self
            .agent_lifecycle
            .get_agent_visible(tenant_id, agent_id)
            .await
            .map_err(|e| {
                SealSessionError::InternalError(format!(
                    "Failed to load agent for tool scoping: {e}"
                ))
            })?;

        let declared_tools = agent.manifest.spec.tools;
        if declared_tools.is_empty() {
            return Ok(Vec::new());
        }

        let security_context = self
            .security_context_repo
            .find_by_name(security_context_name)
            .await
            .map_err(|e| SealSessionError::ConfigurationError(e.to_string()))?
            .ok_or_else(|| {
                SealSessionError::ConfigurationError(format!(
                    "Security context '{security_context_name}' not found"
                ))
            })?;

        let tools = self.get_available_tools().await?;
        Ok(tools
            .into_iter()
            .filter(|tool| {
                declared_tools.iter().any(|name| name == &tool.name)
                    && security_context.permits_tool_name(&tool.name)
            })
            .collect())
    }

    pub async fn get_available_tools_for_context(
        &self,
        security_context_name: &str,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let security_context = self
            .security_context_repo
            .find_by_name(security_context_name)
            .await
            .map_err(|e| SealSessionError::ConfigurationError(e.to_string()))?
            .ok_or_else(|| {
                SealSessionError::ConfigurationError(format!(
                    "Security context '{security_context_name}' not found"
                ))
            })?;

        tracing::debug!(
            context = %security_context.name,
            capabilities_count = security_context.capabilities.len(),
            capabilities = ?security_context.capabilities.iter().map(|c| &c.tool_pattern).collect::<Vec<_>>(),
            "Filtering tools for security context"
        );

        let tools = self.get_available_tools().await?;
        let total_before = tools.len();
        let filtered: Vec<_> = tools
            .into_iter()
            .filter(|tool| {
                let permitted = security_context.permits_tool_name(&tool.name);
                tracing::debug!(tool = %tool.name, permitted, "Tool filter check");
                permitted
            })
            .collect();

        tracing::debug!(
            context = %security_context.name,
            total_before,
            total_after = filtered.len(),
            tools = ?filtered.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "Filtered tools result"
        );

        Ok(filtered)
    }

    pub(super) async fn fetch_gateway_tools_grpc(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let gateway_url = self.seal_gateway_url.as_deref().ok_or_else(|| {
            SealSessionError::ConfigurationError("seal_gateway.url is not configured".to_string())
        })?;
        let mut client = GatewayInvocationServiceClient::connect(gateway_url.to_string())
            .await
            .map_err(|e| SealSessionError::InternalError(e.to_string()))?;
        let response = client
            .list_tools(tonic::Request::new(ListToolsRequest {}))
            .await
            .map_err(|e| SealSessionError::InternalError(e.to_string()))?;

        let mut converted = Vec::new();
        for item in response.into_inner().tools {
            let input_schema = if !item.input_schema_json.is_empty() {
                serde_json::from_str(&item.input_schema_json)
                    .unwrap_or_else(|_| Self::dummy_input_schema(&item.kind))
            } else {
                Self::dummy_input_schema(&item.kind)
            };
            converted.push(crate::infrastructure::tool_router::ToolMetadata {
                name: item.name,
                description: item.description,
                input_schema,
            });
        }

        Ok(converted)
    }

    fn dummy_input_schema(kind: &str) -> serde_json::Value {
        if kind == "cli" {
            serde_json::json!({
                "type":"object",
                "properties": {
                    "subcommand": {"type":"string"},
                    "args": {"type":"array","items":{"type":"string"}}
                },
                "required": ["subcommand"]
            })
        } else {
            serde_json::json!({"type":"object"})
        }
    }

    pub(super) async fn invoke_seal_gateway_internal_grpc(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tool_name: &str,
        args: serde_json::Value,
        tenant_id: Option<&str>,
        zaru_user_token: Option<&str>,
    ) -> Result<serde_json::Value, SealSessionError> {
        let gateway_url = self.seal_gateway_url.as_deref().ok_or_else(|| {
            SealSessionError::ConfigurationError("seal_gateway.url is not configured".to_string())
        })?;
        let mut client = GatewayInvocationServiceClient::connect(gateway_url.to_string())
            .await
            .map_err(|e| SealSessionError::InternalError(e.to_string()))?;

        if args.get("subcommand").is_some() {
            let subcommand = args
                .get("subcommand")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    SealSessionError::InvalidArguments(
                        "CLI tool invocation requires 'subcommand' string".to_string(),
                    )
                })?
                .to_string();
            let cli_args = args
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();

            let fsal_mounts = self
                .volume_registry
                .find_all_by_execution(execution_id)
                .into_iter()
                .map(|ctx| FsalMount {
                    volume_id: ctx.volume_id.to_string(),
                    mount_path: ctx.mount_point.to_string_lossy().to_string(),
                    read_only: ctx.policy.write.is_empty(),
                })
                .collect::<Vec<FsalMount>>();

            if fsal_mounts.is_empty() {
                return Err(SealSessionError::InternalError(format!(
                    "No FSAL mounts registered for execution {execution_id}"
                )));
            }

            let response = client
                .invoke_cli(tonic::Request::new(InvokeCliRequest {
                    execution_id: execution_id.to_string(),
                    tool_name: tool_name.to_string(),
                    subcommand,
                    args: cli_args,
                    fsal_mounts,
                    tenant_id: tenant_id.unwrap_or("").to_string(),
                }))
                .await
                .map_err(|e| SealSessionError::InternalError(e.to_string()))?
                .into_inner();

            return Ok(serde_json::json!({
                "exit_code": response.exit_code,
                "stdout": response.stdout,
                "stderr": response.stderr
            }));
        }

        let response = client
            .invoke_workflow(tonic::Request::new(InvokeWorkflowRequest {
                execution_id: execution_id.to_string(),
                workflow_name: tool_name.to_string(),
                input_json: args.to_string(),
                zaru_user_token: zaru_user_token.unwrap_or("").to_string(),
                tenant_id: tenant_id.unwrap_or("").to_string(),
            }))
            .await
            .map_err(|e| SealSessionError::InternalError(e.to_string()))?
            .into_inner();

        if response.result_json.is_empty() {
            return Ok(serde_json::json!({}));
        }

        serde_json::from_str(&response.result_json)
            .map_err(|e| SealSessionError::InternalError(e.to_string()))
    }
}
