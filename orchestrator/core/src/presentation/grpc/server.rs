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

use crate::application::execution::ExecutionService;
use crate::application::stimulus::StimulusService;
use crate::application::validation_service::ValidationService;
use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionInput;
use crate::domain::stimulus::{Stimulus, StimulusSource};

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
    attestation_service:
        Option<Arc<dyn crate::infrastructure::smcp::attestation::AttestationService>>,
    tool_invocation_service:
        Option<Arc<crate::application::tool_invocation_service::ToolInvocationService>>,
    cortex_client: Option<Arc<crate::infrastructure::CortexGrpcClient>>,
    /// BC-8: Stimulus routing service (ADR-021). Optional until wired.
    stimulus_service: Option<Arc<dyn StimulusService>>,
}

impl AegisRuntimeService {
    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        validation_service: Arc<ValidationService>,
    ) -> Self {
        Self {
            execution_service,
            validation_service,
            attestation_service: None,
            tool_invocation_service: None,
            cortex_client: None,
            stimulus_service: None,
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

    /// Create a gRPC server instance
    pub fn into_server(self) -> AegisRuntimeServer<Self> {
        AegisRuntimeServer::new(self)
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
        let req = request.into_inner();

        // Parse agent_id
        let agent_id = AgentId::from_string(&req.agent_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid agent_id: {}", e)))?;

        // Create execution input
        let payload = if req.context_json.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&req.context_json)
                .map_err(|e| Status::invalid_argument(format!("Invalid context_json: {}", e)))?
        };

        // Let ExecutionService render the agent's prompt_template
        // instead of bypassing it by setting intent directly. This ensures agents
        // behave consistently regardless of API type (gRPC, REST, CLI).
        let input = ExecutionInput {
            intent: None, // Let ExecutionService render agent's prompt_template
            payload: serde_json::json!({
                "input": req.input,  // User-provided input
                "context": payload   // Additional context if provided
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
                                            reason: format!("Invalid parent_execution_id: {}", e),
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
                                            reason: format!("Failed to stream execution: {}", e),
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
                                reason: format!("Failed to start execution: {}", e),
                                total_iterations: 0,
                                failed_at: Utc::now().to_rfc3339(),
                            })),
                        }))
                        .await;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Execute a system command
    async fn execute_system_command(
        &self,
        request: Request<ExecuteSystemCommandRequest>,
    ) -> Result<Response<ExecuteSystemCommandResponse>, Status> {
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
        let timeout = std::time::Duration::from_secs(req.timeout_seconds.unwrap_or(300) as u64);

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
            Ok(Err(e)) => Err(Status::internal(format!("Command execution failed: {}", e))),
            Err(_) => Err(Status::deadline_exceeded("Command execution timed out")),
        }
    }

    /// Validate output using gradient scoring (judge agents)
    async fn validate_with_judges(
        &self,
        request: Request<ValidateRequest>,
    ) -> Result<Response<ValidateResponse>, Status> {
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
            .map_err(|e| Status::invalid_argument(format!("Invalid judge agent_id: {}", e)))?;

        // Parse context_json once; extract execution_id/agent_id for proper child-execution linking
        let context_value: Option<serde_json::Value> = if req.context_json.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&req.context_json).map_err(|e| {
                Status::invalid_argument(format!("Invalid context_json: {}", e))
            })?)
        };

        let exec_id = context_value
            .as_ref()
            .and_then(|v| v["execution_id"].as_str())
            .and_then(|s| crate::domain::execution::ExecutionId::from_string(s).ok())
            .unwrap_or_else(crate::domain::execution::ExecutionId::new);

        let agent_id = context_value
            .as_ref()
            .and_then(|v| v["agent_id"].as_str())
            .and_then(|s| AgentId::from_string(s).ok())
            .unwrap_or_else(AgentId::new);

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
                60,  // 60 second timeout
                500, // 500ms poll interval
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
            Err(e) => Err(Status::internal(format!("Validation failed: {}", e))),
        }
    }

    /// Query Cortex for patterns based on error signature or embedding similarity.
    /// Forwards the call to the standalone `aegis-cortex` service when configured;
    /// returns an empty result set in memoryless mode (no warning per ADR-042).
    async fn query_cortex_patterns(
        &self,
        request: Request<QueryCortexRequest>,
    ) -> Result<Response<QueryCortexResponse>, Status> {
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
        let req = request.into_inner();

        let attestation_service = self
            .attestation_service
            .as_ref()
            .ok_or_else(|| Status::unimplemented("SMCP attestation service is not configured"))?;

        let attestation_req = crate::infrastructure::smcp::attestation::AttestationRequest {
            agent_id: req.agent_id,
            execution_id: req.execution_id,
            container_id: req.container_id,
            public_key_pem: req.public_key_pem,
        };

        match attestation_service.attest(attestation_req).await {
            Ok(res) => Ok(Response::new(AttestAgentResponse {
                security_token: res.security_token,
            })),
            Err(e) => Err(Status::internal(format!("Attestation failed: {}", e))),
        }
    }

    /// Invoke a tool via orchestrator mediation (SMCP)
    async fn invoke_tool(
        &self,
        request: Request<InvokeToolRequest>,
    ) -> Result<Response<InvokeToolResponse>, Status> {
        let req = request.into_inner();

        let tool_invocation_service = self.tool_invocation_service.as_ref().ok_or_else(|| {
            Status::unimplemented("SMCP tool invocation service is not configured")
        })?;

        // Construct SmcpEnvelope
        let envelope = crate::infrastructure::smcp::envelope::SmcpEnvelope {
            security_token: req.security_token,
            signature: req.signature,
            inner_mcp: req.inner_mcp,
        };

        // We need the agent_id to route appropriately. This would typically be extracted
        // from the security token (JWT) by the middleware, but for the sake of the API
        // if we need it here, we should perhaps peek the token or let the service do it.
        // Wait, ToolInvocationService takes agent_id in its arguments. Let's decode it.
        // The middleware `verify_and_unwrap` happens inside `invoke_tool`.
        // The service uses `agent_id` to lookup the session.
        // We can peek the sub (agent_id) from the unverified token right now to pass it to the service,
        // and the service will securely verify it anyway.

        let token_parts: Vec<&str> = envelope.security_token.split('.').collect();
        if token_parts.len() != 3 {
            return Err(Status::unauthenticated("Invalid security token format"));
        }
        use base64::Engine;
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token_parts[1])
            .map_err(|_| Status::unauthenticated("Invalid token payload base64"))?;

        let claims: serde_json::Value = serde_json::from_slice(&payload_bytes)
            .map_err(|_| Status::unauthenticated("Invalid token payload JSON"))?;

        let agent_id_str = claims
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Status::unauthenticated("Token missing agent_id claim"))?;

        let agent_id = crate::domain::agent::AgentId::from_string(agent_id_str)
            .map_err(|e| Status::internal(format!("Invalid agent_id in token: {}", e)))?;

        match tool_invocation_service
            .invoke_tool(&agent_id, &envelope)
            .await
        {
            Ok(result) => {
                let bytes = serde_json::to_vec(&result).map_err(|e| {
                    Status::internal(format!("Failed to serialize tool result: {}", e))
                })?;
                Ok(Response::new(InvokeToolResponse { result_json: bytes }))
            }
            Err(e) => Err(Status::permission_denied(format!(
                "Tool invocation rejected: {}",
                e
            ))),
        }
    }

    /// Ingest an external stimulus and route it to a workflow (BC-8 — ADR-021).
    async fn ingest_stimulus(
        &self,
        request: Request<IngestStimulusRequest>,
    ) -> Result<Response<IngestStimulusResponse>, Status> {
        let req = request.into_inner();
        let (stimulus_id, workflow_execution_id) = self
            .ingest_stimulus_rpc(
                req.source_name,
                req.content,
                req.idempotency_key,
                req.headers,
            )
            .await?;
        Ok(Response::new(IngestStimulusResponse {
            stimulus_id,
            workflow_execution_id,
        }))
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
        headers: std::collections::HashMap<String, String>,
    ) -> Result<(String, String), Status> {
        let stimulus_service = self
            .stimulus_service
            .as_ref()
            .ok_or_else(|| Status::unavailable("Stimulus service is not configured"))?;

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

pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    execution_service: Arc<dyn ExecutionService>,
    validation_service: Arc<ValidationService>,
    attestation_service: Option<
        Arc<dyn crate::infrastructure::smcp::attestation::AttestationService>,
    >,
    tool_invocation_service: Option<
        Arc<crate::application::tool_invocation_service::ToolInvocationService>,
    >,
    cortex_client: Option<Arc<crate::infrastructure::CortexGrpcClient>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut service = AegisRuntimeService::new(execution_service, validation_service);

    if let (Some(a), Some(t)) = (attestation_service, tool_invocation_service) {
        service = service.with_smcp(a, t);
    }

    if let Some(c) = cortex_client {
        service = service.with_cortex(c);
    }

    let server = service.into_server();

    tracing::info!("Starting AEGIS gRPC server on {}", addr);

    tonic::transport::Server::builder()
        .add_service(server)
        .serve(addr)
        .await?;

    Ok(())
}
