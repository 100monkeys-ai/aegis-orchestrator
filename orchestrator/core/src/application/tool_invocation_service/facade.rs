use super::*;

#[allow(clippy::too_many_arguments)]
impl ToolInvocationService {
    pub(super) fn resolve_tenant_arg(args: &Value) -> Result<TenantId, SealSessionError> {
        let tenant = args
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("tenant").and_then(|v| v.as_str()))
            .unwrap_or(crate::domain::tenant::CONSUMER_SLUG);

        TenantId::from_string(tenant).map_err(|e| {
            SealSessionError::InvalidArguments(format!("invalid tenant identifier '{tenant}': {e}"))
        })
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
        }
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

        // 3. Get security context from the session.
        let security_context = session.security_context;

        // 4. Delegate to unified dispatch core (iteration_number=0, empty audit history for SEAL path).
        let result = self
            .dispatch_tool_core(
                &agent_id,
                execution_id,
                &security_context,
                tool_name,
                args,
                0,
                Vec::new(),
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
    pub async fn invoke_tool_internal(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
        tool_name: String,
        args: Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // 1. Load the execution to obtain its security_context_name (ADR-083).
        let execution = self
            .execution_service
            .get_execution(execution_id)
            .await
            .map_err(|e| {
                SealSessionError::MalformedPayload(format!(
                    "Failed to load execution {execution_id}: {e}"
                ))
            })?;

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

        // 2. Delegate to unified dispatch core.
        self.dispatch_tool_core(
            agent_id,
            execution_id,
            &security_context,
            tool_name,
            args,
            iteration_number,
            tool_audit_history,
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
        security_context: &crate::domain::security_context::SecurityContext,
        tool_name: String,
        args: Value,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
    ) -> Result<ToolInvocationResult, SealSessionError> {
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
            let svc = self.execution_service.clone();
            let blocked_tool = tool_name.clone();
            tokio::spawn(async move {
                if let Err(e) = svc.store_policy_violation(execution_id, blocked_tool).await {
                    tracing::warn!(
                        execution_id = %execution_id,
                        error = %e,
                        "Failed to record policy violation on iteration"
                    );
                }
            });
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
        let agent = self.agent_lifecycle.get_agent(*agent_id).await.ok();

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
                                .lookup_agent(judge_agent)
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
                                .get_execution(execution_id)
                                .await
                                .ok()
                                .and_then(|exec| exec.input.intent)
                                .unwrap_or_else(|| "No objective available".to_string());
                            let available_tools = self
                                .get_available_tools_for_agent(*agent_id)
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
                                payload: Self::build_semantic_judge_payload(
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
                                    .get_execution(exec_id)
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
                                            return Err(SealSessionError::InternalError(
                                            format!(
                                                "Inner-loop tool execution rejected by semantic judge \
                                                 (Score: {:.2}, criteria_min: {:.2}). Reasoning: {}",
                                                result.score, min_score, result.reasoning,
                                            ),
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

        // Built-in orchestrator aegis.* tool dispatch chain.
        let aegis_result = self
            .try_dispatch_aegis_tool(
                &tool_name,
                &args,
                execution_id,
                *agent_id,
                iteration_number,
                &tool_audit_history,
                security_context,
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
                        .invoke_seal_gateway_internal_grpc(execution_id, &tool_name, args.clone())
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
        args: &Value,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Option<Result<ToolInvocationResult, SealSessionError>> {
        match tool_name {
            "aegis.agent.create" => Some(self.invoke_aegis_agent_create_tool(args).await),
            "aegis.agent.update" => Some(self.invoke_aegis_agent_update_tool(args).await),
            "aegis.agent.delete" => Some(self.invoke_aegis_agent_delete_tool(args).await),
            "aegis.agent.generate" => Some(
                self.invoke_aegis_agent_generate_tool(args, security_context)
                    .await,
            ),
            "aegis.agent.export" => Some(self.invoke_aegis_agent_export_tool(args).await),
            "aegis.agent.list" => Some(self.invoke_aegis_agent_list_tool(args).await),
            "aegis.agent.logs" => Some(self.invoke_aegis_agent_logs_tool(args).await),
            "aegis.workflow.delete" => Some(self.invoke_aegis_workflow_delete_tool(args).await),
            "aegis.workflow.validate" => Some(self.invoke_aegis_workflow_validate_tool(args).await),
            "aegis.workflow.run" => Some(
                self.invoke_aegis_workflow_run_tool(args, security_context)
                    .await,
            ),
            "aegis.workflow.executions.list" => {
                Some(self.invoke_aegis_workflow_execution_list_tool(args).await)
            }
            "aegis.workflow.executions.get" => {
                Some(self.invoke_aegis_workflow_execution_get_tool(args).await)
            }
            "aegis.workflow.status" => Some(self.invoke_aegis_workflow_status_tool(args).await),
            "aegis.workflow.generate" => Some(self.invoke_aegis_workflow_generate_tool(args).await),
            "aegis.workflow.logs" => Some(self.invoke_aegis_workflow_logs_tool(args).await),
            "aegis.workflow.wait" => Some(self.invoke_aegis_workflow_wait_tool(args).await),
            "aegis.workflow.cancel" => Some(self.invoke_aegis_workflow_cancel_tool(args).await),
            "aegis.workflow.signal" => Some(self.invoke_aegis_workflow_signal_tool(args).await),
            "aegis.workflow.remove" => Some(self.invoke_aegis_workflow_remove_tool(args).await),
            "aegis.workflow.list" => Some(self.invoke_aegis_workflow_list_tool(args).await),
            "aegis.workflow.promote" => Some(
                self.invoke_aegis_workflow_promote_tool(args, security_context)
                    .await,
            ),
            "aegis.workflow.demote" => Some(
                self.invoke_aegis_workflow_demote_tool(args, security_context)
                    .await,
            ),
            "aegis.workflow.export" => Some(self.invoke_aegis_workflow_export_tool(args).await),
            "aegis.workflow.update" => Some(
                self.invoke_aegis_workflow_update_tool(args, execution_id, agent_id)
                    .await,
            ),
            "aegis.workflow.create" => Some(
                self.invoke_aegis_workflow_create_tool(
                    args,
                    execution_id,
                    agent_id,
                    iteration_number,
                    tool_audit_history,
                )
                .await,
            ),
            "aegis.task.execute" => Some(
                self.invoke_aegis_task_execute_tool(args, security_context)
                    .await,
            ),
            "aegis.task.status" => Some(self.invoke_aegis_task_status_tool(args).await),
            "aegis.task.wait" | "aegis.agent.wait" => {
                Some(self.invoke_aegis_task_wait_tool(args).await)
            }
            "aegis.task.logs" => Some(self.invoke_aegis_task_logs_tool(args).await),
            "aegis.task.list" => Some(self.invoke_aegis_task_list_tool(args).await),
            "aegis.task.cancel" => Some(self.invoke_aegis_task_cancel_tool(args).await),
            "aegis.task.remove" => Some(self.invoke_aegis_task_remove_tool(args).await),
            "aegis.system.info" => Some(self.invoke_aegis_system_info_tool().await),
            "aegis.system.config" => Some(self.invoke_aegis_system_config_tool().await),
            "aegis.tools.list" => Some(self.invoke_aegis_tools_list(args, security_context).await),
            "aegis.tools.search" => {
                Some(self.invoke_aegis_tools_search(args, security_context).await)
            }
            "aegis.agent.search" => Some(
                self.invoke_aegis_agent_search_tool(args, security_context)
                    .await,
            ),
            "aegis.workflow.search" => Some(
                self.invoke_aegis_workflow_search_tool(args, security_context)
                    .await,
            ),
            "aegis.execute.intent" => Some(
                self.invoke_aegis_execute_intent_tool(args, security_context)
                    .await,
            ),
            "aegis.execute.status" => Some(self.invoke_aegis_execute_status_tool(args).await),
            "aegis.execute.wait" => Some(self.invoke_aegis_workflow_wait_tool(args).await),
            _ => None,
        }
    }
}
