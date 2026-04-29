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
    ///    caller is a `ServiceAccount`, which may delegate per ADR-100.
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
                if parsed == scope.authenticated_tenant || scope.may_delegate() {
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
            schema_registry: Arc::new(SchemaRegistry::build()),
            workflow_execution_control: None,
            agent_activity: None,
            tool_catalog: None,
            discovery_service: None,
            runtime_registry: None,
            file_operations_service: None,
            user_volume_service: None,
            git_repo_service: None,
            script_service: None,
            edge_dispatcher: None,
            edge_resolver: None,
            edge_fleet_dispatcher: None,
            edge_fleet_cancel: None,
        }
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
        let tenant_scope = TenantScope::new(tenant_id.clone(), scope_identity_kind);

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
            })?;

        // Extract the caller identity from the parent execution's initiating_user_sub.
        let caller_identity: Option<crate::domain::iam::UserIdentity> = execution
            .initiating_user_sub
            .as_ref()
            .map(|sub| crate::domain::iam::UserIdentity {
                sub: sub.clone(),
                realm_slug: "zaru-consumer".to_string(),
                email: None,
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
            })?
            .ok_or_else(|| {
                SealSessionError::MalformedPayload(format!(
                    "Security context '{}' not found for execution {execution_id}",
                    execution.security_context_name
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
    /// - ToolRouter dynamic routing
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
                                    SealSessionError::NotFound(format!(
                                        "Judge agent '{judge_agent}' not found"
                                    ))
                                })?;

                            let execution_objective = self
                                .execution_service
                                .get_execution_unscoped(execution_id)
                                .await
                                .ok()
                                .and_then(|exec| {
                                    exec.input
                                        .intent
                                        .or_else(|| {
                                            exec.input
                                                .input
                                                .get("input")
                                                .and_then(|v| v.as_str())
                                                .map(String::from)
                                        })
                                        .or_else(|| {
                                            exec.input
                                                .input
                                                .get("workflow_input")
                                                .and_then(|v| v.as_str())
                                                .map(String::from)
                                        })
                                })
                                .unwrap_or_else(|| "No objective available".to_string());
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
                                    "semantic_judge_pre_execution_inner_loop",
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
                                            )));
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

        // Try invoking built-in tools (ADR-033, ADR-040, ADR-048)
        match crate::application::tools::try_invoke_builtin(
            &tool_name,
            &args,
            execution_id,
            &self.fsal,
            &self.volume_registry,
            &self.web_tool_port,
            &self.schema_registry,
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

        let server_id = match self.tool_router.route_tool(execution_id, &tool_name).await {
            Ok(id) => {
                self.publish_invocation_started(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    id,
                    &tool_name,
                );
                id
            }
            Err(routing_err) => {
                if self.seal_gateway_url.is_some() {
                    let gateway_result = self
                        .invoke_seal_gateway_internal_grpc(
                            execution_id,
                            &tool_name,
                            args.clone(),
                            Some(tenant_id.as_str()),
                            None,
                        )
                        .await;
                    if let Ok(value) = gateway_result {
                        self.publish_invocation_completed(
                            invocation_id,
                            execution_id,
                            *agent_id,
                            &value,
                            started_at,
                        );
                        return Ok(ToolInvocationResult::Direct(value));
                    }
                }
                self.publish_invocation_failed(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    format!("Routing error: {routing_err}"),
                );
                return Err(SealSessionError::InternalError(format!(
                    "Routing error: {routing_err}"
                )));
            }
        };

        let server = match self.tool_router.get_server(server_id).await {
            Some(server) => server,
            None => {
                let err =
                    SealSessionError::InternalError("Server vanished after routing".to_string());
                self.publish_invocation_failed(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    err.to_string(),
                );
                return Err(err);
            }
        };

        match server.execution_mode {
            crate::domain::mcp::ExecutionMode::Local => {
                tracing::info!(
                    "Executing local tool via FSAL: {} for agent {:?}",
                    tool_name,
                    agent_id
                );
                let result = serde_json::json!({
                    "status": "success",
                    "execution_mode": "local_fsal",
                    "message": format!("Locally executed {} affecting agent volume", tool_name),
                    "args_executed": args
                });
                self.publish_invocation_completed(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    &result,
                    started_at,
                );
                Ok(ToolInvocationResult::Direct(result))
            }
            crate::domain::mcp::ExecutionMode::Remote => {
                tracing::info!(
                    "Proxying remote tool via JSON-RPC: {} to server {:?}",
                    tool_name,
                    server_id
                );
                let result = serde_json::json!({
                    "status": "success",
                    "execution_mode": "remote_jsonrpc",
                    "message": format!("Proxied {} to external MCP server {:?}", tool_name, server_id),
                    "args_proxied": args
                });
                self.publish_invocation_completed(
                    invocation_id,
                    execution_id,
                    *agent_id,
                    &result,
                    started_at,
                );
                Ok(ToolInvocationResult::Direct(result))
            }
        }
    }

    /// Attempt to dispatch an aegis.* tool by name. Returns `Some(result)` if
    /// the tool name matched an aegis.* handler, `None` if it should fall through
    /// to the builtin / ToolRouter / gateway chain.
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
            "aegis.agent.create" => Some(
                self.invoke_aegis_agent_create_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.agent.update" => Some(
                self.invoke_aegis_agent_update_tool(args, tenant_scope)
                    .await,
            ),
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
                self.invoke_aegis_workflow_generate_tool(args, tenant_scope)
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
            // ── ADR-117 Edge fleet system tools ────────────────────
            "aegis.edge.fleet.list" => Some(
                self.invoke_aegis_edge_fleet_list_tool(args, tenant_scope)
                    .await,
            ),
            "aegis.edge.fleet.invoke" => Some(
                self.invoke_aegis_edge_fleet_invoke_tool(args, security_context, tenant_scope)
                    .await,
            ),
            "aegis.edge.fleet.cancel" => Some(self.invoke_aegis_edge_fleet_cancel_tool(args).await),
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
                self.invoke_aegis_execute_intent_tool(args, security_context, tenant_scope)
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
                self.invoke_aegis_git_commit(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.delete" => Some(
                self.invoke_aegis_git_delete(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.diff" => Some(
                self.invoke_aegis_git_diff(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.list" => Some(
                self.invoke_aegis_git_list(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.push" => Some(
                self.invoke_aegis_git_push(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.refresh" => Some(
                self.invoke_aegis_git_refresh(args, caller_identity, tenant_scope)
                    .await,
            ),
            "aegis.git.status" => Some(
                self.invoke_aegis_git_status(args, caller_identity, tenant_scope)
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
                    ))),
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
                    return Some(Err(SealSessionError::InternalError(
                        "edge_selector matched multiple nodes; use \
                         aegis.edge.fleet.invoke for fan-out (\
                         MultiTargetRequiresFleetTool)"
                            .to_string(),
                    )));
                }
                Err(e) => {
                    return Some(Err(SealSessionError::InternalError(format!(
                        "edge resolve: {e}"
                    ))));
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
            Err(e) => Some(Err(SealSessionError::InternalError(format!(
                "edge dispatch: {e}"
            )))),
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
        })?;
        let target_value = args
            .get("target")
            .ok_or_else(|| SealSessionError::MalformedPayload("missing target".to_string()))?;
        let target: crate::domain::edge::EdgeTarget = serde_json::from_value(target_value.clone())
            .map_err(|e| SealSessionError::MalformedPayload(format!("target parse: {e}")))?;
        let resolved = resolver
            .resolve(&tenant_scope.authenticated_tenant, &target)
            .await
            .map_err(|e| SealSessionError::InternalError(format!("resolve: {e}")))?;
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "resolved": resolved.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
            "skipped": Vec::<serde_json::Value>::new(),
        })))
    }

    /// `aegis.edge.fleet.invoke` — multi-target reverse-RPC dispatch via
    /// `FleetDispatcher::spawn`. Returns the fleet command id; per-node
    /// progress is observable via the FleetEvent stream surfaced over the
    /// REST `/api/edge/fleet/invoke` SSE channel.
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
        })?;
        let dispatcher = self.edge_fleet_dispatcher.as_ref().ok_or_else(|| {
            SealSessionError::InternalError("edge fleet dispatcher not configured".to_string())
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
            .map_err(|e| SealSessionError::InternalError(format!("resolve: {e}")))?;

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
    async fn invoke_aegis_edge_fleet_cancel_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let cancel = self.edge_fleet_cancel.as_ref().ok_or_else(|| {
            SealSessionError::InternalError(
                "edge fleet cancel not configured on this orchestrator".to_string(),
            )
        })?;
        let id_str = args
            .get("fleet_command_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::MalformedPayload("missing fleet_command_id".to_string())
            })?;
        let uuid = uuid::Uuid::parse_str(id_str)
            .map_err(|e| SealSessionError::MalformedPayload(format!("fleet_command_id: {e}")))?;
        let cancelled = cancel
            .cancel(crate::domain::cluster::FleetCommandId(uuid))
            .await;
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
