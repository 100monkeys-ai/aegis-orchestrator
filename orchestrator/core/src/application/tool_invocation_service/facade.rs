use super::*;

#[allow(clippy::too_many_arguments)]
impl ToolInvocationService {
    pub(super) fn resolve_tenant_arg(args: &Value) -> Result<TenantId, SmcpSessionError> {
        let tenant = args
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("tenant").and_then(|v| v.as_str()))
            .unwrap_or(crate::domain::tenant::CONSUMER_SLUG);

        TenantId::from_string(tenant).map_err(|e| {
            SmcpSessionError::InvalidArguments(format!("invalid tenant identifier '{tenant}': {e}"))
        })
    }

    pub fn new(
        smcp_session_repo: Arc<dyn SmcpSessionRepository>,
        security_context_repo: Arc<dyn SecurityContextRepository>,
        smcp_middleware: Arc<SmcpMiddleware>,
        tool_router: Arc<ToolRouter>,
        fsal: Arc<AegisFSAL>,
        volume_registry: NfsVolumeRegistry,
        agent_lifecycle: Arc<dyn AgentLifecycleService>,
        execution_service: Arc<dyn ExecutionService>,
        web_tool_port: Arc<dyn ExternalWebToolPort>,
        event_bus: Arc<EventBus>,
        smcp_gateway_url: Option<String>,
    ) -> Self {
        Self {
            smcp_session_repo,
            security_context_repo,
            smcp_middleware,
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
            smcp_gateway_url,
            schema_registry: Arc::new(SchemaRegistry::build()),
            workflow_execution_control: None,
            agent_activity: None,
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

    pub async fn invoke_tool(
        &self,
        envelope: &(impl EnvelopeVerifier + Send + Sync),
    ) -> Result<Value, SmcpSessionError> {
        // 1. Look up the active session by the opaque security_token string.
        //    This avoids relying on unverified JWT payload content to route the request;
        //    the agent_id and other claims are read from the already-loaded session record.
        let mut session = self
            .smcp_session_repo
            .find_active_by_security_token(envelope.security_token())
            .await
            .map_err(|e| {
                SmcpSessionError::InternalError(format!("session repository lookup failed: {}", e))
            })?
            .ok_or(SmcpSessionError::SessionInactive(
                crate::domain::smcp_session::SessionStatus::Expired,
            ))?;

        let agent_id = session.agent_id;

        // 2. Middleware verifies signature and evaluates against SecurityContext
        let args = self
            .smcp_middleware
            .verify_and_unwrap(&mut session, envelope)
            .await?;
        let tool_name = envelope
            .extract_tool_name()
            .ok_or(SmcpSessionError::MalformedPayload(
                "missing tool name".to_string(),
            ))?;

        // 2b. Validate required arguments against the tool's input contract (ADR-055).
        // Domain knowledge lives in ToolInputContract (domain/mcp.rs); this layer
        // only maps the domain error string to the appropriate SmcpSessionError variant.
        ToolInputContract::validate(&tool_name, &args)
            .map_err(SmcpSessionError::InvalidArguments)?;

        // 3. Try builtin tools first (aegis.schema.*, aegis.agent.*, aegis.workflow.*, etc.)
        // These are handled directly by the orchestrator without routing to an external MCP server.
        match crate::application::tools::try_invoke_builtin(
            &tool_name,
            &args,
            session.execution_id,
            &self.fsal,
            &self.volume_registry,
            &self.web_tool_port,
            &self.schema_registry,
        )
        .await
        {
            Ok(crate::application::tools::BuiltinToolResult::Handled(result)) => {
                tracing::info!("SMCP builtin tool executed: {}", tool_name);
                return match result {
                    ToolInvocationResult::Direct(value) => Ok(value),
                    ToolInvocationResult::DispatchRequired(action) => Ok(
                        serde_json::json!({"status": "dispatch_required", "action": format!("{:?}", action)}),
                    ),
                };
            }
            Ok(crate::application::tools::BuiltinToolResult::NotBuiltin) => {
                // Fall through to ToolRouter for MCP server-hosted tools
            }
            Err(e) => {
                return Err(SmcpSessionError::InternalError(format!(
                    "Builtin tool '{}' failed: {}",
                    tool_name, e
                )));
            }
        }

        // 4. Route tool call to the appropriate TCP server.
        // ToolRouter resolves a ToolServerId; invocation execution happens after routing.
        let server_id = self
            .tool_router
            .route_tool(session.execution_id, &tool_name)
            .await
            .map_err(|e| SmcpSessionError::MalformedPayload(format!("Routing error: {e}")))?;

        let server = self.tool_router.get_server(server_id).await.ok_or(
            SmcpSessionError::MalformedPayload("Server vanished after routing".to_string()),
        )?;

        // 5. Execute based on ExecutionMode (Gateway Retrofit)
        match server.execution_mode {
            crate::domain::mcp::ExecutionMode::Local => {
                tracing::info!(
                    "Executing local tool via FSAL: {} for agent {:?}",
                    tool_name,
                    agent_id
                );

                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "local_fsal",
                    "message": format!("Locally executed {} affecting agent volume", tool_name),
                    "args_executed": args
                }))
            }
            crate::domain::mcp::ExecutionMode::Remote => {
                let _json_rpc_payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": tool_name,
                    "params": args,
                    "id": session.execution_id.to_string()
                });

                tracing::info!(
                    "Proxying remote tool via JSON-RPC: {} to server {:?}",
                    tool_name,
                    server_id
                );

                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "remote_jsonrpc",
                    "message": format!("Proxied {} to external MCP server {:?}", tool_name, server_id),
                    "args_proxied": args
                }))
            }
        }
    }

    /// Internal orchestrator-driven tool invocation (Gateway pattern)
    /// Performs SecurityContext validation against the agent's active session,
    /// so that orchestrator-mediated calls (e.g. from generated LLM text)
    /// enforce the same policies as direct agent SMCP calls.
    pub async fn invoke_tool_internal(
        &self,
        agent_id: &AgentId,
        execution_id: crate::domain::execution::ExecutionId,
        iteration_number: u8,
        tool_audit_history: Vec<TrajectoryStep>,
        tool_name: String,
        args: Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let invocation_id = ToolInvocationId::new();
        let started_at = Instant::now();
        self.publish_invocation_requested(
            invocation_id,
            execution_id,
            *agent_id,
            &tool_name,
            &args,
        );

        // 1. Determine the applicable SecurityContext.
        // If the agent is connected via SMCP (Path 1), it has an active session.
        // If the agent is running via Container Dispatch Protocol (Path 3), it does NOT
        // establish an SMCP session natively but still needs policy enforcement.
        let security_context = match self.smcp_session_repo.find_active_by_agent(agent_id).await {
            Ok(Some(session)) => session.security_context,
            _ => {
                // Fallback for inner-loop executions. Try to load the "default" context.
                self.security_context_repo
                    .find_by_name("default")
                    .await
                    .map_err(|e| {
                        SmcpSessionError::MalformedPayload(format!(
                            "Failed to load fallback security context 'default': {e}"
                        ))
                    })?
                    .ok_or(SmcpSessionError::SessionInactive(
                        crate::domain::smcp_session::SessionStatus::Expired,
                    ))?
            }
        };

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
                            "Stripped operator-only parameter from consumer tier tool call (internal path)"
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
            self.publish_invocation_failed(
                invocation_id,
                execution_id,
                *agent_id,
                format!("Policy violation: {details}"),
            );
            return Err(SmcpSessionError::PolicyViolation(violation));
        }

        if !Self::context_allows_tool_name(&security_context.name, &tool_name) {
            let violation = PolicyViolation::ToolExplicitlyDenied {
                tool_name: tool_name.clone(),
            };
            self.event_bus
                .publish_mcp_event(MCPToolEvent::PolicyViolation {
                    execution_id,
                    agent_id: *agent_id,
                    tool_name: tool_name.clone(),
                    violation_type: ViolationType::ToolExplicitlyDenied,
                    details: format!(
                        "Tool '{tool_name}' is not available in security context '{}'",
                        security_context.name
                    ),
                    blocked_at: Utc::now(),
                });
            self.publish_invocation_failed(
                invocation_id,
                execution_id,
                *agent_id,
                format!(
                    "Policy violation: tool '{tool_name}' is not available in security context '{}'",
                    security_context.name
                ),
            );
            return Err(SmcpSessionError::PolicyViolation(violation));
        }

        // --- Inner-Loop Semantic Pre-Execution Validation (ADR-049) ---
        // If the agent manifest specifies an inner-loop semantic validation step,
        // execute it BEFORE the tool call is dispatched to external systems.
        // This is a synchronous block that fails the tool call if the judge rejects it.
        let agent = self
            .agent_lifecycle
            .get_agent(*agent_id)
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!(
                    "Failed to retrieve agent: {e}"
                ))
            })?;

        if let Some(exec_spec) = &agent.manifest.spec.execution {
            // Check operator-level skip_judge flag before running the inner-loop semantic
            // judge pipeline. When the node config marks a tool with `skip_judge: true`
            // (e.g. read-only tools such as fs.read, fs.list), we bypass the judge entirely
            // to reduce latency on low-risk operations (NODE_CONFIGURATION_SPEC_V1.md).
            let should_skip_judge = self.tool_router.is_skip_judge(&tool_name).await;
            if should_skip_judge {
                tracing::debug!(
                    tool_name = %tool_name,
                    "Inner-loop semantic judge skipped (skip_judge=true in node config for this tool)"
                );
            } else if let Some(validation_pipeline) = &exec_spec.tool_validation {
                for validator in validation_pipeline {
                    // Check if this is an Inner-Loop Semantic Validator configuration
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
                                SmcpSessionError::SignatureVerificationFailed(format!(
                                    "Failed to lookup judge: {e}"
                                ))
                            })?
                            .ok_or_else(|| {
                                SmcpSessionError::SignatureVerificationFailed(format!(
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
                                SmcpSessionError::SignatureVerificationFailed(format!(
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
                                return Err(SmcpSessionError::JudgeTimeout(format!(
                                    "Inner-loop semantic judge '{judge_agent}' timed out after {timeout_seconds} seconds."
                                )));
                            }

                            let exec = self
                                .execution_service
                                .get_execution(exec_id)
                                .await
                                .map_err(|e| {
                                    SmcpSessionError::InternalError(format!(
                                        "Failed to get judge execution {exec_id}: {e}"
                                    ))
                                })?;

                            match exec.status {
                                crate::domain::execution::ExecutionStatus::Completed => {
                                    let last_iter = exec.iterations().last().ok_or_else(|| {
                                        SmcpSessionError::SignatureVerificationFailed(
                                            "Judge completed but has no iterations".to_string(),
                                        )
                                    })?;
                                    let output_str =
                                        last_iter.output.as_ref().ok_or_else(|| {
                                            SmcpSessionError::SignatureVerificationFailed(
                                                "Judge completed but has no output".to_string(),
                                            )
                                        })?;

                                    let json_str = extract_json_from_text(output_str)
                                        .unwrap_or_else(|| output_str.clone());
                                    let result: crate::domain::validation::GradientResult =
                                        serde_json::from_str(&json_str).map_err(|e| {
                                            SmcpSessionError::SignatureVerificationFailed(format!(
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
                                        return Err(SmcpSessionError::SignatureVerificationFailed(
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
                                    return Err(SmcpSessionError::SignatureVerificationFailed("Inner-loop semantic judge execution failed or was cancelled".to_string()));
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
        // --- End Pre-Execution Validation ---

        // Built-in orchestrator authoring tools
        if tool_name == "aegis.agent.create" {
            let result = self.invoke_aegis_agent_create_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.agent.update" {
            let result = self.invoke_aegis_agent_update_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.agent.delete" {
            let result = self.invoke_aegis_agent_delete_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.agent.generate" {
            let result = self.invoke_aegis_agent_generate_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.delete" {
            let result = self.invoke_aegis_workflow_delete_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.validate" {
            let result = self.invoke_aegis_workflow_validate_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.run" {
            let result = self.invoke_aegis_workflow_run_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.executions.list" {
            let result = self.invoke_aegis_workflow_execution_list_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.executions.get" {
            let result = self.invoke_aegis_workflow_execution_get_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.status" {
            let result = self.invoke_aegis_workflow_status_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.generate" {
            let result = self.invoke_aegis_workflow_generate_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.logs" {
            let result = self.invoke_aegis_workflow_logs_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.cancel" {
            let result = self.invoke_aegis_workflow_cancel_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.signal" {
            let result = self.invoke_aegis_workflow_signal_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.remove" {
            let result = self.invoke_aegis_workflow_remove_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.task.execute" {
            let result = self.invoke_aegis_task_execute_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.task.status" {
            let result = self.invoke_aegis_task_status_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.task.logs" {
            let result = self.invoke_aegis_task_logs_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.task.list" {
            let result = self.invoke_aegis_task_list_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.task.cancel" {
            let result = self.invoke_aegis_task_cancel_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.task.remove" {
            let result = self.invoke_aegis_task_remove_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.system.info" {
            let result = self.invoke_aegis_system_info_tool().await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.system.config" {
            let result = self.invoke_aegis_system_config_tool().await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.agent.export" {
            let result = self.invoke_aegis_agent_export_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.agent.list" {
            let result = self.invoke_aegis_agent_list_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.agent.logs" {
            let result = self.invoke_aegis_agent_logs_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.list" {
            let result = self.invoke_aegis_workflow_list_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.export" {
            let result = self.invoke_aegis_workflow_export_tool(&args).await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.update" {
            let result = self
                .invoke_aegis_workflow_update_tool(&args, execution_id, *agent_id)
                .await;
            match &result {
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
            }
            return result;
        }
        if tool_name == "aegis.workflow.create" {
            let result = self
                .invoke_aegis_workflow_create_tool(
                    &args,
                    execution_id,
                    *agent_id,
                    iteration_number,
                    &tool_audit_history,
                )
                .await;
            match &result {
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
            }
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
                if self.smcp_gateway_url.is_some() {
                    let gateway_result = self
                        .invoke_smcp_gateway_internal_grpc(execution_id, &tool_name, args.clone())
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
                return Err(SmcpSessionError::SignatureVerificationFailed(format!(
                    "Routing error: {routing_err}"
                )));
            }
        };

        let server = match self.tool_router.get_server(server_id).await {
            Some(server) => server,
            None => {
                let err = SmcpSessionError::SignatureVerificationFailed(
                    "Server vanished after routing".to_string(),
                );
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
}
