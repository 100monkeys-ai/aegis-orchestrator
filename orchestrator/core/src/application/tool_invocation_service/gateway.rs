use super::*;
use crate::application::credential_service::{
    binding_digits, ContextBinding, GroundingRefusal, RemoteServerGrounding, ToolCallActor,
};
use crate::domain::execution::{ContextChoice, ServerChoice};
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

/// The argument by which a call of a remote server's tool says which of
/// several chosen contexts it uses (AEGIS ADR-132 Update (13) S11d, S11e).
/// The name is the platform's, as `_meta` and `_grounding` are; it never
/// leaves the orchestrator.
pub(crate) const CONTEXT_ARGUMENT: &str = "_context";

/// The longest a `_context` value a refusal shows may be (S11e).
const CONTEXT_SHOWN_MAX_CHARS: usize = 64;

/// The refusal `CREDENTIAL_BINDING_REQUIRED` with `message` (AEGIS ADR-132
/// H3, Zaru ADR-0055 D15, ADR-132 Update (13) S11e).
fn binding_required(message: String) -> SealSessionError {
    SealSessionError::NotFound(message.clone())
        .answered(CallerAnswer::CredentialBindingRequired { message })
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
        let contexts_of_run = contexts.clone();
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
            tools.extend(self.listed_per_binding(tenant_id, &acting).await);
        }
        let mut tools: Vec<_> = tools
            .into_iter()
            .filter(|tool| {
                let declared = declared_tools.iter().any(|name| name == &tool.name)
                    || self.remote_tool_of(&tool.name).is_some_and(|(server, _)| {
                        filled_contexts.iter().any(|service| service == server)
                    });
                declared && security_context.permits_tool_name(&tool.name)
            })
            .collect();
        let person = execution
            .as_ref()
            .and_then(|execution| execution.initiating_user_sub.as_deref());
        self.name_chosen_mailboxes(tenant_id, person, &contexts_of_run, &mut tools)
            .await;
        Ok(tools)
    }

    /// AEGIS ADR-125's Update of 2026-10-07 (2) clause 2, ADR-132 Update
    /// (13) S11i: with the run's `imap` set filled, each mail tool's
    /// `mailbox` is an `enum` of the chosen mailboxes' names, described as
    /// "Which of your mailboxes this call uses: " and the names joined by
    /// "; ", so no id reaches a model. ADR-138 K5a gives the calendar tools
    /// the same rule: with the run's `caldav` set filled, `account` is an
    /// `enum` of the chosen accounts' names, described as "Which of your
    /// calendar accounts this call uses: ". With nothing chosen the router's
    /// schema stands.
    async fn name_chosen_mailboxes(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        contexts: &crate::domain::execution::ExecutionContexts,
        tools: &mut [crate::infrastructure::tool_router::ToolMetadata],
    ) {
        self.name_chosen_bindings(
            tenant_id,
            person,
            contexts,
            tools,
            ChosenNames {
                key: crate::application::tools::builtin_mail::MailActing::choice_key(),
                property: "mailbox",
                described: "Which of your mailboxes this call uses: ",
                of_tool: crate::application::tools::builtin_mail::is_mail_tool,
            },
        )
        .await;
        self.name_chosen_bindings(
            tenant_id,
            person,
            contexts,
            tools,
            ChosenNames {
                key: crate::application::tools::builtin_calendar::CalendarActing::choice_key(),
                property: "account",
                described: "Which of your calendar accounts this call uses: ",
                of_tool: crate::application::tools::builtin_calendar::is_calendar_tool,
            },
        )
        .await;
    }

    /// With the run's set for `names.key` filled, each tool `names.of_tool`
    /// holds has its `names.property` rewritten to an `enum` of the chosen
    /// bindings' context names, described as `names.described` and the
    /// names joined by "; ".
    async fn name_chosen_bindings(
        &self,
        tenant_id: &TenantId,
        person: Option<&str>,
        contexts: &crate::domain::execution::ExecutionContexts,
        tools: &mut [crate::infrastructure::tool_router::ToolMetadata],
        names: ChosenNames,
    ) {
        let ServerChoice::Bindings(chosen) = contexts.server(names.key) else {
            return;
        };
        let (Some(person), Some(source)) = (person, &self.tool_credentials) else {
            return;
        };
        let named = match source.context_bindings(tenant_id, person, names.key).await {
            Ok(named) => named,
            Err(e) => {
                tracing::warn!(error = %e, key = names.key, "the run's chosen bindings could not be named; their tools keep their schema");
                return;
            }
        };
        let chosen_names: Vec<String> = chosen
            .iter()
            .filter_map(|id| named.iter().find(|b| &b.id == id))
            .map(|b| b.name.clone())
            .collect();
        if chosen_names.is_empty() {
            return;
        }
        let property = serde_json::json!({
            "type": "string",
            "enum": chosen_names,
            "description": format!("{}{}", names.described, chosen_names.join("; ")),
        });
        for tool in tools.iter_mut().filter(|tool| (names.of_tool)(&tool.name)) {
            if let Some(properties) = tool
                .input_schema
                .get_mut("properties")
                .and_then(serde_json::Value::as_object_mut)
            {
                properties.insert(names.property.to_string(), property.clone());
            }
        }
    }

    /// The tools of each remote server whose chosen set holds two or more
    /// bindings (AEGIS ADR-132 Update (13) S11d, S11k): one gateway
    /// `ListTools` per binding, each carrying exactly that binding's
    /// credential (the gateway lists every bound server under
    /// `<server>.<tool>`, so two credentials in one request would list the
    /// same names with nothing saying which answered). Each tool is listed
    /// once, with a required `_context` naming the bindings whose own
    /// listing carried it, in the set's order. A binding that is not the
    /// person's own active one for the server is not listed.
    async fn listed_per_binding(
        &self,
        tenant_id: &TenantId,
        acting: &GatewayActing,
    ) -> Vec<crate::infrastructure::tool_router::ToolMetadata> {
        let (Some(user_id), Some(source)) = (acting.user_id.as_deref(), &self.tool_credentials)
        else {
            return Vec::new();
        };
        if !self.gateway_channel_is_confidential() {
            return Vec::new();
        }
        let mut tools = Vec::new();
        for server in &self.remote_tool_servers {
            let ServerChoice::Bindings(chosen) = acting.contexts.server(server) else {
                continue;
            };
            if chosen.len() < 2 {
                continue;
            }
            let named = match source.context_bindings(tenant_id, user_id, server).await {
                Ok(named) => named,
                Err(e) => {
                    tracing::warn!(server = %server, error = %e, "the chosen contexts of a remote server could not be named; its tools are not listed");
                    continue;
                }
            };
            let mut by_tool: Vec<(ToolSummary, Vec<ContextBinding>)> = Vec::new();
            for id in &chosen {
                let Some(binding) = named.iter().find(|b| &b.id == id) else {
                    tracing::warn!(server = %server, binding = %binding_digits(id), "a chosen binding is not an active credential of the person's for the server; its tools are not listed");
                    continue;
                };
                let Some(bound) = self
                    .bound_server(
                        tenant_id,
                        user_id,
                        acting,
                        server,
                        ContextChoice::Binding(*id),
                    )
                    .await
                else {
                    continue;
                };
                let listing = ListToolsRequest {
                    tenant_id: tenant_id.as_str().to_string(),
                    acting: Some(acting.to_proto()),
                    bound_servers: vec![bound],
                };
                let listed = match self
                    .list_gateway_tools(listing, GATEWAY_LIST_TOOLS_TIMEOUT)
                    .await
                {
                    Ok(listed) => listed,
                    Err(e) => {
                        tracing::warn!(server = %server, error = %e, "SEAL gateway tool enumeration for one chosen context failed; its tools are not listed");
                        continue;
                    }
                };
                for item in listed {
                    if self
                        .remote_tool_of(&item.name)
                        .is_none_or(|(of, _)| of != server)
                    {
                        continue;
                    }
                    match by_tool.iter_mut().find(|(seen, _)| seen.name == item.name) {
                        Some((_, bindings)) => bindings.push(binding.clone()),
                        None => by_tool.push((item, vec![binding.clone()])),
                    }
                }
            }
            for (summary, bindings) in by_tool {
                if let Some(tool) = Self::with_context_property(
                    Self::gateway_tool_metadata(summary),
                    server,
                    &bindings,
                ) {
                    tools.push(tool);
                }
            }
        }
        tools
    }

    /// `tool` with S11d's required `_context`: a string whose `enum` is the
    /// names of `bindings`, described as
    /// `"Which of your <server> contexts this call uses: "` and each name
    /// with what it reaches, joined by `"; "`.
    /// `None`, and the operator's log says so, when the tool's own schema
    /// already declares `_context`.
    fn with_context_property(
        mut tool: crate::infrastructure::tool_router::ToolMetadata,
        server: &str,
        bindings: &[ContextBinding],
    ) -> Option<crate::infrastructure::tool_router::ToolMetadata> {
        if !tool.input_schema.is_object() {
            tool.input_schema = serde_json::json!({ "type": "object" });
        }
        let schema = tool.input_schema.as_object_mut()?;
        let properties = schema
            .entry("properties")
            .or_insert_with(|| serde_json::json!({}));
        if !properties.is_object() {
            *properties = serde_json::json!({});
        }
        let properties = properties.as_object_mut()?;
        if properties.contains_key(CONTEXT_ARGUMENT) {
            tracing::warn!(
                tool = %tool.name,
                "a remote tool declares the platform's '_context' argument itself; it is not listed while several contexts of its server are chosen"
            );
            return None;
        }
        let described: Vec<String> = bindings
            .iter()
            .map(|b| format!("{} ({})", b.name, b.reach_text()))
            .collect();
        properties.insert(
            CONTEXT_ARGUMENT.to_string(),
            serde_json::json!({
                "type": "string",
                "enum": bindings.iter().map(|b| b.name.clone()).collect::<Vec<_>>(),
                "description": format!(
                    "Which of your {server} contexts this call uses: {}",
                    described.join("; ")
                ),
            }),
        );
        let required = schema
            .entry("required")
            .or_insert_with(|| serde_json::json!([]));
        if !required.is_array() {
            *required = serde_json::json!([]);
        }
        if let Some(required) = required.as_array_mut() {
            required.push(serde_json::Value::String(CONTEXT_ARGUMENT.to_string()));
        }
        Some(tool)
    }

    /// Each remote server the acting person holds a binding to granted to
    /// the acting agent or its workflow, with that person's credential, for
    /// the gateway to list its tools (AEGIS ADR-132 H4); for a server the
    /// execution's dispatch chose one binding or none for, that choice
    /// instead (Zaru ADR-0055 D15). A server whose chosen set holds two or
    /// more bindings is not here: each of its bindings is listed by its own
    /// request (ADR-132 Update (13) S11d, S11k,
    /// [`Self::listed_per_binding`]). None for a run with no person, and
    /// none over a plaintext channel (H8): a credential is never sent there,
    /// and the reason is logged.
    async fn bound_servers(
        &self,
        tenant_id: &TenantId,
        acting: &GatewayActing,
    ) -> Vec<crate::infrastructure::seal_gateway_proto::BoundServer> {
        let (Some(user_id), Some(_)) = (acting.user_id.as_deref(), &self.tool_credentials) else {
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
            let Some(context) = acting.contexts.server(server).single() else {
                continue;
            };
            if let Some(server) = self
                .bound_server(tenant_id, user_id, acting, server, context)
                .await
            {
                bound.push(server);
            }
        }
        bound
    }

    /// `server` bound with the acting person's credential as `context`
    /// selects it, or `None` when it resolves none (the reason logged when
    /// it is a failure).
    async fn bound_server(
        &self,
        tenant_id: &TenantId,
        user_id: &str,
        acting: &GatewayActing,
        server: &str,
        context: ContextChoice,
    ) -> Option<crate::infrastructure::seal_gateway_proto::BoundServer> {
        let source = self.tool_credentials.as_ref()?;
        let actor = ToolCallActor {
            tenant_id,
            user_id,
            agent_id: acting.agent_id,
            workflow_id: acting.workflow_id,
            context,
        };
        match source.tool_server_credential(&actor, server).await {
            Ok(Some(credential)) => Some(crate::infrastructure::seal_gateway_proto::BoundServer {
                server: server.to_string(),
                credential: Some(ResolvedCredential {
                    kind: CredentialKind::BearerToken as i32,
                    value: credential.expose().to_string(),
                }),
            }),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    server = %server,
                    error = %e,
                    "the acting user's credential for a remote tool server could not be resolved; its tools are not listed"
                );
                None
            }
        }
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
    /// dispatch's choices are the execution record's (Zaru ADR-0055 D15,
    /// D17), and a call's own choices are then not read. When the execution
    /// has no record (a session attested for a conversation, with no
    /// execution), the choices are the call's (`call_contexts`, the payload's
    /// `_meta.contexts`, AEGIS ADR-132 S7), and none when it carries none.
    pub(super) async fn gateway_acting(
        &self,
        agent_id: AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &TenantId,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        call_contexts: Option<&crate::domain::execution::ExecutionContexts>,
    ) -> GatewayActing {
        let contexts = match self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
        {
            Ok(execution) => execution.input.contexts(),
            Err(_) => call_contexts.cloned().unwrap_or_default(),
        };
        GatewayActing {
            user_id: crate::application::execution::person_sub(caller_identity),
            agent_id,
            workflow_id: self.workflow_of(execution_id, tenant_id).await,
            contexts,
        }
    }

    /// The tools of each remote server a conversation chose a binding for
    /// (AEGIS ADR-132 S8), for `POST /v1/seal/context-tools`: the envelope
    /// is a `tools/list` request verified as a tool call's is (the session
    /// by its token, its status, its expiry, the signature, the replay
    /// window), its `params._meta.contexts` the choices. A session whose
    /// execution has a record is refused: an agent's run lists its own
    /// tools. Each named server known to the node and chosen with a binding
    /// is listed by the gateway with that binding's credential (D15's rules:
    /// the person's own active binding for the server, no grant); a server
    /// chosen `null`, or not named, is listed with none, so no grant path
    /// opens it. Only those servers' `<server>.<tool>` tools the session's
    /// security context permits are answered.
    pub async fn list_context_tools(
        &self,
        envelope: &(impl EnvelopeVerifier + Send + Sync),
        meta: Option<&Value>,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SealSessionError> {
        let mut session = self
            .seal_session_repo
            .find_active_by_security_token(envelope.security_token())
            .await
            .map_err(|e| {
                SealSessionError::InternalError(format!("session repository lookup failed: {e}"))
            })?
            .ok_or(SealSessionError::SessionInactive(
                crate::domain::seal_session::SessionStatus::Expired,
            ))?;
        verify_listing_envelope(&mut session, envelope)?;
        self.seal_middleware.check_replay(&session, envelope)?;
        let choices = match meta {
            Some(meta) => super::context_args::parse_contexts(meta)?,
            None => None,
        }
        .unwrap_or_default();
        if self
            .execution_service
            .get_execution_unscoped(session.execution_id)
            .await
            .is_ok()
        {
            return Err(SealSessionError::InvalidArguments(
                crate::domain::seal_session::EXECUTION_BOUND_SESSION_MESSAGE.to_string(),
            )
            .answered(CallerAnswer::ExecutionBoundSession));
        }
        // Every server the node knows: the bindings the call chose, else none.
        let mut every_server = serde_json::Map::new();
        for server in &self.remote_tool_servers {
            let choice = match choices.get(server) {
                Some(chosen @ (Value::String(_) | Value::Array(_))) => chosen.clone(),
                _ => Value::Null,
            };
            every_server.insert(server.clone(), choice);
        }
        let contexts = crate::domain::execution::ExecutionContexts::from_value(Some(
            &Value::Object(every_server),
        ));
        let chosen: Vec<&str> = self
            .remote_tool_servers
            .iter()
            .map(String::as_str)
            .filter(|server| contexts.is_filled(server))
            .collect();
        if chosen.is_empty() || self.seal_gateway_url.is_none() {
            return Ok(Vec::new());
        }
        let acting = GatewayActing {
            user_id: session.user_id.clone(),
            agent_id: session.agent_id,
            workflow_id: None,
            contexts,
        };
        let bound_servers = self.bound_servers(&session.tenant_id, &acting).await;
        let mut listed = Vec::new();
        if !bound_servers.is_empty() {
            let listing = ListToolsRequest {
                tenant_id: session.tenant_id.as_str().to_string(),
                acting: Some(acting.to_proto()),
                bound_servers,
            };
            listed.extend(
                self.list_gateway_tools(listing, GATEWAY_LIST_TOOLS_TIMEOUT)
                    .await?
                    .into_iter()
                    .map(Self::gateway_tool_metadata),
            );
        }
        listed.extend(self.listed_per_binding(&session.tenant_id, &acting).await);
        Ok(listed
            .into_iter()
            .filter(|tool| {
                self.remote_tool_of(&tool.name)
                    .is_some_and(|(server, _)| chosen.contains(&server))
                    && session.security_context.permits_tool_name(&tool.name)
            })
            .collect())
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
    pub(super) fn remote_tool_of<'a>(&self, tool_name: &'a str) -> Option<(&'a str, &'a str)> {
        let (server, tool) = tool_name.split_once('.')?;
        (!tool.is_empty() && self.remote_tool_servers.iter().any(|name| name == server))
            .then_some((server, tool))
    }

    /// The acting user's credential for `server` (AEGIS ADR-132 H1, H3,
    /// H6), or the refusal `CREDENTIAL_BINDING_REQUIRED`: for a run with no
    /// recorded person, saying so; for a user with no binding to the server
    /// granted to this agent or its workflow; for an execution whose dispatch
    /// chose none for the server, or chose a binding that is not the user's
    /// own active one for it (Zaru ADR-0055 D15). With a chosen set, the
    /// call's `_context` picks the binding (ADR-132 Update (13) S11e). No
    /// other credential path is tried.
    async fn credential_for(
        &self,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        server: &str,
        context_argument: Option<&Value>,
    ) -> Result<SensitiveString, SealSessionError> {
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
        let context = match acting.contexts.server(server) {
            ServerChoice::NotGiven => ContextChoice::NotGiven,
            ServerChoice::None => ContextChoice::None,
            ServerChoice::Bindings(chosen) => {
                self.pick_binding(tenant_id, user_id, server, &chosen, context_argument)
                    .await?
            }
        };
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
                ContextChoice::None => format!(
                    "This tool needs your own credential for '{server}', and none was chosen for this run."
                ),
                ContextChoice::Binding(_) => format!(
                    "This tool needs your own credential for '{server}', and the one chosen for this run is not an active credential of yours for it."
                ),
                ContextChoice::NotGiven => format!(
                    "This tool needs your own credential for '{server}', granted to this agent."
                ),
            })),
            Err(e) => Err(SealSessionError::InternalError(format!(
                "resolving the credential for the remote tool server '{server}' failed: {e}"
            ))),
        }
    }

    /// The binding of `chosen` a call of `server`'s tool uses (AEGIS ADR-132
    /// Update (13) S11e): with one, that one, unless `_context` names
    /// another; with several, the one whose name `_context` gives. A
    /// chosen binding that resolves no name is named by its first eight hex
    /// digits. Refused with the binding-required code otherwise.
    async fn pick_binding(
        &self,
        tenant_id: &TenantId,
        user_id: &str,
        server: &str,
        chosen: &[crate::domain::credential::CredentialBindingId],
        context_argument: Option<&Value>,
    ) -> Result<ContextChoice, SealSessionError> {
        if let ([only], None) = (chosen, context_argument) {
            return Ok(ContextChoice::Binding(*only));
        }
        let named = match &self.tool_credentials {
            Some(source) => source
                .context_bindings(tenant_id, user_id, server)
                .await
                .map_err(|e| {
                    SealSessionError::InternalError(format!(
                        "naming the chosen contexts of the remote tool server '{server}' failed: {e}"
                    ))
                })?,
            None => Vec::new(),
        };
        let names: Vec<(crate::domain::credential::CredentialBindingId, String)> = chosen
            .iter()
            .map(|id| {
                let name = named
                    .iter()
                    .find(|b| &b.id == id)
                    .map(|b| b.name.clone())
                    .unwrap_or_else(|| binding_digits(id));
                (*id, name)
            })
            .collect();
        let listed = names
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let Some(given) = context_argument else {
            return Err(binding_required(format!(
                "This tool reaches several of your '{server}' contexts; say which in '{CONTEXT_ARGUMENT}': {listed}."
            )));
        };
        let given = match given {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        match names.iter().find(|(_, name)| *name == given) {
            Some((id, _)) => Ok(ContextChoice::Binding(*id)),
            None => {
                let shown: String = given
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(CONTEXT_SHOWN_MAX_CHARS)
                    .collect();
                Err(binding_required(format!(
                    "'{shown}' is not one of the '{server}' contexts chosen for this run; choose one of: {listed}."
                )))
            }
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
    #[allow(clippy::too_many_arguments)]
    async fn invoke_remote_tool(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &TenantId,
        acting: &GatewayActing,
        server: &str,
        tool: &str,
        args: serde_json::Value,
        context_argument: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, SealSessionError> {
        let tool_name = format!("{server}.{tool}");
        let credential = self
            .credential_for(tenant_id, acting, server, context_argument.as_ref())
            .await?;
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
            // AEGIS ADR-132 Update (13) S11e: `_context` says which chosen
            // binding the call uses; it is taken out here, after the
            // narrative's requested line, and never reaches the gateway.
            let mut args = args;
            let context_argument = args
                .as_object_mut()
                .and_then(|arguments| arguments.remove(CONTEXT_ARGUMENT));
            return self
                .invoke_remote_tool(
                    execution_id,
                    tenant_id,
                    acting,
                    server,
                    tool,
                    args,
                    context_argument,
                )
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
    /// The grounding tool `tool` (`cortex.ground`, `get_me`, as
    /// `seal_gateway.remote_servers` names it, AEGIS ADR-136 G14) on the
    /// remote server `server` with `token`, a token a person is storing,
    /// rotating or introspecting (AEGIS ADR-132 (7a) S2):
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
        tool: &str,
        token: &SensitiveString,
    ) -> Result<serde_json::Value, GroundingRefusal> {
        let tool_name = format!("{server}.{tool}");
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
            tool: tool.to_string(),
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
        tool: &str,
        token: &SensitiveString,
    ) -> Result<serde_json::Value, GroundingRefusal> {
        self.ground_remote_token(tenant_id, user_id, server, tool, token)
            .await
    }
}

#[cfg(test)]
#[path = "gateway_wire_tests.rs"]
mod gateway_wire_tests;

/// The checks a tool call's envelope passes before its context is evaluated
/// (`SealSession::evaluate_call`'s first four), for the context-tools
/// listing (AEGIS ADR-132 S8), whose `tools/list` names no tool for a
/// security context to evaluate: the session is active and unexpired, the
/// envelope carries its token, the signature verifies, and the payload is a
/// `tools/list` request.
fn verify_listing_envelope(
    session: &mut crate::domain::seal_session::SealSession,
    envelope: &(impl EnvelopeVerifier + ?Sized),
) -> Result<(), SealSessionError> {
    use crate::domain::seal_session::SessionStatus;
    if session.status != SessionStatus::Active {
        return Err(SealSessionError::SessionInactive(session.status.clone()));
    }
    if chrono::Utc::now() > session.expires_at {
        session.status = SessionStatus::Expired;
        return Err(SealSessionError::SessionExpired);
    }
    if envelope.security_token() != &session.security_token_raw {
        return Err(SealSessionError::SignatureVerificationFailed(
            "security token does not match the active SEAL session".to_string(),
        ));
    }
    envelope.verify_signature(&session.agent_public_key)?;
    match envelope.extract_tool_name().as_deref() {
        Some("tools/list") => Ok(()),
        _ => Err(SealSessionError::MalformedPayload(
            "a context-tools listing is a 'tools/list' request".to_string(),
        )),
    }
}

/// Which tools a run's chosen bindings of one key name, and how
/// ([`ToolInvocationService::name_chosen_bindings`]).
struct ChosenNames {
    /// The choice key (`imap`, `caldav`).
    key: &'static str,
    /// The argument rewritten to the names' `enum`.
    property: &'static str,
    /// The description's opening, before the names.
    described: &'static str,
    /// Whether a tool is one of the key's.
    of_tool: fn(&str) -> bool,
}
