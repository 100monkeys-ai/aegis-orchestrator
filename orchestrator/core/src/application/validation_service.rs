use std::sync::Arc;
use std::time::Duration;
use anyhow::{Result, anyhow, Context};
use crate::domain::agent::AgentId;
use crate::domain::validation::{GradientResult, MultiJudgeConsensus, ValidationRequest};

use crate::application::execution::ExecutionService;
use crate::domain::execution::{ExecutionInput, ExecutionStatus};

pub struct ValidationService {
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    execution_service: Arc<dyn ExecutionService>,
}

impl ValidationService {
    pub fn new(
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
        execution_service: Arc<dyn ExecutionService>
    ) -> Self {
        Self { event_bus, execution_service }
    }

    pub async fn validate_with_judges(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        request: ValidationRequest,
        judges: Vec<AgentId>,
    ) -> Result<MultiJudgeConsensus> {
        if judges.is_empty() {
            return Err(anyhow!("No judges provided for validation"));
        }

        let mut futures = Vec::new();
        for judge_id in judges {
            let service = self.execution_service.clone();
            let req = request.clone();
            
            futures.push(tokio::spawn(async move {
                Self::run_judge(service, judge_id, req).await
            }));
        }

        let mut results = Vec::new();
        for future in futures {
            match future.await {
                Ok(Ok((agent_id, result))) => {
                    // Publish individual judge result
                    self.event_bus.publish_execution_event(
                        crate::domain::events::ExecutionEvent::Validation(
                            crate::domain::events::ValidationEvent::GradientValidationPerformed {
                                execution_id,
                                iteration_number: 0, // TODO: Pass iteration number
                                score: result.score,
                                confidence: result.confidence,
                                validated_at: chrono::Utc::now(),
                            }
                        )
                    );
                    results.push((agent_id, result));
                },
                Ok(Err(e)) => tracing::warn!("Judge execution failed: {}", e),
                Err(e) => tracing::error!("Join error: {}", e),
            }
        }

        if results.is_empty() {
            return Err(anyhow!("All judges failed to produce a result"));
        }

        let consensus = self.compute_consensus(results);

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

        Ok(consensus)
    }

    async fn run_judge(
        service: Arc<dyn ExecutionService>,
        judge_id: AgentId,
        request: ValidationRequest,
    ) -> Result<(AgentId, GradientResult)> {
        // 1. Prepare input
        // payload must match what the Judge Agent expects.
        // Usually { "content": ..., "criteria": ... } which matches ValidationRequest
        let payload = serde_json::to_value(&request)?;
        let input = ExecutionInput {
            intent: Some("Validate content".to_string()),
            payload,
        };

        // 2. Start execution
        let exec_id = service.start_execution(judge_id, input).await?;

        // 3. Poll for completion
        // TODO: Use events/streams instead of polling in future
        let mut attempts = 0;
        let max_attempts = 120; // 60 seconds
        
        loop {
            if attempts >= max_attempts {
                return Err(anyhow!("Judge execution timed out"));
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
                    tokio::time::sleep(Duration::from_millis(500)).await;
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

    fn compute_consensus(&self, results: Vec<(AgentId, GradientResult)>) -> MultiJudgeConsensus {
        let count = results.len() as f64;
        if count == 0.0 {
            // Should be handled by caller, but safe fallback
            return MultiJudgeConsensus {
                final_score: 0.0,
                consensus_confidence: 0.0,
                individual_results: vec![],
                strategy: "none".to_string(),
            };
        }

        // Strategy: Simple Average for score
        // TODO: Weighted average based on judge reputation/competence (Phase 4)
        let total_score: f64 = results.iter().map(|(_, r)| r.score).sum();
        let average_score = total_score / count;

        // Confidence calculation
        // If judges disagree significantly, confidence drops.
        // Variance = sum((x - mean)^2) / n
        let variance: f64 = results.iter()
            .map(|(_, r)| (r.score - average_score).powi(2))
            .sum::<f64>() / count;
            
        // Convert variance to confidence (0 variance = 1.0 confidence, 0.25 variance = 0.0 confidence (max variance for 0-1 range is 0.25))
        // Max variance for [0,1] is 0.25 (e.g. half 0s, half 1s)
        let disagreement_penalty = (variance / 0.25).min(1.0);
        let agreement_factor = 1.0 - disagreement_penalty;
        
        // Also factor in the judges' own confidence
        let avg_judge_confidence: f64 = results.iter().map(|(_, r)| r.confidence).sum::<f64>() / count;
        
        // Final consensus confidence is a mix of agreement and judge self-confidence
        let consensus_confidence = agreement_factor * 0.7 + avg_judge_confidence * 0.3;

        MultiJudgeConsensus {
            final_score: average_score,
            consensus_confidence,
            individual_results: results,
            strategy: "average_with_variance_penalty".to_string(),
        }
    }
}
