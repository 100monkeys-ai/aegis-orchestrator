use super::*;
use crate::application::credential_service::{
    GroundingRefusal, RemoteServerGrounding, ToolCallActor,
};
use crate::domain::seal_session::{CallerAnswer, InternalFailure};
use crate::domain::secrets::{SensitiveString, SensitiveUrl};
use crate::infrastructure::seal_gateway_proto::{
    ActingIdentity, CredentialKind, InvokeToolRequest, ResolvedCredential, ToolSummary,
};
use std::time::Duration;

/// Who a call to the SEAL gateway acts for (AEGIS ADR-132 H4, H6): the
/// person the run acts for (`None` for a run with no recorded person), the
/// calling agent, and the workflow the run belongs to, if any. Taken from
/// the execution's identity, never from a token a caller supplied. With
/// them, the execution's dispatch choices of a binding per remote server
/// (Zaru ADR-0055 D15), read from the execution record; they never reach
/// the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GatewayActing {
    pub(crate) user_id: Option<String>,
    pub(crate) agent_id: AgentId,
    pub(crate) workflow_id: Option<uuid::Uuid>,
    pub(crate) contexts: crate::domain::execution::ExecutionContexts,
}

impl GatewayActing {
    /// The wire's form: an absent person or workflow is the empty string.
    fn to_proto(&self) -> ActingIdentity {
        ActingIdentity {
            user_id: self.user_id.clone().unwrap_or_default(),
            agent_id: self.agent_id.to_string(),
            workflow_id: self
                .workflow_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
        }
    }
}

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

    /// The agent's iteration bound as the supervisor reads it.
    pub(crate) async fn agent_iteration_timeout(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
    ) -> anyhow::Result<std::time::Duration> {
        let agent = self
            .agent_lifecycle
            .get_agent_visible(tenant_id, agent_id)
            .await?;
        Ok(crate::domain::supervisor::iteration_timeout(
            &agent.manifest.spec.execution.clone().unwrap_or_default(),
        ))
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

    /// The tools an agent's run sees (AEGIS ADR-132 G5, H4): the built-ins
    /// and the gateway's tools for the tenant, with the `<server>.<tool>`
    /// tools of each remote server the run's person holds a binding to that
    /// is granted to the agent or its workflow, listed by the gateway with
    /// that person's credential; then only the agent's declared tools that
    /// its security context permits. It is served to the agent's own run,
    /// never on the unauthenticated `GET /v1/seal/tools`, which answers the
    /// node-wide list ([`Self::get_available_tools_for_context`]). A gateway
    /// that fails to list leaves the built-ins.
    pub async fn get_available_tools_for_agent_run(
        &self,
        tenant_id: &TenantId,
        agent_id: AgentId,
        execution_id: crate::domain::execution::ExecutionId,
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
        // The run's person and its dispatch's choices, as the execution
        // recorded them: never a service account's person (G2,
        // `execution::person_sub`).
        let execution = self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
            .ok();
        let contexts = execution
            .as_ref()
            .map(|execution| execution.input.contexts())
            .unwrap_or_default();
        // Zaru ADR-0055 D16: a declared context the dispatch filled with a
        // binding brings its server's tools; an undeclared one brings none.
        let filled_contexts: Vec<String> = agent
            .manifest
            .spec
            .contexts
            .iter()
            .map(|declared| declared.service.trim().to_string())
            .filter(|service| contexts.is_filled(service))
            .collect();
        if declared_tools.is_empty() && filled_contexts.is_empty() {
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

        let mut tools =
            self.tool_router.list_tools().await.map_err(|e| {
                SealSessionError::InternalError(format!("Failed to list tools: {e}"))
            })?;
        if self.seal_gateway_url.is_some() {
            let acting = GatewayActing {
                user_id: execution
                    .as_ref()
                    .and_then(|execution| execution.initiating_user_sub.clone()),
                agent_id,
                workflow_id: self.workflow_of(execution_id, tenant_id).await,
                contexts,
            };
            let listing = ListToolsRequest {
                tenant_id: tenant_id.as_str().to_string(),
                acting: Some(acting.to_proto()),
                bound_servers: self.bound_servers(tenant_id, &acting).await,
            };
            match self
                .list_gateway_tools(listing, GATEWAY_LIST_TOOLS_TIMEOUT)
                .await
            {
                Ok(listed) => tools.extend(listed.into_iter().map(Self::gateway_tool_metadata)),
                Err(e) => tracing::warn!(
                    error = %e,
                    "SEAL gateway tool enumeration for an agent's run failed; proceeding with built-in tools only"
                ),
            }
        }
        Ok(tools
            .into_iter()
            .filter(|tool| {
                let declared = declared_tools.iter().any(|name| name == &tool.name)
                    || self.remote_tool_of(&tool.name).is_some_and(|(server, _)| {
                        filled_contexts.iter().any(|service| service == server)
                    });
                declared && security_context.permits_tool_name(&tool.name)
            })
            .collect())
    }

    /// Each remote server the acting person holds a binding to granted to
    /// the acting agent or its workflow, with that person's credential, for
    /// the gateway to list its tools (AEGIS ADR-132 H4); for a server the
    /// execution's dispatch chose a binding or none for, that choice instead
    /// (Zaru ADR-0055 D15). None for a run with
    /// no person, and none over a plaintext channel (H8): a credential is
    /// never sent there, and the reason is logged.
    async fn bound_servers(
        &self,
        tenant_id: &TenantId,
        acting: &GatewayActing,
    ) -> Vec<crate::infrastructure::seal_gateway_proto::BoundServer> {
        let (Some(user_id), Some(source)) = (acting.user_id.as_deref(), &self.tool_credentials)
        else {
            return Vec::new();
        };
        if self.remote_tool_servers.is_empty() {
            return Vec::new();
        }
        if !self.gateway_channel_is_confidential() {
            tracing::error!(
                "an agent's tool list would carry the acting user's credentials to the SEAL \
                 gateway over a plaintext channel; none was sent and no remote server's tools \
                 are listed. Configure seal_gateway.url as an https address"
            );
            return Vec::new();
        }
        let mut bound = Vec::new();
        for server in &self.remote_tool_servers {
            let actor = ToolCallActor {
                tenant_id,
                user_id,
                agent_id: acting.agent_id,
                workflow_id: acting.workflow_id,
                context: acting.contexts.choice(server),
            };
            match source.tool_server_credential(&actor, server).await {
                Ok(Some(credential)) => {
                    bound.push(crate::infrastructure::seal_gateway_proto::BoundServer {
                        server: server.clone(),
                        credential: Some(ResolvedCredential {
                            kind: CredentialKind::BearerToken as i32,
                            value: credential.expose().to_string(),
                        }),
                    })
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    server = %server,
                    error = %e,
                    "the acting user's credential for a remote tool server could not be resolved; its tools are not listed"
                ),
            }
        }
        bound
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

    /// Who a call of `agent_id` in `execution_id` acts for (AEGIS ADR-132
    /// H4, H6): the caller when the caller is a person, never a service
    /// account; the workflow the execution's workflow run belongs to, read in
    /// the same tenant. A workflow that cannot be read is named as none. The
    /// dispatch's choices are the execution record's (Zaru ADR-0055 D15);
    /// an execution that cannot be read carries none.
    pub(super) async fn gateway_acting(
        &self,
        agent_id: AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &TenantId,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
    ) -> GatewayActing {
        let contexts = self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
            .map(|execution| execution.input.contexts())
            .unwrap_or_default();
        GatewayActing {
            user_id: crate::application::execution::person_sub(caller_identity),
            agent_id,
            workflow_id: self.workflow_of(execution_id, tenant_id).await,
            contexts,
        }
    }

    /// The workflow whose run `execution_id` is a state of, if any.
    async fn workflow_of(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &TenantId,
    ) -> Option<uuid::Uuid> {
        let workflow_executions = self.workflow_execution_repo.as_ref()?;
        let execution = self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
            .ok()?;
        let workflow_execution_id = execution.input.workflow_execution_id?;
        workflow_executions
            .find_by_id_for_tenant(
                tenant_id,
                crate::domain::execution::ExecutionId(workflow_execution_id),
            )
            .await
            .ok()
            .flatten()
            .map(|run| run.workflow_id.as_uuid())
    }

    /// The registered remote server `tool_name` belongs to, and the tool's
    /// own name on it (`<server>.<tool>`; a server's name has no dot).
    fn remote_tool_of<'a>(&self, tool_name: &'a str) -> Option<(&'a str, &'a str)> {
        let (server, tool) = tool_name.split_once('.')?;
        (!tool.is_empty() && self.remote_tool_servers.iter().any(|name| name == server))
            .then_some((server, tool))
    }

    /// The acting user's credential for `server` (AEGIS ADR-132 H1, H3,
    /// H6), or the refusal `CREDENTIAL_BINDING_REQUIRED`: for a run with no
    /// recorded person, saying so; for a user with no binding to the server
    /// granted to this agent or its workflow; for an execution whose dispatch
    /// chose none for the server, or chose a binding that is not the user's
    /// own active one for it (Zaru ADR-0055 D15). No other credential path
    /// is tried.
    async fn credential_for(
        &self,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        server: &str,
    ) -> Result<SensitiveString, SealSessionError> {
        let binding_required = |message: String| {
            SealSessionError::NotFound(message.clone())
                .answered(CallerAnswer::CredentialBindingRequired { message })
        };
        let Some(user_id) = acting.user_id.as_deref() else {
            return Err(binding_required(format!(
                "This tool needs your own credential for '{server}', and no person is recorded for this run."
            )));
        };
        let source = self.tool_credentials.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(format!(
                "no credential source is configured for the remote tool server '{server}'"
            ))
            .answered(CallerAnswer::Internal(InternalFailure::Unavailable))
        })?;
        let context = acting.contexts.choice(server);
        let actor = ToolCallActor {
            tenant_id,
            user_id,
            agent_id: acting.agent_id,
            workflow_id: acting.workflow_id,
            context,
        };
        match source.tool_server_credential(&actor, server).await {
            Ok(Some(credential)) => Ok(credential),
            Ok(None) => Err(binding_required(match context {
                crate::domain::execution::ContextChoice::None => format!(
                    "This tool needs your own credential for '{server}', and none was chosen for this run."
                ),
                crate::domain::execution::ContextChoice::Binding(_) => format!(
                    "This tool needs your own credential for '{server}', and the one chosen for this run is not an active credential of yours for it."
                ),
                crate::domain::execution::ContextChoice::NotGiven => format!(
                    "This tool needs your own credential for '{server}', granted to this agent."
                ),
            })),
            Err(e) => Err(SealSessionError::InternalError(format!(
                "resolving the credential for the remote tool server '{server}' failed: {e}"
            ))),
        }
    }

    /// Whether the configured gateway address is a TLS one: a person's
    /// credential rides only such a channel (AEGIS ADR-132 H8).
    pub(super) fn gateway_channel_is_confidential(&self) -> bool {
        self.seal_gateway_url
            .as_deref()
            .is_some_and(gateway_url_is_tls)
    }

    /// Call `tool` of the remote MCP server `server` through the gateway's
    /// `InvokeTool` (AEGIS ADR-132 H1, H2, H4): the tenant, the acting
    /// identity, the server, the tool, the arguments as given and the acting
    /// user's resolved credential, which the gateway presents to the server
    /// for this call only. The server's `tools/call` result passes back
    /// unchanged. Nothing is sent when there is no credential to send, or
    /// when the gateway's address is plaintext.
    async fn invoke_remote_tool(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, SealSessionError> {
        let tool_name = format!("{server}.{tool}");
        let credential = self.credential_for(tenant_id, acting, server).await?;
        if !self.gateway_channel_is_confidential() {
            tracing::error!(
                server = %server,
                tool = %tool_name,
                "a remote tool call would carry the acting user's credential to the SEAL gateway \
                 over a plaintext channel; nothing was sent. Configure seal_gateway.url as an \
                 https address (and seal_gateway.ca_cert_path for a private CA)"
            );
            return Err(SealSessionError::InternalError(format!(
                "the SEAL gateway channel is not confidential; the call of '{tool_name}' was not sent"
            ))
            .answered(CallerAnswer::CredentialChannelNotConfidential));
        }

        let mut client = self.connect_gateway().await?;
        let mut request = tonic::Request::new(InvokeToolRequest {
            execution_id: execution_id.to_string(),
            tenant_id: tenant_id.as_str().to_string(),
            acting: Some(acting.to_proto()),
            server: server.to_string(),
            tool: tool.to_string(),
            arguments_json: args.to_string(),
            credential: Some(ResolvedCredential {
                kind: CredentialKind::BearerToken as i32,
                value: credential.expose().to_string(),
            }),
        });
        drop(credential);
        self.authorize_gateway_request(&mut request).await?;
        let response =
            match tokio::time::timeout(GATEWAY_INVOKE_TIMEOUT, client.invoke_tool(request)).await {
                Ok(Ok(resp)) => resp.into_inner(),
                Ok(Err(status)) => return Err(gateway_refusal("invoke_tool", &tool_name, &status)),
                Err(_) => {
                    return Err(SealSessionError::InternalError(format!(
                        "seal tooling gateway invoke_tool timeout after {}s",
                        GATEWAY_INVOKE_TIMEOUT.as_secs()
                    )));
                }
            };
        parse_gateway_result(&response.result_json)
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
        acting: &GatewayActing,
    ) -> Result<serde_json::Value, SealSessionError> {
        if let Some((server, tool)) = self.remote_tool_of(tool_name) {
            return self
                .invoke_remote_tool(execution_id, tenant_id, acting, server, tool, args)
                .await;
        }
        let listing = ListToolsRequest {
            tenant_id: tenant_id.as_str().to_string(),
            acting: Some(acting.to_proto()),
            bound_servers: Vec::new(),
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
                acting: Some(acting.to_proto()),
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
            acting: Some(acting.to_proto()),
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

impl ToolInvocationService {
    /// `cortex.ground` on the remote server `server` with `token`, a token a
    /// person is storing, rotating or introspecting (AEGIS ADR-132 (7a) S2):
    /// the gateway's `InvokeTool` with arguments `{}`, the token as the
    /// call's credential and the binding's owner as the acting identity. The
    /// orchestrator makes no connection of its own to the server (H2). The
    /// grounding payload is the response's `grounding_json` when the gateway
    /// answers it (H9a: the `_grounding` the server answered on this call's
    /// `initialize`, with the same token), parsed as JSON, and the result is
    /// then not read, `isError` included; JSON that does not parse is
    /// `INTERNAL_ERROR`. When `grounding_json` is empty, the payload is the
    /// text of the result's first content item, parsed as JSON; a result
    /// that carries none answers `null`, which the caller reads as a
    /// grounding that reports no reach. Nothing is sent over a plaintext
    /// gateway address (H8).
    pub async fn ground_remote_token(
        &self,
        tenant_id: &TenantId,
        user_id: &str,
        server: &str,
        token: &SensitiveString,
    ) -> Result<serde_json::Value, GroundingRefusal> {
        let tool_name = format!("{server}.cortex.ground");
        let unreachable = |code: &str, detail: String| GroundingRefusal::Unreachable {
            code: code.to_string(),
            detail,
        };
        if !self.gateway_channel_is_confidential() {
            tracing::error!(
                server = %server,
                "grounding a remote server's token would carry it to the SEAL gateway over a \
                 plaintext channel; nothing was sent. Configure seal_gateway.url as an https \
                 address (and seal_gateway.ca_cert_path for a private CA)"
            );
            return Err(unreachable(
                "CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL",
                format!("the SEAL gateway channel is not confidential; '{tool_name}' was not sent"),
            ));
        }
        let mut client = self
            .connect_gateway()
            .await
            .map_err(|e| unreachable("UPSTREAM_UNAVAILABLE", e.to_string()))?;
        let mut request = tonic::Request::new(InvokeToolRequest {
            execution_id: String::new(),
            tenant_id: tenant_id.as_str().to_string(),
            acting: Some(ActingIdentity {
                user_id: user_id.to_string(),
                agent_id: String::new(),
                workflow_id: String::new(),
            }),
            server: server.to_string(),
            tool: "cortex.ground".to_string(),
            arguments_json: "{}".to_string(),
            credential: Some(ResolvedCredential {
                kind: CredentialKind::BearerToken as i32,
                value: token.expose().to_string(),
            }),
        });
        self.authorize_gateway_request(&mut request)
            .await
            .map_err(|e| unreachable("SERVICE_UNAVAILABLE", e.to_string()))?;
        let response =
            match tokio::time::timeout(GATEWAY_INVOKE_TIMEOUT, client.invoke_tool(request)).await {
                Ok(Ok(response)) => response.into_inner(),
                Ok(Err(status)) => {
                    let code = status
                        .metadata()
                        .get(REFUSAL_CODE_METADATA)
                        .and_then(|value| value.to_str().ok());
                    return Err(match code {
                        Some(code) => GroundingRefusal::Refused {
                            code: code.to_string(),
                            message: status.message().to_string(),
                        },
                        None => unreachable(
                            "INTERNAL_ERROR",
                            format!(
                            "seal tooling gateway invoke_tool refused '{tool_name}' (no code): {}",
                            status.message()
                        ),
                        ),
                    });
                }
                Err(_) => {
                    return Err(unreachable(
                        "UPSTREAM_UNAVAILABLE",
                        format!(
                            "seal tooling gateway invoke_tool timeout after {}s",
                            GATEWAY_INVOKE_TIMEOUT.as_secs()
                        ),
                    ));
                }
            };
        if !response.grounding_json.is_empty() {
            return serde_json::from_str(&response.grounding_json)
                .map_err(|e| unreachable("INTERNAL_ERROR", e.to_string()));
        }
        let result = parse_gateway_result(&response.result_json)
            .map_err(|e| unreachable("INTERNAL_ERROR", e.to_string()))?;
        grounding_payload(&result)
    }
}

/// The grounding payload in a `tools/call` result: its first content item's
/// text, parsed as JSON (`null` when there is none). A result marked
/// `isError` is the server's refusal, `REMOTE_TOOL_ERROR`, with its text.
fn grounding_payload(result: &serde_json::Value) -> Result<serde_json::Value, GroundingRefusal> {
    let text = result
        .get("content")
        .and_then(serde_json::Value::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(serde_json::Value::as_str);
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        return Err(GroundingRefusal::Refused {
            code: "REMOTE_TOOL_ERROR".to_string(),
            message: text.unwrap_or("The server refused the call.").to_string(),
        });
    }
    Ok(text
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(serde_json::Value::Null))
}

#[async_trait::async_trait]
impl RemoteServerGrounding for ToolInvocationService {
    async fn ground_token(
        &self,
        tenant_id: &TenantId,
        user_id: &str,
        server: &str,
        token: &SensitiveString,
    ) -> Result<serde_json::Value, GroundingRefusal> {
        self.ground_remote_token(tenant_id, user_id, server, token)
            .await
    }
}

#[cfg(test)]
#[path = "gateway_wire_tests.rs"]
mod gateway_wire_tests;
