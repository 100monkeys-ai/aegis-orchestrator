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
//! # Example
//!
//! ```rust,ignore
//! # use aegis_orchestrator_core::application::validation::ValidationService;
//! # use aegis_orchestrator_core::domain::validation::ValidationRequest;
//! # use aegis_orchestrator_core::domain::agent::AgentId;
//! # use aegis_orchestrator_core::domain::execution::ExecutionId;
//! # use aegis_orchestrator_core::domain::workflow::ConsensusConfig;
//! # async fn example(service: &ValidationService) -> anyhow::Result<()> {
//! let execution_id = ExecutionId::new();
//! let agent_id = AgentId::new();
//! let judges = vec![(AgentId::new(), 1.0)];
//! let request = ValidationRequest::new();
//! let config = ConsensusConfig::default();
//!
//! let consensus = service.validate_with_judges(
//!     execution_id,
//!     agent_id,
//!     1,
//!     request,
//!     judges,
//!     Some(config),
//!     30,
//!     100
//! ).await?;
//! # Ok(())
//! # }
//! ```

use crate::domain::agent::{AgentId, FallbackBehavior, SemanticValidation, ValidationConfig};
use crate::domain::llm::{ChatMessage, ChatResponse, GenerationOptions};
use crate::domain::validation::{
    GradientResult, GradientValidator, MultiJudgeConsensus, OutputGradientValidator,
    SystemGradientValidator, ValidationContext, ValidationPipeline, ValidationRequest,
    ValidatorKind,
};
use crate::domain::workflow::{ConfidenceWeighting, ConsensusConfig, ConsensusStrategy};
use crate::infrastructure::llm::registry::ProviderRegistry;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::time::Duration;

use crate::application::execution::ExecutionService;
use crate::domain::execution::{ExecutionInput, ExecutionStatus};

pub struct ValidationService {
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    execution_service: Arc<dyn ExecutionService>,
}

impl ValidationService {
    pub fn new(
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
        execution_service: Arc<dyn ExecutionService>,
    ) -> Self {
        Self {
            event_bus,
            execution_service,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn validate_with_judges(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        request: ValidationRequest,
        judges: Vec<(AgentId, f64)>, // (judge_id, weight)
        config: Option<ConsensusConfig>,
        timeout_seconds: u64,
        poll_interval_ms: u64,
    ) -> Result<MultiJudgeConsensus> {
        if judges.is_empty() {
            return Err(anyhow!("No judges provided for validation"));
        }

        let config = config.unwrap_or(ConsensusConfig {
            strategy: ConsensusStrategy::WeightedAverage,
            threshold: None,
            min_agreement_confidence: None,
            n: None,
            min_judges_required: 1,
            confidence_weighting: None,
        });

        // Validate confidence weighting if provided
        if let Some(ref weighting) = config.confidence_weighting {
            weighting
                .validate()
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
                                agent_id,
                                iteration_number,
                                score: result.score,
                                confidence: result.confidence,
                                validated_at: chrono::Utc::now(),
                            },
                        ),
                    );
                    results.push((agent_id, result, weight));
                }
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
        self.event_bus
            .publish_execution_event(crate::domain::events::ExecutionEvent::Validation(
                crate::domain::events::ValidationEvent::MultiJudgeConsensus {
                    execution_id,
                    agent_id,
                    judge_scores: consensus
                        .individual_results
                        .iter()
                        .map(|(id, r)| (*id, r.score))
                        .collect(),
                    final_score: consensus.final_score,
                    confidence: consensus.consensus_confidence,
                    reached_at: chrono::Utc::now(),
                },
            ));

        Ok(consensus)
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
            intent: None, // Let ExecutionService render judge agent's prompt_template
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
                return Err(anyhow!(
                    "Judge execution timed out after {} seconds",
                    timeout_seconds
                ));
            }

            let exec = service.get_execution(exec_id).await?;
            match exec.status {
                ExecutionStatus::Completed => {
                    // 4. Parse output
                    let last_iter = exec
                        .iterations()
                        .last()
                        .ok_or_else(|| anyhow!("Judge completed but has no iterations"))?;

                    let output_str = last_iter
                        .output
                        .as_ref()
                        .ok_or_else(|| anyhow!("Judge completed but has no output"))?;

                    // Attempt to parse JSON
                    // The judge might wrap it in markdown block ```json ... ```
                    let json_str = Self::extract_json(output_str).unwrap_or(output_str.clone());

                    let result: GradientResult = serde_json::from_str(&json_str)
                        .context(format!("Failed to parse judge output: {}", json_str))?;

                    return Ok((judge_id, result));
                }
                ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                    return Err(anyhow!("Judge execution failed or cancelled"));
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(poll_interval_ms)).await;
                    attempts += 1;
                }
            }
        }
    }

    fn extract_json(text: &str) -> Option<String> {
        extract_json_from_text(text)
    }

    fn compute_consensus(
        &self,
        results: Vec<(AgentId, GradientResult, f64)>,
        config: &ConsensusConfig,
    ) -> Result<MultiJudgeConsensus> {
        if results.is_empty() {
            return Err(anyhow!("Cannot compute consensus with zero results"));
        }

        // Dispatch to appropriate strategy
        match config.strategy {
            ConsensusStrategy::WeightedAverage => {
                self.compute_weighted_average_consensus(results, config)
            }
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
        let weighted_score: f64 =
            results.iter().map(|(_, r, w)| r.score * w).sum::<f64>() / total_weight;

        // Confidence calculation with configurable weighting
        let count = results.len() as f64;
        let unweighted_mean: f64 = results.iter().map(|(_, r, _)| r.score).sum::<f64>() / count;

        // Variance = sum((x - mean)^2) / n
        let variance: f64 = results
            .iter()
            .map(|(_, r, _)| (r.score - unweighted_mean).powi(2))
            .sum::<f64>()
            / count;

        // Max variance for [0,1] is 0.25 (e.g. half 0s, half 1s)
        let disagreement_penalty = (variance / 0.25).min(1.0);
        let agreement_factor = 1.0 - disagreement_penalty;

        // Average of judges' self-confidence (weighted)
        let avg_judge_confidence: f64 = results
            .iter()
            .map(|(_, r, w)| r.confidence * w)
            .sum::<f64>()
            / total_weight;

        // Use configurable confidence weighting or defaults
        let default_weighting = ConfidenceWeighting::default();
        let weighting = config
            .confidence_weighting
            .as_ref()
            .unwrap_or(&default_weighting);
        let consensus_confidence = agreement_factor * weighting.agreement_factor
            + avg_judge_confidence * weighting.self_confidence_factor;

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
        let pass_votes: usize = results
            .iter()
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
        let avg_judge_confidence: f64 =
            results.iter().map(|(_, r, _)| r.confidence).sum::<f64>() / total;

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
        let min_confidence = results
            .iter()
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
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Take top N (or all if N > total)
        let top_n: Vec<_> = sorted_results.iter().take(n).collect();
        let count = top_n.len() as f64;

        // Average of top N scores (weighted)
        let total_weight: f64 = top_n.iter().map(|(_, _, w)| w).sum();
        let final_score = if total_weight > 0.0 {
            top_n.iter().map(|(_, r, w)| r.score * w).sum::<f64>() / total_weight
        } else {
            top_n.iter().map(|(_, r, _)| r.score).sum::<f64>() / count
        };

        // Average confidence of top N
        let consensus_confidence = top_n.iter().map(|(_, r, _)| r.confidence).sum::<f64>() / count;

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

// ── Module-level helpers ─────────────────────────────────────────────────────

/// Extract the first JSON value from `text`, stripping markdown code fences.
///
/// Looks first for a ` ```json ... ``` ` block, then a generic ` ``` ... ``` ` block.
/// Returns the trimmed interior, or `None` if neither fence is found.
pub(crate) fn extract_json_from_text(text: &str) -> Option<String> {
    let json_marker = "```json";
    if let Some(start) = text.find(json_marker) {
        let content_start = start + json_marker.len();
        if let Some(end_offset) = text[content_start..].find("```") {
            return Some(
                text[content_start..content_start + end_offset]
                    .trim()
                    .to_string(),
            );
        }
    }

    let generic_marker = "```";
    if let Some(start) = text.find(generic_marker) {
        let content_start = start + generic_marker.len();
        if let Some(end_offset) = text[content_start..].find("```") {
            return Some(
                text[content_start..content_start + end_offset]
                    .trim()
                    .to_string(),
            );
        }
    }

    None
}

// ── SemanticGradientValidator ────────────────────────────────────────────────

/// Gradient validator that uses an LLM to semantically evaluate iteration output (ADR-017).
///
/// The validator sends the iteration output to an LLM using the prompt template from
/// the agent manifest's `spec.execution.validation.semantic` section.  The LLM must
/// respond with a JSON object matching [`SemanticScoreOutput`].
///
/// If the LLM is unavailable and `config.fallback_on_unavailable == FallbackBehavior::Skip`,
/// the validator returns a passing score of `1.0` with `confidence = 0.0` so the agent
/// can still make progress.  With `FallbackBehavior::Fail` the error propagates to the
/// pipeline, which records the iteration as failed.
pub struct SemanticGradientValidator {
    config: SemanticValidation,
    provider_registry: Arc<ProviderRegistry>,
}

impl SemanticGradientValidator {
    pub fn new(config: SemanticValidation, provider_registry: Arc<ProviderRegistry>) -> Self {
        Self {
            config,
            provider_registry,
        }
    }

    fn handle_unavailable(&self, reason: String) -> anyhow::Result<GradientResult> {
        match self.config.fallback_on_unavailable {
            FallbackBehavior::Skip => Ok(GradientResult {
                score: 1.0,
                confidence: 0.0,
                reasoning: format!("Semantic validation skipped (fallback): {}", reason),
                signals: vec![],
                metadata: std::collections::HashMap::new(),
            }),
            FallbackBehavior::Fail => Err(anyhow::anyhow!("{}", reason)),
        }
    }
}

#[async_trait::async_trait]
impl GradientValidator for SemanticGradientValidator {
    async fn validate(&self, ctx: &ValidationContext) -> anyhow::Result<GradientResult> {
        let system_prompt = self.config.prompt.clone();
        let user_content = format!(
            "Task: {}\n\nAgent output:\n{}\n\nRespond with JSON only: \
            {{\"valid\": <bool>, \"score\": <0.0-1.0>, \"confidence\": <0.0-1.0>, \"reasoning\": \"<explanation>\"}}",
            ctx.task, ctx.output
        );

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: user_content,
                tool_call_id: None,
            },
        ];

        let options = GenerationOptions {
            max_tokens: Some(512),
            temperature: Some(0.0),
            stop_sequences: None,
        };

        let timeout = std::time::Duration::from_secs(self.config.timeout_seconds);
        let generate_fut =
            self.provider_registry
                .generate_chat(&self.config.model, &messages, &[], &options);

        let chat_response = match tokio::time::timeout(timeout, generate_fut).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                return self
                    .handle_unavailable(format!("LLM error during semantic validation: {e}"));
            }
            Err(_elapsed) => {
                return self.handle_unavailable(format!(
                    "Semantic validation timed out after {}s",
                    self.config.timeout_seconds
                ));
            }
        };

        let raw_text = match chat_response {
            ChatResponse::FinalText(resp) => resp.text,
            ChatResponse::ToolCalls(_) => {
                return self.handle_unavailable(
                    "Semantic validator received unexpected tool calls from LLM".to_string(),
                );
            }
        };

        let json_str = extract_json_from_text(&raw_text).unwrap_or(raw_text);
        let scored: SemanticScoreOutput = serde_json::from_str(&json_str).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse semantic validator JSON response ({}): {}",
                e,
                json_str
            )
        })?;

        let score = scored.score.clamp(0.0, 1.0);
        let confidence = scored.confidence.unwrap_or(0.8).clamp(0.0, 1.0);

        Ok(GradientResult {
            score,
            confidence,
            reasoning: scored.reasoning,
            signals: vec![],
            metadata: {
                let mut map = std::collections::HashMap::new();
                map.insert("valid".to_string(), serde_json::json!(scored.valid));
                map.insert("model".to_string(), serde_json::json!(self.config.model));
                map
            },
        })
    }
}

/// Serde target for the JSON the LLM returns during semantic validation.
#[derive(serde::Deserialize)]
struct SemanticScoreOutput {
    valid: bool,
    score: f64,
    confidence: Option<f64>,
    reasoning: String,
}

// ── Pipeline factory ─────────────────────────────────────────────────────────

/// Build a [`ValidationPipeline`] from an agent manifest's [`ValidationConfig`].
///
/// Validators are added in order: `System` → `Output` → `Semantic`.
/// The `Semantic` validator is only added when both a `SemanticValidation` config
/// **and** a `provider_registry` are provided, and `semantic.enabled == true`.
pub fn build_validation_pipeline(
    config: &ValidationConfig,
    provider_registry: Option<Arc<ProviderRegistry>>,
) -> ValidationPipeline {
    let semantic_threshold = config.semantic.as_ref().map(|s| s.threshold).unwrap_or(0.7);

    let mut validators: Vec<(ValidatorKind, Box<dyn GradientValidator>)> = Vec::new();

    if let Some(ref system_cfg) = config.system {
        validators.push((
            ValidatorKind::System,
            Box::new(SystemGradientValidator::from_config(system_cfg)),
        ));
    }

    if let Some(ref output_cfg) = config.output {
        validators.push((
            ValidatorKind::Output,
            Box::new(OutputGradientValidator::from_config(output_cfg)),
        ));
    }

    if let (Some(ref semantic_cfg), Some(registry)) = (&config.semantic, provider_registry) {
        if semantic_cfg.enabled {
            validators.push((
                ValidatorKind::Semantic,
                Box::new(SemanticGradientValidator::new(
                    semantic_cfg.clone(),
                    registry,
                )),
            ));
        }
    }

    ValidationPipeline::new(validators, semantic_threshold)
}
