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
//! - **Integration:** Execution Service → Child Judge Executions → Consensus Calculation
//!
//! # Validation Patterns
//!
//! ## Multi-Judge Consensus (ADR-016, ADR-017)
//!
//! Execute multiple judge agents as **child executions** (isolated peer agents, not
//! direct LLM calls) in parallel and aggregate their verdicts using configurable
//! consensus strategies:
//!
//! - **Weighted Average**: Combine scores using judge-specific weights
//! - **Majority Vote**: Simple majority with optional threshold
//! - **Top-N**: Select best N judges and average their scores
//! - **Unanimous**: Require all judges to agree
//!
//! ## Gradient Evaluation
//!
//! Judge agents return structured `GradientResult` with:
//! - Continuous score 0.0–1.0 (not binary pass/fail)
//! - Confidence score (0.0–1.0)
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

use crate::application::agent::AgentLifecycleService;
use crate::application::execution::ExecutionService;
use crate::domain::agent::{AgentId, ValidatorSpec};
use crate::domain::execution::{ExecutionId, ExecutionInput, ExecutionStatus};
use crate::domain::shared_kernel::TenantId;
use crate::domain::validation::{
    extract_json_from_text, GradientResult, GradientValidator, MultiJudgeConsensus,
    OutputGradientValidator, SystemGradientValidator, ValidationContext, ValidationPipeline,
    ValidationRequest, ValidatorEntry, ValidatorKind,
};
use crate::domain::workflow::{ConfidenceWeighting, ConsensusConfig, ConsensusStrategy};
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::time::Duration;

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
                .map_err(|e| anyhow!("Invalid confidence weighting: {e}"))?;
        }

        let mut futures = Vec::new();
        for (judge_id, weight) in &judges {
            let service = self.execution_service.clone();
            let req = request.clone();
            let judge = *judge_id;
            let w = *weight;
            let timeout = timeout_seconds;
            let poll_interval = poll_interval_ms;
            let parent_id = execution_id;

            futures.push(tokio::spawn(async move {
                match Self::run_judge(service, judge, req, parent_id, timeout, poll_interval).await
                {
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
        parent_execution_id: ExecutionId,
        timeout_seconds: u64,
        poll_interval_ms: u64,
    ) -> Result<(AgentId, GradientResult)> {
        // Prepare input — the judge agent's prompt_template renders against this payload.
        let payload_data = serde_json::to_string(&request)?;
        let input = ExecutionInput {
            intent: None,
            input: serde_json::json!({
                "workflow_input": payload_data,
                "validation_context": "judge_execution"
            }),
        };

        // Spawn as a child execution (ADR-016: judges are isolated peer agents).
        let exec_id = service
            .start_child_execution(judge_id, input, parent_execution_id)
            .await?;

        // Poll for completion with configurable timeout and interval.
        let max_attempts = calculate_max_attempts(timeout_seconds, poll_interval_ms)?;
        let mut attempts = 0;

        loop {
            if attempts >= max_attempts {
                return Err(anyhow!(
                    "Judge execution timed out after {timeout_seconds} seconds"
                ));
            }

            let exec = service.get_execution(exec_id).await?;
            match exec.status {
                ExecutionStatus::Completed => {
                    let last_iter = exec
                        .iterations()
                        .last()
                        .ok_or_else(|| anyhow!("Judge completed but has no iterations"))?;

                    let output_str = last_iter
                        .output
                        .as_ref()
                        .ok_or_else(|| anyhow!("Judge completed but has no output"))?;

                    let json_str = Self::extract_json(output_str).unwrap_or(output_str.clone());

                    let result: GradientResult = serde_json::from_str(&json_str)
                        .context(format!("Failed to parse judge output: {json_str}"))?;

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
        crate::domain::validation::extract_json_from_text(text)
    }

    fn compute_consensus(
        &self,
        results: Vec<(AgentId, GradientResult, f64)>,
        config: &ConsensusConfig,
    ) -> Result<MultiJudgeConsensus> {
        compute_consensus_for_strategy(results, config)
    }
}

fn calculate_max_attempts(timeout_seconds: u64, poll_interval_ms: u64) -> Result<u64> {
    if poll_interval_ms == 0 {
        return Err(anyhow!("poll_interval_ms must be greater than 0"));
    }

    let timeout_ms = timeout_seconds.saturating_mul(1000);
    Ok(timeout_ms.saturating_add(poll_interval_ms - 1) / poll_interval_ms)
}

// ── Consensus helpers (free functions so validators can reuse them) ───────────

fn compute_consensus_for_strategy(
    results: Vec<(AgentId, GradientResult, f64)>,
    config: &ConsensusConfig,
) -> Result<MultiJudgeConsensus> {
    if results.is_empty() {
        return Err(anyhow!("Cannot compute consensus with zero results"));
    }
    match config.strategy {
        ConsensusStrategy::WeightedAverage => compute_weighted_average(results, config),
        ConsensusStrategy::Majority => compute_majority(results, config),
        ConsensusStrategy::Unanimous => compute_unanimous(results, config),
        ConsensusStrategy::BestOfN => compute_best_of_n(results, config),
    }
}

fn compute_weighted_average(
    results: Vec<(AgentId, GradientResult, f64)>,
    config: &ConsensusConfig,
) -> Result<MultiJudgeConsensus> {
    let total_weight: f64 = results.iter().map(|(_, _, w)| w).sum();
    if total_weight == 0.0 {
        return Err(anyhow!("Total weight is zero"));
    }
    let weighted_score: f64 =
        results.iter().map(|(_, r, w)| r.score * w).sum::<f64>() / total_weight;
    let count = results.len() as f64;
    let unweighted_mean: f64 = results.iter().map(|(_, r, _)| r.score).sum::<f64>() / count;
    let variance: f64 = results
        .iter()
        .map(|(_, r, _)| (r.score - unweighted_mean).powi(2))
        .sum::<f64>()
        / count;
    let disagreement_penalty = (variance / 0.25).min(1.0);
    let agreement_factor = 1.0 - disagreement_penalty;
    let avg_judge_confidence: f64 = results
        .iter()
        .map(|(_, r, w)| r.confidence * w)
        .sum::<f64>()
        / total_weight;
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

fn compute_majority(
    results: Vec<(AgentId, GradientResult, f64)>,
    config: &ConsensusConfig,
) -> Result<MultiJudgeConsensus> {
    let threshold = config.threshold.unwrap_or(0.7);
    let pass_votes: usize = results
        .iter()
        .filter(|(_, r, _)| r.score >= threshold)
        .count();
    let fail_votes = results.len() - pass_votes;
    let final_score = if pass_votes > fail_votes {
        1.0
    } else if fail_votes > pass_votes {
        0.0
    } else {
        0.5
    };
    let total = results.len() as f64;
    let margin = ((pass_votes as f64 - fail_votes as f64).abs() / total).min(1.0);
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

fn compute_unanimous(
    results: Vec<(AgentId, GradientResult, f64)>,
    config: &ConsensusConfig,
) -> Result<MultiJudgeConsensus> {
    let threshold = config.threshold.unwrap_or(0.7);
    let all_pass = results.iter().all(|(_, r, _)| r.score >= threshold);
    let final_score = if all_pass {
        let count = results.len() as f64;
        results.iter().map(|(_, r, _)| r.score).sum::<f64>() / count
    } else {
        0.0
    };
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

fn compute_best_of_n(
    results: Vec<(AgentId, GradientResult, f64)>,
    config: &ConsensusConfig,
) -> Result<MultiJudgeConsensus> {
    let n = config.n.unwrap_or(results.len());
    if n == 0 {
        return Err(anyhow!("BestOfN requires n > 0"));
    }
    let mut sorted_results = results.clone();
    sorted_results.sort_by(|(_, a, _), (_, b, _)| {
        let score_a = a.score * a.confidence;
        let score_b = b.score * b.confidence;
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_n: Vec<_> = sorted_results.iter().take(n).collect();
    let count = top_n.len() as f64;
    let total_weight: f64 = top_n.iter().map(|(_, _, w)| w).sum();
    let final_score = if total_weight > 0.0 {
        top_n.iter().map(|(_, r, w)| r.score * w).sum::<f64>() / total_weight
    } else {
        top_n.iter().map(|(_, r, _)| r.score).sum::<f64>() / count
    };
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

// ── SemanticAgentValidator ────────────────────────────────────────────────────

/// Configuration for [`SemanticAgentValidator`].
pub struct SemanticAgentValidatorConfig {
    pub judge_agent_name: String,
    pub criteria: String,
    pub timeout_seconds: u64,
    pub poll_interval_ms: u64,
    pub parent_execution_id: ExecutionId,
    pub tenant_id: TenantId,
}

/// Gradient validator that runs a **judge agent** as a child execution (ADR-016) to
/// semantically evaluate iteration output (ADR-017).
///
/// The judge agent is identified by name via [`AgentLifecycleService::lookup_agent`].
/// It is spawned via [`ExecutionService::start_child_execution`] and polled for
/// completion.  Its stdout must be a JSON [`GradientResult`].
pub struct SemanticAgentValidator {
    judge_agent_name: String,
    criteria: String,
    timeout_seconds: u64,
    poll_interval_ms: u64,
    agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
    execution_service: Arc<dyn ExecutionService>,
    parent_execution_id: ExecutionId,
    tenant_id: TenantId,
}

impl SemanticAgentValidator {
    pub fn new(
        config: SemanticAgentValidatorConfig,
        agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
        execution_service: Arc<dyn ExecutionService>,
    ) -> Self {
        Self {
            judge_agent_name: config.judge_agent_name,
            criteria: config.criteria,
            timeout_seconds: config.timeout_seconds,
            poll_interval_ms: config.poll_interval_ms,
            agent_lifecycle_service,
            execution_service,
            parent_execution_id: config.parent_execution_id,
            tenant_id: config.tenant_id,
        }
    }
}

#[async_trait::async_trait]
impl GradientValidator for SemanticAgentValidator {
    async fn validate(&self, ctx: &ValidationContext) -> Result<GradientResult> {
        // 1. Resolve judge agent id by name — use visible (cross-tenant) lookup so
        //    aegis-system scoped judges (e.g. agent-generator-judge) are found even
        //    when the caller's tenant is not aegis-system.
        let judge_id = self
            .agent_lifecycle_service
            .lookup_agent_visible_for_tenant(&self.tenant_id, None, &self.judge_agent_name)
            .await?
            .ok_or_else(|| anyhow!("Judge agent '{}' not found", self.judge_agent_name))?;

        let generation_evidence = extract_json_from_text(&ctx.output)
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
            .or_else(|| serde_json::from_str::<serde_json::Value>(&ctx.output).ok());
        let current_iter = self
            .execution_service
            .get_execution(self.parent_execution_id)
            .await
            .ok()
            .and_then(|execution| execution.current_iteration().cloned());
        let tool_audit_history = current_iter
            .as_ref()
            .and_then(|iter| iter.trajectory.clone())
            .unwrap_or_default();
        let mut policy_violations: Vec<String> = ctx.policy_violations.clone();
        if let Some(iter) = &current_iter {
            for v in &iter.policy_violations {
                if !policy_violations.contains(v) {
                    policy_violations.push(v.clone());
                }
            }
        }

        // 2. Build input for judge.
        // Parse ctx.output as JSON so the judge receives a proper JSON value
        // rather than a double-encoded string when the agent emits valid JSON.
        let output_value: serde_json::Value = serde_json::from_str(&ctx.output)
            .unwrap_or_else(|_| serde_json::Value::String(ctx.output.clone()));
        let input = ExecutionInput {
            intent: None,
            input: serde_json::json!({
                "task": ctx.task,
                "output": output_value,
                "generation_evidence": generation_evidence,
                "tool_audit_history": tool_audit_history,
                "worker_mounts": ctx.worker_mounts.clone(),
                "criteria": self.criteria,
                "policy_violations": policy_violations,
                "validation_context": "semantic_judge"
            }),
        };

        // 3. Start child execution.
        let exec_id = self
            .execution_service
            .start_child_execution(judge_id, input, self.parent_execution_id)
            .await?;

        // 4. Poll for completion.
        let max_attempts = calculate_max_attempts(self.timeout_seconds, self.poll_interval_ms)?;
        let mut attempts = 0;
        loop {
            if attempts >= max_attempts {
                return Err(anyhow!(
                    "Semantic judge '{}' timed out after {} seconds",
                    self.judge_agent_name,
                    self.timeout_seconds
                ));
            }
            let exec = self.execution_service.get_execution(exec_id).await?;
            match exec.status {
                ExecutionStatus::Completed => {
                    let last_iter = exec
                        .iterations()
                        .last()
                        .ok_or_else(|| anyhow!("Judge completed but has no iterations"))?;
                    let output_str = last_iter
                        .output
                        .as_ref()
                        .ok_or_else(|| anyhow!("Judge completed but has no output"))?;
                    let json_str =
                        extract_json_from_text(output_str).unwrap_or_else(|| output_str.clone());
                    let result: GradientResult = serde_json::from_str(&json_str)
                        .context(format!("Failed to parse semantic judge output: {json_str}"))?;
                    return Ok(result);
                }
                ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                    return Err(anyhow!(
                        "Semantic judge '{}' execution failed or was cancelled",
                        self.judge_agent_name
                    ));
                }
                _ => {
                    tokio::time::sleep(Duration::from_millis(self.poll_interval_ms)).await;
                    attempts += 1;
                }
            }
        }
    }
}

// ── MultiJudgeAgentValidator ──────────────────────────────────────────────────

/// Configuration for [`MultiJudgeAgentValidator`].
pub struct MultiJudgeAgentValidatorConfig {
    pub judges: Vec<String>,
    pub consensus_config: ConsensusConfig,
    pub min_judges_required: usize,
    pub criteria: String,
    pub timeout_seconds: u64,
    pub poll_interval_ms: u64,
    pub parent_execution_id: ExecutionId,
    pub tenant_id: TenantId,
}

/// Gradient validator that runs **multiple judge agents** as parallel child executions
/// (ADR-016) and aggregates their [`GradientResult`]s via a [`ConsensusConfig`] (ADR-017).
///
/// The final [`GradientResult`] has the consensus `score` and `confidence`, with the
/// full [`MultiJudgeConsensus`] packed into `metadata["consensus"]` so the pipeline
/// can store it on the iteration for later audit.
pub struct MultiJudgeAgentValidator {
    judges: Vec<String>,
    consensus_config: ConsensusConfig,
    min_judges_required: usize,
    criteria: String,
    timeout_seconds: u64,
    poll_interval_ms: u64,
    agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
    execution_service: Arc<dyn ExecutionService>,
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    parent_execution_id: ExecutionId,
    tenant_id: TenantId,
}

impl MultiJudgeAgentValidator {
    pub fn new(
        config: MultiJudgeAgentValidatorConfig,
        agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
        execution_service: Arc<dyn ExecutionService>,
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    ) -> Self {
        Self {
            judges: config.judges,
            consensus_config: config.consensus_config,
            min_judges_required: config.min_judges_required,
            criteria: config.criteria,
            timeout_seconds: config.timeout_seconds,
            poll_interval_ms: config.poll_interval_ms,
            agent_lifecycle_service,
            execution_service,
            event_bus,
            parent_execution_id: config.parent_execution_id,
            tenant_id: config.tenant_id,
        }
    }
}

#[async_trait::async_trait]
impl GradientValidator for MultiJudgeAgentValidator {
    async fn validate(&self, ctx: &ValidationContext) -> Result<GradientResult> {
        if self.judges.is_empty() {
            return Err(anyhow!("MultiJudge validator has no judges configured"));
        }

        // 1. Resolve all judge agent ids — use visible (cross-tenant) lookup so
        //    aegis-system scoped judges are found even when the caller's tenant
        //    is not aegis-system.
        let mut judge_ids: Vec<(AgentId, f64)> = Vec::new();
        for name in &self.judges {
            let id = self
                .agent_lifecycle_service
                .lookup_agent_visible_for_tenant(&self.tenant_id, None, name)
                .await?
                .ok_or_else(|| anyhow!("Judge agent '{name}' not found"))?;
            judge_ids.push((id, 1.0)); // Equal weight by default.
        }

        // 2. Build shared input.
        let input_payload = serde_json::json!({
            "task": ctx.task,
            "output": ctx.output,
            "worker_mounts": ctx.worker_mounts.clone(),
            "criteria": self.criteria,
            "validation_context": "multi_judge"
        });

        // 3. Spawn all judges as parallel child executions.
        let mut futures = Vec::new();
        for (judge_id, weight) in &judge_ids {
            let svc = self.execution_service.clone();
            let payload = input_payload.clone();
            let jid = *judge_id;
            let w = *weight;
            let parent_id = self.parent_execution_id;
            let timeout = self.timeout_seconds;
            let poll_interval = self.poll_interval_ms;

            futures.push(tokio::spawn(async move {
                let exec_input = ExecutionInput {
                    intent: None,
                    input: payload,
                };
                let exec_id = svc
                    .start_child_execution(jid, exec_input, parent_id)
                    .await?;
                // Poll for completion.
                let max_attempts = calculate_max_attempts(timeout, poll_interval)?;
                let mut attempts = 0;
                loop {
                    if attempts >= max_attempts {
                        return Err::<(AgentId, GradientResult, f64), _>(anyhow!(
                            "Judge timed out after {timeout} seconds"
                        ));
                    }
                    let exec = svc.get_execution(exec_id).await?;
                    match exec.status {
                        ExecutionStatus::Completed => {
                            let last_iter = exec
                                .iterations()
                                .last()
                                .ok_or_else(|| anyhow!("Judge completed but has no iterations"))?;
                            let output_str = last_iter
                                .output
                                .as_ref()
                                .ok_or_else(|| anyhow!("Judge completed but has no output"))?;
                            let json_str =
                                crate::domain::validation::extract_json_from_text(output_str)
                                    .unwrap_or_else(|| output_str.clone());
                            let result: GradientResult = serde_json::from_str(&json_str)
                                .context(format!("Failed to parse judge output: {json_str}"))?;
                            return Ok((jid, result, w));
                        }
                        ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                            return Err(anyhow!("Judge execution failed or was cancelled"));
                        }
                        _ => {
                            tokio::time::sleep(Duration::from_millis(poll_interval)).await;
                            attempts += 1;
                        }
                    }
                }
            }));
        }

        // 4. Collect results.
        let mut results: Vec<(AgentId, GradientResult, f64)> = Vec::new();
        for future in futures {
            match future.await {
                Ok(Ok(triple)) => results.push(triple),
                Ok(Err(e)) => tracing::warn!("MultiJudge: judge failed: {}", e),
                Err(e) => tracing::error!("MultiJudge: join error: {}", e),
            }
        }

        if results.len() < self.min_judges_required {
            return Err(anyhow!(
                "MultiJudge: insufficient judges succeeded: {} of {} required (total: {})",
                results.len(),
                self.min_judges_required,
                self.judges.len()
            ));
        }

        // 5. Compute consensus.
        let consensus = compute_consensus_for_strategy(results.clone(), &self.consensus_config)?;

        // 6. Publish consensus event.
        self.event_bus
            .publish_execution_event(crate::domain::events::ExecutionEvent::Validation(
                crate::domain::events::ValidationEvent::MultiJudgeConsensus {
                    execution_id: self.parent_execution_id,
                    agent_id: results.first().map(|(id, _, _)| *id).unwrap_or_default(),
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

        // 7. Pack full consensus into result metadata for the pipeline to store.
        let consensus_value = serde_json::to_value(&consensus)?;
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("consensus".to_string(), consensus_value);

        Ok(GradientResult {
            score: consensus.final_score,
            confidence: consensus.consensus_confidence,
            reasoning: format!(
                "MultiJudge consensus via {} strategy ({} judges)",
                consensus.strategy,
                consensus.individual_results.len()
            ),
            signals: vec![],
            metadata,
        })
    }
}

// ── Pipeline factory ──────────────────────────────────────────────────────────

/// Build a [`ValidationPipeline`] from an agent manifest's `validation` list
/// ([`ValidatorSpec`]).
///
/// Each spec produces one [`ValidatorEntry`] with its own `min_score` /
/// `min_confidence`.  `Semantic` and `MultiJudge` entries spawn judge agents as
/// child executions (ADR-016); no LLM is called directly from the orchestrator host.
pub fn build_validation_pipeline(
    validators: &[ValidatorSpec],
    agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
    execution_service: Arc<dyn ExecutionService>,
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    parent_execution_id: ExecutionId,
    tenant_id: TenantId,
) -> ValidationPipeline {
    let mut entries: Vec<ValidatorEntry> = Vec::new();

    for spec in validators {
        match spec {
            ValidatorSpec::ExitCode {
                expected: _,
                min_score,
            } => {
                entries.push(ValidatorEntry {
                    kind: ValidatorKind::System,
                    validator: Box::new(SystemGradientValidator::new(true, false)),
                    min_score: *min_score,
                    min_confidence: 0.0,
                });
            }
            ValidatorSpec::JsonSchema { schema, min_score } => {
                entries.push(ValidatorEntry {
                    kind: ValidatorKind::Output,
                    validator: Box::new(OutputGradientValidator::new(
                        "json".to_string(),
                        Some(schema.clone()),
                        None,
                    )),
                    min_score: *min_score,
                    min_confidence: 0.0,
                });
            }
            ValidatorSpec::Regex {
                pattern,
                target,
                min_score,
            } => {
                entries.push(ValidatorEntry {
                    kind: ValidatorKind::Output,
                    validator: Box::new(OutputGradientValidator::new(
                        target.clone(),
                        None,
                        Some(pattern.clone()),
                    )),
                    min_score: *min_score,
                    min_confidence: 0.0,
                });
            }
            ValidatorSpec::Semantic {
                judge_agent,
                criteria,
                min_score,
                min_confidence,
                timeout_seconds,
            } => {
                entries.push(ValidatorEntry {
                    kind: ValidatorKind::Semantic,
                    validator: Box::new(SemanticAgentValidator::new(
                        SemanticAgentValidatorConfig {
                            judge_agent_name: judge_agent.clone(),
                            criteria: criteria.clone(),
                            timeout_seconds: *timeout_seconds,
                            poll_interval_ms: 500,
                            parent_execution_id,
                            tenant_id: tenant_id.clone(),
                        },
                        agent_lifecycle_service.clone(),
                        execution_service.clone(),
                    )),
                    min_score: *min_score,
                    min_confidence: *min_confidence,
                });
            }
            ValidatorSpec::MultiJudge {
                judges,
                consensus,
                min_judges_required,
                criteria,
                min_score,
                min_confidence,
                timeout_seconds,
            } => {
                let consensus_config = ConsensusConfig {
                    strategy: *consensus,
                    threshold: None,
                    min_agreement_confidence: None,
                    n: None,
                    min_judges_required: *min_judges_required,
                    confidence_weighting: None,
                };
                entries.push(ValidatorEntry {
                    kind: ValidatorKind::MultiJudge,
                    validator: Box::new(MultiJudgeAgentValidator::new(
                        MultiJudgeAgentValidatorConfig {
                            judges: judges.clone(),
                            consensus_config,
                            min_judges_required: *min_judges_required,
                            criteria: criteria.clone(),
                            timeout_seconds: *timeout_seconds,
                            poll_interval_ms: 500,
                            parent_execution_id,
                            tenant_id: tenant_id.clone(),
                        },
                        agent_lifecycle_service.clone(),
                        execution_service.clone(),
                        event_bus.clone(),
                    )),
                    min_score: *min_score,
                    min_confidence: *min_confidence,
                });
            }
        }
    }

    ValidationPipeline::new(entries)
}

#[cfg(test)]
mod tests {
    use super::calculate_max_attempts;

    #[test]
    fn calculate_max_attempts_rejects_zero_poll_interval() {
        let err = calculate_max_attempts(30, 0).expect_err("zero interval should fail");
        assert!(err
            .to_string()
            .contains("poll_interval_ms must be greater than 0"));
    }

    #[test]
    fn calculate_max_attempts_rounds_up() {
        assert_eq!(calculate_max_attempts(1, 1000).unwrap(), 1);
        assert_eq!(calculate_max_attempts(1, 600).unwrap(), 2);
        assert_eq!(calculate_max_attempts(5, 2000).unwrap(), 3);
    }
}
