use super::*;
use std::time::Duration;

/// Connect timeout for SEAL Tooling Gateway gRPC connections.
///
/// Built-in tool dispatch must not hang waiting for the SEAL Tooling Gateway
/// to become reachable. Per ADR-053 / ADR-038 / BC-14, the gateway is a
/// SEPARATE tooling layer — orchestrator built-ins are independent.
const GATEWAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Best-effort timeout for the gateway tool *enumeration* RPC (`list_tools`).
///
/// This call is invoked by the pre-dispatch semantic judge to populate its
/// tool inventory. If the gateway is slow or unresponsive, we proceed with
/// the locally-known built-in tools rather than blocking forever.
const GATEWAY_LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for gateway *invocation* RPCs (`invoke_workflow`, `invoke_cli`).
///
/// Unlike enumeration, an invocation timeout MUST surface as an error — the
/// caller invoked a gateway-routed tool and we cannot silently drop the call.
const GATEWAY_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

impl ToolInvocationService {
    pub async fn get_available_tools(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let mut tools =
            self.tool_router.list_tools().await.map_err(|e| {
                SealSessionError::InternalError(format!("Failed to list tools: {e}"))
            })?;

        // Best-effort enumeration: gateway failures must NEVER block built-in
        // tool dispatch. Errors and timeouts are swallowed and logged inside
        // `fetch_gateway_tools_grpc`.
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

        // Best-effort connect: a slow/unreachable gateway must NOT block the
        // pre-dispatch semantic judge. Bound the connect with an explicit
        // timeout and downgrade any failure (timeout or transport error) to
        // an empty tool list.
        let endpoint = match tonic::transport::Endpoint::from_shared(gateway_url.to_string()) {
            Ok(ep) => ep.connect_timeout(GATEWAY_CONNECT_TIMEOUT),
            Err(e) => {
                tracing::warn!(
                    gateway_url,
                    error = %e,
                    "invalid SEAL gateway URL; skipping gateway tool enumeration"
                );
                return Ok(Vec::new());
            }
        };

        let mut client = match tokio::time::timeout(GATEWAY_CONNECT_TIMEOUT, endpoint.connect())
            .await
        {
            Ok(Ok(channel)) => GatewayInvocationServiceClient::new(channel),
            Ok(Err(e)) => {
                tracing::warn!(
                    gateway_url,
                    error = %e,
                    "SEAL gateway connect failed during tool enumeration; proceeding with built-in tools only"
                );
                return Ok(Vec::new());
            }
            Err(_) => {
                tracing::warn!(
                    gateway_url,
                    timeout_secs = GATEWAY_CONNECT_TIMEOUT.as_secs(),
                    "SEAL gateway connect timed out during tool enumeration; proceeding with built-in tools only"
                );
                return Ok(Vec::new());
            }
        };

        let mut request = tonic::Request::new(ListToolsRequest {});
        if let Ok(token) = std::env::var("AEGIS_SEAL_OPERATOR_TOKEN") {
            if let Ok(val) = token.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
            {
                request.metadata_mut().insert("authorization", val);
            }
        }
        // Bound the RPC await: a hung `list_tools` MUST NOT cause the
        // orchestrator to hang on the tool-invocation path.
        let response = match tokio::time::timeout(
            GATEWAY_LIST_TOOLS_TIMEOUT,
            client.list_tools(request),
        )
        .await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                tracing::warn!(
                    gateway_url,
                    error = %e,
                    "SEAL gateway list_tools failed during tool enumeration; proceeding with built-in tools only"
                );
                return Ok(Vec::new());
            }
            Err(_) => {
                tracing::warn!(
                    gateway_url,
                    timeout_secs = GATEWAY_LIST_TOOLS_TIMEOUT.as_secs(),
                    "SEAL gateway list_tools timed out during tool enumeration; proceeding with built-in tools only"
                );
                return Ok(Vec::new());
            }
        };

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

        // Bound the connect: an unreachable gateway must fail fast with a
        // clear error rather than hanging the caller indefinitely.
        let endpoint = tonic::transport::Endpoint::from_shared(gateway_url.to_string())
            .map_err(|e| {
                SealSessionError::InternalError(format!(
                    "seal tooling gateway invalid URL '{gateway_url}': {e}"
                ))
            })?
            .connect_timeout(GATEWAY_CONNECT_TIMEOUT);

        let mut client =
            match tokio::time::timeout(GATEWAY_CONNECT_TIMEOUT, endpoint.connect()).await {
                Ok(Ok(channel)) => GatewayInvocationServiceClient::new(channel),
                Ok(Err(e)) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway connect failed ({gateway_url}): {e}"
                    )));
                }
                Err(_) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway connect timeout after {}s ({gateway_url})",
                        GATEWAY_CONNECT_TIMEOUT.as_secs()
                    )));
                }
            };

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
                    remote_path: ctx.remote_path.clone(),
                })
                .collect::<Vec<FsalMount>>();

            if fsal_mounts.is_empty() {
                return Err(SealSessionError::InternalError(format!(
                    "No FSAL mounts registered for execution {execution_id}"
                )));
            }

            let mut request = tonic::Request::new(InvokeCliRequest {
                execution_id: execution_id.to_string(),
                tool_name: tool_name.to_string(),
                subcommand,
                args: cli_args,
                fsal_mounts,
                tenant_id: tenant_id.unwrap_or("").to_string(),
            });
            if let Ok(token) = std::env::var("AEGIS_SEAL_OPERATOR_TOKEN") {
                if let Ok(val) =
                    token.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
                {
                    request.metadata_mut().insert("authorization", val);
                }
            }
            let response = match tokio::time::timeout(
                GATEWAY_INVOKE_TIMEOUT,
                client.invoke_cli(request),
            )
            .await
            {
                Ok(Ok(resp)) => resp.into_inner(),
                Ok(Err(e)) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway invoke_cli failed: {e}"
                    )));
                }
                Err(_) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway invoke_cli timeout after {}s",
                        GATEWAY_INVOKE_TIMEOUT.as_secs()
                    )));
                }
            };

            return Ok(serde_json::json!({
                "exit_code": response.exit_code,
                "stdout": response.stdout,
                "stderr": response.stderr
            }));
        }

        let mut request = tonic::Request::new(InvokeWorkflowRequest {
            execution_id: execution_id.to_string(),
            workflow_name: tool_name.to_string(),
            input_json: args.to_string(),
            zaru_user_token: zaru_user_token.unwrap_or("").to_string(),
            tenant_id: tenant_id.unwrap_or("").to_string(),
        });
        if let Ok(token) = std::env::var("AEGIS_SEAL_OPERATOR_TOKEN") {
            if let Ok(val) = token.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
            {
                request.metadata_mut().insert("authorization", val);
            }
        }
        let response =
            match tokio::time::timeout(GATEWAY_INVOKE_TIMEOUT, client.invoke_workflow(request))
                .await
            {
                Ok(Ok(resp)) => resp.into_inner(),
                Ok(Err(e)) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway invoke_workflow failed: {e}"
                    )));
                }
                Err(_) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway invoke_workflow timeout after {}s",
                        GATEWAY_INVOKE_TIMEOUT.as_secs()
                    )));
                }
            };

        if response.result_json.is_empty() {
            return Ok(serde_json::json!({}));
        }

        serde_json::from_str(&response.result_json)
            .map_err(|e| SealSessionError::InternalError(e.to_string()))
    }
}
