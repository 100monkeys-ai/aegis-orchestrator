// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! gRPC Server Implementation for AEGIS Runtime
//! Exposes ExecuteAgent, ExecuteSystemCommand, ValidateWithJudges, QueryCortexPatterns, StoreCortexPattern

use tonic::{Request, Response, Status};
use tokio_stream::wrappers::ReceiverStream;
use tokio::sync::mpsc;
use std::sync::Arc;
use chrono::Utc;

use crate::application::execution::ExecutionService;
use crate::application::validation_service::ValidationService;
use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionInput;

// Generated protobuf code
pub mod aegis_runtime {
    tonic::include_proto!("aegis.runtime.v1");
}

use aegis_runtime::aegis_runtime_server::{AegisRuntime, AegisRuntimeServer};
use aegis_runtime::*;

/// Implementation of the AegisRuntime gRPC service
pub struct AegisRuntimeService {
    execution_service: Arc<dyn ExecutionService>,
    validation_service: Arc<ValidationService>,
}

impl AegisRuntimeService {
    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        validation_service: Arc<ValidationService>,
    ) -> Self {
        Self {
            execution_service,
            validation_service,
        }
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
        let mut context_map = serde_json::Map::new();
        for (k, v) in req.context {
            context_map.insert(k, serde_json::Value::String(v));
        }
        
        let input = ExecutionInput {
            intent: Some(req.input.clone()),
            payload: serde_json::Value::Object(context_map),
        };

        // Channel for streaming events
        let (tx, rx) = mpsc::channel(100);

        // Clone for async task
        let execution_service = self.execution_service.clone();
        let tx_clone = tx.clone();

        // Spawn execution task
        tokio::spawn(async move {
            // Send ExecutionStarted event
            let _ = tx_clone.send(Ok(ExecutionEvent {
                event: Some(execution_event::Event::ExecutionStarted(ExecutionStarted {
                    execution_id: uuid::Uuid::new_v4().to_string(),
                    agent_id: agent_id.0.to_string(),
                    started_at: Utc::now().to_rfc3339(),
                })),
            })).await;

            // Start execution
            match execution_service.start_execution(agent_id, input).await {
                Ok(execution_id) => {
                    // Stream execution events
                    match execution_service.stream_execution(execution_id).await {
                        Ok(mut stream) => {
                            use futures::StreamExt;
                            while let Some(event_result) = stream.next().await {
                                match event_result {
                                    Ok(domain_event) => {
                                        // Convert domain event to protobuf event
                                        if let Some(pb_event) = convert_domain_event_to_proto(domain_event, execution_id) {
                                            if tx_clone.send(Ok(pb_event)).await.is_err() {
                                                break; // Client disconnected
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx_clone.send(Ok(ExecutionEvent {
                                            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                                                execution_id: execution_id.to_string(),
                                                reason: e.to_string(),
                                                total_iterations: 0,
                                                failed_at: Utc::now().to_rfc3339(),
                                            })),
                                        })).await;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx_clone.send(Ok(ExecutionEvent {
                                event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                                    execution_id: execution_id.to_string(),
                                    reason: format!("Failed to stream execution: {}", e),
                                    total_iterations: 0,
                                    failed_at: Utc::now().to_rfc3339(),
                                })),
                            })).await;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx_clone.send(Ok(ExecutionEvent {
                        event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                            execution_id: uuid::Uuid::new_v4().to_string(),
                            reason: format!("Failed to start execution: {}", e),
                            total_iterations: 0,
                            failed_at: Utc::now().to_rfc3339(),
                        })),
                    })).await;
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

        // Parse judge agent IDs
        let judge_ids: Vec<AgentId> = req.judges
            .into_iter()
            .map(|j| AgentId::from_string(&j.agent_id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Status::invalid_argument(format!("Invalid judge agent_id: {}", e)))?;

        // Create validation request
        use crate::domain::validation::ValidationRequest;
        let validation_req = ValidationRequest {
            content: req.output,
            criteria: req.task,
            context: None,
        };

        // TODO: Get actual execution_id from context
        let dummy_exec_id = crate::domain::execution::ExecutionId::new();

        // Call validation service
        match self.validation_service.validate_with_judges(dummy_exec_id, validation_req, judge_ids).await {
            Ok(consensus) => {
                // Convert to protobuf
                let individual_results = consensus.individual_results
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
                    binary_valid: consensus.final_score > 0.5,
                    individual_results,
                }))
            }
            Err(e) => Err(Status::internal(format!("Validation failed: {}", e))),
        }
    }

    /// Query Cortex for patterns (STUBBED - Will be implemented with Vector+RAG)
    async fn query_cortex_patterns(
        &self,
        request: Request<QueryCortexRequest>,
    ) -> Result<Response<QueryCortexResponse>, Status> {
        let _req = request.into_inner();
        
        // TODO: Implement Cortex pattern search with Vector+RAG in future iteration
        tracing::warn!("QueryCortexPatterns called but Cortex is not yet implemented (stubbed)");
        
        // Return empty results for now
        Ok(Response::new(QueryCortexResponse {
            patterns: vec![],
        }))
    }

    /// Store a learned pattern in Cortex (STUBBED - Will be implemented with Vector+RAG)
    async fn store_cortex_pattern(
        &self,
        request: Request<StoreCortexPatternRequest>,
    ) -> Result<Response<StoreCortexPatternResponse>, Status> {
        let _req = request.into_inner();
        
        // TODO: Implement Cortex pattern storage with Vector+RAG in future iteration
        tracing::warn!("StoreCortexPattern called but Cortex is not yet implemented (stubbed)");
        
        // Return placeholder response
        Ok(Response::new(StoreCortexPatternResponse {
            pattern_id: uuid::Uuid::new_v4().to_string(),
            deduplicated: false,
            new_frequency: 1,
        }))
    }
}

/// Convert domain ExecutionEvent to protobuf ExecutionEvent
fn convert_domain_event_to_proto(
    domain_event: crate::domain::events::ExecutionEvent,
    execution_id: crate::domain::execution::ExecutionId,
) -> Option<ExecutionEvent> {
    use crate::domain::events::ExecutionEvent as DomainEvent;

    match domain_event {
        DomainEvent::IterationStarted { iteration_number, action, started_at, .. } => {
            Some(ExecutionEvent {
                event: Some(execution_event::Event::IterationStarted(IterationStarted {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    action,
                    started_at: started_at.to_rfc3339(),
                })),
            })
        }
        DomainEvent::ConsoleOutput { iteration_number, content, .. } => {
            Some(ExecutionEvent {
                event: Some(execution_event::Event::IterationOutput(IterationOutput {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    output: content,
                })),
            })
        }
        DomainEvent::IterationCompleted { iteration_number, output, completed_at, .. } => {
            Some(ExecutionEvent {
                event: Some(execution_event::Event::IterationCompleted(IterationCompleted {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    output,
                    completed_at: completed_at.to_rfc3339(),
                })),
            })
        }
        DomainEvent::IterationFailed { iteration_number, error, failed_at, .. } => {
            Some(ExecutionEvent {
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
            })
        }
        DomainEvent::RefinementApplied { iteration_number, code_diff, applied_at, .. } => {
            Some(ExecutionEvent {
                event: Some(execution_event::Event::RefinementApplied(RefinementApplied {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    code_diff: format!("{}:\n{}", code_diff.file_path, code_diff.diff),
                    applied_at: applied_at.to_rfc3339(),
                })),
            })
        }
        DomainEvent::ExecutionCompleted { final_output, total_iterations, completed_at, .. } => {
            Some(ExecutionEvent {
                event: Some(execution_event::Event::ExecutionCompleted(ExecutionCompleted {
                    execution_id: execution_id.to_string(),
                    final_output,
                    total_iterations: total_iterations as u32,
                    completed_at: completed_at.to_rfc3339(),
                })),
            })
        }
        DomainEvent::ExecutionFailed { reason, total_iterations, failed_at, .. } => {
            Some(ExecutionEvent {
                event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                    execution_id: execution_id.to_string(),
                    reason,
                    total_iterations: total_iterations as u32,
                    failed_at: failed_at.to_rfc3339(),
                })),
            })
        }
        _ => None, // Other events not relevant for streaming
    }
}

/// Start the gRPC server
pub async fn start_grpc_server(
    addr: std::net::SocketAddr,
    execution_service: Arc<dyn ExecutionService>,
    validation_service: Arc<ValidationService>,
) -> Result<(), Box<dyn std::error::Error>> {
    let service = AegisRuntimeService::new(execution_service, validation_service);
    let server = service.into_server();

    tracing::info!("Starting AEGIS gRPC server on {}", addr);

    tonic::transport::Server::builder()
        .add_service(server)
        .serve(addr)
        .await?;

    Ok(())
}
