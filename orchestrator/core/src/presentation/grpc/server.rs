// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! gRPC Server Implementation for AEGIS Runtime
//! Exposes ExecuteAgent, ExecuteSystemCommand, ValidateWithJudges, QueryCortexPatterns, StoreCortexPattern
//!
//! # Architecture
//!
//! - **Layer:** Presentation Layer
//! - **Purpose:** Implements internal responsibilities for server

use chrono::Utc;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::application::agent::AgentLifecycleService;
use crate::application::execution::ExecutionService;
use crate::application::run_container_step::RunContainerStepUseCase;
use crate::application::stimulus::StimulusService;
use crate::application::validation_service::ValidationService;
use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionInput;
use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::tenant::TenantId;

const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 300;
const DEFAULT_VALIDATION_TIMEOUT_SECS: u64 = 60;
const DEFAULT_VALIDATION_POLL_INTERVAL_MS: u64 = 500;
use crate::domain::stimulus::{Stimulus, StimulusSource};
use crate::presentation::grpc::auth_interceptor::{validate_grpc_request, GrpcIamAuthInterceptor};
use crate::presentation::metrics_middleware::GrpcMetricsLayer;

// Generated protobuf code lives in infrastructure::aegis_runtime_proto (ADR-042)
// so that both the server and the CortexGrpcClient client share the same Rust types.
use crate::infrastructure::aegis_runtime_proto::aegis_runtime_server::{
    AegisRuntime, AegisRuntimeServer,
};
use crate::infrastructure::aegis_runtime_proto::*;

/// Implementation of the AegisRuntime gRPC service
pub struct AegisRuntimeService {
    execution_service: Arc<dyn ExecutionService>,
    validation_service: Arc<ValidationService>,
    grpc_auth: Option<GrpcIamAuthInterceptor>,
    attestation_service:
        Option<Arc<dyn crate::infrastructure::smcp::attestation::AttestationService>>,
    tool_invocation_service:
        Option<Arc<crate::application::tool_invocation_service::ToolInvocationService>>,
    cortex_client: Option<Arc<crate::infrastructure::CortexGrpcClient>>,
    /// BC-8: Stimulus routing service (ADR-021). Optional until wired.
    stimulus_service: Option<Arc<dyn StimulusService>>,
    /// BC-3: Container step runner use case (ADR-050). Optional until wired.
    run_container_step_use_case: Option<Arc<RunContainerStepUseCase>>,
    /// BC-1: Agent lifecycle service for resolving agent name → UUID at execution time (Phase 1b).
    agent_service: Option<Arc<dyn AgentLifecycleService>>,
}

impl AegisRuntimeService {
    fn tenant_id_from_identity(identity: Option<&UserIdentity>) -> TenantId {
        match identity.map(|identity| &identity.identity_kind) {
            Some(IdentityKind::TenantUser { tenant_slug }) => {
                TenantId::from_string(tenant_slug).unwrap_or_else(|_| TenantId::local_default())
            }
            _ => TenantId::local_default(),
        }
    }

    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        validation_service: Arc<ValidationService>,
    ) -> Self {
        Self {
            execution_service,
            validation_service,
            grpc_auth: None,
            attestation_service: None,
            tool_invocation_service: None,
            cortex_client: None,
            stimulus_service: None,
            run_container_step_use_case: None,
            agent_service: None,
        }
    }

    /// Set the SMCP services (optional)
    pub fn with_smcp(
        mut self,
        attestation_service: Arc<dyn crate::infrastructure::smcp::attestation::AttestationService>,
        tool_invocation_service: Arc<
            crate::application::tool_invocation_service::ToolInvocationService,
        >,
    ) -> Self {
        self.attestation_service = Some(attestation_service);
        self.tool_invocation_service = Some(tool_invocation_service);
        self
    }

    /// Enable IAM/OIDC auth checks on protected gRPC methods.
    pub fn with_grpc_auth(mut self, interceptor: GrpcIamAuthInterceptor) -> Self {
        self.grpc_auth = Some(interceptor);
        self
    }

    /// Set the Cortex gRPC client (optional — omit for memoryless mode, per ADR-042)
    pub fn with_cortex(
        mut self,
        cortex_client: Arc<crate::infrastructure::CortexGrpcClient>,
    ) -> Self {
        self.cortex_client = Some(cortex_client);
        self
    }

    /// Set the Stimulus routing service (optional — omit if BC-8 is not deployed)
    pub fn with_stimulus(mut self, stimulus_service: Arc<dyn StimulusService>) -> Self {
        self.stimulus_service = Some(stimulus_service);
        self
    }

    /// Set the container step runner use case (optional — omit if BC-3 ADR-050 is not needed)
    pub fn with_container_step_runner(mut self, use_case: Arc<RunContainerStepUseCase>) -> Self {
        self.run_container_step_use_case = Some(use_case);
        self
    }

    /// Set the agent lifecycle service for name-to-UUID resolution at execution time (Phase 1b).
    pub fn with_agent_service(mut self, svc: Arc<dyn AgentLifecycleService>) -> Self {
        self.agent_service = Some(svc);
        self
    }

    /// Create a gRPC server instance
    pub fn into_server(self) -> AegisRuntimeServer<Self> {
        AegisRuntimeServer::new(self)
    }

    async fn authorize<T>(
        &self,
        request: &Request<T>,
        method: &str,
    ) -> Result<Option<UserIdentity>, Status> {
        if let Some(interceptor) = &self.grpc_auth {
            return validate_grpc_request(interceptor, request, method).await;
        }

        Ok(None)
    }
}

#[tonic::async_trait]
impl AegisRuntime for AegisRuntimeService {
    type ExecuteAgentStream = ReceiverStream<Result<ExecutionEvent, Status>>;

    /// Execute an agent with 100monkeys iterative refinement
    /// Streams execution events in real-time
    async fn execute_agent(
        &self,
        request: Request<ExecuteAgentRequest>,
    ) -> Result<Response<Self::ExecuteAgentStream>, Status> {
        let identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/ExecuteAgent")
            .await?;
        let req = request.into_inner();
        let tenant_id = Self::tenant_id_from_identity(identity.as_ref());

        // Parse agent_id — accept UUID or human-readable name (resolved via agent_service)
        let agent_id = if let Ok(id) = AgentId::from_string(&req.agent_id) {
            id
        } else if let Some(ref svc) = self.agent_service {
            match svc.lookup_agent_for_tenant(&tenant_id, &req.agent_id).await {
                Ok(Some(id)) => id,
                Ok(None) => {
                    return Err(Status::not_found(format!(
                        "Agent '{}' not found; deploy it with `aegis agent deploy` first",
                        req.agent_id
                    )))
                }
                Err(e) => {
                    return Err(Status::internal(format!(
                        "Failed to resolve agent '{}': {e}",
                        req.agent_id
                    )))
                }
            }
        } else {
            return Err(Status::invalid_argument(format!(
                "Invalid agent_id '{}': not a UUID and agent name resolution is unavailable",
                req.agent_id
            )));
        };

        // Create execution input
        let payload = if req.context_json.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&req.context_json)
                .map_err(|e| Status::invalid_argument(format!("Invalid context_json: {e}")))?
        };

        // Let ExecutionService render the agent's prompt_template
        // instead of bypassing it by setting intent directly. This ensures agents
        // behave consistently regardless of API type (gRPC, REST, CLI).
        let input = ExecutionInput {
            intent: None, // Let ExecutionService render agent's prompt_template
            payload: serde_json::json!({
                "input": req.input,  // User-provided input
                "context_overrides": payload,  // Additional context if provided
                "tenant_id": tenant_id.to_string(),
            }),
        };

        // Channel for streaming events
        let (tx, rx) = mpsc::channel(100);

        // Clone for async task
        let execution_service = self.execution_service.clone();
        let tx_clone = tx.clone();

        // Spawn execution task
        tokio::spawn(async move {
            // Send ExecutionStarted event
            let _ = tx_clone
                .send(Ok(ExecutionEvent {
                    event: Some(execution_event::Event::ExecutionStarted(ExecutionStarted {
                        execution_id: uuid::Uuid::new_v4().to_string(),
                        agent_id: agent_id.0.to_string(),
                        started_at: Utc::now().to_rfc3339(),
                    })),
                }))
                .await;

            // Check for ADR-016 nested execution
            let start_result = if let Some(parent_id_str) = req.parent_execution_id {
                let parent_id =
                    match crate::domain::execution::ExecutionId::from_string(&parent_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            let _ = tx_clone
                                .send(Ok(ExecutionEvent {
                                    event: Some(execution_event::Event::ExecutionFailed(
                                        ExecutionFailed {
                                            execution_id: uuid::Uuid::new_v4().to_string(),
                                            reason: format!("Invalid parent_execution_id: {e}"),
                                            total_iterations: 0,
                                            failed_at: Utc::now().to_rfc3339(),
                                        },
                                    )),
                                }))
                                .await;
                            return;
                        }
                    };
                execution_service
                    .start_child_execution(agent_id, input, parent_id)
                    .await
            } else {
                execution_service.start_execution(agent_id, input).await
            };

            // Start execution
            match start_result {
                Ok(execution_id) => {
                    // Stream execution events
                    match execution_service.stream_execution(execution_id).await {
                        Ok(mut stream) => {
                            use futures::StreamExt;
                            while let Some(event_result) = stream.next().await {
                                match event_result {
                                    Ok(domain_event) => {
                                        // Convert domain event to protobuf event
                                        if let Some(pb_event) = convert_domain_event_to_proto(
                                            domain_event,
                                            execution_id,
                                        ) {
                                            if tx_clone.send(Ok(pb_event)).await.is_err() {
                                                break; // Client disconnected
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx_clone
                                            .send(Ok(ExecutionEvent {
                                                event: Some(
                                                    execution_event::Event::ExecutionFailed(
                                                        ExecutionFailed {
                                                            execution_id: execution_id.to_string(),
                                                            reason: e.to_string(),
                                                            total_iterations: 0,
                                                            failed_at: Utc::now().to_rfc3339(),
                                                        },
                                                    ),
                                                ),
                                            }))
                                            .await;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx_clone
                                .send(Ok(ExecutionEvent {
                                    event: Some(execution_event::Event::ExecutionFailed(
                                        ExecutionFailed {
                                            execution_id: execution_id.to_string(),
                                            reason: format!("Failed to stream execution: {e}"),
                                            total_iterations: 0,
                                            failed_at: Utc::now().to_rfc3339(),
                                        },
                                    )),
                                }))
                                .await;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx_clone
                        .send(Ok(ExecutionEvent {
                            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                                execution_id: uuid::Uuid::new_v4().to_string(),
                                reason: format!("Failed to start execution: {e}"),
                                total_iterations: 0,
                                failed_at: Utc::now().to_rfc3339(),
                            })),
                        }))
                        .await;
                }
            }
        });

        // Drop the outer sender so the gRPC stream closes once the spawned task
        // finishes and releases its cloned sender.
        drop(tx);

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Execute a system command
    async fn execute_system_command(
        &self,
        request: Request<ExecuteSystemCommandRequest>,
    ) -> Result<Response<ExecuteSystemCommandResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/ExecuteSystemCommand")
            .await?;
        let req = request.into_inner();

        // Execute command using tokio::process::Command
        let start_time = std::time::Instant::now();

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&req.command);

        // Set environment variables
        for (key, value) in req.env {
            cmd.env(key, value);
        }

        // Set working directory
        if let Some(workdir) = req.workdir {
            cmd.current_dir(workdir);
        }

        // Execute with timeout
        let timeout = std::time::Duration::from_secs(
            req.timeout_seconds
                .map(|secs| secs as u64)
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS),
        );

        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => {
                let duration_ms = start_time.elapsed().as_millis() as u64;

                Ok(Response::new(ExecuteSystemCommandResponse {
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    duration_ms,
                }))
            }
            Ok(Err(e)) => Err(Status::internal(format!("Command execution failed: {e}"))),
            Err(_) => Err(Status::deadline_exceeded("Command execution timed out")),
        }
    }

    /// Validate output using gradient scoring (judge agents)
    async fn validate_with_judges(
        &self,
        request: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/ValidateWithJudges")
            .await?;
        let req = request.into_inner();

        // Parse judge agent IDs and preserve per-judge weights (ADR-017)
        let judge_configs: Vec<(AgentId, f64)> = req
            .judges
            .iter()
            .map(|j| {
                AgentId::from_string(&j.agent_id)
                    .map(|id| (id, if j.weight > 0.0 { j.weight as f64 } else { 1.0 }))
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Status::invalid_argument(format!("Invalid judge agent_id: {e}")))?;

        // Parse context_json once; extract execution_id/agent_id for proper child-execution linking
        let context_value: Option<serde_json::Value> = if req.context_json.is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(&req.context_json)
                    .map_err(|e| Status::invalid_argument(format!("Invalid context_json: {e}")))?,
            )
        };

        let exec_id = context_value
            .as_ref()
            .and_then(|v| v["execution_id"].as_str())
            .and_then(|s| crate::domain::execution::ExecutionId::from_string(s).ok())
            .unwrap_or_default();

        let agent_id = context_value
            .as_ref()
            .and_then(|v| v["agent_id"].as_str())
            .and_then(|s| AgentId::from_string(s).ok())
            .unwrap_or_default();

        // Map proto ConsensusConfig → domain ConsensusConfig (ADR-017)
        use crate::domain::workflow::{ConsensusConfig, ConsensusStrategy};
        let consensus_config: Option<ConsensusConfig> = req.consensus.map(|c| {
            let strategy = match c.strategy {
                1 => ConsensusStrategy::Majority,
                2 => ConsensusStrategy::Unanimous,
                _ => ConsensusStrategy::WeightedAverage,
            };
            ConsensusConfig {
                strategy,
                threshold: c.threshold.map(|t| t as f64),
                min_agreement_confidence: c.agreement.map(|a| a as f64),
                n: c.n.map(|n| n as usize),
                min_judges_required: 1,
                confidence_weighting: None,
            }
        });

        // Determine binary pass/fail threshold — ADR-017 §3 specifies default 0.8
        let binary_threshold = consensus_config
            .as_ref()
            .and_then(|c| c.threshold)
            .unwrap_or(0.8);

        // Build domain ValidationRequest
        use crate::domain::validation::ValidationRequest;
        let validation_req = ValidationRequest {
            content: req.output,
            criteria: req.task,
            context: context_value,
        };

        match self
            .validation_service
            .validate_with_judges(
                exec_id,
                agent_id,
                0, // iteration_number tracked by Temporal workflow, not this endpoint
                validation_req,
                judge_configs,
                consensus_config,
                DEFAULT_VALIDATION_TIMEOUT_SECS, // 60 second timeout
                DEFAULT_VALIDATION_POLL_INTERVAL_MS, // 500ms poll interval
            )
            .await
        {
            Ok(consensus) => {
                let individual_results = consensus
                    .individual_results
                    .into_iter()
                    .map(|(agent_id, r)| JudgeResult {
                        judge_id: agent_id.0.to_string(),
                        score: r.score as f32,
                        confidence: r.confidence as f32,
                        reasoning: r.reasoning,
                    })
                    .collect();

                Ok(Response::new(ValidateResponse {
                    score: consensus.final_score as f32,
                    confidence: consensus.consensus_confidence as f32,
                    reasoning: format!("Consensus reached with strategy: {}", consensus.strategy),
                    binary_valid: consensus.final_score >= binary_threshold,
                    individual_results,
                }))
            }
            Err(e) => Err(Status::internal(format!("Validation failed: {e}"))),
        }
    }

    /// Query Cortex for patterns based on error signature or embedding similarity.
    /// Forwards the call to the standalone `aegis-cortex` service when configured;
    /// returns an empty result set in memoryless mode (no warning per ADR-042).
    async fn query_cortex_patterns(
        &self,
        request: Request<QueryCortexRequest>,
    ) -> Result<Response<QueryCortexResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/QueryCortexPatterns")
            .await?;
        let req = request.into_inner();

        match &self.cortex_client {
            Some(client) => client.query_patterns(req).await.map(Response::new),
            None => Ok(Response::new(QueryCortexResponse { patterns: vec![] })),
        }
    }

    /// Store a learned pattern in Cortex with automatic deduplication.
    /// Forwards the call to the standalone `aegis-cortex` service when configured;
    /// returns a no-op response in memoryless mode (no warning per ADR-042).
    async fn store_cortex_pattern(
        &self,
        request: Request<StoreCortexPatternRequest>,
    ) -> Result<Response<StoreCortexPatternResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/StoreCortexPattern")
            .await?;
        let req = request.into_inner();

        match &self.cortex_client {
            Some(client) => client.store_pattern(req).await.map(Response::new),
            None => Ok(Response::new(StoreCortexPatternResponse {
                pattern_id: String::new(),
                deduplicated: false,
                new_frequency: 0,
            })),
        }
    }

    /// Store a learned trajectory pattern in Cortex (ADR-049).
    async fn store_trajectory_pattern(
        &self,
        request: Request<StoreTrajectoryPatternRequest>,
    ) -> Result<Response<StoreTrajectoryPatternResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/StoreTrajectoryPattern")
            .await?;
        let req = request.into_inner();

        match &self.cortex_client {
            Some(client) => client
                .store_trajectory_pattern(req)
                .await
                .map(Response::new),
            None => Ok(Response::new(StoreTrajectoryPatternResponse {
                trajectory_id: String::new(),
                new_weight: 0.0,
                deduplicated: false,
            })),
        }
    }

    /// Attest an agent to receive an SMCP Security Token
    async fn attest_agent(
        &self,
        request: Request<AttestAgentRequest>,
    ) -> Result<Response<AttestAgentResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/AttestAgent")
            .await?;
        let req = request.into_inner();

        let attestation_service = self.attestation_service.as_ref().ok_or_else(|| {
            Status::failed_precondition("SMCP attestation service is not configured")
        })?;

        let attestation_req = crate::infrastructure::smcp::attestation::AttestationRequest {
            agent_id: Some(req.agent_id),
            execution_id: Some(req.execution_id),
            container_id: Some(req.container_id),
            public_key_pem: req.public_key_pem,
            security_context: None,
            principal_subject: None,
            user_id: None,
            workload_id: None,
            zaru_tier: None,
        };

        match attestation_service.attest(attestation_req).await {
            Ok(res) => Ok(Response::new(AttestAgentResponse {
                security_token: res.security_token,
            })),
            Err(e) => Err(Status::internal(format!("Attestation failed: {e}"))),
        }
    }

    /// Invoke a tool via orchestrator mediation (SMCP)
    async fn invoke_tool(
        &self,
        request: Request<InvokeToolRequest>,
    ) -> Result<Response<InvokeToolResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/InvokeTool")
            .await?;
        let req = request.into_inner();

        let tool_invocation_service = self.tool_invocation_service.as_ref().ok_or_else(|| {
            Status::failed_precondition("SMCP tool invocation service is not configured")
        })?;

        // Construct SmcpEnvelope. The ToolInvocationService is responsible for
        // validating the security_token and handling extraction of any required
        // claims (such as agent_id) according to its own verification logic.
        let envelope = crate::infrastructure::smcp::envelope::SmcpEnvelope {
            protocol: None,
            security_token: req.security_token,
            signature: req.signature,
            inner_mcp: req.inner_mcp,
            timestamp: None,
        };

        match tool_invocation_service.invoke_tool(&envelope).await {
            Ok(result) => {
                let bytes = serde_json::to_vec(&result).map_err(|e| {
                    Status::internal(format!("Failed to serialize tool result: {e}"))
                })?;
                Ok(Response::new(InvokeToolResponse { result_json: bytes }))
            }
            Err(e) => Err(Status::permission_denied(format!(
                "Tool invocation rejected: {e}"
            ))),
        }
    }

    /// Ingest an external stimulus and route it to a workflow (BC-8 — ADR-021).
    async fn ingest_stimulus(
        &self,
        request: Request<IngestStimulusRequest>,
    ) -> Result<Response<IngestStimulusResponse>, Status> {
        let identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/IngestStimulus")
            .await?;
        let req = request.into_inner();
        let tenant_id = Self::tenant_id_from_identity(identity.as_ref());
        let (stimulus_id, workflow_execution_id) = self
            .ingest_stimulus_rpc(
                req.source_name,
                req.content,
                req.idempotency_key,
                req.headers,
                tenant_id,
            )
            .await?;
        Ok(Response::new(IngestStimulusResponse {
            stimulus_id,
            workflow_execution_id,
        }))
    }

    /// Execute a deterministic CI/CD container step (ADR-050).
    async fn execute_container_run(
        &self,
        request: Request<ExecuteContainerRunRequest>,
    ) -> Result<Response<ExecuteContainerRunResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/ExecuteContainerRun")
            .await?;
        let req = request.into_inner();

        let use_case = self
            .run_container_step_use_case
            .as_ref()
            .ok_or_else(|| Status::unavailable("ContainerRun is not configured on this node"))?;

        use crate::application::run_container_step::RunContainerStepInput;
        use crate::domain::agent::ImagePullPolicy;
        use crate::domain::execution::ExecutionId;
        use crate::domain::workflow::{ContainerResources, ContainerVolumeMount, StateName};
        use std::collections::HashMap;

        let execution_id = ExecutionId::from_string(&req.execution_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid execution_id: {e}")))?;

        let state_name = StateName::new(req.state_name.clone())
            .map_err(|e| Status::invalid_argument(format!("Invalid state_name: {e}")))?;

        let image_pull_policy = match req.image_pull_policy.to_lowercase().as_str() {
            "always" => ImagePullPolicy::Always,
            "never" => ImagePullPolicy::Never,
            _ => ImagePullPolicy::IfNotPresent,
        };

        let volumes: Vec<ContainerVolumeMount> = req
            .volumes
            .into_iter()
            .map(|v| ContainerVolumeMount {
                name: v.name,
                mount_path: v.mount_path,
                read_only: v.read_only,
            })
            .collect();

        let resources = if let Some(res) = req.resources {
            let timeout = if res.timeout.is_empty() {
                None
            } else {
                humantime_serde::re::humantime::parse_duration(&res.timeout).ok()
            };
            Some(ContainerResources {
                cpu: if res.cpu_millicores == 0 {
                    None
                } else {
                    Some(res.cpu_millicores)
                },
                memory: if res.memory.is_empty() {
                    None
                } else {
                    Some(res.memory)
                },
                timeout,
            })
        } else {
            None
        };

        let env: HashMap<String, String> = req.env;
        let max_attempts = if req.max_attempts == 0 {
            1
        } else {
            req.max_attempts
        };

        let input = RunContainerStepInput {
            execution_id,
            state_name,
            name: req.name,
            image: req.image,
            image_pull_policy,
            command: req.command,
            env,
            workdir: if req.workdir.is_empty() {
                None
            } else {
                Some(req.workdir)
            },
            volumes,
            resources,
            registry_credentials: if req.registry_credentials.is_empty() {
                None
            } else {
                Some(req.registry_credentials)
            },
            max_attempts,
            shell: req.shell,
        };

        match use_case.execute(input).await {
            Ok(output) => Ok(Response::new(ExecuteContainerRunResponse {
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                duration_ms: output.duration_ms,
                attempts: output.attempts,
            })),
            Err(e) => {
                use crate::domain::runtime::ContainerStepError;
                let status = match &e {
                    ContainerStepError::ImagePullFailed { image, error } => {
                        Status::unavailable(format!("Image pull failed for '{image}': {error}"))
                    }
                    ContainerStepError::TimeoutExpired { timeout_secs } => {
                        Status::deadline_exceeded(format!(
                            "Container step timed out after {timeout_secs}s"
                        ))
                    }
                    ContainerStepError::VolumeMountFailed { volume, error } => {
                        Status::internal(format!("Volume mount failed for '{volume}': {error}"))
                    }
                    ContainerStepError::ResourceExhausted { detail } => {
                        Status::resource_exhausted(format!("Resource exhausted: {detail}"))
                    }
                    ContainerStepError::DockerError(msg) => {
                        Status::internal(format!("Docker error: {msg}"))
                    }
                };
                Err(status)
            }
        }
    }
}

// ── BC-8: IngestStimulus gRPC helper ──────────────────────────────────────────
// Shared implementation logic used by the `ingest_stimulus` trait method above.
// The HTTP equivalent is live at `POST /v1/webhooks/{source}` and `POST /v1/stimuli`.
impl AegisRuntimeService {
    /// Handle `IngestStimulus` RPC (BC-8 — ADR-021).
    pub async fn ingest_stimulus_rpc(
        &self,
        source_name: String,
        content: String,
        idempotency_key: String,
        mut headers: std::collections::HashMap<String, String>,
        tenant_id: TenantId,
    ) -> Result<(String, String), Status> {
        let stimulus_service = self
            .stimulus_service
            .as_ref()
            .ok_or_else(|| Status::unavailable("Stimulus service is not configured"))?;

        headers
            .entry("x-aegis-tenant".to_string())
            .or_insert_with(|| tenant_id.to_string());

        let stimulus = Stimulus::new(StimulusSource::Webhook { source_name }, content)
            .with_headers(headers)
            .with_idempotency_key(idempotency_key);

        match stimulus_service.ingest(stimulus).await {
            Ok(resp) => Ok((resp.stimulus_id.to_string(), resp.workflow_execution_id)),
            Err(e) => {
                use crate::application::stimulus::StimulusError;
                match e {
                    StimulusError::IdempotentDuplicate { original_id } => {
                        Err(Status::already_exists(format!(
                            "Idempotent duplicate: original stimulus {original_id}"
                        )))
                    }
                    StimulusError::LowConfidence {
                        confidence,
                        threshold,
                    } => Err(Status::failed_precondition(format!(
                        "Classification confidence {confidence:.2} below threshold {threshold:.2}"
                    ))),
                    StimulusError::NoRouterConfigured { source_name } => Err(Status::not_found(
                        format!("No route or router agent configured for source '{source_name}'"),
                    )),
                    other => Err(Status::internal(other.to_string())),
                }
            }
        }
    }
}

/// Convert domain ExecutionEvent to protobuf ExecutionEvent
fn convert_domain_event_to_proto(
    domain_event: crate::domain::events::ExecutionEvent,
    execution_id: crate::domain::execution::ExecutionId,
) -> Option<ExecutionEvent> {
    use crate::domain::events::ExecutionEvent as DomainEvent;

    match domain_event {
        DomainEvent::IterationStarted {
            iteration_number,
            action,
            started_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationStarted(IterationStarted {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                action,
                started_at: started_at.to_rfc3339(),
            })),
        }),
        DomainEvent::ConsoleOutput {
            iteration_number,
            content,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationOutput(IterationOutput {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                output: content,
            })),
        }),
        DomainEvent::IterationCompleted {
            iteration_number,
            output,
            completed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationCompleted(
                IterationCompleted {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    output,
                    completed_at: completed_at.to_rfc3339(),
                },
            )),
        }),
        DomainEvent::IterationFailed {
            iteration_number,
            error,
            failed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationFailed(IterationFailed {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                error: Some(IterationError {
                    error_type: "runtime_error".to_string(),
                    message: error.message.clone(),
                    stacktrace: error.details.clone(),
                }),
                failed_at: failed_at.to_rfc3339(),
            })),
        }),
        DomainEvent::RefinementApplied {
            iteration_number,
            code_diff,
            applied_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::RefinementApplied(
                RefinementApplied {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    code_diff: format!("{}:\n{}", code_diff.file_path, code_diff.diff),
                    applied_at: applied_at.to_rfc3339(),
                },
            )),
        }),
        DomainEvent::ExecutionCompleted {
            final_output,
            total_iterations,
            completed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionCompleted(
                ExecutionCompleted {
                    execution_id: execution_id.to_string(),
                    final_output,
                    total_iterations: total_iterations as u32,
                    completed_at: completed_at.to_rfc3339(),
                },
            )),
        }),
        DomainEvent::ExecutionFailed {
            reason,
            total_iterations,
            failed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                execution_id: execution_id.to_string(),
                reason,
                total_iterations: total_iterations as u32,
                failed_at: failed_at.to_rfc3339(),
            })),
        }),
        _ => None, // Other events not relevant for streaming
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    execution_service: Arc<dyn ExecutionService>,
    validation_service: Arc<ValidationService>,
    grpc_auth: Option<GrpcIamAuthInterceptor>,
    attestation_service: Option<
        Arc<dyn crate::infrastructure::smcp::attestation::AttestationService>,
    >,
    tool_invocation_service: Option<
        Arc<crate::application::tool_invocation_service::ToolInvocationService>,
    >,
    cortex_client: Option<Arc<crate::infrastructure::CortexGrpcClient>>,
    run_container_step_use_case: Option<Arc<RunContainerStepUseCase>>,
    agent_service: Arc<dyn AgentLifecycleService>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut service = AegisRuntimeService::new(execution_service, validation_service);

    if let Some(auth) = grpc_auth {
        service = service.with_grpc_auth(auth);
    }

    if let (Some(a), Some(t)) = (attestation_service, tool_invocation_service) {
        service = service.with_smcp(a, t);
    }

    if let Some(c) = cortex_client {
        service = service.with_cortex(c);
    }

    if let Some(uc) = run_container_step_use_case {
        service = service.with_container_step_runner(uc);
    }

    service = service.with_agent_service(agent_service);

    let server = service.into_server();

    tracing::info!("Starting AEGIS gRPC server on {}", addr);

    tonic::transport::Server::builder()
        .layer(GrpcMetricsLayer)
        .add_service(server)
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::ExecutionEvent as DomainExecutionEvent;
    use crate::domain::execution::{Execution, ExecutionId, ExecutionInput};
    use crate::infrastructure::event_bus::{DomainEvent, EventBus};
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::Stream;
    use std::pin::Pin;
    use tokio::time::{timeout, Duration};
    use tokio_stream::StreamExt;

    struct TestExecutionService {
        execution_id: ExecutionId,
        stream_events: Vec<DomainExecutionEvent>,
    }

    #[async_trait]
    impl ExecutionService for TestExecutionService {
        async fn start_execution(
            &self,
            _agent_id: AgentId,
            _input: ExecutionInput,
        ) -> Result<ExecutionId> {
            Ok(self.execution_id)
        }

        async fn start_child_execution(
            &self,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _parent_execution_id: ExecutionId,
        ) -> Result<ExecutionId> {
            Err(anyhow!(
                "start_child_execution not used in grpc server tests"
            ))
        }

        async fn get_execution(&self, _id: ExecutionId) -> Result<Execution> {
            Err(anyhow!("get_execution not used in grpc server tests"))
        }

        async fn get_iterations(
            &self,
            _exec_id: ExecutionId,
        ) -> Result<Vec<crate::domain::execution::Iteration>> {
            Err(anyhow!("get_iterations not used in grpc server tests"))
        }

        async fn cancel_execution(&self, _id: ExecutionId) -> Result<()> {
            Err(anyhow!("cancel_execution not used in grpc server tests"))
        }

        async fn stream_execution(
            &self,
            _id: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainExecutionEvent>> + Send>>> {
            Ok(Box::pin(tokio_stream::iter(
                self.stream_events.clone().into_iter().map(Ok),
            )))
        }

        async fn stream_agent_events(
            &self,
            _id: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            Err(anyhow!("stream_agent_events not used in grpc server tests"))
        }

        async fn list_executions(
            &self,
            _agent_id: Option<AgentId>,
            _limit: usize,
        ) -> Result<Vec<Execution>> {
            Err(anyhow!("list_executions not used in grpc server tests"))
        }

        async fn delete_execution(&self, _id: ExecutionId) -> Result<()> {
            Err(anyhow!("delete_execution not used in grpc server tests"))
        }

        async fn record_llm_interaction(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _interaction: crate::domain::execution::LlmInteraction,
        ) -> Result<()> {
            Err(anyhow!(
                "record_llm_interaction not used in grpc server tests"
            ))
        }

        async fn store_iteration_trajectory(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _trajectory: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            Err(anyhow!(
                "store_iteration_trajectory not used in grpc server tests"
            ))
        }
    }

    fn test_validation_service(
        execution_service: Arc<dyn ExecutionService>,
    ) -> Arc<ValidationService> {
        Arc::new(ValidationService::new(
            Arc::new(EventBus::new(8)),
            execution_service,
        ))
    }

    #[tokio::test]
    async fn execute_agent_stream_closes_after_execution_stream_ends() {
        let execution_id = ExecutionId::new();
        let agent_id = AgentId::new();
        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id,
            stream_events: vec![DomainExecutionEvent::ExecutionCompleted {
                execution_id,
                agent_id,
                final_output: "done".to_string(),
                total_iterations: 1,
                completed_at: Utc::now(),
            }],
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service, validation_service);

        let response = service
            .execute_agent(Request::new(ExecuteAgentRequest {
                agent_id: agent_id.0.to_string(),
                input: "generate workflow".to_string(),
                context_json: String::new(),
                parent_execution_id: None,
                workflow_execution_id: None,
                security_policy: None,
            }))
            .await
            .expect("execute_agent should succeed");

        let mut stream = response.into_inner();

        let started = timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("started event should arrive before timeout")
            .expect("stream should emit execution started event")
            .expect("started event should be Ok");
        assert!(matches!(
            started.event,
            Some(execution_event::Event::ExecutionStarted(_))
        ));

        let completed = timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("completed event should arrive before timeout")
            .expect("stream should emit terminal event")
            .expect("completed event should be Ok");
        assert!(matches!(
            completed.event,
            Some(execution_event::Event::ExecutionCompleted(_))
        ));

        let end = timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("stream should close instead of hanging");
        assert!(
            end.is_none(),
            "stream should terminate after task completion"
        );
    }
}
