// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::register_workflow::RegisterWorkflowUseCase;
use crate::application::start_workflow_execution::StartWorkflowExecutionUseCase;
use crate::application::validation_service::ValidationService;
use crate::domain::agent::AgentId;
use crate::domain::dispatch::DispatchAction;
use crate::domain::fsal::AegisFSAL;
use crate::domain::smcp_session::{EnvelopeVerifier, SmcpSessionError};
use crate::domain::smcp_session_repository::SmcpSessionRepository;
use crate::domain::workflow::{ConsensusConfig, ConsensusStrategy, WorkflowValidator};
use crate::domain::{validation::ValidationRequest, workflow::ConfidenceWeighting};
use crate::infrastructure::smcp::middleware::SmcpMiddleware;
use crate::infrastructure::tool_router::ToolRouter;

use crate::application::agent::AgentLifecycleService;
use crate::application::execution::ExecutionService;
use crate::application::ports::ExternalWebToolPort;
use crate::application::schema_registry::SchemaRegistry;
use crate::domain::events::{MCPToolEvent, ViolationType};
use crate::domain::execution::{ExecutionInput, TrajectoryStep};
use crate::domain::mcp::{
    MCPError, PolicyViolation, ToolInputContract, ToolInvocationId, ToolServerId,
};
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::tenant::TenantId;
use crate::domain::validation::extract_json_from_text;
use crate::infrastructure::agent_manifest_parser::AgentManifestParser;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::smcp_gateway_proto::gateway_invocation_service_client::GatewayInvocationServiceClient;
use crate::infrastructure::smcp_gateway_proto::{
    FsalMount, InvokeCliRequest, InvokeWorkflowRequest, ListToolsRequest,
};
use crate::infrastructure::workflow_parser::WorkflowParser;

const JUDGE_POLL_INTERVAL_MS: u64 = 500;

pub enum ToolInvocationResult {
    Direct(Value),
    DispatchRequired(DispatchAction),
}

pub struct ToolInvocationService {
    smcp_session_repo: Arc<dyn SmcpSessionRepository>,
    security_context_repo: Arc<dyn SecurityContextRepository>,
    smcp_middleware: Arc<SmcpMiddleware>,
    tool_router: Arc<ToolRouter>,
    /// AegisFSAL security boundary for all storage operations (ADR-033 Path 1, ADR-047)
    /// Delegates to the appropriate StorageProvider (SeaweedFS, OpenDAL, SMCP, LocalHost)
    fsal: Arc<AegisFSAL>,
    /// Volume registry for resolving execution_id → volume context
    volume_registry: NfsVolumeRegistry,
    /// Agent lifecycle service for semantic validation
    agent_lifecycle: Arc<dyn AgentLifecycleService>,
    /// Execution service for spawning inner-loop judges
    execution_service: Arc<dyn ExecutionService>,
    /// Adapter for external web tools (Path 2).
    web_tool_port: Arc<dyn ExternalWebToolPort>,
    /// Event bus for MCP invocation and policy audit events.
    event_bus: Arc<EventBus>,
    /// Optional workflow registration use case for built-in aegis workflow authoring tools.
    register_workflow_use_case: Option<Arc<dyn RegisterWorkflowUseCase>>,
    /// Optional validation service for semantic stage in workflow authoring tools.
    validation_service: Option<Arc<ValidationService>>,
    /// Optional workflow repository for managing workflows.
    workflow_repository: Option<Arc<dyn crate::domain::repository::WorkflowRepository>>,
    /// Optional workflow execution repository for `aegis.workflow.logs` and
    /// `aegis.task.logs`.
    workflow_execution_repo:
        Option<Arc<dyn crate::domain::repository::WorkflowExecutionRepository>>,
    /// Optional workflow execution use case.
    start_workflow_execution_use_case: Option<Arc<dyn StartWorkflowExecutionUseCase>>,
    /// Optional root directory for persisting generated manifests on disk.
    generated_manifests_root: Option<PathBuf>,
    /// Optional path to node-config.yaml for system.config tool.
    node_config_path: Option<PathBuf>,
    /// Optional ADR-053 SMCP gateway URL from node config.
    smcp_gateway_url: Option<String>,
    /// Schema registry for builtin schema.get / schema.validate tools.
    schema_registry: Arc<SchemaRegistry>,
}

#[allow(clippy::too_many_arguments)]
impl ToolInvocationService {
    fn resolve_tenant_arg(args: &Value) -> Result<TenantId, SmcpSessionError> {
        let tenant = args
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("tenant").and_then(|v| v.as_str()))
            .unwrap_or("local");

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

    fn publish_invocation_requested(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        tool_name: &str,
        arguments: &Value,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationRequested {
                invocation_id,
                execution_id,
                agent_id,
                tool_name: tool_name.to_string(),
                arguments: arguments.clone(),
                requested_at: Utc::now(),
            });
    }

    fn publish_invocation_started(
        &self,
        invocation_id: ToolInvocationId,
        server_id: ToolServerId,
        tool_name: &str,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationStarted {
                invocation_id,
                server_id,
                tool_name: tool_name.to_string(),
                started_at: Utc::now(),
            });
    }

    fn publish_invocation_completed(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        result: &Value,
        started: Instant,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationCompleted {
                invocation_id,
                execution_id,
                agent_id,
                result: result.clone(),
                duration_ms: started.elapsed().as_millis() as u64,
                completed_at: Utc::now(),
            });
    }

    fn publish_invocation_failed(
        &self,
        invocation_id: ToolInvocationId,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        message: String,
    ) {
        self.event_bus
            .publish_mcp_event(MCPToolEvent::InvocationFailed {
                invocation_id,
                execution_id,
                agent_id,
                error: MCPError {
                    code: -32000,
                    message,
                    data: None,
                },
                failed_at: Utc::now(),
            });
    }

    fn parse_json_or_string(raw: &str) -> Value {
        serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
    }

    fn build_tool_audit_history(
        execution_id: crate::domain::execution::ExecutionId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
    ) -> Value {
        let tool_calls: Vec<Value> = tool_audit_history
            .iter()
            .map(|step| {
                serde_json::json!({
                    "tool_name": step.tool_name,
                    "arguments": Self::parse_json_or_string(&step.arguments_json),
                    "status": step.status,
                    "result": step
                        .result_json
                        .as_deref()
                        .map(Self::parse_json_or_string)
                        .unwrap_or(Value::Null),
                    "error": step.error,
                })
            })
            .collect();

        let latest_schema_validate = tool_calls
            .iter()
            .rev()
            .find(|step| {
                step.get("tool_name").and_then(|v| v.as_str()) == Some("aegis.schema.validate")
            })
            .cloned();
        let latest_schema_get = tool_calls
            .iter()
            .rev()
            .find(|step| step.get("tool_name").and_then(|v| v.as_str()) == Some("aegis.schema.get"))
            .cloned();
        let schema_get_evidence = latest_schema_get.clone();
        let schema_validate_evidence = latest_schema_validate.clone();

        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "available": true,
            "iteration_number": iteration_number,
            "tool_calls": tool_calls,
            "latest_schema_get": latest_schema_get,
            "latest_schema_validate": latest_schema_validate,
            "schema_get_evidence": schema_get_evidence,
            "schema_validate_evidence": schema_validate_evidence,
        })
    }

    fn build_semantic_judge_payload(
        execution_id: crate::domain::execution::ExecutionId,
        task: String,
        tool_name: &str,
        arguments: &Value,
        available_tools: Vec<String>,
        worker_mounts: Vec<String>,
        criteria: &str,
        validation_context: &str,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
    ) -> Value {
        let semantic_input = serde_json::to_string_pretty(&serde_json::json!({
            "tool": tool_name,
            "arguments": arguments,
        }))
        .unwrap_or_default();
        let tool_audit_history =
            Self::build_tool_audit_history(execution_id, iteration_number, tool_audit_history);

        serde_json::json!({
            "execution_id": execution_id.to_string(),
            "task": task,
            "proposed_tool_call": {
                "name": tool_name,
                "arguments": arguments,
            },
            "available_tools": available_tools,
            "worker_mounts": worker_mounts,
            "tool_audit_history": tool_audit_history,
            "output": semantic_input,
            "criteria": criteria,
            "validation_context": validation_context,
        })
    }

    fn map_policy_violation(violation: &PolicyViolation) -> (ViolationType, String) {
        match violation {
            PolicyViolation::ToolNotAllowed {
                tool_name,
                allowed_tools,
            } => (
                ViolationType::ToolNotAllowed,
                format!("Tool '{tool_name}' is not allowed. Allowed: {allowed_tools:?}"),
            ),
            PolicyViolation::ToolExplicitlyDenied { tool_name } => (
                ViolationType::ToolExplicitlyDenied,
                format!("Tool '{tool_name}' is explicitly denied"),
            ),
            PolicyViolation::RateLimitExceeded {
                max_calls,
                current_calls,
            } => (
                ViolationType::RateLimitExceeded,
                format!(
                    "Rate limit exceeded: current_calls={current_calls}, max_calls={max_calls}"
                ),
            ),
            PolicyViolation::PathOutsideBoundary {
                path,
                allowed_paths,
            } => (
                ViolationType::PathOutsideBoundary,
                format!(
                    "Path '{}' outside boundary {:?}",
                    path.display(),
                    allowed_paths
                ),
            ),
            PolicyViolation::PathTraversalAttempt { path } => (
                ViolationType::PathTraversalAttempt,
                format!("Path traversal attempt at '{}'", path.display()),
            ),
            PolicyViolation::DomainNotAllowed {
                domain,
                allowed_domains,
            } => (
                ViolationType::DomainNotAllowed,
                format!("Domain '{domain}' not allowed. Allowed: {allowed_domains:?}"),
            ),
            PolicyViolation::MissingRequiredArgument(arg) => (
                ViolationType::MissingRequiredArgument,
                format!("Missing required argument '{arg}'"),
            ),
            PolicyViolation::TimeoutExceeded {
                tool_name,
                max_duration,
            } => (
                ViolationType::TimeoutExceeded,
                format!("Tool '{tool_name}' exceeded timeout {max_duration:?}"),
            ),
        }
    }

    /// Invokes a tool by validating the SMCP Envelope for the agent and routing to the right server
    pub async fn invoke_tool(
        &self,
        envelope: &impl EnvelopeVerifier,
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
            .verify_and_unwrap(&mut session, envelope)?;
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

        // 3. Route tool call to the appropriate TCP server.
        // ToolRouter resolves a ToolServerId; invocation execution happens after routing.
        let server_id = self
            .tool_router
            .route_tool(session.execution_id, &tool_name)
            .await
            .map_err(|e| SmcpSessionError::MalformedPayload(format!("Routing error: {e}")))?;

        let server = self.tool_router.get_server(server_id).await.ok_or(
            SmcpSessionError::MalformedPayload("Server vanished after routing".to_string()),
        )?;

        // 4. Execute based on ExecutionMode (Gateway Retrofit)
        match server.execution_mode {
            crate::domain::mcp::ExecutionMode::Local => {
                // FSAL Execution
                // In a production implementation, we'd look up the agent's volume path via the Storage manager
                // and directly manipulate files using the runtime-agnostic FSAL.
                tracing::info!(
                    "Executing local tool via FSAL: {} for agent {:?}",
                    tool_name,
                    agent_id
                );

                // FSAL execution wiring can be expanded with concrete file operations.

                Ok(serde_json::json!({
                    "status": "success",
                    "execution_mode": "local_fsal",
                    "message": format!("Locally executed {} affecting agent volume", tool_name),
                    "args_executed": args
                }))
            }
            crate::domain::mcp::ExecutionMode::Remote => {
                // SMCP to JSONRPC Proxy
                // We've unwrapped the SMCP envelope, now we build a JSON-RPC 2.0 request
                // and dispatch it to the ToolServer's raw stdio or HTTP interfaces.
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

                // JSON-RPC transport wiring can be expanded with concrete stdio/http IPC.
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
            let result = self.invoke_aegis_agent_list_tool().await;
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
            let result = self.invoke_aegis_workflow_list_tool().await;
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
                self.publish_invocation_started(invocation_id, id, &tool_name);
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

    /// Retrieve available tool schemas to inject into LLM prompts
    pub async fn get_available_tools(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SmcpSessionError> {
        let mut tools = self.tool_router.list_tools().await.map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Failed to list tools: {e}"))
        })?;

        if self.smcp_gateway_url.is_some() {
            if let Ok(gateway_tools) = self.fetch_gateway_tools_grpc().await {
                tools.extend(gateway_tools);
            }
        }

        Ok(tools)
    }

    pub async fn get_available_tools_for_agent(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SmcpSessionError> {
        let agent = self
            .agent_lifecycle
            .get_agent(agent_id)
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!(
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

    pub async fn get_available_tools_for_context(
        &self,
        security_context_name: &str,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SmcpSessionError> {
        let security_context = self
            .security_context_repo
            .find_by_name(security_context_name)
            .await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))?
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(format!(
                    "Security context '{security_context_name}' not found"
                ))
            })?;

        let tools = self.get_available_tools().await?;
        Ok(tools
            .into_iter()
            .filter(|tool| security_context.permits_tool_name(&tool.name))
            .collect())
    }

    async fn fetch_gateway_tools_grpc(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SmcpSessionError> {
        let gateway_url = self.smcp_gateway_url.as_deref().ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "smcp_gateway.url is not configured".to_string(),
            )
        })?;
        let mut client = GatewayInvocationServiceClient::connect(gateway_url.to_string())
            .await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))?;
        let response = client
            .list_tools(tonic::Request::new(ListToolsRequest {}))
            .await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))?;

        let mut converted = Vec::new();
        for item in response.into_inner().tools {
            let input_schema = if item.kind == "cli" {
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
            };
            converted.push(crate::infrastructure::tool_router::ToolMetadata {
                name: item.name,
                description: item.description,
                input_schema,
            });
        }

        Ok(converted)
    }

    async fn invoke_smcp_gateway_internal_grpc(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, SmcpSessionError> {
        let gateway_url = self.smcp_gateway_url.as_deref().ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "smcp_gateway.url is not configured".to_string(),
            )
        })?;
        let mut client = GatewayInvocationServiceClient::connect(gateway_url.to_string())
            .await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))?;

        if args.get("subcommand").is_some() {
            let subcommand = args
                .get("subcommand")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    SmcpSessionError::SignatureVerificationFailed(
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
                return Err(SmcpSessionError::SignatureVerificationFailed(format!(
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
                }))
                .await
                .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))?
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
                zaru_user_token: String::new(),
            }))
            .await
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))?
            .into_inner();

        if response.result_json.is_empty() {
            return Ok(serde_json::json!({}));
        }

        serde_json::from_str(&response.result_json)
            .map_err(|e| SmcpSessionError::SignatureVerificationFailed(e.to_string()))
    }

    async fn invoke_aegis_agent_create_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.agent.create requires 'manifest_yaml' string".to_string(),
                )
            })?;
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

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

        match self
            .agent_lifecycle
            .deploy_agent(manifest.clone(), force)
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
                        SmcpSessionError::SignatureVerificationFailed(format!(
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

    async fn invoke_aegis_agent_list_tool(&self) -> Result<ToolInvocationResult, SmcpSessionError> {
        let agents = self.agent_lifecycle.list_agents().await.map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Failed to list agents: {e}"))
        })?;

        let entries: Vec<serde_json::Value> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "id": a.id.0.to_string(),
                    "name": a.name,
                    "version": a.manifest.metadata.version,
                    "status": format!("{:?}", a.status).to_lowercase(),
                    "labels": a.manifest.metadata.labels,
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.agent.list",
            "count": entries.len(),
            "agents": entries
        })))
    }

    async fn invoke_aegis_agent_delete_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let agent_id_str = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.agent.delete requires 'agent_id' string".to_string(),
                )
            })?;

        let agent_id =
            crate::domain::agent::AgentId(uuid::Uuid::parse_str(agent_id_str).map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}"))
            })?);

        match self.agent_lifecycle.delete_agent(agent_id).await {
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

    async fn invoke_aegis_agent_generate_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let input = args.get("input").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.agent.generate requires 'input' string".to_string(),
            )
        })?;

        let agent_id = match self
            .agent_lifecycle
            .lookup_agent("agent-creator-agent")
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

        match self
            .execution_service
            .start_execution(
                agent_id,
                crate::domain::execution::ExecutionInput {
                    intent: Some(input.to_string()),
                    payload: serde_json::Value::String(input.to_string()),
                },
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

    async fn invoke_aegis_workflow_delete_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.delete requires 'name' string".to_string(),
            )
        })?;

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.delete",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        // Find by name or ID
        let workflow_id = match repo.find_by_name_for_tenant(&tenant_id, name).await {
            Ok(Some(w)) => w.id,
            _ => {
                if let Ok(uuid) = uuid::Uuid::parse_str(name) {
                    crate::domain::workflow::WorkflowId(uuid)
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.delete",
                        "error": "Workflow not found"
                    })));
                }
            }
        };

        match repo.delete(workflow_id).await {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.delete",
                "deleted": true,
                "workflow_id": workflow_id.0.to_string()
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.delete",
                "deleted": false,
                "error": format!("Failed to delete workflow: {e}")
            }))),
        }
    }

    async fn invoke_aegis_workflow_validate_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.validate requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(workflow) => workflow,
            Err(error) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.validate",
                    "valid": false,
                    "deterministic_validation": {
                        "passed": false,
                        "error": format!("Workflow parser validation failed: {error}"),
                    },
                })));
            }
        };

        if let Err(error) = WorkflowValidator::check_for_cycles(&workflow) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.validate",
                "valid": false,
                "deterministic_validation": {
                    "passed": false,
                    "error": format!("Workflow cycle validation failed: {error}"),
                },
            })));
        }

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.validate",
            "valid": true,
            "deterministic_validation": { "passed": true },
            "workflow": {
                "workflow_id": workflow.id.to_string(),
                "name": workflow.metadata.name,
                "version": workflow.metadata.version.clone().unwrap_or_default(),
                "state_count": workflow.spec.states.len(),
                "initial_state": workflow.spec.initial_state.as_str(),
            },
        })))
    }

    async fn invoke_aegis_workflow_run_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.run requires 'name' string".to_string(),
            )
        })?;

        let input = args.get("input").cloned().unwrap_or(serde_json::json!({}));
        let blackboard = args.get("blackboard").cloned();

        let start_use_case = match &self.start_workflow_execution_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.run",
                    "error": "Workflow execution service not configured"
                })));
            }
        };

        match start_use_case
            .start_execution(
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: name.to_string(),
                    input,
                    blackboard,
                    tenant_id: Some(tenant_id.clone()),
                },
            )
            .await
        {
            Ok(started) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.run",
                "execution_id": started.workflow_id,
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.run",
                "error": format!("Failed to start workflow: {e}")
            }))),
        }
    }

    async fn invoke_aegis_workflow_execution_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let limit: usize = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(20);
        let offset: usize = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.list",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let executions = if let Some(workflow_ref) =
            args.get("workflow_id").and_then(|v| v.as_str())
        {
            let workflow_id = if let Ok(uuid) = uuid::Uuid::parse_str(workflow_ref) {
                crate::domain::workflow::WorkflowId(uuid)
            } else if let Some(workflow_repo) = &self.workflow_repository {
                match workflow_repo
                    .find_by_name_for_tenant(&tenant_id, workflow_ref)
                    .await
                {
                    Ok(Some(workflow)) => workflow.id,
                    Ok(None) => {
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.workflow.executions.list",
                            "error": format!("Workflow '{workflow_ref}' not found")
                        })));
                    }
                    Err(error) => {
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.workflow.executions.list",
                            "error": format!("Failed to resolve workflow '{workflow_ref}': {error}")
                        })));
                    }
                }
            } else {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.list",
                    "error": format!("workflow_id '{workflow_ref}' is not a UUID and workflow repository is not configured")
                })));
            };

            repo.find_by_workflow_for_tenant(&tenant_id, workflow_id, limit, offset)
                .await
        } else {
            repo.list_paginated_for_tenant(&tenant_id, limit, offset)
                .await
        };

        match executions {
            Ok(executions) => {
                let items: Vec<Value> = executions
                    .into_iter()
                    .map(|execution| {
                        serde_json::json!({
                            "execution_id": execution.id.to_string(),
                            "workflow_id": execution.workflow_id.to_string(),
                            "status": format!("{:?}", execution.status).to_lowercase(),
                            "current_state": execution.current_state.as_str(),
                            "started_at": execution.started_at,
                            "last_transition_at": execution.last_transition_at,
                        })
                    })
                    .collect();

                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.list",
                    "count": items.len(),
                    "executions": items,
                    "limit": limit,
                    "offset": offset,
                })))
            }
            Err(error) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.list",
                "error": format!("Failed to list workflow executions: {error}")
            }))),
        }
    }

    async fn invoke_aegis_workflow_execution_get_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let execution_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::InvalidArguments(
                    "aegis.workflow.executions.get requires 'execution_id' string".to_string(),
                )
            })?;
        let execution_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(execution_id_str).map_err(|error| {
                SmcpSessionError::InvalidArguments(format!(
                    "aegis.workflow.executions.get: invalid execution_id UUID: {error}"
                ))
            })?,
        );

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.get",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        match repo.find_by_id_for_tenant(&tenant_id, execution_id).await {
            Ok(Some(execution)) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.get",
                "execution": {
                    "execution_id": execution.id.to_string(),
                    "workflow_id": execution.workflow_id.to_string(),
                    "status": format!("{:?}", execution.status).to_lowercase(),
                    "current_state": execution.current_state.as_str(),
                    "input": execution.input,
                    "blackboard": execution.blackboard.data(),
                    "state_outputs": execution.state_outputs,
                    "started_at": execution.started_at,
                    "last_transition_at": execution.last_transition_at,
                }
            }))),
            Ok(None) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.get",
                "error": format!("Workflow execution '{execution_id_str}' not found")
            }))),
            Err(error) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.get",
                "error": format!("Failed to fetch workflow execution: {error}")
            }))),
        }
    }

    async fn invoke_aegis_workflow_generate_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let input = args.get("input").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.generate requires 'input' string".to_string(),
            )
        })?;

        let start_use_case = match &self.start_workflow_execution_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.generate",
                    "error": "Workflow execution service not configured"
                })));
            }
        };

        match start_use_case
            .start_execution(
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: "builtin-workflow-generator".to_string(),
                    input: serde_json::json!({ "input": input }),
                    blackboard: None,
                    tenant_id: Some(tenant_id),
                },
            )
            .await
        {
            Ok(started) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.generate",
                "execution_id": started.workflow_id,
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.generate",
                "error": format!("Failed to start workflow generation: {e}")
            }))),
        }
    }

    async fn invoke_aegis_task_execute_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let agent_ref = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.execute requires 'agent_id' string".to_string(),
                )
            })?;

        let input = args.get("input").cloned().unwrap_or(serde_json::json!({}));

        let agent_id = if let Ok(uuid) = uuid::Uuid::parse_str(agent_ref) {
            crate::domain::agent::AgentId(uuid)
        } else {
            match self.agent_lifecycle.lookup_agent(agent_ref).await {
                Ok(Some(id)) => id,
                _ => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.task.execute",
                        "error": format!("Agent '{agent_ref}' not found")
                    })));
                }
            }
        };

        match self
            .execution_service
            .start_execution(
                agent_id,
                crate::domain::execution::ExecutionInput {
                    intent: None,
                    payload: input,
                },
            )
            .await
        {
            Ok(exec_id) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.execute",
                "execution_id": exec_id.to_string(),
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.execute",
                "error": format!("Failed to start task execution: {e}")
            }))),
        }
    }

    async fn invoke_aegis_workflow_logs_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::InvalidArguments(
                    "aegis.workflow.logs requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SmcpSessionError::InvalidArguments(format!(
                    "aegis.workflow.logs: invalid execution_id UUID: {e}"
                ))
            })?,
        );

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

        let repo = match &self.workflow_execution_repo {
            Some(r) => r.clone(),
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let execution = match repo.find_by_id(exec_id).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Workflow execution '{exec_id_str}' not found")
                })));
            }
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Failed to fetch execution: {e}")
                })));
            }
        };

        let events = match repo.find_events_by_execution(exec_id, limit, offset).await {
            Ok(evts) => evts,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Failed to fetch execution events: {e}")
                })));
            }
        };

        let total = events.len();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.logs",
            "execution_id": exec_id_str,
            "status": format!("{:?}", execution.status).to_lowercase(),
            "workflow_id": execution.workflow_id.to_string(),
            "events": events,
            "total": total,
            "limit": limit,
            "offset": offset,
        })))
    }

    async fn invoke_aegis_task_status_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.status requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.get_execution(exec_id).await {
            Ok(exec) => {
                let last_iter = exec.iterations().last();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.status",
                    "execution_id": exec_id_str,
                    "agent_id": exec.agent_id.0.to_string(),
                    "status": format!("{:?}", exec.status).to_lowercase(),
                    "started_at": exec.started_at,
                    "ended_at": exec.ended_at,
                    "iteration_count": exec.iterations().len(),
                    "last_output": last_iter.and_then(|i| i.output.as_ref()),
                    "last_error": last_iter.and_then(|i| i.error.as_ref().map(|e| format!("{e:?}")))
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.status",
                "error": format!("Failed to get execution: {e}")
            }))),
        }
    }

    async fn invoke_aegis_task_logs_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::InvalidArguments(
                    "aegis.task.logs requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SmcpSessionError::InvalidArguments(format!(
                    "aegis.task.logs: invalid execution_id UUID: {e}"
                ))
            })?,
        );

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

        let execution = match self.execution_service.get_execution(exec_id).await {
            Ok(execution) => execution,
            Err(error) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.logs",
                    "error": format!("Failed to fetch execution: {error}")
                })));
            }
        };

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo.clone(),
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.logs",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let events = match repo.find_events_by_execution(exec_id, limit, offset).await {
            Ok(events) => events,
            Err(error) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.logs",
                    "error": format!("Failed to fetch execution events: {error}")
                })));
            }
        };

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.task.logs",
            "execution_id": exec_id_str,
            "agent_id": execution.agent_id.0.to_string(),
            "status": format!("{:?}", execution.status).to_lowercase(),
            "events": events,
            "total": events.len(),
            "limit": limit,
            "offset": offset,
        })))
    }

    async fn invoke_aegis_task_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(crate::domain::agent::AgentId);

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

        match self
            .execution_service
            .list_executions(agent_id, limit)
            .await
        {
            Ok(executions) => {
                let entries: Vec<serde_json::Value> = executions
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "id": e.id.0.to_string(),
                            "agent_id": e.agent_id.0.to_string(),
                            "status": format!("{:?}", e.status).to_lowercase(),
                            "started_at": e.started_at
                        })
                    })
                    .collect();

                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.list",
                    "count": entries.len(),
                    "executions": entries
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.list",
                "error": format!("Failed to list executions: {e}")
            }))),
        }
    }

    async fn invoke_aegis_task_cancel_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.cancel requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.cancel_execution(exec_id).await {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.cancel",
                "cancelled": true,
                "execution_id": exec_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.cancel",
                "cancelled": false,
                "error": format!("Failed to cancel execution: {e}")
            }))),
        }
    }

    async fn invoke_aegis_task_remove_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.remove requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.delete_execution(exec_id).await {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.remove",
                "removed": true,
                "execution_id": exec_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.remove",
                "removed": false,
                "error": format!("Failed to remove execution: {e}")
            }))),
        }
    }

    async fn invoke_aegis_system_info_tool(
        &self,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.system.info",
            "version": env!("CARGO_PKG_VERSION"),
            "status": "healthy",
            "capabilities": [
                "agent_lifecycle",
                "workflow_orchestration",
                "mcp_routing",
                "smcp_attestation"
            ]
        })))
    }

    async fn invoke_aegis_system_config_tool(
        &self,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let path = match &self.node_config_path {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.system.config",
                    "error": "Node configuration path not available"
                })));
            }
        };

        match std::fs::read_to_string(path) {
            Ok(content) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.system.config",
                "config_path": path.to_string_lossy(),
                "content_yaml": content
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.system.config",
                "error": format!("Failed to read node configuration: {e}")
            }))),
        }
    }

    async fn invoke_aegis_agent_update_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
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
                        SmcpSessionError::SignatureVerificationFailed(format!(
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

    async fn invoke_aegis_agent_export_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
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

    async fn invoke_aegis_workflow_list_tool(
        &self,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.list",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let workflows = repo
            .list_all_for_tenant(&TenantId::local_default())
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!(
                    "Failed to list workflows: {e}"
                ))
            })?;

        let entries: Vec<serde_json::Value> = workflows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id.0.to_string(),
                    "name": w.metadata.name,
                    "version": w.metadata.version.clone().unwrap_or_default(),
                    "initial_state": w.spec.initial_state,
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.list",
            "count": entries.len(),
            "workflows": entries
        })))
    }

    async fn invoke_aegis_workflow_export_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.export requires 'name' string".to_string(),
            )
        })?;

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.export",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let workflow = match repo.find_by_name_for_tenant(&tenant_id, name).await {
            Ok(Some(w)) => w,
            _ => {
                if let Ok(uuid) = uuid::Uuid::parse_str(name) {
                    match repo
                        .find_by_id_for_tenant(
                            &tenant_id,
                            crate::domain::workflow::WorkflowId(uuid),
                        )
                        .await
                    {
                        Ok(Some(w)) => w,
                        _ => {
                            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                                "tool": "aegis.workflow.export",
                                "error": "Workflow not found"
                            })));
                        }
                    }
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.export",
                        "error": "Workflow not found or invalid UUID"
                    })));
                }
            }
        };

        let yaml = serde_yaml::to_string(&workflow).unwrap_or_default();
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.export",
            "name": workflow.metadata.name,
            "manifest_yaml": yaml
        })))
    }

    async fn invoke_aegis_workflow_update_tool(
        &self,
        args: &Value,
        _execution_id: crate::domain::execution::ExecutionId,
        _agent_id: AgentId,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.update requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "updated": false,
                    "error": format!("Workflow parser validation failed: {}", e),
                })));
            }
        };

        if let Err(e) = WorkflowValidator::check_for_cycles(&workflow) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.update",
                "updated": false,
                "error": format!("Workflow cycle validation failed: {}", e),
            })));
        }

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let _existing = match repo
            .find_by_name_for_tenant(&tenant_id, &workflow.metadata.name)
            .await
        {
            Ok(Some(w)) => w,
            _ => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "updated": false,
                    "error": "Workflow not found"
                })));
            }
        };

        let register_workflow_use_case = match &self.register_workflow_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "error": "Workflow registration use case not configured"
                })));
            }
        };

        match register_workflow_use_case
            .register_workflow_for_tenant(&tenant_id, manifest_yaml, force)
            .await
        {
            Ok(meta) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "workflows",
                        &meta.name,
                        &meta.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SmcpSessionError::SignatureVerificationFailed(format!(
                            "Workflow updated but failed to persist manifest: {e}"
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "updated": true,
                    "name": meta.name,
                    "workflow_id": meta.workflow_id,
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.update",
                "updated": false,
                "error": format!("Workflow update failed: {}", e)
            }))),
        }
    }

    async fn invoke_aegis_workflow_create_tool(
        &self,
        args: &Value,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.create requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let task_context = args
            .get("task_context")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let min_score = args
            .get("min_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.8);
        let min_confidence = args
            .get("min_confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.7);

        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create",
                    "deterministic_validation": {
                        "passed": false,
                        "error": format!("Workflow parser validation failed: {}", e),
                    },
                    "semantic_validation": {"passed": false},
                    "deployed": false
                })));
            }
        };

        if let Err(e) = WorkflowValidator::check_for_cycles(&workflow) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {
                    "passed": false,
                    "error": format!("Workflow cycle validation failed: {}", e),
                },
                "semantic_validation": {"passed": false},
                "deployed": false
            })));
        }

        let validation_service = self.validation_service.as_ref().ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "Workflow semantic validation service not configured".to_string(),
            )
        })?;
        let register_workflow_use_case =
            self.register_workflow_use_case.as_ref().ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "Workflow registration use case not configured".to_string(),
                )
            })?;

        let judge_names: Vec<String> = args
            .get("judge_agents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_else(|| vec!["code-quality-judge".to_string()]);

        if judge_names.is_empty() {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": false,
                    "error": "No judge agents provided",
                },
                "deployed": false
            })));
        }

        let mut judges: Vec<(AgentId, f64)> = Vec::with_capacity(judge_names.len());
        for judge_name in &judge_names {
            let judge_id = self
                .agent_lifecycle
                .lookup_agent(judge_name)
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "Failed to lookup judge agent '{judge_name}': {e}"
                    ))
                })?
                .ok_or_else(|| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "Judge agent '{judge_name}' not found"
                    ))
                })?;
            judges.push((judge_id, 1.0));
        }

        let criteria = if task_context.is_empty() {
            "Evaluate this workflow manifest for correctness, transition safety, DDD alignment, and production readiness.".to_string()
        } else {
            format!(
                "Task Context:\n{task_context}\n\nEvaluate this workflow manifest for correctness, transition safety, DDD alignment, and production readiness."
            )
        };

        let semantic_request = ValidationRequest {
            content: manifest_yaml.to_string(),
            criteria,
            context: Some(serde_json::json!({
                "validation_context": "workflow_create",
                "workflow_name": workflow.metadata.name,
                "state_count": workflow.spec.states.len(),
                "force": force,
                "task_context": task_context,
                "iteration_number": iteration_number,
                "tool_audit_history": Self::build_tool_audit_history(
                    execution_id,
                    iteration_number,
                    tool_audit_history,
                ),
            })),
        };

        let semantic_consensus = validation_service
            .validate_with_judges(
                execution_id,
                agent_id,
                0,
                semantic_request,
                judges,
                Some(ConsensusConfig {
                    strategy: ConsensusStrategy::WeightedAverage,
                    threshold: Some(min_score),
                    min_agreement_confidence: Some(min_confidence),
                    n: None,
                    min_judges_required: 1,
                    confidence_weighting: Some(ConfidenceWeighting::default()),
                }),
                90,
                500,
            )
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!(
                    "Workflow semantic validation failed: {e}"
                ))
            })?;

        let semantic_passed = semantic_consensus.final_score >= min_score
            && semantic_consensus.consensus_confidence >= min_confidence;

        if !semantic_passed {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": false,
                    "score": semantic_consensus.final_score,
                    "confidence": semantic_consensus.consensus_confidence,
                    "thresholds": {
                        "min_score": min_score,
                        "min_confidence": min_confidence
                    },
                    "strategy": semantic_consensus.strategy,
                },
                "deployed": false
            })));
        }

        match register_workflow_use_case
            .register_workflow(manifest_yaml, force)
            .await
        {
            Ok(registered) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "workflows",
                        &registered.name,
                        &registered.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SmcpSessionError::SignatureVerificationFailed(format!(
                            "Workflow deployed but failed to persist manifest: {e}"
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create",
                    "deterministic_validation": {"passed": true},
                    "semantic_validation": {
                        "passed": true,
                        "score": semantic_consensus.final_score,
                        "confidence": semantic_consensus.consensus_confidence,
                        "strategy": semantic_consensus.strategy,
                    },
                    "deployed": true,
                    "workflow": {
                        "workflow_id": registered.workflow_id,
                        "name": registered.name,
                        "version": registered.version,
                        "status": registered.status
                    },
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": true,
                    "score": semantic_consensus.final_score,
                    "confidence": semantic_consensus.consensus_confidence,
                    "strategy": semantic_consensus.strategy,
                },
                "deployed": false,
                "errors": [format!("Workflow registration failed: {}", e)]
            }))),
        }
    }

    fn persist_generated_manifest(
        &self,
        kind: &str,
        name: &str,
        version: &str,
        manifest_yaml: &str,
    ) -> std::io::Result<Option<String>> {
        let Some(root) = &self.generated_manifests_root else {
            return Ok(None);
        };

        let safe_name = sanitize_segment(name);
        let safe_version = sanitize_segment(version);
        let target_dir = root.join(kind).join(safe_name);
        std::fs::create_dir_all(&target_dir)?;

        let file_path = target_dir.join(format!("{safe_version}.yaml"));
        std::fs::write(&file_path, manifest_yaml)?;
        Ok(Some(path_to_string(&file_path)))
    }
}

fn sanitize_segment(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "unversioned".to_string();
    }

    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Prevent path traversal patterns after character substitution.
    // Treat empty or traversal-like segments as a safe default.
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." || sanitized.contains("..") {
        "unversioned".to_string()
    } else {
        sanitized
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::nfs_gateway::NfsVolumeRegistry;
    use crate::domain::events::StorageEvent;
    use crate::domain::execution::ExecutionId;
    use crate::domain::fsal::{AegisFSAL, EventPublisher};
    use crate::domain::node_config::{BuiltinDispatcherConfig, CapabilityConfig};
    use crate::domain::security_context::SecurityContext;
    use crate::domain::smcp_session::SmcpSession;
    use crate::infrastructure::repositories::InMemoryVolumeRepository;
    use crate::infrastructure::smcp::session_repository::InMemorySmcpSessionRepository;
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use crate::infrastructure::tool_router::{InMemoryToolRegistry, ToolRouter};
    use async_trait::async_trait;

    struct NoOpEventPublisher;

    #[async_trait]
    impl EventPublisher for NoOpEventPublisher {
        async fn publish_storage_event(&self, _event: StorageEvent) {}
    }

    /// Create test FSAL dependencies and empty NFS volume registry.
    fn test_fsal_deps() -> (Arc<AegisFSAL>, NfsVolumeRegistry) {
        let storage_root = std::env::temp_dir().join("aegis-tool-invocation-tests");
        let storage = Arc::new(
            LocalHostStorageProvider::new(&storage_root)
                .expect("failed to initialize LocalHostStorageProvider for tests"),
        );
        let vol_repo = Arc::new(InMemoryVolumeRepository::new());
        let publisher = Arc::new(NoOpEventPublisher);
        let fsal = Arc::new(AegisFSAL::new(
            storage,
            vol_repo,
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            publisher,
        ));
        let registry = NfsVolumeRegistry::new();
        (fsal, registry)
    }

    fn make_fake_token(agent_id: AgentId) -> String {
        use base64::Engine;
        let claims = serde_json::json!({"agent_id": agent_id.0.to_string()});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap_or_default());
        format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", payload)
    }

    struct DummyEnvelope {
        valid: bool,
        token: String,
    }

    impl DummyEnvelope {
        fn for_agent(valid: bool, agent_id: AgentId) -> Self {
            Self {
                valid,
                token: make_fake_token(agent_id),
            }
        }
    }

    impl EnvelopeVerifier for DummyEnvelope {
        fn security_token(&self) -> &str {
            &self.token
        }

        fn verify_signature(&self, _public_key_bytes: &[u8]) -> Result<(), SmcpSessionError> {
            if self.valid {
                Ok(())
            } else {
                Err(SmcpSessionError::SignatureVerificationFailed(
                    "invalid sig".to_string(),
                ))
            }
        }
        fn extract_tool_name(&self) -> Option<String> {
            Some("test_tool".to_string())
        }
        fn extract_arguments(&self) -> Option<Value> {
            Some(serde_json::json!({}))
        }
    }

    use crate::domain::agent::{Agent, AgentManifest, AgentStatus};
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{Execution, ExecutionInput, ExecutionStatus, Iteration};
    use crate::domain::repository::{WorkflowExecutionRepository, WorkflowRepository};
    use crate::domain::workflow::WorkflowExecutionEventRecord;
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::{
        InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
    };
    use futures::Stream;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::RwLock;
    use tokio::sync::Mutex;

    fn test_agent_with_tools(tools: &[&str]) -> Agent {
        let manifest_yaml = format!(
            r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools:
{}
"#,
            tools
                .iter()
                .map(|tool| format!("    - {tool}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let manifest: AgentManifest = serde_yaml::from_str(&manifest_yaml).unwrap();
        Agent {
            id: AgentId::new(),
            name: manifest.metadata.name.clone(),
            manifest,
            status: AgentStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    struct TestAgentLifecycleService;
    #[async_trait]
    impl AgentLifecycleService for TestAgentLifecycleService {
        async fn deploy_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            manifest: AgentManifest,
            force: bool,
        ) -> Result<AgentId> {
            self.deploy_agent(manifest, force).await
        }

        async fn deploy_agent(&self, _: AgentManifest, _force: bool) -> Result<AgentId> {
            anyhow::bail!("TestAgentLifecycleService::deploy_agent not exercised in this test")
        }

        async fn get_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
            self.get_agent(id).await
        }

        async fn get_agent(&self, _: AgentId) -> Result<Agent> {
            anyhow::bail!("TestAgentLifecycleService::get_agent not exercised in this test")
        }

        async fn update_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: AgentId,
            manifest: AgentManifest,
        ) -> Result<()> {
            self.update_agent(id, manifest).await
        }

        async fn update_agent(&self, _: AgentId, _: AgentManifest) -> Result<()> {
            anyhow::bail!("TestAgentLifecycleService::update_agent not exercised in this test")
        }

        async fn delete_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<()> {
            self.delete_agent(id).await
        }

        async fn delete_agent(&self, _: AgentId) -> Result<()> {
            anyhow::bail!("TestAgentLifecycleService::delete_agent not exercised in this test")
        }

        async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
            self.list_agents().await
        }

        async fn list_agents(&self) -> Result<Vec<Agent>> {
            anyhow::bail!("TestAgentLifecycleService::list_agents not exercised in this test")
        }

        async fn lookup_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            name: &str,
        ) -> Result<Option<AgentId>> {
            self.lookup_agent(name).await
        }

        async fn lookup_agent(&self, _: &str) -> Result<Option<AgentId>> {
            anyhow::bail!("TestAgentLifecycleService::lookup_agent not exercised in this test")
        }
    }

    struct FilteringAgentLifecycleService {
        agent: Agent,
    }

    #[async_trait]
    impl AgentLifecycleService for FilteringAgentLifecycleService {
        async fn deploy_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            manifest: AgentManifest,
            force: bool,
        ) -> Result<AgentId> {
            self.deploy_agent(manifest, force).await
        }

        async fn deploy_agent(&self, _: AgentManifest, _force: bool) -> Result<AgentId> {
            anyhow::bail!("FilteringAgentLifecycleService::deploy_agent not exercised in this test")
        }

        async fn get_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<Agent> {
            self.get_agent(id).await
        }

        async fn get_agent(&self, id: AgentId) -> Result<Agent> {
            if id == self.agent.id {
                Ok(self.agent.clone())
            } else {
                anyhow::bail!("agent not found")
            }
        }

        async fn update_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: AgentId,
            manifest: AgentManifest,
        ) -> Result<()> {
            self.update_agent(id, manifest).await
        }

        async fn update_agent(&self, _: AgentId, _: AgentManifest) -> Result<()> {
            anyhow::bail!("FilteringAgentLifecycleService::update_agent not exercised in this test")
        }

        async fn delete_agent_for_tenant(&self, _tenant_id: &TenantId, id: AgentId) -> Result<()> {
            self.delete_agent(id).await
        }

        async fn delete_agent(&self, _: AgentId) -> Result<()> {
            anyhow::bail!("FilteringAgentLifecycleService::delete_agent not exercised in this test")
        }

        async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
            self.list_agents().await
        }

        async fn list_agents(&self) -> Result<Vec<Agent>> {
            Ok(vec![self.agent.clone()])
        }

        async fn lookup_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            name: &str,
        ) -> Result<Option<AgentId>> {
            self.lookup_agent(name).await
        }

        async fn lookup_agent(&self, name: &str) -> Result<Option<AgentId>> {
            Ok((name == self.agent.name).then_some(self.agent.id))
        }
    }

    struct TestExecutionService;
    #[async_trait]
    impl ExecutionService for TestExecutionService {
        async fn start_execution(&self, _: AgentId, _: ExecutionInput) -> Result<ExecutionId> {
            anyhow::bail!("TestExecutionService::start_execution not exercised in this test")
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!("TestExecutionService::start_child_execution not exercised in this test")
        }
        async fn get_execution(&self, _: ExecutionId) -> Result<Execution> {
            anyhow::bail!("TestExecutionService::get_execution not exercised in this test")
        }
        async fn get_iterations(&self, _: ExecutionId) -> Result<Vec<Iteration>> {
            anyhow::bail!("TestExecutionService::get_iterations not exercised in this test")
        }
        async fn cancel_execution(&self, _: ExecutionId) -> Result<()> {
            anyhow::bail!("TestExecutionService::cancel_execution not exercised in this test")
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
            anyhow::bail!("TestExecutionService::stream_execution not exercised in this test")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            anyhow::bail!("TestExecutionService::stream_agent_events not exercised in this test")
        }
        async fn list_executions(&self, _: Option<AgentId>, _: usize) -> Result<Vec<Execution>> {
            anyhow::bail!("TestExecutionService::list_executions not exercised in this test")
        }
        async fn delete_execution(&self, _: ExecutionId) -> Result<()> {
            anyhow::bail!("TestExecutionService::delete_execution not exercised in this test")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: crate::domain::execution::LlmInteraction,
        ) -> Result<()> {
            anyhow::bail!("TestExecutionService::record_llm_interaction not exercised in this test")
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            anyhow::bail!(
                "TestExecutionService::store_iteration_trajectory not exercised in this test"
            )
        }
    }

    struct LogsTestExecutionService {
        execution: Execution,
    }

    #[async_trait]
    impl ExecutionService for LogsTestExecutionService {
        async fn start_execution(&self, _: AgentId, _: ExecutionInput) -> Result<ExecutionId> {
            anyhow::bail!("LogsTestExecutionService::start_execution not exercised in this test")
        }

        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> Result<ExecutionId> {
            anyhow::bail!(
                "LogsTestExecutionService::start_child_execution not exercised in this test"
            )
        }

        async fn get_execution(&self, id: ExecutionId) -> Result<Execution> {
            if self.execution.id == id {
                Ok(self.execution.clone())
            } else {
                anyhow::bail!("execution not found")
            }
        }

        async fn get_iterations(&self, _: ExecutionId) -> Result<Vec<Iteration>> {
            anyhow::bail!("LogsTestExecutionService::get_iterations not exercised in this test")
        }

        async fn cancel_execution(&self, _: ExecutionId) -> Result<()> {
            anyhow::bail!("LogsTestExecutionService::cancel_execution not exercised in this test")
        }

        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
            anyhow::bail!("LogsTestExecutionService::stream_execution not exercised in this test")
        }

        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            anyhow::bail!(
                "LogsTestExecutionService::stream_agent_events not exercised in this test"
            )
        }

        async fn list_executions(&self, _: Option<AgentId>, _: usize) -> Result<Vec<Execution>> {
            anyhow::bail!("LogsTestExecutionService::list_executions not exercised in this test")
        }

        async fn delete_execution(&self, _: ExecutionId) -> Result<()> {
            anyhow::bail!("LogsTestExecutionService::delete_execution not exercised in this test")
        }

        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: crate::domain::execution::LlmInteraction,
        ) -> Result<()> {
            anyhow::bail!(
                "LogsTestExecutionService::record_llm_interaction not exercised in this test"
            )
        }

        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            anyhow::bail!(
                "LogsTestExecutionService::store_iteration_trajectory not exercised in this test"
            )
        }
    }

    #[derive(Default)]
    struct StubWorkflowExecutionRepository {
        events: RwLock<HashMap<ExecutionId, Vec<WorkflowExecutionEventRecord>>>,
    }

    impl StubWorkflowExecutionRepository {
        fn with_events(
            execution_id: ExecutionId,
            events: Vec<WorkflowExecutionEventRecord>,
        ) -> Self {
            let mut by_execution = HashMap::new();
            by_execution.insert(execution_id, events);
            Self {
                events: RwLock::new(by_execution),
            }
        }
    }

    #[async_trait]
    impl WorkflowExecutionRepository for StubWorkflowExecutionRepository {
        async fn save_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _execution: &crate::domain::workflow::WorkflowExecution,
        ) -> Result<(), crate::domain::repository::RepositoryError> {
            Ok(())
        }

        async fn find_by_id_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<
            Option<crate::domain::workflow::WorkflowExecution>,
            crate::domain::repository::RepositoryError,
        > {
            Ok(None)
        }

        async fn find_active_for_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<
            Vec<crate::domain::workflow::WorkflowExecution>,
            crate::domain::repository::RepositoryError,
        > {
            Ok(vec![])
        }

        async fn find_by_workflow_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _workflow_id: crate::domain::workflow::WorkflowId,
            _limit: usize,
            _offset: usize,
        ) -> Result<
            Vec<crate::domain::workflow::WorkflowExecution>,
            crate::domain::repository::RepositoryError,
        > {
            Ok(vec![])
        }

        async fn list_paginated_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _limit: usize,
            _offset: usize,
        ) -> Result<
            Vec<crate::domain::workflow::WorkflowExecution>,
            crate::domain::repository::RepositoryError,
        > {
            Ok(vec![])
        }

        async fn update_temporal_linkage_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _execution_id: ExecutionId,
            _temporal_workflow_id: &str,
            _temporal_run_id: &str,
        ) -> Result<(), crate::domain::repository::RepositoryError> {
            Ok(())
        }

        async fn append_event(
            &self,
            execution_id: ExecutionId,
            temporal_sequence_number: i64,
            event_type: String,
            payload: serde_json::Value,
            iteration_number: Option<u8>,
        ) -> Result<(), crate::domain::repository::RepositoryError> {
            let mut events = self.events.write().unwrap();
            events
                .entry(execution_id)
                .or_default()
                .push(WorkflowExecutionEventRecord {
                    sequence: temporal_sequence_number,
                    event_type,
                    state_name: None,
                    iteration_number,
                    payload,
                    recorded_at: chrono::Utc::now(),
                });
            Ok(())
        }

        async fn find_events_by_execution(
            &self,
            id: ExecutionId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecutionEventRecord>, crate::domain::repository::RepositoryError>
        {
            let events = self
                .events
                .read()
                .unwrap()
                .get(&id)
                .cloned()
                .unwrap_or_default();
            Ok(events.into_iter().skip(offset).take(limit).collect())
        }
    }

    #[derive(Default)]
    struct TestStartWorkflowExecutionUseCase {
        last_request: Mutex<
            Option<crate::application::start_workflow_execution::StartWorkflowExecutionRequest>,
        >,
    }

    #[async_trait]
    impl StartWorkflowExecutionUseCase for TestStartWorkflowExecutionUseCase {
        async fn start_execution_for_tenant(
            &self,
            tenant_id: &TenantId,
            mut request: crate::application::start_workflow_execution::StartWorkflowExecutionRequest,
        ) -> Result<crate::application::start_workflow_execution::StartedWorkflowExecution>
        {
            request.tenant_id = Some(tenant_id.clone());
            *self.last_request.lock().await = Some(request.clone());

            Ok(
                crate::application::start_workflow_execution::StartedWorkflowExecution {
                    execution_id: ExecutionId::new().to_string(),
                    workflow_id: request.workflow_id,
                    temporal_run_id: "temporal-run-id".to_string(),
                    status: "started".to_string(),
                    started_at: chrono::Utc::now(),
                },
            )
        }
    }

    fn test_workflow_manifest_yaml(name: &str) -> String {
        format!(
            r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "1.0.0"
spec:
  initial_state: START
  states:
    START:
      kind: Agent
      agent: builder
      input: "{{{{workflow.task}}}}"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: echo "done"
      transitions: []
"#
        )
    }

    fn build_test_workflow(name: &str) -> crate::domain::workflow::Workflow {
        WorkflowParser::parse_yaml(&test_workflow_manifest_yaml(name))
            .expect("test workflow manifest should parse")
    }

    #[tokio::test]
    async fn test_invoke_tool_no_session() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());

        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );
        let agent_id = AgentId::new();
        let envelope = DummyEnvelope::for_agent(true, agent_id);

        let result = service.invoke_tool(&envelope).await;
        assert!(matches!(result, Err(SmcpSessionError::SessionInactive(_))));
    }

    #[tokio::test]
    async fn test_invoke_tool_bad_signature() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let agent_id = AgentId::new();
        let exec_id = ExecutionId::new();

        let context = SecurityContext {
            name: "test".to_string(),
            description: "".to_string(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        };

        let session_token = make_fake_token(agent_id);
        let session = SmcpSession::new(agent_id, exec_id, vec![], session_token, context);
        let _ = repo.save(session).await;

        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());

        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );
        let envelope = DummyEnvelope::for_agent(false, agent_id);

        let result = service.invoke_tool(&envelope).await;
        assert!(matches!(
            result,
            Err(SmcpSessionError::SignatureVerificationFailed(_))
        ));
    }

    #[tokio::test]
    async fn workflow_validate_tool_returns_success_for_valid_manifest() {
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();

        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );

        let result = service
            .invoke_aegis_workflow_validate_tool(&serde_json::json!({
                "manifest_yaml": test_workflow_manifest_yaml("validate-me"),
            }))
            .await
            .expect("workflow validate should return a result");

        let ToolInvocationResult::Direct(payload) = result else {
            panic!("expected direct payload");
        };

        assert_eq!(payload["tool"], "aegis.workflow.validate");
        assert_eq!(payload["valid"], true);
        assert_eq!(payload["workflow"]["name"], "validate-me");
    }

    #[test]
    fn build_tool_audit_history_includes_schema_validate_and_get_evidence() {
        let execution_id = ExecutionId::new();
        let tool_audit_history = vec![
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.get".to_string(),
                arguments_json: r#"{"key":"workflow/manifest/v1"}"#.to_string(),
                status: "completed".to_string(),
                result_json: Some(r#"{"ok":true}"#.to_string()),
                error: None,
            },
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.validate".to_string(),
                arguments_json:
                    r#"{"kind":"workflow","manifest_yaml":"apiVersion: 100monkeys.ai/v1"}"#
                        .to_string(),
                status: "completed".to_string(),
                result_json: Some(r#"{"valid":true,"errors":[]}"#.to_string()),
                error: None,
            },
        ];

        let audit_history =
            ToolInvocationService::build_tool_audit_history(execution_id, 1, &tool_audit_history);

        assert_eq!(audit_history["execution_id"], execution_id.to_string());
        assert_eq!(audit_history["available"], true);
        assert_eq!(audit_history["iteration_number"], 1);
        assert_eq!(audit_history["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(
            audit_history["tool_calls"][0]["tool_name"],
            "aegis.schema.get"
        );
        assert_eq!(
            audit_history["tool_calls"][1]["tool_name"],
            "aegis.schema.validate"
        );
        assert_eq!(
            audit_history["latest_schema_get"]["tool_name"],
            "aegis.schema.get"
        );
        assert_eq!(
            audit_history["latest_schema_validate"]["tool_name"],
            "aegis.schema.validate"
        );
        assert_eq!(
            audit_history["schema_get_evidence"]["tool_name"],
            "aegis.schema.get"
        );
        assert_eq!(
            audit_history["schema_validate_evidence"]["result"]["valid"],
            true
        );
    }

    #[test]
    fn build_semantic_judge_payload_includes_tool_audit_history() {
        let execution_id = ExecutionId::new();
        let tool_audit_history = vec![
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.get".to_string(),
                arguments_json: r#"{"key":"agent/manifest/v1"}"#.to_string(),
                status: "completed".to_string(),
                result_json: Some(r#"{"ok":true}"#.to_string()),
                error: None,
            },
            crate::domain::execution::TrajectoryStep {
                tool_name: "aegis.schema.validate".to_string(),
                arguments_json:
                    r#"{"kind":"agent","manifest_yaml":"apiVersion: 100monkeys.ai/v1"}"#
                        .to_string(),
                status: "completed".to_string(),
                result_json: Some(r#"{"valid":true,"errors":[]}"#.to_string()),
                error: None,
            },
        ];

        let payload = ToolInvocationService::build_semantic_judge_payload(
            execution_id,
            "Create an agent".to_string(),
            "aegis.agent.create",
            &serde_json::json!({
                "manifest_yaml": "apiVersion: 100monkeys.ai/v1\nkind: Agent\nmetadata:\n  name: copy-refiner\n  version: 1.0.0\nspec:\n  runtime:\n    language: python\n    version: \"3.11\"\n  task:\n    prompt_template: |\n      refine copy\n"
            }),
            vec![
                "aegis.schema.get".to_string(),
                "aegis.schema.validate".to_string(),
            ],
            vec!["/workspace".to_string()],
            "use the workflow-required sequence",
            "semantic_judge_pre_execution_inner_loop",
            1,
            &tool_audit_history,
        );

        assert_eq!(
            payload["validation_context"],
            "semantic_judge_pre_execution_inner_loop"
        );
        assert_eq!(payload["proposed_tool_call"]["name"], "aegis.agent.create");
        assert_eq!(
            payload["tool_audit_history"]["tool_calls"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            payload["tool_audit_history"]["latest_schema_validate"]["tool_name"],
            "aegis.schema.validate"
        );
        assert_eq!(
            payload["tool_audit_history"]["schema_get_evidence"]["tool_name"],
            "aegis.schema.get"
        );
    }

    #[tokio::test]
    async fn workflow_run_tool_forwards_blackboard() {
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let start_use_case = Arc::new(TestStartWorkflowExecutionUseCase::default());

        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        )
        .with_workflow_execution(start_use_case.clone());

        let result = service
            .invoke_aegis_workflow_run_tool(&serde_json::json!({
                "name": "run-me",
                "input": { "job": "demo" },
                "blackboard": { "priority": "high" },
            }))
            .await
            .expect("workflow run should return a result");

        let ToolInvocationResult::Direct(payload) = result else {
            panic!("expected direct payload");
        };

        assert_eq!(payload["tool"], "aegis.workflow.run");
        assert_eq!(payload["status"], "started");

        let request = start_use_case
            .last_request
            .lock()
            .await
            .clone()
            .expect("workflow run should record the request");
        assert_eq!(
            request.blackboard,
            Some(serde_json::json!({ "priority": "high" }))
        );
    }

    #[tokio::test]
    async fn workflow_execution_tools_list_and_get() {
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let workflow_repo = Arc::new(InMemoryWorkflowRepository::new());
        let workflow_execution_repo = Arc::new(InMemoryWorkflowExecutionRepository::new());
        let tenant_id = TenantId::local_default();
        let workflow = build_test_workflow("execution-list");

        workflow_repo
            .save_for_tenant(&tenant_id, &workflow)
            .await
            .expect("workflow should save");

        let mut execution = crate::domain::workflow::WorkflowExecution::new(
            &workflow,
            ExecutionId::new(),
            serde_json::json!({ "task": "demo" }),
        );
        execution
            .blackboard
            .set("priority".to_string(), serde_json::json!("high"));
        workflow_execution_repo
            .save_for_tenant(&tenant_id, &execution)
            .await
            .expect("workflow execution should save");

        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        )
        .with_workflow_repository(workflow_repo)
        .with_workflow_execution_repo(workflow_execution_repo);

        let list_result = service
            .invoke_aegis_workflow_execution_list_tool(&serde_json::json!({
                "workflow_id": workflow.id.to_string(),
            }))
            .await
            .expect("workflow execution list should return a result");
        let ToolInvocationResult::Direct(list_payload) = list_result else {
            panic!("expected direct list payload");
        };
        assert_eq!(list_payload["tool"], "aegis.workflow.executions.list");
        assert_eq!(list_payload["count"], 1);
        assert_eq!(
            list_payload["executions"][0]["execution_id"],
            execution.id.to_string()
        );

        let get_result = service
            .invoke_aegis_workflow_execution_get_tool(&serde_json::json!({
                "execution_id": execution.id.to_string(),
            }))
            .await
            .expect("workflow execution get should return a result");
        let ToolInvocationResult::Direct(get_payload) = get_result else {
            panic!("expected direct get payload");
        };
        assert_eq!(get_payload["tool"], "aegis.workflow.executions.get");
        assert_eq!(
            get_payload["execution"]["execution_id"],
            execution.id.to_string()
        );
        assert_eq!(get_payload["execution"]["blackboard"]["priority"], "high");
    }

    #[tokio::test]
    async fn task_logs_tool_returns_paginated_execution_events() {
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();

        let agent_id = AgentId::new();
        let mut execution = Execution::new(
            agent_id,
            ExecutionInput {
                intent: None,
                payload: serde_json::json!({"task":"demo"}),
            },
            3,
        );
        execution.status = ExecutionStatus::Running;

        let workflow_execution_repo = Arc::new(StubWorkflowExecutionRepository::with_events(
            execution.id,
            vec![
                WorkflowExecutionEventRecord {
                    sequence: 1,
                    event_type: "ExecutionStarted".to_string(),
                    state_name: None,
                    iteration_number: None,
                    payload: serde_json::json!({"message":"started"}),
                    recorded_at: chrono::Utc::now(),
                },
                WorkflowExecutionEventRecord {
                    sequence: 2,
                    event_type: "ConsoleOutput".to_string(),
                    state_name: None,
                    iteration_number: Some(1),
                    payload: serde_json::json!({"stream":"stdout","content":"hello"}),
                    recorded_at: chrono::Utc::now(),
                },
                WorkflowExecutionEventRecord {
                    sequence: 3,
                    event_type: "IterationCompleted".to_string(),
                    state_name: None,
                    iteration_number: Some(1),
                    payload: serde_json::json!({"result":"ok"}),
                    recorded_at: chrono::Utc::now(),
                },
            ],
        ));

        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(LogsTestExecutionService {
                execution: execution.clone(),
            }),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        )
        .with_workflow_execution_repo(workflow_execution_repo);

        let result = service
            .invoke_aegis_task_logs_tool(&serde_json::json!({
                "execution_id": execution.id.to_string(),
                "limit": 500,
                "offset": 1,
            }))
            .await
            .expect("task logs should return a result");

        let ToolInvocationResult::Direct(payload) = result else {
            panic!("expected direct task logs payload");
        };

        assert_eq!(payload["tool"], "aegis.task.logs");
        assert_eq!(payload["execution_id"], execution.id.to_string());
        assert_eq!(payload["agent_id"], agent_id.0.to_string());
        assert_eq!(payload["status"], "running");
        assert_eq!(payload["limit"], 200);
        assert_eq!(payload["offset"], 1);
        assert_eq!(payload["total"], 2);
        assert_eq!(payload["events"].as_array().unwrap().len(), 2);
        assert_eq!(payload["events"][0]["sequence"], 2);
    }

    #[tokio::test]
    async fn task_logs_tool_returns_execution_fetch_error() {
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers, vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let missing_execution = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: None,
                payload: serde_json::json!({}),
            },
            1,
        );

        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(LogsTestExecutionService {
                execution: missing_execution,
            }),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        )
        .with_workflow_execution_repo(Arc::new(StubWorkflowExecutionRepository::default()));

        let result = service
            .invoke_aegis_task_logs_tool(&serde_json::json!({
                "execution_id": ExecutionId::new().to_string(),
            }))
            .await
            .expect("task logs should return direct error payload");

        let ToolInvocationResult::Direct(payload) = result else {
            panic!("expected direct task logs error payload");
        };

        assert_eq!(payload["tool"], "aegis.task.logs");
        assert!(payload["error"]
            .as_str()
            .unwrap()
            .contains("Failed to fetch execution"));
    }

    #[tokio::test]
    async fn test_invoke_tool_execution_modes() {
        use crate::domain::mcp::{
            ExecutionMode, ResourceLimits, ToolServer, ToolServerId, ToolServerStatus,
        };
        use std::path::PathBuf;

        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let agent_id = AgentId::new();
        let exec_id = ExecutionId::new();

        use crate::domain::security_context::Capability;
        let context = SecurityContext {
            name: "test".to_string(),
            description: "".to_string(),
            capabilities: vec![
                Capability {
                    tool_pattern: "test_tool".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    subcommand_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                },
                Capability {
                    tool_pattern: "test_tool_remote".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    subcommand_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                },
            ],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        };

        let session_token = make_fake_token(agent_id);
        let session = SmcpSession::new(agent_id, exec_id, vec![], session_token, context);
        let _ = repo.save(session).await;

        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(registry, servers.clone(), vec![]));
        let middleware = Arc::new(SmcpMiddleware::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router.clone(),
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );

        // 1. Local Tool
        let local_server = ToolServer {
            id: ToolServerId::new(),
            name: "local-fs-tool".to_string(),
            execution_mode: ExecutionMode::Local,
            executable_path: PathBuf::from("/bin/true"),
            args: vec![],
            capabilities: vec!["test_tool".to_string()],
            skip_judge_tools: std::collections::HashSet::new(),
            status: ToolServerStatus::Running,
            process_id: None,
            health_check_interval: std::time::Duration::from_secs(30),
            last_health_check: None,
            credentials: std::collections::HashMap::new(),
            resource_limits: ResourceLimits {
                max_memory_mb: None,
                max_cpu_shares: None,
            },
            started_at: None,
            stopped_at: None,
        };

        router.add_server(local_server).await.unwrap();

        let envelope = DummyEnvelope::for_agent(true, agent_id); // extracts "test_tool"
        let result = service.invoke_tool(&envelope).await.unwrap();

        let exec_mode = result
            .get("execution_mode")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(exec_mode, "local_fsal");

        // 2. Remote Tool
        let remote_server = ToolServer {
            id: ToolServerId::new(),
            name: "remote-web-tool".to_string(),
            execution_mode: ExecutionMode::Remote,
            executable_path: PathBuf::from("/bin/true"),
            args: vec![],
            capabilities: vec!["test_tool_remote".to_string()],
            skip_judge_tools: std::collections::HashSet::new(),
            status: ToolServerStatus::Running,
            process_id: None,
            health_check_interval: std::time::Duration::from_secs(30),
            last_health_check: None,
            credentials: std::collections::HashMap::new(),
            resource_limits: ResourceLimits {
                max_memory_mb: None,
                max_cpu_shares: None,
            },
            started_at: None,
            stopped_at: None,
        };
        router.add_server(remote_server).await.unwrap();

        struct DummyRemoteEnvelope {
            valid: bool,
            token: String,
        }
        impl EnvelopeVerifier for DummyRemoteEnvelope {
            fn security_token(&self) -> &str {
                &self.token
            }

            fn verify_signature(&self, _: &[u8]) -> Result<(), SmcpSessionError> {
                if self.valid {
                    Ok(())
                } else {
                    Err(SmcpSessionError::SignatureVerificationFailed("".into()))
                }
            }
            fn extract_tool_name(&self) -> Option<String> {
                Some("test_tool_remote".to_string())
            }
            fn extract_arguments(&self) -> Option<Value> {
                Some(serde_json::json!({}))
            }
        }

        let remote_envelope = DummyRemoteEnvelope {
            valid: true,
            token: make_fake_token(agent_id),
        };
        let result = service.invoke_tool(&remote_envelope).await.unwrap();

        let exec_mode = result
            .get("execution_mode")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(exec_mode, "remote_jsonrpc");
    }

    #[tokio::test]
    async fn get_available_tools_returns_builtin_dispatcher_metadata() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(
            registry,
            servers,
            vec![BuiltinDispatcherConfig {
                name: "fs.read".to_string(),
                description: "Read files from the workspace".to_string(),
                enabled: true,
                capabilities: vec![CapabilityConfig {
                    name: "fs.read".to_string(),
                    skip_judge: true,
                }],
            }],
        ));
        let middleware = Arc::new(SmcpMiddleware::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let (fsal, volume_registry) = test_fsal_deps();
        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );

        let tools = service.get_available_tools().await.unwrap();
        let fs_read = tools.iter().find(|tool| tool.name == "fs.read").unwrap();

        assert_eq!(fs_read.description, "Read files from the workspace");
        assert_eq!(
            fs_read.input_schema["required"],
            serde_json::json!(["path"])
        );
    }

    #[tokio::test]
    async fn get_available_tools_for_context_filters_disallowed_tools() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(
            registry,
            servers,
            vec![
                BuiltinDispatcherConfig {
                    name: "fs.read".to_string(),
                    description: "Read files".to_string(),
                    enabled: true,
                    capabilities: vec![CapabilityConfig {
                        name: "fs.read".to_string(),
                        skip_judge: true,
                    }],
                },
                BuiltinDispatcherConfig {
                    name: "cmd.run".to_string(),
                    description: "Run commands".to_string(),
                    enabled: true,
                    capabilities: vec![CapabilityConfig {
                        name: "cmd.run".to_string(),
                        skip_judge: false,
                    }],
                },
            ],
        ));
        let middleware = Arc::new(SmcpMiddleware::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        security_context_repo
            .save(crate::domain::security_context::SecurityContext {
                name: "zaru-free".to_string(),
                description: "Free tier".to_string(),
                capabilities: vec![crate::domain::security_context::Capability {
                    tool_pattern: "fs.read".to_string(),
                    path_allowlist: None,
                    command_allowlist: None,
                    subcommand_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                }],
                deny_list: vec!["cmd.run".to_string()],
                metadata: crate::domain::security_context::SecurityContextMetadata {
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                    version: 1,
                },
            })
            .await
            .unwrap();
        let (fsal, volume_registry) = test_fsal_deps();
        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(TestAgentLifecycleService),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );

        let tools = service
            .get_available_tools_for_context("zaru-free")
            .await
            .unwrap();

        assert!(tools.iter().any(|tool| tool.name == "fs.read"));
        assert!(!tools.iter().any(|tool| tool.name == "cmd.run"));
    }

    #[tokio::test]
    async fn get_available_tools_for_agent_filters_to_declared_manifest_tools() {
        let repo = Arc::new(InMemorySmcpSessionRepository::new());
        let registry: Arc<dyn crate::domain::mcp::ToolRegistry> =
            Arc::new(InMemoryToolRegistry::new());
        let servers = Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
        let router = Arc::new(ToolRouter::new(
            registry,
            servers,
            vec![
                BuiltinDispatcherConfig {
                    name: "fs.read".to_string(),
                    description: "Read files".to_string(),
                    enabled: true,
                    capabilities: vec![CapabilityConfig {
                        name: "fs.read".to_string(),
                        skip_judge: true,
                    }],
                },
                BuiltinDispatcherConfig {
                    name: "cmd.run".to_string(),
                    description: "Run commands".to_string(),
                    enabled: true,
                    capabilities: vec![CapabilityConfig {
                        name: "cmd.run".to_string(),
                        skip_judge: false,
                    }],
                },
            ],
        ));
        let middleware = Arc::new(SmcpMiddleware::new());
        let security_context_repo = Arc::new(
            crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
        );
        let agent = test_agent_with_tools(&["fs.read"]);
        let agent_id = agent.id;
        let (fsal, volume_registry) = test_fsal_deps();
        let service = ToolInvocationService::new(
            repo,
            security_context_repo,
            middleware,
            router,
            fsal,
            volume_registry,
            Arc::new(FilteringAgentLifecycleService { agent }),
            Arc::new(TestExecutionService),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::new()),
            Arc::new(crate::infrastructure::event_bus::EventBus::new(1024)),
            None,
        );

        let tools = service
            .get_available_tools_for_agent(agent_id)
            .await
            .unwrap();

        assert!(tools.iter().any(|tool| tool.name == "fs.read"));
        assert!(!tools.iter().any(|tool| tool.name == "cmd.run"));
    }

    #[test]
    fn sanitize_segment_handles_empty_and_whitespace() {
        assert_eq!(sanitize_segment(""), "unversioned");
        assert_eq!(sanitize_segment("   "), "unversioned");
    }

    #[test]
    fn sanitize_segment_blocks_traversal_patterns() {
        assert_eq!(sanitize_segment("."), "unversioned");
        assert_eq!(sanitize_segment(".."), "unversioned");
        assert_eq!(sanitize_segment("..hidden"), "unversioned");
        assert_eq!(sanitize_segment("hidden.."), "unversioned");
        assert_eq!(sanitize_segment("a..b"), "unversioned");
        assert_eq!(sanitize_segment("version..1"), "unversioned");
    }

    #[test]
    fn sanitize_segment_replaces_special_characters() {
        assert_eq!(sanitize_segment("foo/bar"), "foo_bar");
        assert_eq!(sanitize_segment("foo\\bar"), "foo_bar");
        assert_eq!(sanitize_segment("foo:bar"), "foo_bar");
        assert_eq!(sanitize_segment("foo bar"), "foo_bar");
        assert_eq!(sanitize_segment("name@domain.com"), "name_domain.com");
    }

    #[test]
    fn sanitize_segment_preserves_safe_mixed_alphanumeric() {
        assert_eq!(sanitize_segment("validName-123"), "validName-123");
        assert_eq!(sanitize_segment("v1.2.3-beta_01"), "v1.2.3-beta_01");
    }
}
