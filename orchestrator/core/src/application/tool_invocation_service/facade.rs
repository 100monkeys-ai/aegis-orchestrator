use super::*;
use crate::domain::iam::TenantScope;

#[allow(clippy::too_many_arguments)]
impl ToolInvocationService {
    /// Bind the `tenant_id` argument of an `aegis.*` tool call to the
    /// authenticated caller's tenant per [`TenantScope`].
    ///
    /// Semantics (ADR-097, ADR-100):
    /// 1. If `args.tenant_id` (or legacy `args.tenant`) is **absent**,
    ///    inject `scope.authenticated_tenant` and return it.
    /// 2. If present and equal to `scope.authenticated_tenant`, accept
    ///    the value and return it.
    /// 3. If present and **different**, reject with
    ///    [`SealSessionError::TenantMismatch`] — **except** when the
    ///    caller is a `ServiceAccount`, which may delegate per ADR-100, or an
    ///    `aegis:admin` under an active operator escalation (AEGIS ADR-129
    ///    D17, the tool-path equivalent of `X-Aegis-Tenant`).
    ///
    /// On acceptance, the canonical `tenant_id` is also written back into
    /// `args` (overwriting any legacy `tenant` key normalization) so that
    /// downstream handlers see a single, authoritative value.
    pub(super) fn enforce_tenant_arg(
        args: &mut Value,
        scope: &TenantScope,
    ) -> Result<TenantId, SealSessionError> {
        let supplied = args
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("tenant").and_then(|v| v.as_str()))
            .map(|s| s.to_string());

        let resolved = match supplied {
            None => scope.authenticated_tenant.clone(),
            Some(raw) => {
                let parsed = TenantId::from_string(&raw).map_err(|e| {
                    SealSessionError::InvalidArguments(format!(
                        "invalid tenant identifier '{raw}': {e}"
                    ))
                })?;
                if parsed == scope.authenticated_tenant
                    || scope.may_delegate()
                    || scope.may_name_tenant_as_admin()
                {
                    parsed
                } else {
                    return Err(SealSessionError::TenantMismatch {
                        authenticated: scope.authenticated_tenant.as_str().to_string(),
                        requested: parsed.as_str().to_string(),
                    });
                }
            }
        };

        if let Value::Object(map) = args {
            map.insert(
                "tenant_id".to_string(),
                Value::String(resolved.as_str().to_string()),
            );
        }

        Ok(resolved)
    }

    pub fn new(
        seal_session_repo: Arc<dyn SealSessionRepository>,
        security_context_repo: Arc<dyn SecurityContextRepository>,
        seal_middleware: Arc<SealMiddleware>,
        tool_router: Arc<ToolRouter>,
        fsal: Arc<AegisFSAL>,
        volume_registry: NfsVolumeRegistry,
        agent_lifecycle: Arc<dyn AgentLifecycleService>,
        execution_service: Arc<dyn ExecutionService>,
        web_tool_port: Arc<dyn ExternalWebToolPort>,
        event_bus: Arc<EventBus>,
        seal_gateway_url: Option<String>,
    ) -> Self {
        Self {
            seal_session_repo,
            security_context_repo,
            seal_middleware,
            tool_router,
            fsal,
            volume_registry,
            agent_lifecycle,
            execution_service,
            web_tool_port,
            event_bus,
            register_workflow_use_case: None,
            validation_service: None,
            workflow_repository: None,
            workflow_execution_repo: None,
            start_workflow_execution_use_case: None,
            generated_manifests_root: None,
            node_config_path: None,
            seal_gateway_url,
            seal_gateway_operator_token: None,
            seal_gateway_ca: None,
            tool_credentials: None,
            remote_tool_servers: Vec::new(),
            mail_tools: None,
            schema_registry: Arc::new(SchemaRegistry::build()),
            workflow_execution_control: None,
            agent_activity: None,
            tool_catalog: None,
            discovery_service: None,
            runtime_registry: None,
            program_runner: None,
            file_operations_service: None,
            user_volume_service: None,
            git_repo_service: None,
            script_service: None,
            edge_dispatcher: None,
            edge_resolver: None,
            edge_fleet_dispatcher: None,
            edge_fleet_cancel: None,
            tool_approval_service: None,
            operator_escalations: None,
            execution_repository: None,
            goal_service: None,
        }
    }

    /// AEGIS ADR-129: re-check and audit the operator escalation of every
    /// session attested under one.
    pub fn with_operator_escalations(
        mut self,
        service: Arc<crate::application::operator_escalation_service::OperatorEscalationService>,
    ) -> Self {
        self.operator_escalations = Some(service);
        self
    }

    /// AEGIS ADR-129 D17: the execution store the escalated all-tenant
    /// `aegis.task.list` reads.
    pub fn with_execution_repository(
        mut self,
        repository: Arc<dyn crate::domain::repository::ExecutionRepository>,
    ) -> Self {
        self.execution_repository = Some(repository);
        self
    }

    /// AEGIS ADR-126: enable the approval gate. A call of a tool the router
    /// marks `requires_approval` then waits for its user's answer.
    pub fn with_tool_approvals(
        mut self,
        service: Arc<crate::application::tool_approval_service::ToolApprovalService>,
    ) -> Self {
        self.tool_approval_service = Some(service);
        self
    }

    /// The approval gate's service, when enabled (the daemon's
    /// `/v1/tool-approvals` routes answer through it).
    pub fn tool_approvals(
        &self,
    ) -> Option<Arc<crate::application::tool_approval_service::ToolApprovalService>> {
        self.tool_approval_service.clone()
    }

    /// Authenticate calls to the SEAL gateway's gRPC services with this
    /// operator token source.
    pub fn with_seal_gateway_operator_token(
        mut self,
        source: Arc<crate::infrastructure::seal::operator_token::OperatorTokenSource>,
    ) -> Self {
        self.seal_gateway_operator_token = Some(source);
        self
    }

    /// Verify the SEAL gateway's TLS certificate against the CA in the PEM
    /// file at `path` (`seal_gateway.ca_cert_path`, AEGIS ADR-132 H8).
    /// Without it an `https` gateway is verified against the system's roots.
    /// A file that cannot be read, or holds no certificate, is an error.
    pub fn with_seal_gateway_ca_cert(mut self, path: &Path) -> anyhow::Result<Self> {
        let pem = std::fs::read(path).map_err(|e| {
            anyhow::anyhow!(
                "seal_gateway.ca_cert_path '{}' cannot be read: {e}",
                path.display()
            )
        })?;
        if !String::from_utf8_lossy(&pem).contains("-----BEGIN CERTIFICATE-----") {
            anyhow::bail!(
                "seal_gateway.ca_cert_path '{}' holds no PEM certificate",
                path.display()
            );
        }
        self.seal_gateway_ca = Some(tonic::transport::Certificate::from_pem(pem));
        Ok(self)
    }

    /// Resolve the acting user's credential for a remote tool server's call,
    /// checking the binding's grant to the calling agent or its workflow
    /// where the secret is read (AEGIS ADR-132 H1).
    pub fn with_tool_credentials(
        mut self,
        source: Arc<dyn crate::application::credential_service::ToolCredentialSource>,
    ) -> Self {
        self.tool_credentials = Some(source);
        self
    }

    /// The mail tools, resolving the acting person's mailbox through
    /// `mailboxes` and opening their sessions with the production connector
    /// (TLS and the address guard; AEGIS ADR-125 D4).
    pub fn with_mail_tools(
        self,
        mailboxes: Arc<dyn crate::application::credential_service::ToolMailboxSource>,
    ) -> Self {
        self.with_mail_tools_over(
            mailboxes,
            Arc::new(crate::infrastructure::mail::RustlsMailConnector::new()),
        )
    }

    /// The mail tools over another connector (tests: a plaintext one to
    /// loopback stand-ins).
    pub fn with_mail_tools_over(
        mut self,
        mailboxes: Arc<dyn crate::application::credential_service::ToolMailboxSource>,
        connector: Arc<dyn crate::infrastructure::mail::MailConnector>,
    ) -> Self {
        self.mail_tools = Some(Arc::new(
            crate::application::tools::builtin_mail::MailTools::with_connector(
                mailboxes, connector,
            ),
        ));
        self
    }

    /// The remote MCP servers registered with the SEAL gateway, by name (the
    /// gateway's `spec.mcp_servers[].name`). A tool named `<server>.<tool>`
    /// of one of them is called with `InvokeTool`, carrying the acting
    /// user's credential for the binding whose provider is the server's
    /// name (AEGIS ADR-132 H1, H4).
    pub fn with_remote_tool_servers(mut self, names: Vec<String>) -> Self {
        self.remote_tool_servers = names;
        self
    }

    /// The SEAL gateway's part of the service as the node configures it
    /// (AEGIS ADR-132 H1, H4, H8), the one way the daemon wires it:
    /// `seal_gateway.ca_cert_path`'s CA, `seal_gateway.remote_servers`'
    /// names, and `credentials`, the source each remote tool call resolves
    /// the acting user's credential through. The gateway's address is the
    /// one [`Self::new`] was given.
    ///
    /// With no `seal_gateway` (`None`) nothing changes: no CA, no remote
    /// server, no credential source, so a tool no builtin serves is
    /// answered as not found and nothing dials. A CA file that cannot be
    /// read, a server name the gateway could not register, or remote
    /// servers on a node with no credential store are refused with the
    /// reason, so the daemon does not start with them.
    pub fn with_seal_gateway_config(
        self,
        gateway: Option<&crate::domain::node_config::SealGatewayConfig>,
        credentials: Option<Arc<dyn crate::application::credential_service::ToolCredentialSource>>,
    ) -> anyhow::Result<Self> {
        let Some(gateway) = gateway else {
            return Ok(self);
        };
        let names = gateway.remote_server_names()?;
        let mut service = match &gateway.ca_cert_path {
            Some(ca) => self.with_seal_gateway_ca_cert(ca)?,
            None => self,
        };
        if names.is_empty() {
            return Ok(service);
        }
        let Some(credentials) = credentials else {
            anyhow::bail!(
                "seal_gateway.remote_servers names {names:?}, but this node has no credential \
                 store (no database) to resolve a person's credential from"
            );
        };
        if !service.gateway_channel_is_confidential() {
            tracing::warn!(
                remote_servers = ?names,
                "seal_gateway.url is not an https address: every call of a remote server's tool \
                 is refused CREDENTIAL_CHANNEL_NOT_CONFIDENTIAL and none of their tools is listed"
            );
        }
        service = service
            .with_remote_tool_servers(names)
            .with_tool_credentials(credentials);
        Ok(service)
    }

    /// ADR-117: enable the four-step edge dispatch pre-routing hook.
    pub fn with_edge_router(
        mut self,
        dispatcher: Arc<crate::application::edge::dispatch_to_edge::DispatchToEdgeService>,
        resolver: Arc<crate::application::edge::fleet::EdgeFleetResolver>,
        fleet_dispatcher: Arc<crate::application::edge::fleet::dispatcher::FleetDispatcher>,
        fleet_cancel: Arc<crate::application::edge::fleet::CancelFleetService>,
    ) -> Self {
        self.edge_dispatcher = Some(dispatcher);
        self.edge_resolver = Some(resolver);
        self.edge_fleet_dispatcher = Some(fleet_dispatcher);
        self.edge_fleet_cancel = Some(fleet_cancel);
        self
    }

    /// Enables built-in workflow authoring tools that require registration and semantic validation.
    pub fn with_workflow_authoring(
        mut self,
        register_workflow_use_case: Arc<dyn RegisterWorkflowUseCase>,
        validation_service: Arc<ValidationService>,
    ) -> Self {
        self.register_workflow_use_case = Some(register_workflow_use_case);
        self.validation_service = Some(validation_service);
        self
    }

    pub fn with_workflow_repository(
        mut self,
        workflow_repository: Arc<dyn crate::domain::repository::WorkflowRepository>,
    ) -> Self {
        self.workflow_repository = Some(workflow_repository);
        self
    }

    /// Attach a `WorkflowExecutionRepository` to enable `aegis.workflow.logs`
    /// and `aegis.task.logs`.
    pub fn with_workflow_execution_repo(
        mut self,
        repo: Arc<dyn crate::domain::repository::WorkflowExecutionRepository>,
    ) -> Self {
        self.workflow_execution_repo = Some(repo);
        self
    }

    pub fn with_workflow_execution(
        mut self,
        use_case: Arc<dyn StartWorkflowExecutionUseCase>,
    ) -> Self {
        self.start_workflow_execution_use_case = Some(use_case);
        self
    }

    /// Enables persistence of generated manifests to local disk.
    pub fn with_generated_manifests_root(mut self, root: PathBuf) -> Self {
        self.generated_manifests_root = Some(root);
        self
    }

    pub fn with_node_config_path(mut self, path: Option<PathBuf>) -> Self {
        self.node_config_path = path;
        self
    }

    /// Attach a `WorkflowExecutionControlPort` to enable `aegis.workflow.cancel`,
    /// `aegis.workflow.signal`, and `aegis.workflow.remove`.
    pub fn with_workflow_execution_control(
        mut self,
        port: Arc<dyn WorkflowExecutionControlPort>,
    ) -> Self {
        self.workflow_execution_control = Some(port);
        self
    }

    /// Attach an `AgentActivityPort` to enable `aegis.agent.logs`.
    pub fn with_agent_activity(mut self, port: Arc<dyn AgentActivityPort>) -> Self {
        self.agent_activity = Some(port);
        self
    }

    /// Attach a `StandardToolCatalog` to enable `aegis.tools.list` and `aegis.tools.search`.
    pub fn with_tool_catalog(mut self, catalog: Arc<StandardToolCatalog>) -> Self {
        self.tool_catalog = Some(catalog);
        self
    }

    /// Attach a `DiscoveryService` for semantic search over agents and workflows (ADR-075).
    pub fn with_discovery_service(
        mut self,
        svc: Arc<dyn crate::application::discovery_service::DiscoveryService>,
    ) -> Self {
        self.discovery_service = Some(svc);
        self
    }

    /// Attach a `StandardRuntimeRegistry` to enable `aegis.runtime.list`.
    pub fn with_runtime_registry(
        mut self,
        registry: Arc<crate::domain::runtime_registry::StandardRuntimeRegistry>,
    ) -> Self {
        self.runtime_registry = Some(registry);
        self
    }

    /// Attach a `FileOperationsService` to enable `aegis.execution.file`.
    pub fn with_file_operations_service(
        mut self,
        svc: Arc<crate::application::file_operations_service::FileOperationsService>,
    ) -> Self {
        self.file_operations_service = Some(svc);
        self
    }

    /// Attach a `UserVolumeService` to enable `aegis.volume.*` tools.
    pub fn with_user_volume_service(
        mut self,
        svc: Arc<crate::application::user_volume_service::UserVolumeService>,
    ) -> Self {
        self.user_volume_service = Some(svc);
        self
    }

    /// Attach a `GitRepoService` to enable `aegis.git.*` tools.
    pub fn with_git_repo_service(
        mut self,
        svc: Arc<crate::application::git_repo_service::GitRepoService>,
    ) -> Self {
        self.git_repo_service = Some(svc);
        self
    }

    /// Attach a `ScriptService` to enable `aegis.script.*` tools.
    pub fn with_script_service(
        mut self,
        svc: Arc<crate::application::script_service::ScriptService>,
    ) -> Self {
        self.script_service = Some(svc);
        self
    }

    /// Return the `max_concurrent` limit for `cmd.run` from the first matching
    /// `Capability` in the named `SecurityContext`, if any.
    pub async fn get_cmd_run_max_concurrent(
        &self,
        _tenant_id: &TenantId,
        security_context_name: &str,
    ) -> anyhow::Result<Option<u32>> {
        let ctx = self
            .security_context_repo
            .find_by_name(security_context_name)
            .await?;
        Ok(ctx.and_then(|c| {
            c.capabilities
                .into_iter()
                .find(|cap| cap.matches_tool_name("cmd.run"))
                .and_then(|cap| cap.max_concurrent)
        }))
    }

    /// SEAL envelope-based tool invocation (Path 1).
    /// Verifies the SEAL envelope, validates tool input contracts, then delegates
    /// to `dispatch_tool_core` for unified tool dispatch.
    pub async fn invoke_tool(
        &self,
        envelope: &(impl EnvelopeVerifier + Send + Sync),
    ) -> Result<Value, SealSessionError> {
        self.invoke_tool_with_meta(envelope, None).await
    }

    /// [`Self::invoke_tool`] with the payload's `params._meta`, as the invoke
    /// route reads it from the signed payload. Its `contexts`
    /// (`{"<server>": "<binding id>" | null}`) is a conversation's choice of
    /// a binding per remote server (AEGIS ADR-132 S7): it chooses the
    /// credential of a remote tool call only when the session's execution
    /// has no record, and it never reaches the remote server. Any other
    /// shape is refused before anything runs.
    pub async fn invoke_tool_with_meta(
        &self,
        envelope: &(impl EnvelopeVerifier + Send + Sync),
        meta: Option<&Value>,
    ) -> Result<Value, SealSessionError> {
        // 1. Look up the active session by the opaque security_token string.
        let mut session = self
            .seal_session_repo
            .find_active_by_security_token(envelope.security_token())
            .await
            .map_err(|e| {
                SealSessionError::InternalError(format!("session repository lookup failed: {}", e))
            })?
            .ok_or(SealSessionError::SessionInactive(
                crate::domain::seal_session::SessionStatus::Expired,
            ))?;

        let agent_id = session.agent_id;
        let execution_id = session.execution_id;

        // 2. Middleware verifies signature and evaluates against SecurityContext
        let args = self
            .seal_middleware
            .verify_and_unwrap(&mut session, envelope)
            .await?;
        let tool_name = envelope
            .extract_tool_name()
            .ok_or(SealSessionError::MalformedPayload(
                "missing tool name".to_string(),
            ))?;

        // 2b. Validate required arguments against the tool's input contract (ADR-055).
        ToolInputContract::validate(&tool_name, &args)
            .map_err(SealSessionError::InvalidArguments)?;

        // 2b2. AEGIS ADR-132 S7: the conversation's choices, refused before
        // anything runs when malformed.
        let call_contexts = match meta {
            Some(meta) => super::context_args::parse_contexts(meta)?.map(|choices| {
                crate::domain::execution::ExecutionContexts::from_value(Some(&Value::Object(
                    choices,
                )))
            }),
            None => None,
        };

        // 2c. AEGIS ADR-129 D19: a session attested under an operator
        // escalation runs a call only while that escalation is active,
        // checked here at dispatch beside the session's policy evaluation.
        // A call dispatched before the end runs to its own completion; one
        // arriving after it is refused `operator_escalation_expired`.
        let escalation = self.active_session_escalation(&session).await?;

        // 3. Get security context and tenant_id from the session.
        let security_context = session.security_context;

        // 4. Tenant is already carried on the session — no DB lookup needed.
        let tenant_id = session.tenant_id.clone();

        // 5. Build caller identity from the session's user_id, if present.
        let seal_caller_identity: Option<crate::domain::iam::UserIdentity> = session
            .user_id
            .as_ref()
            .map(|uid| crate::domain::iam::UserIdentity {
                sub: uid.clone(),
                realm_slug: "zaru-consumer".to_string(),
                email: None,
                email_verified: false,
                name: None,
                identity_kind: crate::domain::iam::IdentityKind::ConsumerUser {
                    zaru_tier: crate::domain::iam::ZaruTier::from_security_context_name(
                        &security_context.name,
                    )
                    .unwrap_or(crate::domain::iam::ZaruTier::Free),
                    tenant_id: tenant_id.clone(),
                },
            });

        // 6. Build the authoritative TenantScope for this dispatch from the
        //    SEAL session's tenant + the reconstructed identity kind.
        let scope_identity_kind = seal_caller_identity
            .as_ref()
            .map(|id| id.identity_kind.clone())
            .unwrap_or_else(|| crate::domain::iam::IdentityKind::ConsumerUser {
                zaru_tier: crate::domain::iam::ZaruTier::from_security_context_name(
                    &security_context.name,
                )
                .unwrap_or(crate::domain::iam::ZaruTier::Free),
                tenant_id: tenant_id.clone(),
            });
        let mut tenant_scope = TenantScope::new(tenant_id.clone(), scope_identity_kind);
        if let Some(escalation) = &escalation {
            tenant_scope = tenant_scope.with_operator_escalation(
                crate::domain::iam::tenant_scope::EscalationScope {
                    aegis_role: escalation.aegis_role.clone(),
                },
            );
            self.audit_escalated_call(escalation, &tool_name, &args, &tenant_scope)
                .await?;
        }

        // 7. Delegate to unified dispatch core (iteration_number=0, empty audit history for SEAL path).
        let result = self
            .dispatch_tool_core(
                &agent_id,
                execution_id,
                &tenant_scope,
                &security_context,
                tool_name,
                args,
                0,
                Vec::new(),
                seal_caller_identity.as_ref(),
                call_contexts.as_ref(),
            )
            .await?;

        // 5. Map ToolInvocationResult to Value for SEAL return type.
        match result {
            ToolInvocationResult::Direct(value) => Ok(value),
            ToolInvocationResult::DispatchRequired(action) => Ok(serde_json::json!({
                "status": "dispatch_required",
                "action": format!("{:?}", action)
            })),
        }
    }

    /// The active operator escalation `session` was attested under, if any
    /// (AEGIS ADR-129 D19). A session bound to an escalation that has ended,
    /// or on a node without the escalation service, is refused.
    ///
    /// Once the escalation is found active, the operator's federated record
    /// is re-read on every escalated call (ADR-129 — Updates, V1): a record
    /// that no longer grants the escalation's role ends every active
    /// escalation of that person (`operator_demoted`) and refuses the call
    /// `operator_escalation_expired`; a record that cannot be read, or a
    /// node that cannot read it, refuses the call and ends nothing (V5).
    async fn active_session_escalation(
        &self,
        session: &crate::domain::seal_session::SealSession,
    ) -> Result<Option<crate::domain::operator_escalation::OperatorEscalation>, SealSessionError>
    {
        use crate::application::operator_escalation_service::OperatorEscalationError;
        let Some(binding) = &session.operator_escalation else {
            return Ok(None);
        };
        let service = self
            .operator_escalations
            .as_ref()
            .ok_or(SealSessionError::OperatorEscalationExpired)?;
        let checked = match service.check_active(binding.escalation_id).await {
            Ok(escalation) => service.confirm_role(&escalation).await.map(|()| escalation),
            Err(e) => Err(e),
        };
        match checked {
            Ok(escalation) => Ok(Some(escalation)),
            Err(OperatorEscalationError::Expired) => {
                Err(SealSessionError::OperatorEscalationExpired)
            }
            Err(e) => Err(SealSessionError::InternalError(format!(
                "operator escalation check failed: {e}"
            ))),
        }
    }

    /// Audit a call made under an operator escalation (AEGIS ADR-129 D18):
    /// the tool and the tenant it acts on, `*` when an escalated read can
    /// reach every tenant (D17). A call naming another tenant also emits
    /// `TenantEvent::AdminCrossTenantAccess` (ADR-056: non-optional). An
    /// audit row that cannot be written refuses the call.
    async fn audit_escalated_call(
        &self,
        escalation: &crate::domain::operator_escalation::OperatorEscalation,
        tool_name: &str,
        args: &Value,
        scope: &TenantScope,
    ) -> Result<(), SealSessionError> {
        let named = args
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("tenant").and_then(|v| v.as_str()));
        let target = match named {
            Some(tenant) => tenant.to_string(),
            None if Self::escalated_read_reaches_every_tenant(tool_name, args) => "*".to_string(),
            None => scope.authenticated_tenant.as_str().to_string(),
        };
        if let Some(named) = named {
            if let Ok(target_tenant) = TenantId::from_string(named) {
                if target_tenant != scope.authenticated_tenant && scope.may_name_tenant_as_admin() {
                    self.event_bus.publish_tenant_event(
                        crate::domain::events::TenantEvent::AdminCrossTenantAccess {
                            admin_identity: escalation.system_sub.clone(),
                            target_tenant_id: target_tenant,
                            accessed_at: Utc::now(),
                        },
                    );
                }
            }
        }
        let service = self
            .operator_escalations
            .as_ref()
            .ok_or(SealSessionError::OperatorEscalationExpired)?;
        service
            .audit_tool_call(escalation, tool_name, &target)
            .await
            .map_err(|e| {
                SealSessionError::InternalError(format!("operator escalation audit failed: {e}"))
            })
    }

    /// The reads an active escalation widens to every tenant when no
    /// `tenant_id` is named (AEGIS ADR-129 D17): `aegis.task.list` without
    /// an `agent_id`, and `aegis.task.status`, `aegis.task.logs` and
    /// `aegis.workflow.logs` by execution id.
    pub(super) fn escalated_read_reaches_every_tenant(tool_name: &str, args: &Value) -> bool {
        match tool_name {
            "aegis.task.list" => args.get("agent_id").is_none(),
            "aegis.task.status" | "aegis.task.logs" | "aegis.workflow.logs" => true,
            _ => false,
        }
    }

    /// Internal orchestrator-driven tool invocation (Gateway pattern).
    /// Resolves the SecurityContext from the execution's `security_context_name`
    /// (ADR-083), then delegates to `dispatch_tool_core`.
    ///
    /// `tenant_id` is passed by the caller — for the inner loop path this comes from
    /// `ExecutionContext::tenant_id` (sourced from the execution record at loop init),
    /// avoiding a redundant per-call DB lookup just to recover the tenant.
    pub async fn invoke_tool_internal(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: TenantId,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
        tool_name: String,
        args: Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // 1. Load the execution to obtain its security_context_name (ADR-083).
        // Use the unscoped lookup — the caller holds a trusted orchestrator-provisioned
        // ExecutionId. tenant_id is already provided by the caller.
        let execution = self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
            .map_err(|e| {
                SealSessionError::MalformedPayload(format!(
                    "Failed to load execution {execution_id}: {e}"
                ))
                .answered(crate::domain::seal_session::CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Server,
                ))
            })?;

        // Extract the caller identity from the parent execution's initiating_user_sub:
        // the person the execution acts for, or none. It is never a service
        // account's subject: a workflow-started agent state records the
        // workflow's starter, and a service account with no workflow records
        // none (AEGIS ADR-132's Update, G2; `execution::person_sub`). A row
        // written before that rule may still hold the worker's subject.
        let caller_identity: Option<crate::domain::iam::UserIdentity> = execution
            .initiating_user_sub
            .as_ref()
            .map(|sub| crate::domain::iam::UserIdentity {
                sub: sub.clone(),
                realm_slug: "zaru-consumer".to_string(),
                email: None,
                email_verified: false,
                name: None,
                identity_kind: crate::domain::iam::IdentityKind::ConsumerUser {
                    zaru_tier: crate::domain::iam::ZaruTier::Free,
                    tenant_id: execution.tenant_id.clone(),
                },
            });

        let security_context = self
            .security_context_repo
            .find_by_name(&execution.security_context_name)
            .await
            .map_err(|e| {
                SealSessionError::MalformedPayload(format!(
                    "Failed to load security context '{}': {e}",
                    execution.security_context_name
                ))
                .answered(crate::domain::seal_session::CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Server,
                ))
            })?
            .ok_or_else(|| {
                SealSessionError::MalformedPayload(format!(
                    "Security context '{}' not found for execution {execution_id}",
                    execution.security_context_name
                ))
                .answered(crate::domain::seal_session::CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Server,
                ))
            })?;

        // Build the authoritative TenantScope for this internal dispatch from
        // the parent execution's tenant + the reconstructed caller identity.
        let scope_identity_kind = caller_identity
            .as_ref()
            .map(|id| id.identity_kind.clone())
            .unwrap_or_else(|| crate::domain::iam::IdentityKind::ConsumerUser {
                zaru_tier: crate::domain::iam::ZaruTier::Free,
                tenant_id: tenant_id.clone(),
            });
        let tenant_scope = TenantScope::new(tenant_id.clone(), scope_identity_kind);

        // 2. Delegate to unified dispatch core.
        self.dispatch_tool_core(
            agent_id,
            execution_id,
            &tenant_scope,
            &security_context,
            tool_name,
            args,
            iteration_number,
            tool_audit_history,
            caller_identity.as_ref(),
            None,
        )
        .await
    }

    /// Unified tool dispatch core shared by both SEAL (`invoke_tool`) and
    /// container (`invoke_tool_internal`) paths. Contains ALL dispatch logic:
    /// - ADR-073 operator-only param stripping
    /// - SecurityContext policy enforcement
    /// - Inner-loop semantic judge (ADR-049)
    /// - aegis.* built-in tool dispatch
    /// - try_invoke_builtin fallback (cmd.run, fs.*, web.*, aegis.schema.*)
    /// - SEAL gateway fallback
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_tool_core(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_scope: &TenantScope,
        security_context: &crate::domain::security_context::SecurityContext,
        tool_name: String,
        args: Value,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        // A conversation's choices from the call's payload (AEGIS ADR-132
        // S7); `None` on every path but the invoke route's.
        call_contexts: Option<&crate::domain::execution::ExecutionContexts>,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // Convenience binding for code paths that only need the authenticated
        // tenant — agent lookups, judge spawning, gateway forwarding, etc.
        // All `aegis.*` arg-bearing tool dispatch goes through `enforce_tenant_arg`.
        let tenant_id = &tenant_scope.authenticated_tenant;
        let invocation_id = ToolInvocationId::new();
        let started_at = Instant::now();
        self.publish_invocation_requested(
            invocation_id,
            execution_id,
            *agent_id,
            &tool_name,
            &args,
        );

        // ADR-073: Strip operator-only parameters for consumer tier contexts.
        // Consumer security contexts (zaru-*) must not pass `force` or `version`
        // through to tool handlers — those are operator-level overrides.
        let mut args = args;
        if security_context.name.starts_with("zaru-") {
            if let Some(map) = args.as_object_mut() {
                for key in &["force", "version"] {
                    if map.remove(*key).is_some() {
                        tracing::info!(
                            param = *key,
                            "Stripped operator-only parameter from consumer tier tool call"
                        );
                    }
                }
            }
        }

        // Normalize relative paths for fs.* tools to /workspace before policy check.
        // The agent container's working directory is /workspace, so a relative path
        // like "solution.py" is equivalent to "/workspace/solution.py".
        if tool_name.starts_with("fs.") {
            if let Some(path_val) = args.get("path").and_then(|v| v.as_str()) {
                if !path_val.starts_with('/') {
                    let normalized = format!("/workspace/{}", path_val);
                    args["path"] = serde_json::Value::String(normalized);
                }
            }
        }

        // Enforce SecurityContext constraints (e.g. subcommand_allowlist for cmd.run)
        if let Err(violation) = security_context.evaluate(&tool_name, &args) {
            let (violation_type, details) = Self::map_policy_violation(&violation);
            self.event_bus
                .publish_mcp_event(MCPToolEvent::PolicyViolation {
                    execution_id,
                    agent_id: *agent_id,
                    tool_name: tool_name.clone(),
                    violation_type,
                    details: details.clone(),
                    blocked_at: Utc::now(),
                });
            // Record the blocked tool name on the iteration so validators can
            // surface policy violations to the judge agent (ADR-049).
            if let Err(e) = self
                .execution_service
                .store_policy_violation(execution_id, tool_name.clone())
                .await
            {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "Failed to record policy violation on iteration"
                );
            }
            self.publish_invocation_failed(
                invocation_id,
                execution_id,
                *agent_id,
                format!("Policy violation: {details}"),
            );
            return Err(SealSessionError::PolicyViolation(violation));
        }

        // --- Approval gate (ADR-126 D2) ---
        // After the security context allowed the call and before the
        // inner-loop judge: a call the policy forbids never reaches a person,
        // and a person is never asked about a call that is then rejected for
        // a reason they could not see.
        let mut auto_allowed = None;
        if let Some(approvals) = &self.tool_approval_service {
            if self.tool_router.requires_approval(&tool_name) {
                use crate::application::tool_approval_service::{
                    GateOutcome, GatedCall, ToolApprovalError,
                };
                let gated = approvals
                    .gate(GatedCall {
                        tenant_id,
                        user_sub: caller_identity.map(|id| id.sub.as_str()),
                        execution_id,
                        agent_id: *agent_id,
                        tool_name: &tool_name,
                        arguments: &args,
                        security_context_name: &security_context.name,
                        contract: self.tool_router.approval_contract(&tool_name),
                    })
                    .await;
                match gated {
                    Ok(GateOutcome::Proceed { approval_id }) => auto_allowed = Some(approval_id),
                    Ok(GateOutcome::Pending { result }) => {
                        self.publish_invocation_completed(
                            invocation_id,
                            execution_id,
                            *agent_id,
                            &result,
                            started_at,
                        );
                        return Ok(ToolInvocationResult::Direct(result));
                    }
                    Err(e) => {
                        self.publish_invocation_failed(
                            invocation_id,
                            execution_id,
                            *agent_id,
                            e.to_string(),
                        );
                        return Err(match e {
                            ToolApprovalError::RequiresUser(_) => {
                                SealSessionError::ConfigurationError(e.to_string())
                            }
                            other => SealSessionError::InternalError(other.to_string()),
                        });
                    }
                }
            }
        }

        let outcome = self
            .dispatch_after_gate_choosing(
                agent_id,
                execution_id,
                tenant_scope,
                security_context,
                tool_name,
                args,
                iteration_number,
                tool_audit_history,
                caller_identity,
                invocation_id,
                started_at,
                call_contexts,
            )
            .await;
        if let (Some(approval_id), Some(approvals)) = (auto_allowed, &self.tool_approval_service) {
            let recorded = match &outcome {
                Ok(ToolInvocationResult::Direct(value)) => Ok(value.clone()),
                Ok(ToolInvocationResult::DispatchRequired(_)) => {
                    Ok(serde_json::json!({"status": "dispatch_required"}))
                }
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = approvals.record_outcome(approval_id, &recorded).await {
                tracing::warn!(
                    approval_id = %approval_id,
                    error = %e,
                    "Failed to record the outcome of an auto-allowed tool call"
                );
            }
        }
        outcome
    }

    /// The dispatch stages after the approval gate (ADR-126 D2): the
    /// inner-loop judge, then edge, `aegis.*`, built-in, router and gateway
    /// dispatch. Called by [`Self::dispatch_tool_core`] for every call the
    /// gate lets through, and by the run of a stored call on its user's
    /// approval (`approvals.rs`), with the stored arguments and no call
    /// choices: a stored call carries no `_meta`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch_after_gate(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_scope: &TenantScope,
        security_context: &crate::domain::security_context::SecurityContext,
        tool_name: String,
        args: Value,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        invocation_id: ToolInvocationId,
        started_at: Instant,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        self.dispatch_after_gate_choosing(
            agent_id,
            execution_id,
            tenant_scope,
            security_context,
            tool_name,
            args,
            iteration_number,
            tool_audit_history,
            caller_identity,
            invocation_id,
            started_at,
            None,
        )
        .await
    }

    /// [`Self::dispatch_after_gate`] with a conversation's choices from the
    /// call's payload (AEGIS ADR-132 S7), handed to the gateway's acting
    /// identity.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_after_gate_choosing(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_scope: &TenantScope,
        security_context: &crate::domain::security_context::SecurityContext,
        tool_name: String,
        mut args: Value,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        invocation_id: ToolInvocationId,
        started_at: Instant,
        call_contexts: Option<&crate::domain::execution::ExecutionContexts>,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = &tenant_scope.authenticated_tenant;

        // --- Inner-Loop Semantic Pre-Execution Validation (ADR-049) ---
        // Agent lookup is optional — Zaru SEAL sessions use synthetic agent IDs
        // that don't correspond to registered agents. Skip the judge pipeline
        // when no agent manifest is available.
        let agent = self
            .agent_lifecycle
            .get_agent_visible(tenant_id, *agent_id)
            .await
            .ok();

        if let Some(ref agent) = agent {
            if let Some(exec_spec) = &agent.manifest.spec.execution {
                let should_skip_judge = self.tool_router.is_skip_judge(&tool_name).await;
                if should_skip_judge {
                    tracing::debug!(
                        tool_name = %tool_name,
                        "Inner-loop semantic judge skipped (skip_judge=true in node config for this tool)"
                    );
                } else if let Some(validation_pipeline) = &exec_spec.tool_validation {
                    for validator in validation_pipeline {
                        if let crate::domain::agent::ValidatorSpec::Semantic {
                            judge_agent,
                            criteria,
                            min_score,
                            min_confidence,
                            timeout_seconds,
                        } = validator
                        {
                            tracing::info!(
                                "Running inner-loop semantic validation for tool '{}' via judge '{}'",
                                tool_name,
                                judge_agent
                            );

                            let judge_id = self
                                .agent_lifecycle
                                .lookup_agent_visible_for_tenant(tenant_id, judge_agent)
                                .await
                                .map_err(|e| {
                                    SealSessionError::InternalError(format!(
                                        "Failed to lookup judge: {e}"
                                    ))
                                })?
                                .ok_or_else(|| {
                                    // The node's configured judge, not the caller's.
                                    SealSessionError::NotFound(format!(
                                        "Judge agent '{judge_agent}' not found"
                                    ))
                                    .answered(
                                        crate::domain::seal_session::CallerAnswer::Internal(
                                            crate::domain::seal_session::InternalFailure::Server,
                                        ),
                                    )
                                })?;

                            let worker_execution = self
                                .execution_service
                                .get_execution_unscoped(execution_id)
                                .await
                                .map_err(|e| {
                                    SealSessionError::InternalError(format!(
                                        "Inner-loop semantic judge: cannot read execution {execution_id}: {e}"
                                    ))
                                })?;
                            let execution_objective = Self::semantic_judge_task(
                                execution_id,
                                &worker_execution.input,
                                agent,
                            )?;
                            let available_tools = self
                                .get_available_tools_for_agent(tenant_id, *agent_id)
                                .await
                                .unwrap_or_default()
                                .into_iter()
                                .map(|t| t.name)
                                .collect::<Vec<String>>();
                            let worker_mounts = self
                                .volume_registry
                                .find_all_by_execution(execution_id)
                                .into_iter()
                                .map(|ctx| ctx.mount_point.to_string_lossy().to_string())
                                .collect::<Vec<String>>();

                            let input = ExecutionInput {
                                intent: None,
                                input: Self::build_semantic_judge_payload(
                                    execution_id,
                                    execution_objective,
                                    &tool_name,
                                    &args,
                                    available_tools,
                                    worker_mounts,
                                    criteria,
                                    INNER_LOOP_VALIDATION_CONTEXT,
                                    iteration_number,
                                    &tool_audit_history,
                                ),
                                workspace_volume_id: None,
                                workspace_volume_mount_path: None,
                                workspace_remote_path: None,
                                workflow_execution_id: None,
                                attachments: Vec::new(),
                            };

                            // Start the single iteration judge as child execution
                            let exec_id = self
                                .execution_service
                                .start_child_execution(judge_id, input, execution_id)
                                .await
                                .map_err(|e| {
                                    SealSessionError::InternalError(format!(
                                        "Failed to spawn judge child execution: {e}"
                                    ))
                                })?;

                            let poll_interval_ms = JUDGE_POLL_INTERVAL_MS;
                            let timeout_ms = timeout_seconds.saturating_mul(1000);
                            let max_attempts = timeout_ms
                                .saturating_add(poll_interval_ms.saturating_sub(1))
                                / poll_interval_ms;
                            let mut attempts = 0;

                            loop {
                                if attempts >= max_attempts {
                                    self.publish_invocation_failed(
                                    invocation_id,
                                    execution_id,
                                    *agent_id,
                                    format!(
                                        "Inner-loop semantic judge '{judge_agent}' timed out after {timeout_seconds} seconds"
                                    ),
                                );
                                    return Err(SealSessionError::JudgeTimeout(format!(
                                        "Inner-loop semantic judge '{judge_agent}' timed out after {timeout_seconds} seconds."
                                    )));
                                }

                                let exec = self
                                    .execution_service
                                    .get_execution_for_tenant(tenant_id, exec_id)
                                    .await
                                    .map_err(|e| {
                                        SealSessionError::InternalError(format!(
                                            "Failed to get judge execution {exec_id}: {e}"
                                        ))
                                    })?;

                                match exec.status {
                                    crate::domain::execution::ExecutionStatus::Completed => {
                                        let last_iter =
                                            exec.iterations().last().ok_or_else(|| {
                                                SealSessionError::InternalError(
                                                    "Judge completed but has no iterations"
                                                        .to_string(),
                                                )
                                            })?;
                                        let output_str =
                                            last_iter.output.as_ref().ok_or_else(|| {
                                                SealSessionError::InternalError(
                                                    "Judge completed but has no output".to_string(),
                                                )
                                            })?;

                                        let json_str = extract_json_from_text(output_str)
                                            .unwrap_or_else(|| output_str.clone());
                                        let result: crate::domain::validation::GradientResult =
                                            serde_json::from_str(&json_str).map_err(|e| {
                                                SealSessionError::InternalError(format!(
                                                    "Failed to parse judge output: {e}"
                                                ))
                                            })?;

                                        if !(result.score >= *min_score
                                            && result.confidence >= *min_confidence)
                                        {
                                            self.publish_invocation_failed(
                                            invocation_id,
                                            execution_id,
                                            *agent_id,
                                            format!(
                                                "Inner-loop tool execution rejected by semantic judge \
                                                 (Score: {:.2}, criteria_min: {:.2}). Reasoning: {}",
                                                result.score, min_score, result.reasoning
                                            ),
                                        );
                                            return Err(SealSessionError::InternalError(format!(
                                                "Inner-loop tool execution rejected by semantic judge \
                                                 (Score: {:.2}, criteria_min: {:.2}). Reasoning: {}",
                                                result.score, min_score, result.reasoning,
                                            ))
                                            .answered(
                                                crate::domain::seal_session::CallerAnswer::JudgeRejected(format!(
                                                    "The semantic judge rejected this tool call \
                                                     (score {:.2}, minimum {:.2}): {}",
                                                    result.score, min_score, result.reasoning,
                                                )),
                                            ));
                                        }
                                        break;
                                    }
                                    crate::domain::execution::ExecutionStatus::Failed
                                    | crate::domain::execution::ExecutionStatus::Cancelled => {
                                        self.publish_invocation_failed(
                                        invocation_id,
                                        execution_id,
                                        *agent_id,
                                        "Inner-loop semantic judge execution failed or was cancelled"
                                            .to_string(),
                                    );
                                        return Err(SealSessionError::InternalError("Inner-loop semantic judge execution failed or was cancelled".to_string()));
                                    }
                                    _ => {
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            poll_interval_ms,
                                        ))
                                        .await;
                                        attempts += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        } // end if let Some(ref agent)
          // --- End Pre-Execution Validation ---

        // Helper closure to publish invocation events based on tool result.
        let publish_result = |result: &Result<ToolInvocationResult, SealSessionError>| match result
        {
            Ok(ToolInvocationResult::Direct(value)) => self.publish_invocation_completed(
                invocation_id,
                execution_id,
                *agent_id,
                value,
                started_at,
            ),
            Ok(ToolInvocationResult::DispatchRequired(_)) => self.publish_invocation_completed(
                invocation_id,
                execution_id,
                *agent_id,
                &serde_json::json!({"status":"dispatch_required"}),
                started_at,
            ),
            Err(e) => self.publish_invocation_failed(
                invocation_id,
                execution_id,
                *agent_id,
                e.to_string(),
            ),
        };

        // Tenant arg injection / enforcement happens per-handler via
        // `Self::enforce_tenant_arg(&mut args, tenant_scope)` so that any
        // caller-supplied `tenant_id` mismatching the authenticated scope is
        // rejected (ADR-097) instead of silently honored. The previous
        // `entry().or_insert_with(...)` shortcut was a leak: it accepted any
        // caller-supplied value without comparing it to the session tenant.

        // ADR-117 §D: edge dispatch pre-routing hook. Resolves four cases in
        // strict order before the standard aegis.* / builtin / MCP / SEAL
        // chain runs:
        //   1. args.target.edge_node_id          → DispatchToEdge
        //   2. args.target.edge_selector         → resolve → DispatchToEdge
        //                                          (singular) or fail with
        //                                          MultiTargetRequiresFleetTool
        //   3. tool descriptor `executor=="edge"` and tenant has exactly one
        //      connected edge → DispatchToEdge that node
        //   4. otherwise fall through to the existing routing.
        if let Some(edge_result) = self
            .try_dispatch_via_edge(&tool_name, &args, security_context, tenant_scope)
            .await
        {
            publish_result(&edge_result);
            return edge_result;
        }

        // Built-in orchestrator aegis.* tool dispatch chain.
        let aegis_result = self
            .try_dispatch_aegis_tool(
                &tool_name,
                &mut args,
                execution_id,
                *agent_id,
                iteration_number,
                &tool_audit_history,
                security_context,
                caller_identity,
                tenant_scope,
            )
            .await;
        if let Some(result) = aegis_result {
            publish_result(&result);
            return result;
        }

        // A mail tool acts for the call's person, under its run's choice
        // and grant (AEGIS ADR-125 D4, its Update of 2026-10-07 clause 7).
        let mail_call = if crate::application::tools::builtin_mail::is_mail_tool(&tool_name) {
            Some(crate::application::tools::MailCall {
                tools: self.mail_tools.as_deref(),
                acting: self
                    .mail_acting(
                        *agent_id,
                        execution_id,
                        tenant_id,
                        caller_identity,
                        call_contexts,
                    )
                    .await,
            })
        } else {
            None
        };

        // Try invoking built-in tools (ADR-033, ADR-040, ADR-048)
        match crate::application::tools::try_invoke_builtin(
            &tool_name,
            &args,
            execution_id,
            &self.fsal,
            &self.volume_registry,
            &self.web_tool_port,
            &self.schema_registry,
            mail_call,
        )
        .await
        {
            Ok(crate::application::tools::BuiltinToolResult::Handled(result)) => {
                match &result {
                    ToolInvocationResult::Direct(value) => self.publish_invocation_completed(
                        invocation_id,
                        execution_id,
                        *agent_id,
                        value,
                        started_at,
                    ),
                    ToolInvocationResult::DispatchRequired(_) => self.publish_invocation_completed(
                        invocation_id,
                        execution_id,
                        *agent_id,
                        &serde_json::json!({"status":"dispatch_required"}),
                        started_at,
                    ),
                }
                return Ok(result);
            }
            Ok(crate::application::tools::BuiltinToolResult::NotBuiltin) => {} // Continue to dynamic routing
            Err(e) => {
                self.publish_invocation_failed(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    e.to_string(),
                );
                return Err(e);
            }
        }

        // No builtin serves the tool: the SEAL gateway is the one path for an
        // external tool (AEGIS ADR-132 G1, G4). The orchestrator runs no MCP
        // server of its own, so nothing here answers for a tool it did not run.
        // What the gateway answers reaches the caller: its result, or its
        // refusal by code (H5), never a not-found in place of a refusal.
        let outcome = if self.seal_gateway_url.is_some() {
            let acting = self
                .gateway_acting(
                    *agent_id,
                    execution_id,
                    tenant_id,
                    caller_identity,
                    call_contexts,
                )
                .await;
            self.invoke_seal_gateway_internal_grpc(
                execution_id,
                &tool_name,
                args,
                tenant_id,
                &acting,
            )
            .await
        } else {
            Err(super::gateway::tool_not_found(&tool_name))
        };
        match outcome {
            Ok(value) => {
                self.publish_invocation_completed(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    &value,
                    started_at,
                );
                Ok(ToolInvocationResult::Direct(value))
            }
            Err(e) => {
                // A tool nothing serves keeps the words its failure event has
                // always carried; any other failure carries its own.
                let message = match &e {
                    SealSessionError::Answered {
                        answer: crate::domain::seal_session::CallerAnswer::NotFound(_),
                        ..
                    } => format!("Tool not found: {tool_name}"),
                    other => other.to_string(),
                };
                self.publish_invocation_failed(invocation_id, execution_id, *agent_id, message);
                Err(e)
            }
        }
    }

    /// Who a mail tool's call acts for (AEGIS ADR-125's Update of
    /// 2026-10-07 clause 7): the call's person, the calling agent and its
    /// workflow, the choice for `imap` (the execution record's `contexts`
    /// when the execution has a record, else the call's `_meta.contexts`),
    /// a set of any number of mailboxes (its Update of 2026-10-07 (2)), and
    /// whether the execution has a record.
    async fn mail_acting(
        &self,
        agent_id: AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        tenant_id: &TenantId,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        call_contexts: Option<&crate::domain::execution::ExecutionContexts>,
    ) -> crate::application::tools::builtin_mail::MailActing {
        let has_execution_record = self
            .execution_service
            .get_execution_unscoped(execution_id)
            .await
            .is_ok();
        // The same person, workflow and choices a remote server's call
        // carries (AEGIS ADR-132 S7, Zaru ADR-0055 D15).
        let acting = self
            .gateway_acting(
                agent_id,
                execution_id,
                tenant_id,
                caller_identity,
                call_contexts,
            )
            .await;
        crate::application::tools::builtin_mail::MailActing {
            tenant_id: tenant_id.clone(),
            user_id: acting.user_id,
            agent_id: acting.agent_id,
            workflow_id: acting.workflow_id,
            choice: acting
                .contexts
                .server(crate::application::tools::builtin_mail::MailActing::choice_key()),
            has_execution_record,
        }
    }

    /// Attempt to dispatch an aegis.* tool by name. Returns `Some(result)` if
    /// the tool name matched an aegis.* handler, `None` if it should fall through
    /// to the builtin / gateway chain.
    #[allow(clippy::too_many_arguments)]
    async fn try_dispatch_aegis_tool(
        &self,
        tool_name: &str,
        args: &mut Value,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
        security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        tenant_scope: &TenantScope,
    ) -> Option<Result<ToolInvocationResult, SealSessionError>> {
        match tool_name {
            "aegis.agent.create" => {
                // The calling agent's name reaches the generator's floor
                // (AEGIS ADR-005 O7d).
                let caller = self.calling_agent_name(tenant_scope, agent_id).await;
                Some(
                    self.invoke_aegis_agent_create_tool(args, tenant_scope, caller.as_deref())
                        .await,
                )
            }
            "aegis.agent.update" => {
                let caller = self.calling_agent_name(tenant_scope, agent_id).await;
                Some(
                    self.invoke_aegis_agent_update_tool(args, tenant_scope, caller.as_deref())
                        .await,
                )
            }
            "aegis.agent.delete" => Some(
                self.invoke_aegis_agent_delete_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.agent.generate" => Some(
                self.invoke_aegis_agent_generate_tool(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.agent.export" => Some(
                self.invoke_aegis_agent_export_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.agent.list" => Some(self.invoke_aegis_agent_list_tool(args, tenant_scope).await),
            "aegis.agent.logs" => Some(self.invoke_aegis_agent_logs_tool(args, tenant_scope).await),
            "aegis.workflow.delete" => Some(
                self.invoke_aegis_workflow_delete_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.validate" => Some(self.invoke_aegis_workflow_validate_tool(args).await),
            "aegis.workflow.run" => Some(
                self.invoke_aegis_workflow_run_tool(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.workflow.executions.list" => Some(
                self.invoke_aegis_workflow_execution_list_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.executions.get" => Some(
                self.invoke_aegis_workflow_execution_get_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.status" => Some(
                self.invoke_aegis_workflow_status_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.generate" => Some(
                self.invoke_aegis_workflow_generate_tool(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.workflow.logs" => Some(
                self.invoke_aegis_workflow_logs_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.wait" => Some(
                self.invoke_aegis_workflow_wait_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.cancel" => Some(
                self.invoke_aegis_workflow_cancel_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.signal" => Some(
                self.invoke_aegis_workflow_signal_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.remove" => Some(
                self.invoke_aegis_workflow_remove_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.list" => Some(
                self.invoke_aegis_workflow_list_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.promote" => Some(
                self.invoke_aegis_workflow_promote_tool(args, security_context, tenant_scope)
                    .await,
            ),
            "aegis.workflow.demote" => Some(
                self.invoke_aegis_workflow_demote_tool(args, security_context, tenant_scope)
                    .await,
            ),
            "aegis.workflow.export" => Some(
                self.invoke_aegis_workflow_export_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.workflow.update" => Some(
                self.invoke_aegis_workflow_update_tool(args, execution_id, agent_id, tenant_scope)
                    .await,
            ),
            "aegis.workflow.create" => Some(
                self.invoke_aegis_workflow_create_tool(
                    args,
                    execution_id,
                    agent_id,
                    iteration_number,
                    tool_audit_history,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.task.execute" => Some(
                self.invoke_aegis_task_execute_tool(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.task.status" => {
                Some(self.invoke_aegis_task_status_tool(args, tenant_scope).await)
            }
            "aegis.task.wait" | "aegis.agent.wait" => {
                Some(self.invoke_aegis_task_wait_tool(args, tenant_scope).await)
            }
            "aegis.task.logs" => Some(self.invoke_aegis_task_logs_tool(args, tenant_scope).await),
            "aegis.task.list" => Some(self.invoke_aegis_task_list_tool(args, tenant_scope).await),
            "aegis.task.cancel" => {
                Some(self.invoke_aegis_task_cancel_tool(args, tenant_scope).await)
            }
            "aegis.task.remove" => {
                Some(self.invoke_aegis_task_remove_tool(args, tenant_scope).await)
            }
            "aegis.system.info" => Some(self.invoke_aegis_system_info_tool().await),
            "aegis.system.config" => Some(self.invoke_aegis_system_config_tool().await),
            "aegis.approval.status" => Some(
                self.invoke_aegis_approval_status_tool(args, caller_identity, tenant_scope)
                    .await,
            ),
            // ── AEGIS ADR-131 goals ─────────────────────────────────────
            "aegis.goal.create" => Some(
                self.invoke_aegis_goal_create_tool(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.goal.evaluate" => Some(
                self.invoke_aegis_goal_evaluate_tool(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.goal.status" => Some(
                self.invoke_aegis_goal_status_tool(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.goal.cancel" => Some(
                self.invoke_aegis_goal_cancel_tool(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            // ── ADR-117 Edge fleet system tools ────────────────────
            "aegis.edge.fleet.list" => Some(
                self.invoke_aegis_edge_fleet_list_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.edge.fleet.invoke" => Some(
                self.invoke_aegis_edge_fleet_invoke_tool(args, security_context, tenant_scope)
                    .await,
            ),
            "aegis.edge.fleet.cancel" => Some(
                self.invoke_aegis_edge_fleet_cancel_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.tools.list" => Some(self.invoke_aegis_tools_list(args, security_context).await),
            "aegis.tools.search" => {
                Some(self.invoke_aegis_tools_search(args, security_context).await)
            }
            "aegis.agent.search" => Some(
                self.invoke_aegis_agent_search_tool(args, security_context, tenant_scope)
                    .await,
            ),
            "aegis.workflow.search" => Some(
                self.invoke_aegis_workflow_search_tool(args, security_context, tenant_scope)
                    .await,
            ),
            "aegis.execute.intent" => Some(
                self.invoke_aegis_execute_intent_for_goal(
                    args,
                    security_context,
                    caller_identity,
                    tenant_scope,
                )
                .await,
            ),
            "aegis.execute.status" => Some(
                self.invoke_aegis_execute_status_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.execute.wait" => Some(
                self.invoke_aegis_workflow_wait_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.runtime.list" => Some(self.invoke_aegis_runtime_list_tool(args).await),
            // AEGIS ADR-135 D6: a document rendered by the renderer's fixed
            // program, answered as the file it produced.
            "aegis.document.render" => Some(
                self.invoke_aegis_document_render_tool(args, caller_identity, tenant_scope)
                    .await,
            ),

            // ── File operations (aegis.file.*) ─────────────────────────
            "aegis.file.list" => Some(
                self.invoke_aegis_file_list(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.file.read" => Some(
                self.invoke_aegis_file_read(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.file.write" => Some(
                self.invoke_aegis_file_write(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.file.delete" => Some(
                self.invoke_aegis_file_delete(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.file.mkdir" => Some(
                self.invoke_aegis_file_mkdir(args, caller_identity, tenant_scope)
                    .await,
            ),

            // ── Volume operations (aegis.volume.*) ─────────────────────
            "aegis.volume.create" => Some(
                self.invoke_aegis_volume_create(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.volume.delete" => {
                Some(self.invoke_aegis_volume_delete(args, caller_identity).await)
            }
            "aegis.volume.list" => Some(
                self.invoke_aegis_volume_list(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.volume.quota" => Some(
                self.invoke_aegis_volume_quota(args, caller_identity, tenant_scope)
                    .await,
            ),

            // ── Git operations (aegis.git.*) ───────────────────────────
            "aegis.git.clone" => Some(
                self.invoke_aegis_git_clone(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.commit" => Some(
                self.invoke_aegis_git_commit(args, caller_identity, tenant_scope, execution_id)
                    .await,
            ),
            "aegis.git.delete" => Some(
                self.invoke_aegis_git_delete(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.diff" => Some(
                self.invoke_aegis_git_diff(args, caller_identity, tenant_scope, execution_id)
                    .await,
            ),
            "aegis.git.list" => Some(
                self.invoke_aegis_git_list(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.push" => Some(
                self.invoke_aegis_git_push(args, caller_identity, tenant_scope, execution_id)
                    .await,
            ),
            "aegis.git.refresh" => Some(
                self.invoke_aegis_git_refresh(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.status" => Some(
                self.invoke_aegis_git_status(args, caller_identity, tenant_scope, execution_id)
                    .await,
            ),

            // ── Script operations (aegis.script.*) ─────────────────────
            "aegis.script.delete" => Some(
                self.invoke_aegis_script_delete(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.script.get" => Some(
                self.invoke_aegis_script_get(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.script.list" => Some(
                self.invoke_aegis_script_list(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.script.save" => Some(
                self.invoke_aegis_script_save(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.script.update" => Some(
                self.invoke_aegis_script_update(args, caller_identity, tenant_scope)
                    .await,
            ),

            "aegis.execution.file" => {
                let tenant_id = match Self::enforce_tenant_arg(args, tenant_scope) {
                    Ok(id) => id,
                    Err(e) => return Some(Err(e)),
                };
                match &self.file_operations_service {
                    Some(svc) => Some(
                        crate::application::tools::builtin_execution_file::invoke_execution_file_tool(
                            svc,
                            &tenant_id,
                            args,
                        )
                        .await,
                    ),
                    None => Some(Err(SealSessionError::InternalError(
                        "aegis.execution.file: file operations service not configured".to_string(),
                    )
                    .answered(crate::domain::seal_session::CallerAnswer::Internal(crate::domain::seal_session::InternalFailure::Unavailable)))),
                }
            }
            "aegis.attachment.read" => {
                let tenant_id = match Self::enforce_tenant_arg(args, tenant_scope) {
                    Ok(id) => id,
                    Err(e) => return Some(Err(e)),
                };
                match &self.file_operations_service {
                    Some(svc) => Some(
                        super::attachments::invoke_aegis_attachment_read_tool(
                            svc, &tenant_id, args,
                        )
                        .await,
                    ),
                    None => Some(Err(SealSessionError::InternalError(
                        "aegis.attachment.read: file operations service not configured".to_string(),
                    )
                    .answered(
                        crate::domain::seal_session::CallerAnswer::Internal(
                            crate::domain::seal_session::InternalFailure::Unavailable,
                        ),
                    ))),
                }
            }
            _ => None,
        }
    }

    /// ADR-117 §D: edge dispatch pre-routing hook.
    ///
    /// Returns `Some(result)` when the tool was dispatched (or rejected) via
    /// the EdgeRouter, `None` to fall through to the standard routing chain.
    ///
    /// Decision order (strict):
    ///   1. `args.target.edge_node_id` set → DispatchToEdge that node.
    ///   2. `args.target.edge_selector` set → resolve via EdgeFleetResolver.
    ///       - exactly 1 match → DispatchToEdge that node.
    ///       - >1 matches → reject with `MultiTargetRequiresFleetTool` (single-target path; fleet calls go through `aegis.edge.fleet.invoke`).
    ///   3. tool descriptor `executor=="edge"` and tenant has exactly one
    ///      connected edge → DispatchToEdge that node.
    ///   4. fall through.
    async fn try_dispatch_via_edge(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        security_context: &crate::domain::security_context::SecurityContext,
        tenant_scope: &TenantScope,
    ) -> Option<Result<ToolInvocationResult, SealSessionError>> {
        let dispatcher = self.edge_dispatcher.as_ref()?;
        let resolver = self.edge_resolver.as_ref()?;

        let tenant = &tenant_scope.authenticated_tenant;

        // Step 1 — explicit node id.
        let target = args.get("target");
        let explicit_node = target
            .and_then(|t| t.get("edge_node_id"))
            .and_then(|v| v.as_str())
            .and_then(|s| crate::domain::shared_kernel::NodeId::from_string(s).ok());

        // Step 2 — selector.
        let explicit_selector = target.and_then(|t| t.get("edge_selector")).cloned();

        let resolved_node: Option<crate::domain::shared_kernel::NodeId> = if let Some(n) =
            explicit_node
        {
            Some(n)
        } else if let Some(sel_value) = explicit_selector {
            let sel: crate::domain::edge::EdgeSelector = match serde_json::from_value(sel_value) {
                Ok(s) => s,
                Err(e) => {
                    return Some(Err(SealSessionError::MalformedPayload(format!(
                        "edge_selector parse: {e}"
                    ))));
                }
            };
            match resolver
                .resolve(tenant, &crate::domain::edge::EdgeTarget::Selector(sel))
                .await
            {
                Ok(nodes) if nodes.len() == 1 => Some(nodes[0]),
                Ok(_) => {
                    let message = "edge_selector matched multiple nodes; use \
                         aegis.edge.fleet.invoke for fan-out (\
                         MultiTargetRequiresFleetTool)";
                    return Some(Err(SealSessionError::InternalError(message.to_string())
                        .answered(
                            crate::domain::seal_session::CallerAnswer::InvalidArguments(format!(
                                "Invalid tool arguments: {message}"
                            )),
                        )));
                }
                Err(e) => {
                    return Some(Err(edge_refusal("edge resolve", e)));
                }
            }
        } else {
            // Step 3 — implicit single-edge tenant for executor=="edge" tools.
            let advertises_edge = self.tool_advertises_edge_executor(tool_name).await;
            if advertises_edge {
                match resolver
                    .resolve(tenant, &crate::domain::edge::EdgeTarget::All)
                    .await
                {
                    Ok(nodes) if nodes.len() == 1 => Some(nodes[0]),
                    Ok(_) => None, // ambiguous; fall through to other routing
                    Err(_) => None,
                }
            } else {
                None
            }
        };

        let node_id = resolved_node?;

        // Build args struct.
        let args_struct: prost_types::Struct =
            match crate::application::edge::json_value_to_prost_struct(args.clone()) {
                Ok(s) => s,
                Err(e) => {
                    return Some(Err(SealSessionError::MalformedPayload(format!(
                        "args must be a JSON object for edge dispatch: {e}"
                    ))));
                }
            };

        let dispatch_req = crate::application::edge::dispatch_to_edge::DispatchRequest {
            node_id,
            tenant_id: tenant.clone(),
            tool_name: tool_name.to_string(),
            args: args_struct,
            security_context_name: security_context.name.clone(),
            user_seal_envelope: crate::infrastructure::aegis_cluster_proto::SealEnvelope {
                user_security_token: String::new(),
                tenant_id: tenant.as_str().to_string(),
                security_context_name: security_context.name.clone(),
                payload: None,
                signature: vec![],
            },
            deadline: std::time::Duration::from_secs(60),
        };

        match dispatcher.dispatch(dispatch_req).await {
            Ok(result) => Some(Ok(ToolInvocationResult::Direct(serde_json::json!({
                "ok": result.ok,
                "exit_code": result.exit_code,
                "stdout": String::from_utf8_lossy(&result.stdout).to_string(),
                "stderr": String::from_utf8_lossy(&result.stderr).to_string(),
                "error_kind": result.error_kind,
                "error_message": result.error_message,
            })))),
            Err(e) => Some(Err(edge_refusal("edge dispatch", e))),
        }
    }

    /// `aegis.edge.fleet.list` — resolve an EdgeTarget and return matched +
    /// skipped lists without dispatching.
    async fn invoke_aegis_edge_fleet_list_tool(
        &self,
        args: &Value,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let resolver = self.edge_resolver.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(
                "edge fleet not configured on this orchestrator".to_string(),
            )
            .answered(crate::domain::seal_session::CallerAnswer::Internal(
                crate::domain::seal_session::InternalFailure::Unavailable,
            ))
        })?;
        let target_value = args
            .get("target")
            .ok_or_else(|| SealSessionError::MalformedPayload("missing target".to_string()))?;
        let target: crate::domain::edge::EdgeTarget = serde_json::from_value(target_value.clone())
            .map_err(|e| SealSessionError::MalformedPayload(format!("target parse: {e}")))?;
        let resolved = resolver
            .resolve(&tenant_scope.authenticated_tenant, &target)
            .await
            .map_err(|e| edge_refusal("resolve", e))?;
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "resolved": resolved.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            "skipped": Vec::<serde_json::Value>::new(),
        })))
    }

    /// `aegis.edge.fleet.invoke` — multi-target reverse-RPC dispatch via
    /// `FleetDispatcher::spawn`. Returns the fleet command id; per-node
    /// progress is observable via the FleetEvent stream surfaced over the
    /// REST `/v1/edge/fleet/invoke` SSE channel.
    async fn invoke_aegis_edge_fleet_invoke_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let resolver = self.edge_resolver.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(
                "edge fleet not configured on this orchestrator".to_string(),
            )
            .answered(crate::domain::seal_session::CallerAnswer::Internal(
                crate::domain::seal_session::InternalFailure::Unavailable,
            ))
        })?;
        let dispatcher = self.edge_fleet_dispatcher.as_ref().ok_or_else(|| {
            SealSessionError::InternalError("edge fleet dispatcher not configured".to_string())
                .answered(crate::domain::seal_session::CallerAnswer::Internal(
                    crate::domain::seal_session::InternalFailure::Unavailable,
                ))
        })?;

        let target_value = args
            .get("target")
            .ok_or_else(|| SealSessionError::MalformedPayload("missing target".to_string()))?;
        let target: crate::domain::edge::EdgeTarget = serde_json::from_value(target_value.clone())
            .map_err(|e| SealSessionError::MalformedPayload(format!("target parse: {e}")))?;
        let tool_name = args
            .get("tool_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SealSessionError::MalformedPayload("missing tool_name".to_string()))?
            .to_string();
        let inner_args = args.get("args").cloned().unwrap_or(serde_json::json!({}));
        let args_struct: prost_types::Struct =
            crate::application::edge::json_value_to_prost_struct(inner_args)
                .map_err(|e| SealSessionError::MalformedPayload(format!("args: {e}")))?;

        let resolved = resolver
            .resolve(&tenant_scope.authenticated_tenant, &target)
            .await
            .map_err(|e| edge_refusal("resolve", e))?;

        let policy = crate::domain::cluster::FleetDispatchPolicy {
            mode: crate::domain::cluster::FleetMode::Parallel,
            max_concurrency: None,
            failure_policy: crate::domain::cluster::FailurePolicy::ContinueOnError,
            require_min_targets: None,
            per_target_deadline: std::time::Duration::from_secs(60),
        };

        let inv = crate::application::edge::fleet::dispatcher::FleetInvocation {
            fleet_command_id: crate::domain::cluster::FleetCommandId::new(),
            tenant_id: tenant_scope.authenticated_tenant.clone(),
            tool_name,
            args: args_struct,
            security_context_name: security_context.name.clone(),
            user_seal_envelope: crate::infrastructure::aegis_cluster_proto::SealEnvelope {
                user_security_token: String::new(),
                tenant_id: tenant_scope.authenticated_tenant.as_str().to_string(),
                security_context_name: security_context.name.clone(),
                payload: None,
                signature: vec![],
            },
            resolved: resolved.clone(),
            policy,
        };
        let fleet_id = inv.fleet_command_id.0.to_string();
        // Spawn returns a Receiver of FleetEvent; we don't consume it here —
        // SSE consumers go through the REST endpoint.
        let _rx = dispatcher.clone().spawn(inv);
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "fleet_command_id": fleet_id,
            "resolved": resolved.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
        })))
    }

    /// `aegis.edge.fleet.cancel` — cancel a running fleet operation.
    ///
    /// Only the tenant that invoked the command, or an operator whose role
    /// may write, may cancel it (ADR-117 §F; ADR-073 §3e, §12). Any other
    /// caller gets `cancelled: false`, the answer for a command that does
    /// not exist.
    async fn invoke_aegis_edge_fleet_cancel_tool(
        &self,
        args: &Value,
        tenant_scope: &TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let cancel = self.edge_fleet_cancel.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(
                "edge fleet cancel not configured on this orchestrator".to_string(),
            )
            .answered(crate::domain::seal_session::CallerAnswer::Internal(
                crate::domain::seal_session::InternalFailure::Unavailable,
            ))
        })?;
        let id_str = args
            .get("fleet_command_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::MalformedPayload("missing fleet_command_id".to_string())
            })?;
        let uuid = uuid::Uuid::parse_str(id_str)
            .map_err(|e| SealSessionError::MalformedPayload(format!("fleet_command_id: {e}")))?;
        let cancelled = match crate::application::edge::fleet::FleetCancelAuthority::for_identity(
            &tenant_scope.identity_kind,
            &tenant_scope.authenticated_tenant,
        ) {
            Some(authority) => {
                cancel
                    .cancel(crate::domain::cluster::FleetCommandId(uuid), authority)
                    .await
            }
            None => false,
        };
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "cancelled": cancelled,
        })))
    }

    /// Look up a tool descriptor in the catalog and check whether it
    /// advertises `executor == "edge"`. Returns false when the catalog is not
    /// configured or the tool is not present.
    ///
    /// ADR-117 §D step 3: when a tool's descriptor declares `executor: "edge"`
    /// and the caller's tenant has exactly one connected edge daemon, the
    /// EdgeRouter dispatches the tool through that edge implicitly (no
    /// explicit `target.*` argument required). This accessor is the data
    /// source for that decision.
    async fn tool_advertises_edge_executor(&self, tool_name: &str) -> bool {
        let Some(catalog) = self.tool_catalog.as_ref() else {
            return false;
        };
        catalog
            .lookup(tool_name)
            .await
            .map(|entry| entry.executor.as_deref() == Some("edge"))
            .unwrap_or(false)
    }
}

/// An edge routing error, answered by its variant (AEGIS ADR-035, Update of
/// 2026-10-04, R5): each names only the caller's own edges, groups and
/// selectors. What the inner loop and the log see is `"<context>: <error>"`,
/// as before.
fn edge_refusal(context: &str, e: crate::domain::edge::EdgeRouterError) -> SealSessionError {
    use crate::domain::edge::EdgeRouterError;
    use crate::domain::seal_session::CallerAnswer;
    let answer = match &e {
        EdgeRouterError::NoMatchingEdges | EdgeRouterError::GroupNotFound(_) => {
            CallerAnswer::NotFound(format!("Not found: {e}."))
        }
        EdgeRouterError::CrossTenantAccessDenied { node_id, .. } => {
            CallerAnswer::TenantMismatch(format!("Edge {node_id} is not in your tenant."))
        }
        EdgeRouterError::InsufficientTargets { .. } => {
            CallerAnswer::InvalidArguments(format!("Invalid tool arguments: {e}"))
        }
        EdgeRouterError::EdgeUnavailable { .. }
        | EdgeRouterError::Timeout(_)
        | EdgeRouterError::EdgeDisconnected => {
            CallerAnswer::EdgeUnavailable(format!("Your edge did not answer: {e}."))
        }
    };
    SealSessionError::InternalError(format!("{context}: {e}")).answered(answer)
}
