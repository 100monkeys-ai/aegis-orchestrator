use super::*;
use crate::domain::secrets::SensitiveUrl;
use crate::infrastructure::seal_gateway_proto::ToolSummary;
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
    /// Put the operator token in `request`'s `authorization` metadata, as
    /// `Bearer <token>`. Without configured credentials the request goes
    /// unauthenticated and the gateway refuses it unless its operator
    /// authentication is off.
    async fn authorize_gateway_request<T>(
        &self,
        request: &mut tonic::Request<T>,
    ) -> Result<(), SealSessionError> {
        let Some(source) = &self.seal_gateway_operator_token else {
            return Ok(());
        };
        let authorization = source.authorization().await.map_err(|e| {
            SealSessionError::InternalError(format!("SEAL gateway operator token: {e}"))
        })?;
        let value = authorization
            .expose()
            .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
            .map_err(|_| {
                SealSessionError::InternalError(
                    "SEAL gateway operator token is not a valid metadata value".to_string(),
                )
            })?;
        request.metadata_mut().insert("authorization", value);
        Ok(())
    }

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

    /// The agent's `spec.execution.llm_timeout_seconds`: the bound on one of
    /// its model calls, which the inner loop enforces (the default when the
    /// manifest has no execution block).
    pub(crate) async fn agent_llm_timeout_seconds(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> anyhow::Result<u64> {
        let agent = self
            .agent_lifecycle
            .get_agent_visible(tenant_id, agent_id)
            .await?;
        Ok(llm_timeout_seconds_of(&agent))
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

    /// The SEAL gateway's node-wide tool list, best effort: a slow, failing or
    /// unreachable gateway answers an empty list, so built-in dispatch and
    /// the semantic judge's inventory never wait on it. The request names no
    /// user and carries no credential: a tool a user's own binding opens is
    /// never in this list (AEGIS ADR-132's Update (3), H7's row on the
    /// unauthenticated `GET /v1/seal/tools`).
    pub(super) async fn fetch_gateway_tools_grpc(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        if self.seal_gateway_url.is_none() {
            return Err(SealSessionError::ConfigurationError(
                "seal_gateway.url is not configured".to_string(),
            ));
        }
        let listed = match self
            .list_gateway_tools(ListToolsRequest::default(), GATEWAY_LIST_TOOLS_TIMEOUT)
            .await
        {
            Ok(listed) => listed,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "SEAL gateway tool enumeration failed; proceeding with built-in tools only"
                );
                return Ok(Vec::new());
            }
        };
        Ok(listed
            .into_iter()
            .map(Self::gateway_tool_metadata)
            .collect())
    }

    /// A tool the gateway listed, as the orchestrator advertises it.
    pub(super) fn gateway_tool_metadata(
        item: ToolSummary,
    ) -> crate::infrastructure::tool_router::ToolMetadata {
        let input_schema = if !item.input_schema_json.is_empty() {
            serde_json::from_str(&item.input_schema_json)
                .unwrap_or_else(|_| Self::dummy_input_schema(&item.kind))
        } else {
            Self::dummy_input_schema(&item.kind)
        };
        crate::infrastructure::tool_router::ToolMetadata {
            name: item.name,
            description: item.description,
            input_schema,
            ..Default::default()
        }
    }

    /// The gateway's `ListTools` answer to `listing`, bounded by `timeout`.
    /// Every failure is an error here; the best-effort callers downgrade it.
    pub(super) async fn list_gateway_tools(
        &self,
        listing: ListToolsRequest,
        timeout: Duration,
    ) -> Result<Vec<ToolSummary>, SealSessionError> {
        let mut client = self.connect_gateway().await?;
        let mut request = tonic::Request::new(listing);
        self.authorize_gateway_request(&mut request).await?;
        match tokio::time::timeout(timeout, client.list_tools(request)).await {
            Ok(Ok(response)) => Ok(response.into_inner().tools),
            Ok(Err(status)) => Err(gateway_refusal("list_tools", "", &status)),
            Err(_) => Err(SealSessionError::InternalError(format!(
                "seal tooling gateway list_tools timeout after {}s",
                timeout.as_secs()
            ))),
        }
    }

    /// A channel to the configured gateway, its connect bounded: an
    /// unreachable gateway fails fast with an internal error that names the
    /// gateway by its redacted address.
    pub(super) async fn connect_gateway(
        &self,
    ) -> Result<GatewayInvocationServiceClient<tonic::transport::Channel>, SealSessionError> {
        let gateway_url = self.seal_gateway_url.as_deref().ok_or_else(|| {
            SealSessionError::ConfigurationError("seal_gateway.url is not configured".to_string())
        })?;
        let endpoint = self.gateway_endpoint(gateway_url)?;
        match tokio::time::timeout(GATEWAY_CONNECT_TIMEOUT, endpoint.connect()).await {
            Ok(Ok(channel)) => Ok(GatewayInvocationServiceClient::new(channel)),
            Ok(Err(e)) => Err(SealSessionError::InternalError(format!(
                "seal tooling gateway connect failed ({}): {e}",
                SensitiveUrl::new(gateway_url)
            ))),
            Err(_) => Err(SealSessionError::InternalError(format!(
                "seal tooling gateway connect timeout after {}s ({})",
                GATEWAY_CONNECT_TIMEOUT.as_secs(),
                SensitiveUrl::new(gateway_url)
            ))),
        }
    }

    /// The endpoint of `gateway_url`. An `https` gateway is dialled over TLS
    /// and its certificate verified against the configured CA, or the
    /// system's roots when none is configured (AEGIS ADR-132 H8); the name
    /// verified is the URL's host.
    fn gateway_endpoint(
        &self,
        gateway_url: &str,
    ) -> Result<tonic::transport::Endpoint, SealSessionError> {
        let endpoint = tonic::transport::Endpoint::from_shared(gateway_url.to_string())
            .map_err(|e| {
                SealSessionError::InternalError(format!(
                    "seal tooling gateway invalid URL '{}': {e}",
                    SensitiveUrl::new(gateway_url)
                ))
            })?
            .connect_timeout(GATEWAY_CONNECT_TIMEOUT);
        if !gateway_url_is_tls(gateway_url) {
            return Ok(endpoint);
        }
        let tls = match &self.seal_gateway_ca {
            Some(ca) => tonic::transport::ClientTlsConfig::new().ca_certificate(ca.clone()),
            None => tonic::transport::ClientTlsConfig::new().with_native_roots(),
        };
        endpoint.tls_config(tls).map_err(|e| {
            SealSessionError::InternalError(format!(
                "seal tooling gateway TLS configuration for '{}' failed: {e}",
                SensitiveUrl::new(gateway_url)
            ))
        })
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

    /// Call `tool_name` on the SEAL gateway. The tool goes by the kind the
    /// gateway lists it under for this tenant: a CLI tool to `InvokeCli`
    /// (it needs the execution's FSAL mounts), any other to
    /// `InvokeWorkflow`. A tool the gateway does not list is answered as one
    /// that does not exist, and nothing is invoked. A refusal the gateway
    /// answers reaches the caller by its code (AEGIS ADR-132 H5).
    pub(super) async fn invoke_seal_gateway_internal_grpc(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tool_name: &str,
        args: serde_json::Value,
        tenant_id: &TenantId,
    ) -> Result<serde_json::Value, SealSessionError> {
        let listing = ListToolsRequest {
            tenant_id: tenant_id.as_str().to_string(),
            ..Default::default()
        };
        let listed = self
            .list_gateway_tools(listing, GATEWAY_INVOKE_TIMEOUT)
            .await?
            .into_iter()
            .find(|tool| tool.name == tool_name);
        let Some(listed) = listed else {
            return Err(tool_not_found(tool_name));
        };

        let mut client = self.connect_gateway().await?;
        if listed.kind == "cli" {
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
                tenant_id: tenant_id.as_str().to_string(),
                acting: None,
            });
            self.authorize_gateway_request(&mut request).await?;
            let response = match tokio::time::timeout(
                GATEWAY_INVOKE_TIMEOUT,
                client.invoke_cli(request),
            )
            .await
            {
                Ok(Ok(resp)) => resp.into_inner(),
                Ok(Err(status)) => return Err(gateway_refusal("invoke_cli", tool_name, &status)),
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
            zaru_user_token: String::new(),
            tenant_id: tenant_id.as_str().to_string(),
            acting: None,
        });
        self.authorize_gateway_request(&mut request).await?;
        let response =
            match tokio::time::timeout(GATEWAY_INVOKE_TIMEOUT, client.invoke_workflow(request))
                .await
            {
                Ok(Ok(resp)) => resp.into_inner(),
                Ok(Err(status)) => {
                    return Err(gateway_refusal("invoke_workflow", tool_name, &status))
                }
                Err(_) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway invoke_workflow timeout after {}s",
                        GATEWAY_INVOKE_TIMEOUT.as_secs()
                    )));
                }
            };

        parse_gateway_result(&response.result_json)
    }
}

/// Whether `gateway_url` names a TLS endpoint (`https`).
pub(super) fn gateway_url_is_tls(gateway_url: &str) -> bool {
    url::Url::parse(gateway_url).is_ok_and(|url| url.scheme() == "https")
}

/// The gRPC metadata key in which the SEAL gateway carries a refusal's
/// AEGIS ADR-035 R5 code; the status message is the caller-facing text.
pub(super) const REFUSAL_CODE_METADATA: &str = "seal-refusal-code";

/// The answer for a tool nothing serves: the caller's tool name, never the
/// node's tool list.
pub(super) fn tool_not_found(tool_name: &str) -> SealSessionError {
    SealSessionError::InternalError(format!("Tool not found: {tool_name}")).answered(
        crate::domain::seal_session::CallerAnswer::NotFound(format!(
            "Not found: tool '{tool_name}'."
        )),
    )
}

/// The result a gateway invocation answered, as JSON (`{}` when empty).
fn parse_gateway_result(result_json: &str) -> Result<serde_json::Value, SealSessionError> {
    if result_json.is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(result_json).map_err(|e| SealSessionError::InternalError(e.to_string()))
}

/// What the caller is answered for a gateway RPC that failed with `status`
/// (AEGIS ADR-132 H5; ADR-035 R5 and its rows for the gateway). A refusal
/// carries its R5 code in [`REFUSAL_CODE_METADATA`] and its caller-facing
/// text as the message, and is answered by that code's row, never as a tool
/// that does not exist. What an agent's inner loop sees (ADR-035 R7) is the
/// `shown` error of each: a refusal the agent cannot cure by retrying (no
/// binding, a refused credential, a tool the server does not have) is shown
/// as a not-found, which the loop feeds back as "not available, do not
/// retry"; the server's own error, as invalid arguments with its text; an
/// upstream or rate limit, as the upstream's failure. A failure with no code
/// is internal: its detail goes to the operator's log only.
pub(super) fn gateway_refusal(
    rpc: &str,
    tool_name: &str,
    status: &tonic::Status,
) -> SealSessionError {
    use crate::domain::seal_session::{CallerAnswer, InternalFailure};
    let code = status
        .metadata()
        .get(REFUSAL_CODE_METADATA)
        .and_then(|value| value.to_str().ok());
    let message = status.message().to_string();
    let detail = || {
        format!(
            "seal tooling gateway {rpc} refused '{tool_name}' ({}): {message}",
            code.unwrap_or("no code")
        )
    };
    match code {
        Some("CREDENTIAL_BINDING_REQUIRED") => SealSessionError::NotFound(message.clone())
            .answered(CallerAnswer::CredentialBindingRequired { message }),
        Some("CREDENTIAL_REJECTED") => SealSessionError::NotFound(message.clone())
            .answered(CallerAnswer::CredentialRejected { message }),
        Some("REMOTE_TOOL_ERROR") => SealSessionError::InvalidArguments(message.clone())
            .answered(CallerAnswer::RemoteToolError(message)),
        Some("CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL") => {
            tracing::error!(
                rpc,
                tool = %tool_name,
                "the SEAL gateway refused a call carrying a credential because its gRPC \
                 listener is plaintext; serve the gateway's gRPC over TLS"
            );
            SealSessionError::InternalError(detail())
                .answered(CallerAnswer::CredentialChannelNotConfidential)
        }
        Some("NOT_FOUND") => {
            SealSessionError::InternalError(format!("Tool not found: {tool_name}"))
                .answered(CallerAnswer::NotFound(message))
        }
        Some("INVALID_ARGUMENTS") => SealSessionError::InvalidArguments(message),
        Some("RATE_LIMIT_EXCEEDED") | Some("UPSTREAM_UNAVAILABLE") => {
            SealSessionError::UpstreamUnavailable(message)
        }
        Some("SERVICE_UNAVAILABLE") => SealSessionError::InternalError(detail())
            .answered(CallerAnswer::Internal(InternalFailure::Unavailable)),
        _ => SealSessionError::InternalError(format!(
            "seal tooling gateway {rpc} failed ({:?}): {message}",
            status.code()
        )),
    }
}

fn llm_timeout_seconds_of(agent: &crate::domain::agent::Agent) -> u64 {
    agent
        .manifest
        .spec
        .execution
        .clone()
        .unwrap_or_default()
        .llm_timeout_seconds
}

#[cfg(test)]
mod llm_timeout_tests {
    use super::llm_timeout_seconds_of;
    use crate::domain::agent::{Agent, AgentId, AgentManifest, AgentStatus};

    fn agent(execution: &str) -> Agent {
        let manifest: AgentManifest = serde_yaml::from_str(&format!(
            r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: timeout-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
{execution}
"#
        ))
        .unwrap();
        Agent {
            id: AgentId::new(),
            tenant_id: crate::domain::tenant::TenantId::default(),
            scope: crate::domain::agent::AgentScope::default(),
            name: manifest.metadata.name.clone(),
            manifest,
            status: AgentStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn the_manifests_llm_timeout_seconds_is_read() {
        let agent = agent("  execution:\n    llm_timeout_seconds: 45\n");
        assert_eq!(llm_timeout_seconds_of(&agent), 45);
    }

    #[test]
    fn without_an_execution_block_the_default_300_is_read() {
        assert_eq!(llm_timeout_seconds_of(&agent("")), 300);
    }
}

#[cfg(test)]
#[path = "gateway_wire_tests.rs"]
mod gateway_wire_tests;
