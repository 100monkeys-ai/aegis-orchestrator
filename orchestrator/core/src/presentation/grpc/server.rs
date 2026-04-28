// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! gRPC Server Implementation for AEGIS Runtime
//! Exposes ExecuteAgent, ExecuteSystemCommand, ValidateWithJudges
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
use crate::application::discovery_service::DiscoveryService;
use crate::application::execution::ExecutionService;
use crate::application::run_container_step::RunContainerStepUseCase;
use crate::application::stimulus::StimulusService;
use crate::application::validation_service::ValidationService;
use crate::application::volume_manager::VolumeService;
use crate::domain::agent::AgentId;
use crate::domain::discovery::DiscoveryQuery;
use crate::domain::execution::{Execution, ExecutionInput, ExecutionStatus};
use crate::domain::iam::{resolve_effective_tenant, IdentityKind, UserIdentity, ZaruTier};
use crate::domain::tenant::TenantId;
use crate::domain::volume::{StorageClass, VolumeBackend, VolumeOwnership};

const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 600;
const DEFAULT_JUDGE_WEIGHT: f64 = 1.0;
const DEFAULT_VALIDATION_TIMEOUT_SECS: u64 = 600;
const DEFAULT_VALIDATION_POLL_INTERVAL_MS: u64 = 1000;
const EXECUTION_TERMINAL_POLL_INTERVAL_MS: u64 = 250;
use crate::domain::stimulus::{Stimulus, StimulusSource};
use crate::presentation::grpc::auth_interceptor::{validate_grpc_request, GrpcIamAuthInterceptor};
use crate::presentation::keycloak_auth::ScopeGuard;
use crate::presentation::metrics_middleware::GrpcMetricsLayer;

// Generated protobuf code lives in infrastructure::aegis_runtime_proto (ADR-042)
// so that both the server and the CortexGrpcClient client share the same Rust types.
use crate::infrastructure::aegis_runtime_proto::aegis_runtime_server::{
    AegisRuntime, AegisRuntimeServer,
};
use crate::infrastructure::aegis_runtime_proto::*;

/// Normalizes the maximum number of attempts for running a container step.
/// Treats `0` (the proto3 default for unset fields) as `1`, since at least
/// one attempt must always be made to run the container step.
fn normalize_max_attempts(value: u32) -> u32 {
    if value == 0 {
        1
    } else {
        value
    }
}

/// Implementation of the AegisRuntime gRPC service
pub struct AegisRuntimeService {
    execution_service: Arc<dyn ExecutionService>,
    validation_service: Arc<ValidationService>,
    grpc_auth: Option<GrpcIamAuthInterceptor>,
    attestation_service:
        Option<Arc<dyn crate::infrastructure::seal::attestation::AttestationService>>,
    tool_invocation_service:
        Option<Arc<crate::application::tool_invocation_service::ToolInvocationService>>,
    cortex_client: Option<Arc<crate::infrastructure::CortexGrpcClient>>,
    /// BC-8: Stimulus routing service (ADR-021). Optional until wired.
    stimulus_service: Option<Arc<dyn StimulusService>>,
    /// BC-3: Container step runner use case (ADR-050). Optional until wired.
    run_container_step_use_case: Option<Arc<RunContainerStepUseCase>>,
    /// BC-1: Agent lifecycle service for resolving agent name → UUID at execution time (Phase 1b).
    agent_service: Option<Arc<dyn AgentLifecycleService>>,
    /// ADR-075: Discovery service for semantic search over agents and workflows.
    discovery_service: Option<Arc<dyn DiscoveryService>>,
    /// BC-7: Volume lifecycle service for workspace volume provisioning (ADR-032/087).
    volume_service: Option<Arc<dyn VolumeService>>,
    /// ADR-103: Output handler service for post-execution egress dispatch.
    output_handler_service:
        Option<Arc<dyn crate::application::output_handler_service::OutputHandlerService>>,
    /// gRPC client to the host-side FUSE daemon's FuseMountService (ADR-107).
    /// Used by `destroy_workspace_volume` to unmount FUSE mounts before volume destruction.
    fuse_mount_client: Option<
        crate::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient<
            tonic::transport::Channel,
        >,
    >,
}

impl AegisRuntimeService {
    /// Resolve the effective tenant for a gRPC request, honoring the
    /// `x-tenant-id` metadata key when the caller is a service account.
    ///
    /// Delegates to the canonical domain-layer gate [`resolve_effective_tenant`]
    /// (ADR-100).
    fn tenant_id_from_request<T>(
        identity: Option<&UserIdentity>,
        request: &Request<T>,
    ) -> TenantId {
        let delegation = request
            .metadata()
            .get("x-tenant-id")
            .and_then(|v| v.to_str().ok());
        resolve_effective_tenant(identity, delegation)
    }

    fn zaru_tier_from_identity(identity: Option<&UserIdentity>) -> ZaruTier {
        match identity.map(|id| &id.identity_kind) {
            Some(IdentityKind::ConsumerUser { zaru_tier, .. }) => zaru_tier.clone(),
            // Operators and service accounts get Enterprise-level discovery access.
            Some(IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. }) => {
                ZaruTier::Enterprise
            }
            Some(IdentityKind::TenantUser { .. }) | None => ZaruTier::Free,
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
            discovery_service: None,
            volume_service: None,
            output_handler_service: None,
            fuse_mount_client: None,
        }
    }

    /// Set the SEAL services (optional)
    pub fn with_seal(
        mut self,
        attestation_service: Arc<dyn crate::infrastructure::seal::attestation::AttestationService>,
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

    /// Set the discovery service for semantic search over agents and workflows (ADR-075).
    pub fn with_discovery_service(mut self, svc: Arc<dyn DiscoveryService>) -> Self {
        self.discovery_service = Some(svc);
        self
    }

    /// Set the volume lifecycle service for workspace volume provisioning (ADR-032/087).
    pub fn with_volume_service(mut self, svc: Arc<dyn VolumeService>) -> Self {
        self.volume_service = Some(svc);
        self
    }

    /// Attach an output handler service for post-execution egress dispatch (ADR-103).
    pub fn with_output_handler_service(
        mut self,
        svc: Arc<dyn crate::application::output_handler_service::OutputHandlerService>,
    ) -> Self {
        self.output_handler_service = Some(svc);
        self
    }

    /// Set the gRPC FUSE mount client for unmounting volumes on workspace destruction (ADR-107).
    pub fn with_fuse_mount_client(
        mut self,
        client: crate::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient<
            tonic::transport::Channel,
        >,
    ) -> Self {
        self.fuse_mount_client = Some(client);
        self
    }

    /// Create a gRPC server instance
    pub fn into_server(self) -> AegisRuntimeServer<Self> {
        AegisRuntimeServer::new(self)
    }

    /// Authenticate the request and derive the effective `TenantId`.
    ///
    /// Returns `Ok(None)` for exempt methods. For authenticated calls, returns
    /// `Ok(Some((identity, tenant_id, scope_guard)))` where `tenant_id` incorporates
    /// the `x-aegis-tenant` admin override when applicable (gap 056-10), and
    /// `scope_guard` carries the resource:action scopes from the JWT.
    ///
    /// For service-account callers, the `x-tenant-id` delegation is applied
    /// on top of the interceptor-derived value via `tenant_id_from_request`.
    async fn authorize<T>(
        &self,
        request: &Request<T>,
        method: &str,
    ) -> Result<Option<(UserIdentity, TenantId, ScopeGuard)>, Status> {
        if let Some(interceptor) = &self.grpc_auth {
            return validate_grpc_request(interceptor, request, method).await;
        }

        Ok(None)
    }
}

fn normalize_judge_weight(weight: f32) -> f64 {
    if weight > 0.0 {
        weight as f64
    } else {
        DEFAULT_JUDGE_WEIGHT
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
        let auth = self
            .authorize(&request, "/aegis.v1.AegisRuntime/ExecuteAgent")
            .await?;
        let (identity, tenant_id, scope_guard) = match auth {
            Some((id, tid, sg)) => {
                // For service accounts, apply x-tenant-id delegation on top of the
                // interceptor-derived tenant (ADR-100).
                let effective_tid = Self::tenant_id_from_request(Some(&id), &request);
                // Use effective_tid (service-account delegation) if it differs from the
                // interceptor's derived tenant, otherwise use the interceptor's value
                // (which includes x-aegis-tenant admin override).
                let final_tid = if matches!(
                    id.identity_kind,
                    crate::domain::iam::IdentityKind::ServiceAccount { .. }
                ) {
                    effective_tid
                } else {
                    tid
                };
                (Some(id), final_tid, sg)
            }
            None => (
                None,
                Self::tenant_id_from_request(None, &request),
                ScopeGuard::default(),
            ),
        };
        if identity.is_some() {
            scope_guard.require("agent:execute").map_err(|(_, body)| {
                tonic::Status::permission_denied(
                    body.0
                        .get("required")
                        .and_then(|v| v.as_str())
                        .unwrap_or("insufficient_scope"),
                )
            })?;
        }
        let req = request.into_inner();

        // Parse agent_id — accept UUID or human-readable name (resolved via agent_service)
        let agent_id = if let Ok(id) = AgentId::from_string(&req.agent_id) {
            id
        } else if let Some(ref svc) = self.agent_service {
            match svc
                .lookup_agent_visible_for_tenant(&tenant_id, &req.agent_id)
                .await
            {
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

        let resolved_intent = req
            .intent
            .filter(|s| !s.is_empty())
            .or_else(|| Some(req.input.clone()).filter(|s| !s.is_empty()));

        let input = ExecutionInput {
            intent: resolved_intent,
            input: serde_json::json!({
                "context_overrides": payload,
                "tenant_id": tenant_id.to_string(),
            }),
            workspace_volume_id: req
                .workspace_volume_id
                .filter(|s| !s.is_empty())
                .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                .map(crate::domain::shared_kernel::VolumeId),
            workspace_volume_mount_path: req
                .workspace_volume_mount_path
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| Some(std::path::PathBuf::from("/workspace"))),
            workspace_remote_path: req.workspace_remote_path.filter(|s| !s.is_empty()),
            workflow_execution_id: req
                .workflow_execution_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|s| uuid::Uuid::parse_str(s).ok()),
            attachments: {
                let mut refs = Vec::with_capacity(req.attachments.len());
                for a in &req.attachments {
                    // Skip attachments with malformed volume_id (defensive: pre-existing
                    // behavior preserved). Reject negative size loudly — silent
                    // coercion to 0 hides client bugs.
                    let Ok(volume_uuid) = uuid::Uuid::parse_str(&a.volume_id) else {
                        continue;
                    };
                    let size = u64::try_from(a.size).map_err(|_| {
                        Status::invalid_argument("AttachmentRef.size must be non-negative")
                    })?;
                    refs.push(crate::domain::execution::AttachmentRef {
                        volume_id: crate::domain::shared_kernel::VolumeId(volume_uuid),
                        path: a.path.clone(),
                        name: a.name.clone(),
                        mime_type: a.mime_type.clone(),
                        size,
                        sha256: a.sha256.clone(),
                    });
                }
                refs
            },
        };

        // Channel for streaming events
        let (tx, rx) = mpsc::channel(100);

        // ADR-083: prefer explicit security_context_name from the request (e.g. when
        // the Temporal worker calls back with the context propagated through the
        // workflow input), falling back to the identity-derived context name.
        let security_context_name = req
            .security_context_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                identity
                    .as_ref()
                    .map(|id| id.to_security_context_name())
                    .unwrap_or_else(|| "aegis-system-operator".to_string())
            });

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
            let start_result = if let Some(parent_execution_id_str) = req.parent_execution_id {
                let parent_id = match crate::domain::execution::ExecutionId::from_string(
                    &parent_execution_id_str,
                ) {
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
                execution_service
                    .start_execution(agent_id, input, security_context_name, identity.as_ref())
                    .await
            };

            // Start execution
            match start_result {
                Ok(execution_id) => {
                    // Stream execution events
                    match execution_service.stream_execution(execution_id).await {
                        Ok(mut stream) => {
                            use futures::StreamExt;
                            let mut terminal_sent = false;
                            let mut terminal_poll =
                                tokio::time::interval(std::time::Duration::from_millis(
                                    EXECUTION_TERMINAL_POLL_INTERVAL_MS,
                                ));

                            loop {
                                tokio::select! {
                                    event_result = stream.next() => {
                                        match event_result {
                                            Some(Ok(domain_event)) => {
                                                // Convert domain event to protobuf event
                                                if let Some(pb_event) = convert_domain_event_to_proto(
                                                    domain_event,
                                                    execution_id,
                                                ) {
                                                    terminal_sent = is_terminal_proto_event(&pb_event);
                                                    if tx_clone.send(Ok(pb_event)).await.is_err() {
                                                        break; // Client disconnected
                                                    }
                                                    if terminal_sent {
                                                        break;
                                                    }
                                                }
                                            }
                                            Some(Err(e)) => {
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
                                            None => {
                                                if !terminal_sent {
                                                    let _ = send_persisted_terminal_event(
                                                        &*execution_service,
                                                        &tenant_id,
                                                        execution_id,
                                                        &tx_clone,
                                                    )
                                                    .await;
                                                }
                                                break;
                                            }
                                        }
                                    }
                                    _ = terminal_poll.tick(), if !terminal_sent => {
                                        if send_persisted_terminal_event(
                                            &*execution_service,
                                            &tenant_id,
                                            execution_id,
                                            &tx_clone,
                                        )
                                        .await
                                        {
                                            break;
                                        }
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
        let auth = self
            .authorize(&request, "/aegis.v1.AegisRuntime/ValidateWithJudges")
            .await?;
        let tenant_id = match auth {
            Some((_id, tid, _sg)) => tid,
            None => Self::tenant_id_from_request(None, &request),
        };
        let req = request.into_inner();

        // Parse judge agent IDs and preserve per-judge weights (ADR-017)
        let judge_configs: Vec<(AgentId, f64)> = req
            .judges
            .iter()
            .map(|j| {
                AgentId::from_string(&j.agent_id).map(|id| (id, normalize_judge_weight(j.weight)))
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
                DEFAULT_VALIDATION_TIMEOUT_SECS, // 300 second timeout
                DEFAULT_VALIDATION_POLL_INTERVAL_MS, // 1000ms poll interval
                &tenant_id,
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

    /// Attest an agent to receive an SEAL Security Token
    async fn attest_agent(
        &self,
        request: Request<AttestAgentRequest>,
    ) -> Result<Response<AttestAgentResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/AttestAgent")
            .await?;
        let req = request.into_inner();

        let attestation_service = self.attestation_service.as_ref().ok_or_else(|| {
            Status::failed_precondition("SEAL attestation service is not configured")
        })?;

        let attestation_req = crate::infrastructure::seal::attestation::AttestationRequest {
            agent_id: Some(req.agent_id),
            execution_id: Some(req.execution_id),
            container_id: Some(req.container_id),
            public_key_pem: req.public_key_pem,
            security_context: None,
            principal_subject: None,
            user_id: None,
            workload_id: None,
            zaru_tier: None,
            // gRPC attestation is used by agent containers which run under the
            // system tenant (orchestrator-managed workloads).
            tenant_id: crate::domain::tenant::TenantId::system(),
            realm: crate::domain::iam::RealmKind::System,
            task_summary: None,
        };

        match attestation_service.attest(attestation_req).await {
            Ok(res) => Ok(Response::new(AttestAgentResponse {
                security_token: res.security_token,
            })),
            Err(e) => Err(Status::internal(format!("Attestation failed: {e}"))),
        }
    }

    /// Invoke a tool via orchestrator mediation (SEAL)
    async fn invoke_tool(
        &self,
        request: Request<InvokeToolRequest>,
    ) -> Result<Response<InvokeToolResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/InvokeTool")
            .await?;

        // The SEAL protocol requires 'protocol' and 'timestamp' fields for replay
        // protection and canonical signing (ADR-035 §3). The current protobuf schema
        // for InvokeToolRequest does not carry these fields. Agents must use the HTTP
        // POST /v1/seal/tool/invoke endpoint until the proto is updated.
        Err(Status::unimplemented(
            "SEAL InvokeTool via gRPC requires protocol and timestamp fields \
             not present in this protobuf version; use the HTTP SEAL endpoint",
        ))
    }

    /// Ingest an external stimulus and route it to a workflow (BC-8 — ADR-021).
    async fn ingest_stimulus(
        &self,
        request: Request<IngestStimulusRequest>,
    ) -> Result<Response<IngestStimulusResponse>, Status> {
        let auth = self
            .authorize(&request, "/aegis.v1.AegisRuntime/IngestStimulus")
            .await?;
        let (identity, tenant_id, scope_guard) = match auth {
            Some((id, tid, sg)) => {
                let effective_tid = Self::tenant_id_from_request(Some(&id), &request);
                let final_tid = if matches!(
                    id.identity_kind,
                    crate::domain::iam::IdentityKind::ServiceAccount { .. }
                ) {
                    effective_tid
                } else {
                    tid
                };
                (Some(id), final_tid, sg)
            }
            None => (
                None,
                Self::tenant_id_from_request(None, &request),
                ScopeGuard::default(),
            ),
        };
        if identity.is_some() {
            scope_guard.require("workflow:run").map_err(|(_, body)| {
                tonic::Status::permission_denied(
                    body.0
                        .get("required")
                        .and_then(|v| v.as_str())
                        .unwrap_or("insufficient_scope"),
                )
            })?;
        }
        let req = request.into_inner();
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
                match humantime_serde::re::humantime::parse_duration(&res.timeout) {
                    Ok(dur) => Some(dur),
                    Err(e) => {
                        return Err(Status::invalid_argument(format!(
                            "Invalid timeout duration '{}': {e}",
                            res.timeout
                        )));
                    }
                }
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

        let max_attempts = normalize_max_attempts(req.max_attempts);

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
            // ADR-087 D5: security fields will be sent by the temporal worker once
            // the proto is extended. Defaulting to false/None here is safe because
            // the EXECUTE_CODE state enforces these invariants via the workflow YAML,
            // which flows through a separate execution path.
            read_only_root_filesystem: false,
            run_as_user: None,
            network_mode: None,
            workflow_execution_id: if req.workflow_execution_id.is_empty() {
                None
            } else {
                uuid::Uuid::parse_str(&req.workflow_execution_id).ok()
            },
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

    /// Search for agents matching a natural-language query (ADR-075).
    async fn search_agents(
        &self,
        request: Request<SearchAgentsRequest>,
    ) -> Result<Response<SearchAgentsResponse>, Status> {
        let auth = self
            .authorize(&request, "/aegis.v1.AegisRuntime/SearchAgents")
            .await?;
        let (identity, tenant_id, scope_guard) = match auth {
            Some((id, _tid, sg)) => {
                let effective_tid = Self::tenant_id_from_request(Some(&id), &request);
                (Some(id), effective_tid, sg)
            }
            None => (
                None,
                Self::tenant_id_from_request(None, &request),
                ScopeGuard::default(),
            ),
        };
        if identity.is_some() {
            scope_guard.require("agent:list").map_err(|(_, body)| {
                tonic::Status::permission_denied(
                    body.0
                        .get("required")
                        .and_then(|v| v.as_str())
                        .unwrap_or("insufficient_scope"),
                )
            })?;
        }

        let Some(ref discovery) = self.discovery_service else {
            return Err(Status::unavailable(
                "Discovery service not configured — enterprise feature",
            ));
        };

        let req = request.into_inner();

        let query = DiscoveryQuery {
            query: req.query,
            limit: req.limit,
            min_score: req.min_score,
            label_filters: req.label_filters,
            status_filter: if req.status_filter.is_empty() {
                None
            } else {
                Some(req.status_filter)
            },
            include_platform_templates: req.include_platform_templates,
        };

        let tier = Self::zaru_tier_from_identity(identity.as_ref());

        match discovery.search_agents(&tenant_id, &tier, query).await {
            Ok(response) => {
                let results = response
                    .results
                    .into_iter()
                    .map(|r| DiscoveryResultProto {
                        resource_id: r.resource_id,
                        kind: format!("{:?}", r.kind),
                        name: r.name,
                        version: r.version,
                        description: r.description,
                        labels: r.labels,
                        similarity_score: r.similarity_score,
                        relevance_score: r.relevance_score,
                        tenant_id: r.tenant_id,
                        updated_at: r.updated_at.to_rfc3339(),
                        is_platform_template: r.is_platform_template,
                    })
                    .collect();

                Ok(Response::new(SearchAgentsResponse {
                    results,
                    total_indexed: response.total_indexed,
                    query_time_ms: response.query_time_ms,
                    search_mode: format!("{:?}", response.search_mode),
                }))
            }
            Err(e) => Err(Status::internal(format!("Agent search failed: {e}"))),
        }
    }

    /// Search for workflows matching a natural-language query (ADR-075).
    async fn search_workflows(
        &self,
        request: Request<SearchWorkflowsRequest>,
    ) -> Result<Response<SearchWorkflowsResponse>, Status> {
        let auth = self
            .authorize(&request, "/aegis.v1.AegisRuntime/SearchWorkflows")
            .await?;
        let (identity, tenant_id, scope_guard) = match auth {
            Some((id, _tid, sg)) => {
                let effective_tid = Self::tenant_id_from_request(Some(&id), &request);
                (Some(id), effective_tid, sg)
            }
            None => (
                None,
                Self::tenant_id_from_request(None, &request),
                ScopeGuard::default(),
            ),
        };
        if identity.is_some() {
            scope_guard.require("workflow:list").map_err(|(_, body)| {
                tonic::Status::permission_denied(
                    body.0
                        .get("required")
                        .and_then(|v| v.as_str())
                        .unwrap_or("insufficient_scope"),
                )
            })?;
        }

        let Some(ref discovery) = self.discovery_service else {
            return Err(Status::unavailable(
                "Discovery service not configured — enterprise feature",
            ));
        };

        let req = request.into_inner();

        let query = DiscoveryQuery {
            query: req.query,
            limit: req.limit,
            min_score: req.min_score,
            label_filters: req.label_filters,
            status_filter: None,
            include_platform_templates: req.include_platform_templates,
        };

        let tier = Self::zaru_tier_from_identity(identity.as_ref());

        match discovery.search_workflows(&tenant_id, &tier, query).await {
            Ok(response) => {
                let results = response
                    .results
                    .into_iter()
                    .map(|r| DiscoveryResultProto {
                        resource_id: r.resource_id,
                        kind: format!("{:?}", r.kind),
                        name: r.name,
                        version: r.version,
                        description: r.description,
                        labels: r.labels,
                        similarity_score: r.similarity_score,
                        relevance_score: r.relevance_score,
                        tenant_id: r.tenant_id,
                        updated_at: r.updated_at.to_rfc3339(),
                        is_platform_template: r.is_platform_template,
                    })
                    .collect();

                Ok(Response::new(SearchWorkflowsResponse {
                    results,
                    total_indexed: response.total_indexed,
                    query_time_ms: response.query_time_ms,
                    search_mode: format!("{:?}", response.search_mode),
                }))
            }
            Err(e) => Err(Status::internal(format!("Workflow search failed: {e}"))),
        }
    }

    /// Create an ephemeral workspace volume for the intent-to-execution pipeline (ADR-087).
    async fn create_workspace_volume(
        &self,
        request: Request<CreateWorkspaceVolumeRequest>,
    ) -> Result<Response<CreateWorkspaceVolumeResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/CreateWorkspaceVolume")
            .await?;
        let req = request.into_inner();

        let execution_id =
            crate::domain::execution::ExecutionId::from_string(&req.workflow_execution_id)
                .map_err(|e| {
                    Status::invalid_argument(format!("Invalid workflow_execution_id: {e}"))
                })?;

        let tenant_id = if req.tenant_id.is_empty() {
            TenantId::default()
        } else {
            TenantId::new(&req.tenant_id)
                .map_err(|e| Status::invalid_argument(format!("Invalid tenant_id: {e}")))?
        };

        let ttl_hours = if req.ttl_hours == 0 {
            24
        } else {
            req.ttl_hours
        };
        let size_limit_mb = if req.size_limit_mb == 0 {
            512
        } else {
            req.size_limit_mb
        };

        let Some(ref volume_service) = self.volume_service else {
            return Err(Status::unimplemented(
                "Volume service is not configured on this node",
            ));
        };

        let workflow_execution_uuid =
            uuid::Uuid::parse_str(&req.workflow_execution_id).map_err(|e| {
                Status::invalid_argument(format!("Invalid workflow_execution_id UUID: {e}"))
            })?;

        let volume_name = format!("workspace-{}", execution_id);
        let storage_class = StorageClass::ephemeral_hours(ttl_hours as i64);
        let ownership = VolumeOwnership::WorkflowExecution {
            workflow_execution_id: workflow_execution_uuid,
        };

        let volume_id = volume_service
            .create_volume(
                volume_name,
                tenant_id.clone(),
                storage_class,
                size_limit_mb,
                ownership,
            )
            .await
            .map_err(|e| Status::internal(format!("Failed to create workspace volume: {e}")))?;

        let volume = volume_service
            .get_volume(volume_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to retrieve created volume: {e}")))?;

        let remote_path = match &volume.backend {
            VolumeBackend::SeaweedFS { remote_path, .. } => remote_path.clone(),
            VolumeBackend::HostPath { path } => path.to_string_lossy().into_owned(),
            VolumeBackend::OpenDal { .. } | VolumeBackend::Seal { .. } => {
                format!("/aegis/volumes/{tenant_id}/{volume_id}")
            }
        };

        tracing::info!(
            volume_id = %volume_id,
            %remote_path,
            ttl_hours,
            size_limit_mb,
            "Created workspace volume (ADR-087)"
        );

        Ok(Response::new(CreateWorkspaceVolumeResponse {
            volume_id: volume_id.to_string(),
            remote_path,
        }))
    }

    /// Destroy a workspace volume after pipeline completion or failure (ADR-087).
    async fn destroy_workspace_volume(
        &self,
        request: Request<DestroyWorkspaceVolumeRequest>,
    ) -> Result<Response<DestroyWorkspaceVolumeResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/DestroyWorkspaceVolume")
            .await?;
        let req = request.into_inner();

        if req.volume_id.is_empty() {
            return Err(Status::invalid_argument("volume_id is required"));
        }
        if req.workflow_execution_id.is_empty() {
            return Err(Status::invalid_argument(
                "workflow_execution_id is required for ownership verification",
            ));
        }

        tracing::info!(
            volume_id = %req.volume_id,
            workflow_execution_id = %req.workflow_execution_id,
            "Destroying workspace volume (ADR-087)"
        );

        // Unmount any FUSE mounts for this volume before destroying it (ADR-107).
        // Uses wildcard unmount (empty execution_id) to tear down all mounts for
        // this volume_id, preventing stale FUSE mountpoints that generate endless
        // DirectoryListed storage events. Best-effort: failures are logged but
        // do not block volume destruction.
        if let Some(ref fuse_mount_client) = self.fuse_mount_client {
            let mut client = fuse_mount_client.clone();
            let unmount_req = crate::infrastructure::aegis_runtime_proto::FuseUnmountRequest {
                volume_id: req.volume_id.clone(),
                execution_id: String::new(), // Wildcard: unmount all executions
            };
            match tokio::time::timeout(
                crate::infrastructure::container_step_runner::FUSE_GRPC_TIMEOUT,
                client.unmount(unmount_req),
            )
            .await
            {
                Err(_elapsed) => {
                    tracing::warn!(
                        volume_id = %req.volume_id,
                        timeout_secs = crate::infrastructure::container_step_runner::FUSE_GRPC_TIMEOUT.as_secs(),
                        "gRPC FUSE unmount timed out during workspace volume destruction"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        volume_id = %req.volume_id,
                        "gRPC FUSE unmount failed during workspace volume destruction"
                    );
                }
                Ok(Ok(resp)) => {
                    if resp.into_inner().unmounted {
                        tracing::info!(
                            volume_id = %req.volume_id,
                            "FUSE mount(s) unmounted before workspace volume destruction"
                        );
                    }
                }
            }
        }

        Ok(Response::new(DestroyWorkspaceVolumeResponse {
            destroyed: true,
        }))
    }

    /// Proxy a `QueryCortexPatterns` request to the Cortex service (Gap 042-1).
    ///
    /// The Orchestrator is the trust boundary: it holds the Cortex API key and
    /// proxies pattern queries so Zaru Client / SDK never connect to Cortex directly.
    /// Returns an empty pattern list in memoryless mode (no `cortex_client` configured).
    async fn query_cortex_patterns(
        &self,
        request: Request<QueryCortexPatternsRequest>,
    ) -> Result<Response<QueryCortexPatternsResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/QueryCortexPatterns")
            .await?;
        let req = request.into_inner();

        let Some(ref cortex_client) = self.cortex_client else {
            tracing::debug!("QueryCortexPatterns called in memoryless mode — returning empty");
            return Ok(Response::new(QueryCortexPatternsResponse {
                patterns: vec![],
            }));
        };

        let cortex_req = crate::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: req.error_signature,
            error_type: req.error_type,
            limit: req.limit,
            min_success_score: req.min_success_score,
            tenant_id: req.tenant_id,
        };

        match cortex_client.query_patterns(cortex_req).await {
            Ok(resp) => {
                let patterns = resp
                    .patterns
                    .into_iter()
                    .map(|p| CortexPatternProto {
                        pattern_id: p.id,
                        error_signature: p.error_signature_hash,
                        error_type: p.error_type,
                        solution_approach: p.solution_approach,
                        solution_code: p.solution_code,
                        success_score: p.success_score,
                        frequency: p.frequency,
                    })
                    .collect();
                Ok(Response::new(QueryCortexPatternsResponse { patterns }))
            }
            Err(e) => Err(Status::unavailable(format!(
                "Cortex pattern query failed: {e}"
            ))),
        }
    }

    /// Proxy a `StoreCortexPattern` request to the Cortex service (Gap 042-3).
    ///
    /// Returns a no-op response in memoryless mode (no `cortex_client` configured).
    async fn store_cortex_pattern(
        &self,
        request: Request<StoreCortexPatternRequest>,
    ) -> Result<Response<StoreCortexPatternResponse>, Status> {
        let _identity = self
            .authorize(&request, "/aegis.v1.AegisRuntime/StoreCortexPattern")
            .await?;
        let req = request.into_inner();

        let Some(ref cortex_client) = self.cortex_client else {
            tracing::debug!("StoreCortexPattern called in memoryless mode — returning noop");
            return Ok(Response::new(StoreCortexPatternResponse {
                pattern_id: String::new(),
                deduplicated: false,
                new_frequency: 0,
            }));
        };

        let cortex_req = crate::infrastructure::aegis_cortex_proto::StorePatternRequest {
            error_signature: req.error_signature,
            error_type: req.error_type,
            error_message: req.error_message,
            solution_approach: req.solution_approach,
            solution_code: req.solution_code,
            agent_id: req.agent_id,
            tags: req.tags,
            tenant_id: req.tenant_id,
        };

        match cortex_client.store_pattern(cortex_req).await {
            Ok(resp) => Ok(Response::new(StoreCortexPatternResponse {
                pattern_id: resp.pattern_id,
                deduplicated: resp.deduplicated,
                new_frequency: resp.new_frequency,
            })),
            Err(e) => Err(Status::unavailable(format!(
                "Cortex pattern store failed: {e}"
            ))),
        }
    }

    async fn invoke_output_handler(
        &self,
        request: Request<InvokeOutputHandlerRequest>,
    ) -> Result<Response<InvokeOutputHandlerResponse>, Status> {
        let req = request.into_inner();

        let handler_config: crate::domain::output_handler::OutputHandlerConfig =
            serde_json::from_str(&req.handler_config_json).map_err(|e| {
                Status::invalid_argument(format!("Failed to deserialize handler_config_json: {e}"))
            })?;

        let svc = self
            .output_handler_service
            .as_ref()
            .ok_or_else(|| Status::unavailable("Output handler service is not configured"))?;

        // An empty execution_id means no parent agent execution is available
        // (e.g. output handler on a ContainerRun or ParallelAgents state).
        let parent_execution_id = if req.execution_id.is_empty() {
            None
        } else {
            Some(
                crate::domain::execution::ExecutionId::from_string(&req.execution_id)
                    .map_err(|e| Status::invalid_argument(format!("Invalid execution_id: {e}")))?,
            )
        };

        let tenant_id = TenantId::new(&req.tenant_id)
            .map_err(|e| Status::invalid_argument(format!("Invalid tenant_id: {e}")))?;

        match svc
            .invoke(
                &handler_config,
                &req.final_output,
                parent_execution_id.as_ref(),
                &tenant_id,
                req.intent.as_deref(),
            )
            .await
        {
            Ok(result) => Ok(Response::new(InvokeOutputHandlerResponse {
                success: true,
                result: result.unwrap_or_default(),
                error: String::new(),
            })),
            // ADR-103: deferred handler variants (Container, McpTool) map to
            // gRPC UNIMPLEMENTED (HTTP 501 equivalent) at the daemon boundary.
            // Surfacing this as a Status rather than embedding it in the
            // response body lets clients distinguish "the feature doesn't
            // exist" from "the feature ran but failed".
            Err(
                crate::application::output_handler_service::OutputHandlerError::NotYetImplemented(
                    msg,
                ),
            ) => Err(Status::unimplemented(msg)),
            Err(e) => Ok(Response::new(InvokeOutputHandlerResponse {
                success: false,
                result: String::new(),
                error: e.to_string(),
            })),
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

        match stimulus_service.ingest(stimulus, None).await {
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
        // Surface LLM upstream failures on the streaming channel so parent
        // agents see a clear, structured signal instead of a silent timeout.
        DomainEvent::LlmCallFailed {
            iteration_number,
            error_class,
            message,
            timestamp,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationFailed(IterationFailed {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                error: Some(IterationError {
                    error_type: format!("llm_{error_class:?}").to_lowercase(),
                    message: message.clone(),
                    stacktrace: None,
                }),
                failed_at: timestamp.to_rfc3339(),
            })),
        }),
        _ => None, // Other events not relevant for streaming
    }
}

fn persisted_execution_to_proto(execution: &Execution) -> Option<ExecutionEvent> {
    match execution.status {
        ExecutionStatus::Completed => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionCompleted(
                ExecutionCompleted {
                    execution_id: execution.id.to_string(),
                    final_output: execution
                        .iterations()
                        .last()
                        .and_then(|iteration| iteration.output.clone())
                        .unwrap_or_default(),
                    total_iterations: execution.total_attempts() as u32,
                    completed_at: execution
                        .ended_at
                        .unwrap_or(execution.started_at)
                        .to_rfc3339(),
                },
            )),
        }),
        ExecutionStatus::Failed => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                execution_id: execution.id.to_string(),
                reason: execution
                    .error
                    .clone()
                    .unwrap_or_else(|| "Execution failed".to_string()),
                total_iterations: execution.total_attempts() as u32,
                failed_at: execution
                    .ended_at
                    .unwrap_or(execution.started_at)
                    .to_rfc3339(),
            })),
        }),
        _ => None,
    }
}

fn is_terminal_proto_event(event: &ExecutionEvent) -> bool {
    matches!(
        event.event,
        Some(execution_event::Event::ExecutionCompleted(_))
            | Some(execution_event::Event::ExecutionFailed(_))
    )
}

async fn send_persisted_terminal_event(
    execution_service: &dyn ExecutionService,
    tenant_id: &TenantId,
    execution_id: crate::domain::execution::ExecutionId,
    tx: &mpsc::Sender<Result<ExecutionEvent, Status>>,
) -> bool {
    match execution_service
        .get_execution_for_tenant(tenant_id, execution_id)
        .await
    {
        Ok(execution) if execution.is_completed() => {
            if let Some(pb_event) = persisted_execution_to_proto(&execution) {
                return tx.send(Ok(pb_event)).await.is_ok();
            }
            false
        }
        Ok(_) => false,
        Err(_) => false,
    }
}

#[derive(Clone)]
pub struct GrpcServerConfig {
    pub addr: std::net::SocketAddr,
    pub execution_service: Arc<dyn ExecutionService>,
    pub validation_service: Arc<ValidationService>,
    pub grpc_auth: Option<GrpcIamAuthInterceptor>,
    pub attestation_service:
        Option<Arc<dyn crate::infrastructure::seal::attestation::AttestationService>>,
    pub tool_invocation_service:
        Option<Arc<crate::application::tool_invocation_service::ToolInvocationService>>,
    pub cortex_client: Option<Arc<crate::infrastructure::CortexGrpcClient>>,
    pub run_container_step_use_case: Option<Arc<RunContainerStepUseCase>>,
    pub agent_service: Option<Arc<dyn AgentLifecycleService>>,
    pub stimulus_service: Option<Arc<dyn StimulusService>>,
    pub discovery_service: Option<Arc<dyn DiscoveryService>>,
    pub volume_service: Option<Arc<dyn VolumeService>>,
    /// Optional output handler service for post-execution egress dispatch (ADR-103).
    pub output_handler_service:
        Option<Arc<dyn crate::application::output_handler_service::OutputHandlerService>>,
    /// FSAL instance for the FsalService gRPC endpoint used by the external FUSE daemon (ADR-107).
    pub fsal: Option<Arc<crate::domain::fsal::AegisFSAL>>,
    /// gRPC client to the host-side FUSE daemon's FuseMountService (ADR-107).
    /// Passed to `AegisRuntimeService` for FUSE unmount on workspace volume destruction.
    pub fuse_mount_client: Option<
        crate::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient<
            tonic::transport::Channel,
        >,
    >,
}

pub async fn start_grpc_server(config: GrpcServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mut service = AegisRuntimeService::new(config.execution_service, config.validation_service);

    if let Some(auth) = config.grpc_auth {
        service = service.with_grpc_auth(auth);
    }

    if let (Some(a), Some(t)) = (config.attestation_service, config.tool_invocation_service) {
        service = service.with_seal(a, t);
    }

    if let Some(c) = config.cortex_client {
        service = service.with_cortex(c);
    }

    if let Some(uc) = config.run_container_step_use_case {
        service = service.with_container_step_runner(uc);
    }

    if let Some(stimulus) = config.stimulus_service {
        service = service.with_stimulus(stimulus);
    }

    if let Some(agent_service) = config.agent_service {
        service = service.with_agent_service(agent_service);
    }

    if let Some(discovery) = config.discovery_service {
        service = service.with_discovery_service(discovery);
    }

    if let Some(volume_service) = config.volume_service {
        service = service.with_volume_service(volume_service);
    }

    if let Some(output_handler) = config.output_handler_service {
        service = service.with_output_handler_service(output_handler);
    }

    if let Some(fuse_client) = config.fuse_mount_client {
        service = service.with_fuse_mount_client(fuse_client);
    }

    let server = service.into_server();

    tracing::info!("Starting AEGIS gRPC server on {}", config.addr);

    let mut builder = tonic::transport::Server::builder()
        .layer(GrpcMetricsLayer)
        .add_service(server);

    if let Some(fsal) = config.fsal {
        builder = builder.add_service(FsalServiceServer::new(FsalGrpcService::new(fsal)));
    }

    builder.serve(config.addr).await?;

    Ok(())
}

// =============================================================================
// FsalService gRPC implementation (ADR-107)
// Proxies FSAL operations from the host-side FUSE daemon to the shared
// AegisFSAL instance, preserving identical authorization and audit semantics.
// =============================================================================

use crate::infrastructure::aegis_runtime_proto::fsal_service_server::{
    FsalService as FsalServiceTrait, FsalServiceServer,
};
use crate::infrastructure::aegis_runtime_proto::{
    FsalCreateFileRequest as ProtoCreateFileReq, FsalCreateFileResponse, FsalDirEntry,
    FsalGetattrRequest as ProtoGetattrReq, FsalGetattrResponse,
    FsalLookupRequest as ProtoLookupReq, FsalLookupResponse, FsalMutateRequest as ProtoMutateReq,
    FsalMutateResponse, FsalReadRequest as ProtoReadReq, FsalReadResponse,
    FsalReaddirRequest as ProtoReaddirReq, FsalReaddirResponse,
    FsalRenameRequest as ProtoRenameReq, FsalWriteRequest as ProtoWriteReq, FsalWriteResponse,
};

/// gRPC implementation of `FsalService` that delegates to the shared `AegisFSAL`.
pub struct FsalGrpcService {
    fsal: Arc<crate::domain::fsal::AegisFSAL>,
}

impl FsalGrpcService {
    pub fn new(fsal: Arc<crate::domain::fsal::AegisFSAL>) -> Self {
        Self { fsal }
    }
}

/// Parse a UUID string from a proto field, returning a gRPC `InvalidArgument` on failure.
fn parse_uuid_field(field: &str, name: &str) -> Result<uuid::Uuid, Status> {
    uuid::Uuid::parse_str(field)
        .map_err(|e| Status::invalid_argument(format!("Invalid {name}: {e}")))
}

/// Parse an optional execution_id from a proto string field.
/// Empty string (proto3 default) is treated as `None`.
fn parse_optional_execution_id(field: &str) -> Result<Option<uuid::Uuid>, Status> {
    if field.is_empty() {
        Ok(None)
    } else {
        uuid::Uuid::parse_str(field)
            .map(Some)
            .map_err(|e| Status::invalid_argument(format!("Invalid execution_id: {e}")))
    }
}

/// Resolve an execution_id for FSAL handlers.
///
/// Workflow-owned volumes send an empty `execution_id` with a populated
/// `workflow_execution_id`.  In that case we use `Uuid::nil()` as a sentinel —
/// `authorize_inner` checks the workflow ownership path and never inspects the
/// execution_id when the workflow_execution_id matches.
///
/// Returns `InvalidArgument` when *both* fields are empty.
fn resolve_fsal_execution_id(
    execution_id_field: &str,
    workflow_execution_id_field: &str,
) -> Result<crate::domain::execution::ExecutionId, Status> {
    match parse_optional_execution_id(execution_id_field)? {
        Some(uuid) => Ok(crate::domain::execution::ExecutionId(uuid)),
        None => {
            // execution_id is empty — require workflow_execution_id to be present
            if workflow_execution_id_field.is_empty() {
                Err(Status::invalid_argument(
                    "At least one of execution_id or workflow_execution_id must be provided",
                ))
            } else {
                Ok(crate::domain::execution::ExecutionId(uuid::Uuid::nil()))
            }
        }
    }
}

/// Parse an optional workflow_execution_id from a proto string field.
/// Empty string (proto3 default) is treated as `None`.
fn parse_optional_wf_id(field: &str) -> Result<Option<uuid::Uuid>, Status> {
    if field.is_empty() {
        Ok(None)
    } else {
        uuid::Uuid::parse_str(field)
            .map(Some)
            .map_err(|e| Status::invalid_argument(format!("Invalid workflow_execution_id: {e}")))
    }
}

/// Convert an `FsalError` to a gRPC `Status`.
fn fsal_error_to_status(e: crate::domain::fsal::FsalError) -> Status {
    use crate::domain::fsal::FsalError;
    match &e {
        FsalError::UnauthorizedAccess { .. } => Status::permission_denied(e.to_string()),
        FsalError::VolumeNotFound(_) | FsalError::VolumeNotAttached(_) => {
            Status::not_found(e.to_string())
        }
        FsalError::PolicyViolation(_) => Status::permission_denied(e.to_string()),
        FsalError::PathSanitization(_) => Status::invalid_argument(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}

/// Convert a proto `FsalAccessPolicy` to the domain type.
fn proto_to_fsal_policy(
    proto: Option<crate::infrastructure::aegis_runtime_proto::FsalAccessPolicy>,
) -> crate::domain::fsal::FsalAccessPolicy {
    match proto {
        Some(p) => crate::domain::fsal::FsalAccessPolicy {
            read: p.read_paths,
            write: p.write_paths,
        },
        None => crate::domain::fsal::FsalAccessPolicy::default(),
    }
}

/// Convert a domain `FileType` to the string representation used in proto.
fn file_type_to_string(ft: crate::domain::storage::FileType) -> String {
    match ft {
        crate::domain::storage::FileType::File => "file".to_string(),
        crate::domain::storage::FileType::Directory => "directory".to_string(),
        crate::domain::storage::FileType::Symlink => "symlink".to_string(),
    }
}

#[tonic::async_trait]
impl FsalServiceTrait for FsalGrpcService {
    async fn getattr(
        &self,
        request: Request<ProtoGetattrReq>,
    ) -> Result<Response<FsalGetattrResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        let attrs = self
            .fsal
            .getattr(
                execution_id,
                volume_id,
                &req.path,
                req.container_uid,
                req.container_gid,
                wf_id,
            )
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalGetattrResponse {
            size: attrs.size,
            mode: attrs.mode,
            nlink: attrs.nlink,
            atime: attrs.atime,
            mtime: attrs.mtime,
            ctime: attrs.ctime,
            file_type: file_type_to_string(attrs.file_type),
        }))
    }

    async fn lookup(
        &self,
        request: Request<ProtoLookupReq>,
    ) -> Result<Response<FsalLookupResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;
        let parent_handle = if let Some(wf_uuid) = wf_id {
            crate::domain::fsal::AegisFileHandle::new_for_workflow(
                wf_uuid,
                volume_id,
                &req.parent_path,
            )
        } else {
            crate::domain::fsal::AegisFileHandle::new(execution_id, volume_id, &req.parent_path)
        };

        let child_handle = self
            .fsal
            .lookup(&parent_handle, &req.parent_path, &req.name)
            .await
            .map_err(fsal_error_to_status)?;

        let handle_bytes = child_handle.to_bytes().map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalLookupResponse {
            file_handle: handle_bytes,
        }))
    }

    async fn readdir(
        &self,
        request: Request<ProtoReaddirReq>,
    ) -> Result<Response<FsalReaddirResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        let entries = self
            .fsal
            .readdir(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                None,
                None,
                wf_id,
            )
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalReaddirResponse {
            entries: entries
                .into_iter()
                .map(|e| FsalDirEntry {
                    name: e.name,
                    file_type: file_type_to_string(e.file_type),
                })
                .collect(),
        }))
    }

    async fn read(
        &self,
        request: Request<ProtoReadReq>,
    ) -> Result<Response<FsalReadResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;
        let handle = if let Some(wf_uuid) = wf_id {
            crate::domain::fsal::AegisFileHandle::new_for_workflow(wf_uuid, volume_id, &req.path)
        } else {
            crate::domain::fsal::AegisFileHandle::new(execution_id, volume_id, &req.path)
        };

        let data = self
            .fsal
            .read(&handle, &req.path, &policy, req.offset, req.size as usize)
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalReadResponse { data }))
    }

    async fn write(
        &self,
        request: Request<ProtoWriteReq>,
    ) -> Result<Response<FsalWriteResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;
        let handle = if let Some(wf_uuid) = wf_id {
            crate::domain::fsal::AegisFileHandle::new_for_workflow(wf_uuid, volume_id, &req.path)
        } else {
            crate::domain::fsal::AegisFileHandle::new(execution_id, volume_id, &req.path)
        };

        let bytes_written = self
            .fsal
            .write(&handle, &req.path, &policy, req.offset, &req.data)
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalWriteResponse {
            bytes_written: bytes_written as u32,
        }))
    }

    async fn create_file(
        &self,
        request: Request<ProtoCreateFileReq>,
    ) -> Result<Response<FsalCreateFileResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        let handle = self
            .fsal
            .create_file(crate::domain::fsal::CreateFsalFileRequest {
                execution_id,
                volume_id,
                path: &req.path,
                policy: &policy,
                emit_event: true,
                caller_node_id: None,
                host_node_id: None,
                workflow_execution_id: wf_id,
            })
            .await
            .map_err(fsal_error_to_status)?;

        let handle_bytes = handle.to_bytes().map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalCreateFileResponse {
            file_handle: handle_bytes,
        }))
    }

    async fn create_directory(
        &self,
        request: Request<ProtoMutateReq>,
    ) -> Result<Response<FsalMutateResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        self.fsal
            .create_directory(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                None,
                None,
                wf_id,
            )
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalMutateResponse {}))
    }

    async fn delete_file(
        &self,
        request: Request<ProtoMutateReq>,
    ) -> Result<Response<FsalMutateResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        self.fsal
            .delete_file(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                None,
                None,
                wf_id,
            )
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalMutateResponse {}))
    }

    async fn delete_directory(
        &self,
        request: Request<ProtoMutateReq>,
    ) -> Result<Response<FsalMutateResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        self.fsal
            .delete_directory(
                execution_id,
                volume_id,
                &req.path,
                &policy,
                None,
                None,
                wf_id,
            )
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalMutateResponse {}))
    }

    async fn rename(
        &self,
        request: Request<ProtoRenameReq>,
    ) -> Result<Response<FsalMutateResponse>, Status> {
        let req = request.into_inner();
        let execution_id =
            resolve_fsal_execution_id(&req.execution_id, &req.workflow_execution_id)?;
        let volume_id =
            crate::domain::volume::VolumeId(parse_uuid_field(&req.volume_id, "volume_id")?);
        let policy = proto_to_fsal_policy(req.policy);

        let wf_id = parse_optional_wf_id(&req.workflow_execution_id)?;

        self.fsal
            .rename(crate::domain::fsal::RenameFsalRequest {
                execution_id,
                volume_id,
                from_path: &req.from_path,
                to_path: &req.to_path,
                policy: &policy,
                caller_node_id: None,
                host_node_id: None,
                workflow_execution_id: wf_id,
            })
            .await
            .map_err(fsal_error_to_status)?;

        Ok(Response::new(FsalMutateResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::ExecutionEvent as DomainExecutionEvent;
    use crate::domain::execution::{Execution, ExecutionId, ExecutionInput, ExecutionStatus};
    use crate::infrastructure::event_bus::{DomainEvent, EventBus};
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::Mutex;
    use tokio::time::{timeout, Duration};
    use tokio_stream::StreamExt;

    struct TestExecutionService {
        execution_id: ExecutionId,
        stream_events: Vec<DomainExecutionEvent>,
        persisted_execution: Option<Execution>,
        tenant_lookups: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ExecutionService for TestExecutionService {
        async fn start_execution(
            &self,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _security_context_name: String,
            _identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            Ok(self.execution_id)
        }

        async fn start_execution_with_id(
            &self,
            execution_id: ExecutionId,
            _agent_id: AgentId,
            _input: ExecutionInput,
            _security_context_name: String,
            _identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            Ok(execution_id)
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

        async fn get_execution_unscoped(&self, _id: ExecutionId) -> Result<Execution> {
            Err(anyhow!(
                "get_execution_unscoped not used in grpc server tests"
            ))
        }

        async fn get_execution_for_tenant(
            &self,
            tenant_id: &TenantId,
            id: ExecutionId,
        ) -> Result<Execution> {
            self.tenant_lookups
                .lock()
                .expect("tenant_lookups mutex poisoned")
                .push(tenant_id.to_string());

            match &self.persisted_execution {
                Some(execution) if execution.id == id => Ok(execution.clone()),
                Some(_) => Err(anyhow!("unexpected execution id")),
                None => Err(anyhow!("persisted execution not configured")),
            }
        }

        async fn get_iterations_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _exec_id: ExecutionId,
        ) -> Result<Vec<crate::domain::execution::Iteration>> {
            Err(anyhow!(
                "get_iterations_for_tenant not used in grpc server tests"
            ))
        }

        async fn cancel_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<()> {
            Err(anyhow!(
                "cancel_execution_for_tenant not used in grpc server tests"
            ))
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

        async fn list_executions_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _agent_id: Option<AgentId>,
            _workflow_id: Option<crate::domain::workflow::WorkflowId>,
            _limit: usize,
        ) -> Result<Vec<Execution>> {
            Err(anyhow!(
                "list_executions_for_tenant not used in grpc server tests"
            ))
        }

        async fn delete_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<()> {
            Err(anyhow!(
                "delete_execution_for_tenant not used in grpc server tests"
            ))
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

    struct NoopAgentLifecycleService;

    #[async_trait]
    impl crate::application::agent::AgentLifecycleService for NoopAgentLifecycleService {
        async fn deploy_agent_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _manifest: crate::domain::agent::AgentManifest,
            _force: bool,
            _scope: crate::domain::agent::AgentScope,
            _caller_identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<AgentId> {
            Err(anyhow!(
                "NoopAgentLifecycleService: deploy_agent_for_tenant not implemented"
            ))
        }

        async fn get_agent_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _id: AgentId,
        ) -> Result<crate::domain::agent::Agent> {
            Err(anyhow!(
                "NoopAgentLifecycleService: get_agent_for_tenant not implemented"
            ))
        }

        async fn update_agent_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _id: AgentId,
            _manifest: crate::domain::agent::AgentManifest,
        ) -> Result<()> {
            Err(anyhow!(
                "NoopAgentLifecycleService: update_agent_for_tenant not implemented"
            ))
        }

        async fn delete_agent_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _id: AgentId,
        ) -> Result<()> {
            Err(anyhow!(
                "NoopAgentLifecycleService: delete_agent_for_tenant not implemented"
            ))
        }

        async fn list_agents_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
        ) -> Result<Vec<crate::domain::agent::Agent>> {
            Ok(vec![])
        }

        async fn list_agents_visible_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
        ) -> Result<Vec<crate::domain::agent::Agent>> {
            Ok(vec![])
        }

        async fn lookup_agent_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _name: &str,
        ) -> Result<Option<AgentId>> {
            Ok(None)
        }

        async fn lookup_agent_visible_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _name: &str,
        ) -> Result<Option<AgentId>> {
            Ok(None)
        }

        async fn list_versions_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _agent_id: AgentId,
        ) -> Result<Vec<crate::domain::repository::AgentVersion>> {
            Ok(vec![])
        }

        async fn lookup_agent_for_tenant_with_version(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _name: &str,
            _version: &str,
        ) -> Result<Option<AgentId>> {
            Ok(None)
        }
    }

    fn test_validation_service(
        execution_service: Arc<dyn ExecutionService>,
    ) -> Arc<ValidationService> {
        Arc::new(ValidationService::new(
            Arc::new(EventBus::new(8)),
            execution_service,
            Arc::new(NoopAgentLifecycleService),
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
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
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
                tenant_id: String::new(),
                security_context_name: Some("aegis-system-operator".to_string()),
                intent: None,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                attachments: Vec::new(),
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

    #[tokio::test]
    async fn execute_agent_emits_persisted_terminal_event_when_stream_misses_completion() {
        let execution_id = ExecutionId::new();
        let agent_id = AgentId::new();
        let persisted_execution = Execution {
            id: execution_id,
            agent_id,
            tenant_id: crate::domain::tenant::TenantId::default(),
            status: ExecutionStatus::Completed,
            iterations: Vec::new(),
            max_iterations: 10,
            input: ExecutionInput {
                intent: Some("generate workflow".to_string()),
                input: serde_json::Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            error: None,
            hierarchy: crate::domain::execution::ExecutionHierarchy::root(execution_id),
            container_uid: 1000,
            container_gid: 1000,
            security_context_name: "aegis-system-operator".to_string(),
            initiating_user_sub: None,
        };
        let execution_service = Arc::new(TestExecutionService {
            execution_id,
            stream_events: Vec::new(),
            persisted_execution: Some(persisted_execution),
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service.clone(), validation_service);

        let response = service
            .execute_agent(Request::new(ExecuteAgentRequest {
                agent_id: agent_id.0.to_string(),
                input: "generate workflow".to_string(),
                context_json: String::new(),
                parent_execution_id: None,
                workflow_execution_id: None,
                security_policy: None,
                tenant_id: String::new(),
                security_context_name: Some("aegis-system-operator".to_string()),
                intent: None,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                attachments: Vec::new(),
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
            .expect("persisted terminal event should arrive before timeout")
            .expect("stream should emit synthesized terminal event")
            .expect("terminal event should be Ok");
        assert!(matches!(
            completed.event,
            Some(execution_event::Event::ExecutionCompleted(_))
        ));

        let tenant_lookups = execution_service
            .tenant_lookups
            .lock()
            .expect("tenant_lookups mutex poisoned")
            .clone();
        assert!(
            !tenant_lookups.is_empty(),
            "persisted fallback should query execution state"
        );

        let end = timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("stream should close after synthesized terminal event");
        assert!(
            end.is_none(),
            "stream should terminate after fallback terminal"
        );
    }

    #[tokio::test]
    async fn execute_agent_rejects_negative_attachment_size() {
        // Regression: previously the proto-to-domain mapping silently coerced
        // negative AttachmentRef.size values to 0 via `u64::try_from(...).unwrap_or(0)`.
        // Negative sizes from buggy or malicious clients must now fail loudly
        // with InvalidArgument.
        let execution_id = ExecutionId::new();
        let agent_id = AgentId::new();
        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id,
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service, validation_service);

        let result = service
            .execute_agent(Request::new(ExecuteAgentRequest {
                agent_id: agent_id.0.to_string(),
                input: "do something".to_string(),
                context_json: String::new(),
                parent_execution_id: None,
                workflow_execution_id: None,
                security_policy: None,
                tenant_id: String::new(),
                security_context_name: Some("aegis-system-operator".to_string()),
                intent: None,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                attachments: vec![AttachmentRef {
                    volume_id: uuid::Uuid::new_v4().to_string(),
                    path: "foo.txt".to_string(),
                    name: "foo.txt".to_string(),
                    mime_type: "text/plain".to_string(),
                    size: -1,
                    sha256: None,
                }],
            }))
            .await;

        let status = result.expect_err("negative attachment size must error");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("non-negative"),
            "error message should mention non-negative: {}",
            status.message()
        );
    }

    #[tokio::test]
    async fn invoke_tool_returns_unimplemented_pending_proto_update() {
        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id: ExecutionId::new(),
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service, validation_service);

        let err = service
            .invoke_tool(Request::new(InvokeToolRequest {
                security_token: String::new(),
                signature: String::new(),
                payload: Vec::new(),
            }))
            .await
            .expect_err("invoke_tool should be unimplemented via gRPC pending proto update");

        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    /// Regression: GrpcServerConfig must include output_handler_service so the
    /// InvokeOutputHandler RPC has a service implementation to delegate to.
    /// Before this fix, the field was missing from GrpcServerConfig and
    /// start_grpc_server never called with_output_handler_service, causing
    /// all InvokeOutputHandler calls to fail with UNAVAILABLE.
    #[test]
    fn test_grpc_server_config_includes_output_handler_service() {
        let has_field = |config: &GrpcServerConfig| config.output_handler_service.is_some();

        let execution_id = ExecutionId::new();
        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id,
            stream_events: vec![],
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());

        let config = GrpcServerConfig {
            addr: "127.0.0.1:0".parse().unwrap(),
            execution_service,
            validation_service,
            grpc_auth: None,
            attestation_service: None,
            tool_invocation_service: None,
            cortex_client: None,
            run_container_step_use_case: None,
            agent_service: None,
            stimulus_service: None,
            discovery_service: None,
            volume_service: None,
            output_handler_service: None,
            fsal: None,
            fuse_mount_client: None,
        };

        assert!(
            !has_field(&config),
            "output_handler_service should be None when not provided"
        );
    }

    /// Regression: invoke_output_handler returned UNAVAILABLE because
    /// OutputHandlerService was never wired into the gRPC server. Verify that
    /// when the service IS attached, the handler does not return UNAVAILABLE.
    #[tokio::test]
    async fn invoke_output_handler_not_unavailable_when_service_wired() {
        use crate::application::output_handler_service::{
            OutputHandlerError, OutputHandlerService,
        };
        use crate::domain::output_handler::OutputHandlerConfig;

        struct StubOutputHandler;
        #[async_trait]
        impl OutputHandlerService for StubOutputHandler {
            async fn invoke(
                &self,
                _config: &OutputHandlerConfig,
                _final_output: &str,
                _parent_execution_id: Option<&ExecutionId>,
                _tenant_id: &TenantId,
                _intent: Option<&str>,
            ) -> std::result::Result<Option<String>, OutputHandlerError> {
                Ok(Some("stub-result".to_string()))
            }
        }

        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id: ExecutionId::new(),
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());

        let service = AegisRuntimeService::new(execution_service, validation_service)
            .with_output_handler_service(Arc::new(StubOutputHandler));

        let exec_id = ExecutionId::new();
        let webhook_config = serde_json::json!({
            "type": "webhook",
            "url": "http://localhost:9999/hook",
            "method": "POST",
            "headers": {},
            "required": false
        });

        let result = service
            .invoke_output_handler(Request::new(InvokeOutputHandlerRequest {
                execution_id: exec_id.0.to_string(),
                tenant_id: TenantId::system().to_string(),
                final_output: "hello".to_string(),
                handler_config_json: webhook_config.to_string(),
                intent: None,
            }))
            .await;

        match &result {
            Err(status) => {
                assert_ne!(
                    status.code(),
                    tonic::Code::Unavailable,
                    "invoke_output_handler must not return UNAVAILABLE when service is wired"
                );
            }
            Ok(resp) => {
                assert!(
                    resp.get_ref().success,
                    "invoke_output_handler should succeed with stub service"
                );
            }
        }
    }

    /// Regression: verify that WITHOUT the service wired, the handler returns
    /// UNAVAILABLE (the pre-fix behavior, guarding the error path).
    #[tokio::test]
    async fn invoke_output_handler_unavailable_without_service() {
        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id: ExecutionId::new(),
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service, validation_service);

        let exec_id = ExecutionId::new();
        let webhook_config = serde_json::json!({
            "type": "webhook",
            "url": "http://localhost:9999/hook",
            "method": "POST",
            "headers": {},
            "required": false
        });

        let err = service
            .invoke_output_handler(Request::new(InvokeOutputHandlerRequest {
                execution_id: exec_id.0.to_string(),
                tenant_id: TenantId::system().to_string(),
                final_output: "hello".to_string(),
                handler_config_json: webhook_config.to_string(),
                intent: None,
            }))
            .await
            .expect_err("invoke_output_handler should fail when service is not wired");

        assert_eq!(
            err.code(),
            tonic::Code::Unavailable,
            "should return UNAVAILABLE when output_handler_service is None"
        );
    }

    /// Regression: passing a workflow execution ID (instead of an agent execution
    /// ID) to InvokeOutputHandler caused `start_child_execution` to fail with
    /// "Parent execution not found". Empty execution_id must be accepted and
    /// treated as `None` (no parent), causing the output handler to spawn a
    /// standalone execution.
    #[tokio::test]
    async fn invoke_output_handler_accepts_empty_execution_id() {
        use crate::application::output_handler_service::{
            OutputHandlerError, OutputHandlerService,
        };
        use crate::domain::output_handler::OutputHandlerConfig;

        struct AssertNoParentHandler;
        #[async_trait]
        impl OutputHandlerService for AssertNoParentHandler {
            async fn invoke(
                &self,
                _config: &OutputHandlerConfig,
                _final_output: &str,
                parent_execution_id: Option<&ExecutionId>,
                _tenant_id: &TenantId,
                _intent: Option<&str>,
            ) -> std::result::Result<Option<String>, OutputHandlerError> {
                assert!(
                    parent_execution_id.is_none(),
                    "empty execution_id in gRPC request must be passed as None to invoke()"
                );
                Ok(Some("ok".to_string()))
            }
        }

        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id: ExecutionId::new(),
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service, validation_service)
            .with_output_handler_service(Arc::new(AssertNoParentHandler));

        let webhook_config = serde_json::json!({
            "type": "webhook",
            "url": "http://localhost:9999/hook",
            "method": "POST",
            "headers": {},
            "required": false
        });

        let result = service
            .invoke_output_handler(Request::new(InvokeOutputHandlerRequest {
                execution_id: String::new(), // empty — ContainerRun/ParallelAgents case
                tenant_id: TenantId::system().to_string(),
                final_output: "hello".to_string(),
                handler_config_json: webhook_config.to_string(),
                intent: None,
            }))
            .await;

        match result {
            Ok(resp) => {
                assert!(
                    resp.get_ref().success,
                    "invoke_output_handler with empty execution_id should succeed"
                );
            }
            Err(status) => {
                panic!(
                    "invoke_output_handler with empty execution_id should not error: {:?}",
                    status
                );
            }
        }
    }

    /// Regression: when a valid agent execution ID is provided, it must be
    /// passed as `Some(&ExecutionId)` to the output handler service.
    #[tokio::test]
    async fn invoke_output_handler_passes_execution_id_when_present() {
        use crate::application::output_handler_service::{
            OutputHandlerError, OutputHandlerService,
        };
        use crate::domain::output_handler::OutputHandlerConfig;

        let expected_id = ExecutionId::new();
        let expected_id_clone = expected_id;

        struct AssertHasParentHandler {
            expected: ExecutionId,
        }
        #[async_trait]
        impl OutputHandlerService for AssertHasParentHandler {
            async fn invoke(
                &self,
                _config: &OutputHandlerConfig,
                _final_output: &str,
                parent_execution_id: Option<&ExecutionId>,
                _tenant_id: &TenantId,
                _intent: Option<&str>,
            ) -> std::result::Result<Option<String>, OutputHandlerError> {
                let parent = parent_execution_id.expect(
                    "non-empty execution_id in gRPC request must be passed as Some to invoke()",
                );
                assert_eq!(
                    *parent, self.expected,
                    "execution_id must match the value from the gRPC request"
                );
                Ok(Some("ok".to_string()))
            }
        }

        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id: ExecutionId::new(),
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());
        let service = AegisRuntimeService::new(execution_service, validation_service)
            .with_output_handler_service(Arc::new(AssertHasParentHandler {
                expected: expected_id_clone,
            }));

        let webhook_config = serde_json::json!({
            "type": "webhook",
            "url": "http://localhost:9999/hook",
            "method": "POST",
            "headers": {},
            "required": false
        });

        let result = service
            .invoke_output_handler(Request::new(InvokeOutputHandlerRequest {
                execution_id: expected_id.0.to_string(),
                tenant_id: TenantId::system().to_string(),
                final_output: "hello".to_string(),
                handler_config_json: webhook_config.to_string(),
                intent: None,
            }))
            .await;

        match result {
            Ok(resp) => {
                assert!(
                    resp.get_ref().success,
                    "invoke_output_handler with valid execution_id should succeed"
                );
            }
            Err(status) => {
                panic!(
                    "invoke_output_handler with valid execution_id should not error: {:?}",
                    status
                );
            }
        }
    }

    /// Regression: FsalGrpcService was defined but never registered with the tonic
    /// server. The FUSE daemon connected to the orchestrator's FsalService endpoint
    /// and received connection refused, preventing volume mounts for ContainerStep
    /// executions. GrpcServerConfig must expose a `fsal` field so that
    /// start_grpc_server can register FsalServiceServer when it is Some.
    #[test]
    fn test_grpc_server_config_includes_fsal_field() {
        let execution_service: Arc<dyn ExecutionService> = Arc::new(TestExecutionService {
            execution_id: ExecutionId::new(),
            stream_events: Vec::new(),
            persisted_execution: None,
            tenant_lookups: Mutex::new(Vec::new()),
        });
        let validation_service = test_validation_service(execution_service.clone());

        // Constructing GrpcServerConfig with fsal: None must compile and the field
        // must be accessible. This proves the field exists on the struct.
        let config = GrpcServerConfig {
            addr: "127.0.0.1:0".parse().unwrap(),
            execution_service,
            validation_service,
            grpc_auth: None,
            attestation_service: None,
            tool_invocation_service: None,
            cortex_client: None,
            run_container_step_use_case: None,
            agent_service: None,
            stimulus_service: None,
            discovery_service: None,
            volume_service: None,
            output_handler_service: None,
            fsal: None,
            fuse_mount_client: None,
        };
        assert!(
            config.fsal.is_none(),
            "GrpcServerConfig.fsal must be accessible and must be None when not provided"
        );
    }

    /// Regression: `resolve_fsal_execution_id` must accept an empty
    /// `execution_id` when `workflow_execution_id` is populated (workflow-owned
    /// volumes send `HandleExecutionContext::Workflow` which has no execution_id).
    /// Before this fix every FSAL handler called `parse_uuid_field` on
    /// `execution_id`, which returned `InvalidArgument` on the empty string.
    #[test]
    fn test_resolve_fsal_execution_id_empty_with_workflow() {
        let wf_id = uuid::Uuid::new_v4().to_string();

        // Empty execution_id + populated workflow_execution_id => nil sentinel
        let result = resolve_fsal_execution_id("", &wf_id);
        assert!(
            result.is_ok(),
            "should accept empty execution_id when workflow_execution_id is set"
        );
        assert_eq!(
            result.unwrap().0,
            uuid::Uuid::nil(),
            "sentinel must be the nil UUID"
        );
    }

    /// Regression: both fields empty must still be rejected.
    #[test]
    fn test_resolve_fsal_execution_id_both_empty_rejected() {
        let result = resolve_fsal_execution_id("", "");
        assert!(
            result.is_err(),
            "must reject when both execution_id and workflow_execution_id are empty"
        );
        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// Regression: a valid execution_id must still parse normally regardless of
    /// workflow_execution_id.
    #[test]
    fn test_resolve_fsal_execution_id_valid_uuid() {
        let exec_id = uuid::Uuid::new_v4();

        // With empty workflow_execution_id
        let result = resolve_fsal_execution_id(&exec_id.to_string(), "");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().0, exec_id);

        // With populated workflow_execution_id
        let wf_id = uuid::Uuid::new_v4().to_string();
        let result = resolve_fsal_execution_id(&exec_id.to_string(), &wf_id);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().0, exec_id);
    }
}
