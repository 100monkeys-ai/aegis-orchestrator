// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Validation Service
//!
//! This application service implements multi-agent validation patterns including
//! Judge agents, consensus strategies, and gradient-based evaluation for agent outputs.
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Orchestrate validation workflows with multiple judge agents
//! - **Integration:** Execution Service → Judge Agents → Consensus Calculation
//!
//! # Validation Patterns
//!
//! ## Multi-Judge Consensus
//!
//! Execute multiple judge agents in parallel and aggregate their verdicts using
//! configurable consensus strategies:
//!
//! - **Weighted Average**: Combine scores using judge-specific weights
//! - **Majority Vote**: Simple majority with optional threshold
//! - **Top-N**: Select best N judges and average their scores
//! - **Unanimous**: Require all judges to agree
//!
//! ## Gradient Evaluation
//!
//! Judge agents return structured `GradientResult` with:
//! - Pass/fail verdict
//! - Confidence score (0.0 - 1.0)
//! - Detailed reasoning
//!
//! ## Cortex Integration
//!
//! When enabled, validation results are sent to Cortex for pattern learning:
//! - Success/failure signatures
//! - Error pattern detection
//! - Confidence calibration
//!
//! # Usage
//!
//! ```no_run
//! use validation_service::ValidationService;
//! use domain::validation::ValidationRequest;
//!
//! let service = ValidationService::new(event_bus, execution_service, cortex);
//! let judges = vec![
//!     (judge1_id, 1.0),  // weight: 1.0
//!     (judge2_id, 0.8),  // weight: 0.8
//! ];
//!
//! let consensus = service.validate_with_judges(
//!     execution_id,
//!     request,
//!     judges,
//!     Some(config),
//!     timeout_secs,
//!     poll_interval_ms
//! ).await?;
//! ```

use std::sync::Arc;
use std::time::Duration;
use anyhow::{Result, anyhow, Context};
use crate::domain::agent::AgentId;
use crate::domain::validation::{GradientResult, MultiJudgeConsensus, ValidationRequest};
use crate::domain::workflow::{ConsensusConfig, ConsensusStrategy, ConfidenceWeighting};

use crate::application::execution::ExecutionService;
use crate::domain::execution::{ExecutionInput, ExecutionStatus};

// Import Cortex for pattern learning
use aegis_cortex::application::CortexService;
use aegis_cortex::domain::ErrorSignature;
use aegis_cortex::infrastructure::EmbeddingClient;

pub struct ValidationService {
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    execution_service: Arc<dyn ExecutionService>,
    cortex_service: Option<Arc<dyn CortexService>>,
    embedding_client: Option<Arc<EmbeddingClient>>,
}

impl ValidationService {
    pub fn new(
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
        execution_service: Arc<dyn ExecutionService>,
        cortex_service: Option<Arc<dyn CortexService>>,
    ) -> Self {
        // Create embedding client if Cortex is enabled
        let embedding_client = cortex_service.as_ref().map(|_| Arc::new(EmbeddingClient::new()));
        
        Self { event_bus, execution_service, cortex_service, embedding_client }
    }

    pub async fn validate_with_judges(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        request: ValidationRequest,
        judges: Vec<(AgentId, f64)>, // (judge_id, weight)
        config: Option<ConsensusConfig>,
        timeout_seconds: u64,
        poll_interval_ms: u64,
    ) -> Result<MultiJudgeConsensus> {
        if judges.is_empty() {
            return Err(anyhow!("No judges provided for validation"));
        }

        let config = config.unwrap_or_else(|| ConsensusConfig {
            strategy: ConsensusStrategy::WeightedAverage,
            threshold: None,
            min_agreement_confidence: None,
            n: None,
            min_judges_required: 1,
            confidence_weighting: None,
        });

        // Validate confidence weighting if provided
        if let Some(ref weighting) = config.confidence_weighting {
            weighting.validate()
                .map_err(|e| anyhow!("Invalid confidence weighting: {}", e))?;
        }

        let mut futures = Vec::new();
        for (judge_id, weight) in &judges {
            let service = self.execution_service.clone();
            let req = request.clone();
            let judge = *judge_id;
            let w = *weight;
            let timeout = timeout_seconds;
            let poll_interval = poll_interval_ms;
            
            futures.push(tokio::spawn(async move {
                match Self::run_judge(service, judge, req, timeout, poll_interval).await {
                    Ok((_agent_id, gradient_result)) => Ok((judge, gradient_result, w)),
                    Err(e) => Err(e),
                }
            }));
        }

        let mut results = Vec::new();
        for future in futures {
            match future.await {
                Ok(Ok((agent_id, result, weight))) => {
                    // Publish individual judge result
                    self.event_bus.publish_execution_event(
                        crate::domain::events::ExecutionEvent::Validation(
                            crate::domain::events::ValidationEvent::GradientValidationPerformed {
                                execution_id,
                                iteration_number: 0, // TODO: Pass iteration number from caller
                                score: result.score,
                                confidence: result.confidence,
                                validated_at: chrono::Utc::now(),
                            }
                        )
                    );
                    results.push((agent_id, result, weight));
                },
                Ok(Err(e)) => tracing::warn!("Judge execution failed: {}", e),
                Err(e) => tracing::error!("Join error: {}", e),
            }
        }

        // Check minimum judges requirement
        if results.len() < config.min_judges_required {
            return Err(anyhow!(
                "Insufficient judges succeeded: {} of {} required (total: {})",
                results.len(),
                config.min_judges_required,
                judges.len()
            ));
        }

        let consensus = self.compute_consensus(results, &config)?;

        // Publish consensus event
        self.event_bus.publish_execution_event(
            crate::domain::events::ExecutionEvent::Validation(
                crate::domain::events::ValidationEvent::MultiJudgeConsensus {
                    execution_id,
                    judge_scores: consensus.individual_results.iter().map(|(id, r)| (*id, r.score)).collect(),
                    final_score: consensus.final_score,
                    confidence: consensus.consensus_confidence,
                    reached_at: chrono::Utc::now(),
                }
            )
        );

        // Pattern Learning: Capture patterns based on validation score
        if let Some(cortex) = &self.cortex_service {
            self.capture_pattern(cortex.clone(), execution_id, &request, &consensus).await
                .unwrap_or_else(|e| tracing::warn!("Failed to capture pattern: {}", e));
        }

        Ok(consensus)
    }

    /// Capture pattern from validation result
    async fn capture_pattern(
        &self,
        cortex: Arc<dyn CortexService>,
        execution_id: crate::domain::execution::ExecutionId,
        request: &ValidationRequest,
        consensus: &MultiJudgeConsensus,
    ) -> Result<()> {
        // Extract error signature from request content
        // For now, use a simple heuristic: look for error patterns
        let signature = ErrorSignature::new(
            "validation_error".to_string(),
            &request.content,
        );

        // Generate embedding using EmbeddingClient
        let embedding = if let Some(client) = &self.embedding_client {
            client.generate_embedding(&signature.error_message_hash).await?
        } else {
            // Fallback to empty embedding if client not available
            vec![0.0; 384]
        };

        if consensus.final_score > 0.7 {
            // High score: Store pattern and apply dopamine (success reinforcement)
            tracing::info!(
                score = consensus.final_score,
                "Capturing successful pattern"
            );

            let pattern_id = cortex.store_pattern(
                Some(execution_id.0),
                signature,
                request.content.clone(), // solution
                "validation".to_string(), // category
                embedding,
            ).await?;

            // Apply dopamine for successful pattern
            cortex.apply_dopamine(pattern_id, Some(execution_id.0), 0.5).await?;
        } else if consensus.final_score < 0.3 {
            // Low score: Apply cortisol (failure penalty) if pattern exists
            tracing::debug!(
                score = consensus.final_score,
                "Low validation score - applying cortisol if pattern exists"
            );

            // Search for similar patterns
            let similar = cortex.search_patterns(embedding.clone(), 1).await?;
            if let Some(pattern) = similar.first() {
                cortex.apply_cortisol(pattern.id, 0.3).await?;
            }
        }

        Ok(())
    }

    async fn run_judge(
        service: Arc<dyn ExecutionService>,
        judge_id: AgentId,
        request: ValidationRequest,
        timeout_seconds: u64,
        poll_interval_ms: u64,
    ) -> Result<(AgentId, GradientResult)> {
        // 1. Prepare input
        // Let ExecutionService render the judge agent's prompt_template
        // The judge agent's manifest defines how it should receive validation requests.
        // We pass the validation data as workflow_input so it gets rendered through
        // the judge's template (e.g., "You are a validation judge...{user_input}").
        let payload_data = serde_json::to_value(&request)?;
        let input = ExecutionInput {
            intent: None,  // Let ExecutionService render judge agent's prompt_template
            payload: serde_json::json!({
                "workflow_input": payload_data,  // ValidationRequest data
                "validation_context": "judge_execution"
            }),
        };

        // 2. Start execution
        let exec_id = service.start_execution(judge_id, input).await?;

        // 3. Poll for completion with configurable timeout and interval
        let max_attempts = (timeout_seconds * 1000) / poll_interval_ms;
        let mut attempts = 0;
        
        loop {
            if attempts >= max_attempts {
                return Err(anyhow!("Judge execution timed out after {} seconds", timeout_seconds));
            }
            
            let exec = service.get_execution(exec_id).await?;
            match exec.status {
                ExecutionStatus::Completed => {
                    // 4. Parse output
                    let last_iter = exec.iterations().last()
                        .ok_or_else(|| anyhow!("Judge completed but has no iterations"))?;
                        
                    let output_str = last_iter.output.as_ref()
                        .ok_or_else(|| anyhow!("Judge completed but has no output"))?;
                    
                    // Attempt to parse JSON
                    // The judge might wrap it in markdown block ```json ... ```
                    let json_str = Self::extract_json(output_str).unwrap_or(output_str.clone());
                    
                    let result: GradientResult = serde_json::from_str(&json_str)
                        .context(format!("Failed to parse judge output: {}", json_str))?;
                        
                    return Ok((judge_id, result));
                },
                ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                    return Err(anyhow!("Judge execution failed or cancelled"));
                },
                _ => {
                    tokio::time::sleep(Duration::from_millis(poll_interval_ms)).await;
                    attempts += 1;
                }
            }
        }
    }
    
    fn extract_json(text: &str) -> Option<String> {
        // Find start of markdown code block
        let start_marker = "```json";
        if let Some(start) = text.find(start_marker) {
            let content_start = start + start_marker.len();
            // Find end marker AFTER the content start
            if let Some(end_offset) = text[content_start..].find("```") {
                let content_end = content_start + end_offset;
                return Some(text[content_start..content_end].trim().to_string());
            }
        }
        
        // Try generic code block if json specific one not found
        let generic_marker = "```";
        if let Some(start) = text.find(generic_marker) {
             let content_start = start + generic_marker.len();
             if let Some(end_offset) = text[content_start..].find("```") {
                 let content_end = content_start + end_offset;
                 return Some(text[content_start..content_end].trim().to_string());
             }
        }
        
        None
    }

    fn compute_consensus(&self, results: Vec<(AgentId, GradientResult, f64)>, config: &ConsensusConfig) -> Result<MultiJudgeConsensus> {
        if results.is_empty() {
            return Err(anyhow!("Cannot compute consensus with zero results"));
        }

        // Dispatch to appropriate strategy
        match config.strategy {
            ConsensusStrategy::WeightedAverage => self.compute_weighted_average_consensus(results, config),
            ConsensusStrategy::Majority => self.compute_majority_consensus(results, config),
            ConsensusStrategy::Unanimous => self.compute_unanimous_consensus(results, config),
            ConsensusStrategy::BestOfN => self.compute_best_of_n_consensus(results, config),
        }
    }

    fn compute_weighted_average_consensus(
        &self,
        results: Vec<(AgentId, GradientResult, f64)>,
        config: &ConsensusConfig,
    ) -> Result<MultiJudgeConsensus> {
        let total_weight: f64 = results.iter().map(|(_, _, w)| w).sum();
        
        if total_weight == 0.0 {
            return Err(anyhow!("Total weight is zero"));
        }

        // Weighted average of scores
        let weighted_score: f64 = results.iter()
            .map(|(_, r, w)| r.score * w)
            .sum::<f64>() / total_weight;

        // Confidence calculation with configurable weighting
        let count = results.len() as f64;
        let unweighted_mean: f64 = results.iter().map(|(_, r, _)| r.score).sum::<f64>() / count;
        
        // Variance = sum((x - mean)^2) / n
        let variance: f64 = results.iter()
            .map(|(_, r, _)| (r.score - unweighted_mean).powi(2))
            .sum::<f64>() / count;
            
        // Max variance for [0,1] is 0.25 (e.g. half 0s, half 1s)
        let disagreement_penalty = (variance / 0.25).min(1.0);
        let agreement_factor = 1.0 - disagreement_penalty;
        
        // Average of judges' self-confidence (weighted)
        let avg_judge_confidence: f64 = results.iter()
            .map(|(_, r, w)| r.confidence * w)
            .sum::<f64>() / total_weight;
        
        // Use configurable confidence weighting or defaults
        let default_weighting = ConfidenceWeighting::default();
        let weighting = config.confidence_weighting.as_ref().unwrap_or(&default_weighting);
        let consensus_confidence = 
            agreement_factor * weighting.agreement_factor + 
            avg_judge_confidence * weighting.self_confidence_factor;

        Ok(MultiJudgeConsensus {
            final_score: weighted_score,
            consensus_confidence,
            individual_results: results.iter().map(|(id, r, _)| (*id, r.clone())).collect(),
            strategy: "weighted_average".to_string(),
            metadata: std::collections::HashMap::new(),
        })
    }

    fn compute_majority_consensus(
        &self,
        results: Vec<(AgentId, GradientResult, f64)>,
        config: &ConsensusConfig,
    ) -> Result<MultiJudgeConsensus> {
        let threshold = config.threshold.unwrap_or(0.7);
        
        // Count votes: pass (score >= threshold) vs fail (score < threshold)
        let pass_votes: usize = results.iter()
            .filter(|(_, r, _)| r.score >= threshold)
            .count();
        
        let fail_votes = results.len() - pass_votes;
        
        // Majority decision
        let final_score = if pass_votes > fail_votes {
            1.0 // Pass
        } else if fail_votes > pass_votes {
            0.0 // Fail
        } else {
            0.5 // Tie - neutral
        };
        
        // Confidence based on margin of victory
        let total = results.len() as f64;
        let margin = ((pass_votes as f64 - fail_votes as f64).abs() / total).min(1.0);
        
        // Also factor in individual judge confidence
        let avg_judge_confidence: f64 = results.iter()
            .map(|(_, r, _)| r.confidence)
            .sum::<f64>() / total;
        
        let consensus_confidence = margin * 0.7 + avg_judge_confidence * 0.3;

        Ok(MultiJudgeConsensus {
            final_score,
            consensus_confidence,
            individual_results: results.iter().map(|(id, r, _)| (*id, r.clone())).collect(),
            strategy: "majority".to_string(),
            metadata: {
                let mut map = std::collections::HashMap::new();
                map.insert("pass_votes".to_string(), serde_json::json!(pass_votes));
                map.insert("fail_votes".to_string(), serde_json::json!(fail_votes));
                map.insert("threshold".to_string(), serde_json::json!(threshold));
                map
            },
        })
    }

    fn compute_unanimous_consensus(
        &self,
        results: Vec<(AgentId, GradientResult, f64)>,
        config: &ConsensusConfig,
    ) -> Result<MultiJudgeConsensus> {
        let threshold = config.threshold.unwrap_or(0.7);
        
        // All judges must score >= threshold
        let all_pass = results.iter().all(|(_, r, _)| r.score >= threshold);
        
        let final_score = if all_pass {
            // Average of actual scores
            let count = results.len() as f64;
            results.iter().map(|(_, r, _)| r.score).sum::<f64>() / count
        } else {
            0.0 // Any dissenter fails consensus
        };
        
        // Confidence is minimum of all judge confidences (weakest link)
        let min_confidence = results.iter()
            .map(|(_, r, _)| r.confidence)
            .fold(f64::INFINITY, f64::min);
        
        Ok(MultiJudgeConsensus {
            final_score,
            consensus_confidence: min_confidence,
            individual_results: results.iter().map(|(id, r, _)| (*id, r.clone())).collect(),
            strategy: "unanimous".to_string(),
            metadata: {
                let mut map = std::collections::HashMap::new();
                map.insert("all_pass".to_string(), serde_json::json!(all_pass));
                map.insert("threshold".to_string(), serde_json::json!(threshold));
                map
            },
        })
    }

    fn compute_best_of_n_consensus(
        &self,
        results: Vec<(AgentId, GradientResult, f64)>,
        config: &ConsensusConfig,
    ) -> Result<MultiJudgeConsensus> {
        let n = config.n.unwrap_or(results.len());
        
        if n == 0 {
            return Err(anyhow!("BestOfN requires n > 0"));
        }
        
        // Sort by score * confidence (descending)
        let mut sorted_results = results.clone();
        sorted_results.sort_by(|(_, a, _), (_, b, _)| {
            let score_a = a.score * a.confidence;
            let score_b = b.score * b.confidence;
            score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
        });
        
        // Take top N (or all if N > total)
        let top_n: Vec<_> = sorted_results.iter().take(n).collect();
        let count = top_n.len() as f64;
        
        // Average of top N scores (weighted)
        let total_weight: f64 = top_n.iter().map(|(_, _, w)| w).sum();
        let final_score = if total_weight > 0.0 {
            top_n.iter()
                .map(|(_, r, w)| r.score * w)
                .sum::<f64>() / total_weight
        } else {
            top_n.iter().map(|(_, r, _)| r.score).sum::<f64>() / count
        };
        
        // Average confidence of top N
        let consensus_confidence = top_n.iter()
            .map(|(_, r, _)| r.confidence)
            .sum::<f64>() / count;

        Ok(MultiJudgeConsensus {
            final_score,
            consensus_confidence,
            individual_results: results.iter().map(|(id, r, _)| (*id, r.clone())).collect(),
            strategy: "best_of_n".to_string(),
            metadata: {
                let mut map = std::collections::HashMap::new();
                map.insert("n".to_string(), serde_json::json!(n));
                map.insert("total_judges".to_string(), serde_json::json!(results.len()));
                map
            },
        })
    }
}
