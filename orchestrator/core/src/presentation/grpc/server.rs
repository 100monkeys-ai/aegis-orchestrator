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
    cortex_service: Option<Arc<dyn aegis_cortex::application::CortexService>>,
    embedding_client: Option<Arc<aegis_cortex::infrastructure::EmbeddingClient>>,
}

impl AegisRuntimeService {
    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        validation_service: Arc<ValidationService>,
    ) -> Self {
        Self {
            execution_service,
            validation_service,
            cortex_service: None,
            embedding_client: None,
        }
    }

    /// Set the Cortex service (optional)
    pub fn with_cortex(
        mut self,
        cortex_service: Arc<dyn aegis_cortex::application::CortexService>,
        embedding_client: Arc<aegis_cortex::infrastructure::EmbeddingClient>,
    ) -> Self {
        self.cortex_service = Some(cortex_service);
        self.embedding_client = Some(embedding_client);
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

        let input = ExecutionInput {
            intent: Some(req.input.clone()),
            payload,
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

        // Convert to Vec<(AgentId, f64)> with default weight of 1.0
        let judge_configs: Vec<(AgentId, f64)> = judge_ids.into_iter().map(|id| (id, 1.0)).collect();

        // Create validation request
        use crate::domain::validation::ValidationRequest;
        let validation_req = ValidationRequest {
            content: req.output,
            criteria: req.task,
            context: None,
        };

        // TODO: Get actual execution_id from context
        let dummy_exec_id = crate::domain::execution::ExecutionId::new();

        // Call validation service with default configuration
        match self.validation_service.validate_with_judges(
            dummy_exec_id, 
            validation_req, 
            judge_configs,
            None, // Use default consensus config
            60, // 60 second timeout
            500 // 500ms poll interval
        ).await {
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

    /// Query Cortex for patterns based on error signature or embedding similarity
    async fn query_cortex_patterns(
        &self,
        request: Request<QueryCortexRequest>,
    ) -> Result<Response<QueryCortexResponse>, Status> {
        let req = request.into_inner();
        
        // Check if Cortex is enabled
        let (cortex, embedding_client) = match (&self.cortex_service, &self.embedding_client) {
            (Some(c), Some(e)) => (c, e),
            _ => {
                tracing::warn!("QueryCortexPatterns called but Cortex is not enabled");
                return Ok(Response::new(QueryCortexResponse {
                    patterns: vec![],
                }));
            }
        };

        // Generate embedding from error signature
        let query_embedding = embedding_client
            .generate_embedding(&req.error_signature)
            .await
            .map_err(|e| Status::internal(format!("Failed to generate embedding: {}", e)))?;

        // Search for similar patterns
        let limit = req.limit.unwrap_or(10) as usize;
        let patterns = cortex
            .search_patterns(query_embedding, limit)
            .await
            .map_err(|e| Status::internal(format!("Failed to search patterns: {}", e)))?;

        // Filter by success score if requested
        let min_score = req.min_success_score.unwrap_or(0.0);
        let filtered_patterns: Vec<_> = patterns
            .into_iter()
            .filter(|p| p.success_score >= min_score as f64)
            .collect();

        // Convert domain patterns to proto
        let proto_patterns: Vec<CortexPattern> = filtered_patterns
            .into_iter()
            .map(|p| CortexPattern {
                id: p.id.0.to_string(),
                error_signature_hash: p.error_signature.error_message_hash.clone(),
                error_type: p.error_signature.error_type.clone(),
                error_message: p.error_signature.error_message_hash.clone(), // Hash serves as message identifier
                solution_approach: p.task_category.clone(),
                solution_code: Some(p.solution_code.clone()),
                frequency: p.execution_count as u32,
                success_count: (p.execution_count as f64 * p.success_score).round() as u32,
                total_count: p.execution_count as u32,
                success_score: p.success_score as f32,
                created_at: p.created_at.to_rfc3339(),
                last_used_at: p.last_verified.to_rfc3339(),
            })
            .collect();

        tracing::info!("Returned {} patterns from Cortex", proto_patterns.len());

        Ok(Response::new(QueryCortexResponse {
            patterns: proto_patterns,
        }))
    }

    /// Store a learned pattern in Cortex with automatic deduplication
    async fn store_cortex_pattern(
        &self,
        request: Request<StoreCortexPatternRequest>,
    ) -> Result<Response<StoreCortexPatternResponse>, Status> {
        let req = request.into_inner();
        
        // Check if Cortex is enabled
        let (cortex, embedding_client) = match (&self.cortex_service, &self.embedding_client) {
            (Some(c), Some(e)) => (c, e),
            _ => {
                tracing::warn!("StoreCortexPattern called but Cortex is not enabled");
                return Ok(Response::new(StoreCortexPatternResponse {
                    pattern_id: uuid::Uuid::new_v4().to_string(),
                    deduplicated: false,
                    new_frequency: 1,
                }));
            }
        };

        // Generate embedding
        let content = format!(
            "{} {} {}",
            req.error_message,
            req.solution_approach,
            req.solution_code.as_ref().unwrap_or(&String::new())
        );
        let embedding = embedding_client
            .generate_embedding(&content)
            .await
            .map_err(|e| Status::internal(format!("Failed to generate embedding: {}", e)))?;

        // Create error signature
        let error_signature = aegis_cortex::domain::ErrorSignature::new(
            req.error_type.clone(),
            &req.error_message,
        );

        // Store pattern (with automatic deduplication)
        let solution_code = req.solution_code.unwrap_or_else(|| req.solution_approach.clone());
        let pattern_id = cortex
            .store_pattern(
                None, // No execution_id for gRPC-stored patterns
                error_signature,
                solution_code,
                req.solution_approach,
                embedding,
            )
            .await
            .map_err(|e| Status::internal(format!("Failed to store pattern: {}", e)))?;

        // Get the stored pattern to check if it was deduplicated
        let pattern = cortex
            .get_pattern(pattern_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to retrieve pattern: {}", e)))?
            .ok_or_else(|| Status::internal("Pattern not found after storing"))?;

        // Pattern is deduplicated if execution_count > 1 (means it was incremented)
        let deduplicated = pattern.execution_count > 1;
        let new_frequency = pattern.execution_count as u32;

        tracing::info!(
            "Stored pattern {} (deduplicated: {}, frequency: {})",
            pattern_id.0,
            deduplicated,
            new_frequency
        );

        Ok(Response::new(StoreCortexPatternResponse {
            pattern_id: pattern_id.0.to_string(),
            deduplicated,
            new_frequency,
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
