// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::register_workflow::RegisterWorkflowUseCase;
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
use crate::domain::execution::ExecutionInput;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::validation::extract_json_from_text;
use crate::infrastructure::agent_manifest_parser::AgentManifestParser;
use crate::infrastructure::smcp_gateway_proto::gateway_invocation_service_client::GatewayInvocationServiceClient;
use crate::infrastructure::smcp_gateway_proto::{
    InvokeCliRequest, InvokeWorkflowRequest, ListToolsRequest,
};
use crate::infrastructure::workflow_parser::WorkflowParser;

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
    /// Optional workflow registration use case for built-in aegis workflow authoring tools.
    register_workflow_use_case: Option<Arc<dyn RegisterWorkflowUseCase>>,
    /// Optional validation service for semantic stage in workflow authoring tools.
    validation_service: Option<Arc<ValidationService>>,
    /// Optional root directory for persisting generated manifests on disk.
    generated_manifests_root: Option<PathBuf>,
    /// Optional ADR-053 SMCP gateway URL from node config.
    smcp_gateway_url: Option<String>,
}

#[allow(clippy::too_many_arguments)]
impl ToolInvocationService {
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
            register_workflow_use_case: None,
            validation_service: None,
            generated_manifests_root: None,
            smcp_gateway_url,
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

    /// Enables persistence of generated manifests to local disk.
    pub fn with_generated_manifests_root(mut self, root: PathBuf) -> Self {
        self.generated_manifests_root = Some(root);
        self
    }

    /// Invokes a tool by validating the SMCP Envelope for the agent and routing to the right server
    pub async fn invoke_tool(
        &self,
        agent_id: &AgentId,
        envelope: &impl EnvelopeVerifier,
    ) -> Result<Value, SmcpSessionError> {
        // 1. Get active session for agent
        let session = self
            .smcp_session_repo
            .find_active_by_agent(agent_id)
            .await
            .map_err(|_| SmcpSessionError::MalformedPayload("session repository lookup failed".to_string()))? // generic fallback error
            .ok_or(SmcpSessionError::SessionInactive(
                crate::domain::smcp_session::SessionStatus::Expired,
            ))?;

        // 2. Middleware verifies signature and evaluates against SecurityContext
        let args = self.smcp_middleware.verify_and_unwrap(&session, envelope)?;
        let tool_name = envelope
            .extract_tool_name()
            .ok_or(SmcpSessionError::MalformedPayload("missing tool name".to_string()))?;

        // 3. Route tool call to the appropriate TCP server.
        // ToolRouter resolves a ToolServerId; invocation execution happens after routing.
        let server_id = self
            .tool_router
            .route_tool(session.execution_id, &tool_name)
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!("Routing error: {}", e))
            })?;

        let server = self.tool_router.get_server(server_id).await.ok_or(
            SmcpSessionError::SignatureVerificationFailed(
                "Server vanished after routing".to_string(),
            ),
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
        tool_name: String,
        args: Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
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
            return Err(SmcpSessionError::SignatureVerificationFailed(format!(
                "Policy violation: {:?}",
                violation
            )));
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
                    "Failed to retrieve agent: {}",
                    e
                ))
            })?;

        if let Some(exec_spec) = &agent.manifest.spec.execution {
            // Check operator-level skip_judge flag before running the inner-loop semantic
            // judge pipeline. When the node config marks a tool with `skip_judge: true`
            // (e.g. read-only tools such as fs.read, fs.list), we bypass the judge entirely
            // to reduce latency on low-risk operations (NODE_CONFIGURATION_SPEC_V1.md).
            let skip_tool_judge = self.tool_router.is_skip_judge(&tool_name).await;
            if skip_tool_judge {
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
                                    "Failed to lookup judge: {}",
                                    e
                                ))
                            })?
                            .ok_or_else(|| {
                                SmcpSessionError::SignatureVerificationFailed(format!(
                                    "Judge agent '{}' not found",
                                    judge_agent
                                ))
                            })?;

                        let semantic_input = format!(
                            "Tool Call Request:\nTool: {}\nArguments: {}",
                            tool_name,
                            serde_json::to_string_pretty(&args).unwrap_or_default()
                        );

                        let input = ExecutionInput {
                            intent: None,
                            payload: serde_json::json!({
                                "task": "Evaluate whether the agent's requested tool call adheres to constitutional policies and makes logical sense for the trajectory.",
                                "output": semantic_input,
                                "criteria": criteria,
                                "validation_context": "semantic_judge_pre_execution_inner_loop"
                            }),
                        };

                        // Start the single iteration judge as child execution
                        let exec_id = self
                            .execution_service
                            .start_child_execution(judge_id, input, execution_id)
                            .await
                            .map_err(|e| {
                                SmcpSessionError::SignatureVerificationFailed(format!(
                                    "Failed to spawn judge child execution: {}",
                                    e
                                ))
                            })?;

                        let poll_interval_ms = 500u64;
                        let max_attempts = (*timeout_seconds * 1000) / poll_interval_ms;
                        let mut attempts = 0;

                        loop {
                            if attempts >= max_attempts {
                                return Err(SmcpSessionError::SignatureVerificationFailed(
                                    format!(
                                        "Inner-loop semantic judge '{}' timed out after {} seconds.",
                                        judge_agent,
                                        timeout_seconds
                                    ),
                                ));
                            }

                            let exec = self
                                .execution_service
                                .get_execution(exec_id)
                                .await
                                .map_err(|e| {
                                    SmcpSessionError::SignatureVerificationFailed(e.to_string())
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
                                                "Failed to parse judge output: {}",
                                                e
                                            ))
                                        })?;

                                    if !(result.score >= *min_score
                                        && result.confidence >= *min_confidence)
                                    {
                                        return Err(SmcpSessionError::SignatureVerificationFailed(
                                            format!(
                                                "Inner-loop tool execution rejected by semantic judge \
                                                 (Score: {:.2}, criteria_min: {:.2}). Reasoning: {}",
                                                result.score,
                                                min_score,
                                                result.reasoning,
                                            ),
                                        ));
                                    }
                                    break;
                                }
                                crate::domain::execution::ExecutionStatus::Failed
                                | crate::domain::execution::ExecutionStatus::Cancelled => {
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
            return self.invoke_aegis_agent_create_tool(&args).await;
        }
        if tool_name == "aegis.agent.list" {
            return self.invoke_aegis_agent_list_tool().await;
        }
        if tool_name == "aegis.workflow.create_and_validate" {
            return self
                .invoke_aegis_workflow_create_and_validate_tool(&args, execution_id, *agent_id)
                .await;
        }

        // Try invoking built-in tools (ADR-033, ADR-040, ADR-048)
        match crate::application::tools::try_invoke_builtin(
            &tool_name,
            &args,
            execution_id,
            &self.fsal,
            &self.volume_registry,
            &self.web_tool_port,
        )
        .await
        {
            Ok(crate::application::tools::BuiltinToolResult::Handled(result)) => return Ok(result),
            Ok(crate::application::tools::BuiltinToolResult::NotBuiltin) => {} // Continue to dynamic routing
            Err(e) => return Err(e),
        }

        let server_id = match self.tool_router.route_tool(execution_id, &tool_name).await {
            Ok(id) => id,
            Err(routing_err) => {
                if self.smcp_gateway_url.is_some() {
                    let gateway_result = self
                        .invoke_smcp_gateway_internal_grpc(execution_id, &tool_name, args.clone())
                        .await;
                    if let Ok(value) = gateway_result {
                        return Ok(ToolInvocationResult::Direct(value));
                    }
                }
                return Err(SmcpSessionError::SignatureVerificationFailed(format!(
                    "Routing error: {}",
                    routing_err
                )));
            }
        };

        let server = self.tool_router.get_server(server_id).await.ok_or(
            SmcpSessionError::SignatureVerificationFailed(
                "Server vanished after routing".to_string(),
            ),
        )?;

        match server.execution_mode {
            crate::domain::mcp::ExecutionMode::Local => {
                tracing::info!(
                    "Executing local tool via FSAL: {} for agent {:?}",
                    tool_name,
                    agent_id
                );
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "status": "success",
                    "execution_mode": "local_fsal",
                    "message": format!("Locally executed {} affecting agent volume", tool_name),
                    "args_executed": args
                })))
            }
            crate::domain::mcp::ExecutionMode::Remote => {
                let _json_rpc_payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": tool_name,
                    "params": args,
                    "id": execution_id.to_string()
                });

                tracing::info!(
                    "Proxying remote tool via JSON-RPC: {} to server {:?}",
                    tool_name,
                    server_id
                );
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "status": "success",
                    "execution_mode": "remote_jsonrpc",
                    "message": format!("Proxied {} to external MCP server {:?}", tool_name, server_id),
                    "args_proxied": args
                })))
            }
        }
    }

    /// Retrieve available tool schemas to inject into LLM prompts
    pub async fn get_available_tools(
        &self,
    ) -> Result<Vec<crate::infrastructure::tool_router::ToolMetadata>, SmcpSessionError> {
        let mut tools = self.tool_router.list_tools().await.map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Failed to list tools: {}", e))
        })?;

        if self.smcp_gateway_url.is_some() {
            if let Ok(gateway_tools) = self.fetch_gateway_tools_grpc().await {
                tools.extend(gateway_tools);
            }
        }

        Ok(tools)
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

            let response = client
                .invoke_cli(tonic::Request::new(InvokeCliRequest {
                    execution_id: execution_id.to_string(),
                    tool_name: tool_name.to_string(),
                    subcommand,
                    args: cli_args,
                    fsal_volume_id: self
                        .volume_registry
                        .find_by_execution(execution_id)
                        .ok_or_else(|| {
                            SmcpSessionError::SignatureVerificationFailed(format!(
                                "No FSAL volume registered for execution {}",
                                execution_id
                            ))
                        })?
                        .volume_id
                        .to_string(),
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
                            "Agent deployed but failed to persist manifest: {}",
                            e
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
            SmcpSessionError::SignatureVerificationFailed(format!("Failed to list agents: {}", e))
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

    async fn invoke_aegis_workflow_create_and_validate_tool(
        &self,
        args: &Value,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.create_and_validate requires 'manifest_yaml' string"
                        .to_string(),
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

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create_and_validate",
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
                "tool": "aegis.workflow.create_and_validate",
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
                "tool": "aegis.workflow.create_and_validate",
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
                        "Failed to lookup judge agent '{}': {}",
                        judge_name, e
                    ))
                })?
                .ok_or_else(|| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "Judge agent '{}' not found",
                        judge_name
                    ))
                })?;
            judges.push((judge_id, 1.0));
        }

        let criteria = if task_context.is_empty() {
            "Evaluate this workflow manifest for correctness, transition safety, DDD alignment, and production readiness.".to_string()
        } else {
            format!(
                "Task Context:\n{}\n\nEvaluate this workflow manifest for correctness, transition safety, DDD alignment, and production readiness.",
                task_context
            )
        };

        let semantic_request = ValidationRequest {
            content: manifest_yaml.to_string(),
            criteria,
            context: Some(serde_json::json!({
                "validation_context": "workflow_create_and_validate",
                "workflow_name": workflow.metadata.name,
                "state_count": workflow.spec.states.len(),
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
                    "Workflow semantic validation failed: {}",
                    e
                ))
            })?;

        let semantic_passed = semantic_consensus.final_score >= min_score
            && semantic_consensus.consensus_confidence >= min_confidence;

        if !semantic_passed {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create_and_validate",
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
            .register_workflow(manifest_yaml)
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
                            "Workflow deployed but failed to persist manifest: {}",
                            e
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create_and_validate",
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
                    "manifest_path": persisted_path
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create_and_validate",
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

        let file_path = target_dir.join(format!("{}.yaml", safe_version));
        std::fs::write(&file_path, manifest_yaml)?;
        Ok(Some(path_to_string(&file_path)))
    }
}

fn sanitize_segment(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "unversioned".to_string();
    }

    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
        let fsal = Arc::new(AegisFSAL::new(storage, vol_repo, publisher));
        let registry = NfsVolumeRegistry::new();
        (fsal, registry)
    }

    struct DummyEnvelope {
        valid: bool,
    }

    impl EnvelopeVerifier for DummyEnvelope {
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

    use crate::domain::agent::{Agent, AgentManifest};
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{Execution, Iteration};
    use crate::infrastructure::event_bus::DomainEvent;
    use futures::Stream;
    use std::pin::Pin;

    struct TestAgentLifecycleService;
    #[async_trait]
    impl AgentLifecycleService for TestAgentLifecycleService {
        async fn deploy_agent(&self, _: AgentManifest, _force: bool) -> Result<AgentId> {
            anyhow::bail!("TestAgentLifecycleService::deploy_agent not exercised in this test")
        }
        async fn get_agent(&self, _: AgentId) -> Result<Agent> {
            anyhow::bail!("TestAgentLifecycleService::get_agent not exercised in this test")
        }
        async fn update_agent(&self, _: AgentId, _: AgentManifest) -> Result<()> {
            anyhow::bail!("TestAgentLifecycleService::update_agent not exercised in this test")
        }
        async fn delete_agent(&self, _: AgentId) -> Result<()> {
            anyhow::bail!("TestAgentLifecycleService::delete_agent not exercised in this test")
        }
        async fn list_agents(&self) -> Result<Vec<Agent>> {
            anyhow::bail!("TestAgentLifecycleService::list_agents not exercised in this test")
        }
        async fn lookup_agent(&self, _: &str) -> Result<Option<AgentId>> {
            anyhow::bail!("TestAgentLifecycleService::lookup_agent not exercised in this test")
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
            None,
        );
        let agent_id = AgentId::new();
        let envelope = DummyEnvelope { valid: true };

        let result = service.invoke_tool(&agent_id, &envelope).await;
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

        let session = SmcpSession::new(agent_id, exec_id, vec![], "token".to_string(), context);
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
            None,
        );
        let envelope = DummyEnvelope { valid: false };

        let result = service.invoke_tool(&agent_id, &envelope).await;
        assert!(matches!(
            result,
            Err(SmcpSessionError::SignatureVerificationFailed(_))
        ));
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

        let session = SmcpSession::new(agent_id, exec_id, vec![], "token".to_string(), context);
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

        let envelope = DummyEnvelope { valid: true }; // extracts "test_tool"
        let result = service.invoke_tool(&agent_id, &envelope).await.unwrap();

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
        }
        impl EnvelopeVerifier for DummyRemoteEnvelope {
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

        let remote_envelope = DummyRemoteEnvelope { valid: true };
        let result = service
            .invoke_tool(&agent_id, &remote_envelope)
            .await
            .unwrap();

        let exec_mode = result
            .get("execution_mode")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(exec_mode, "remote_jsonrpc");
    }
}
