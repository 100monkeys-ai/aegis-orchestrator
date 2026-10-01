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
//! Judge agents return a verdict read as a `GradientResult` with:
//! - Continuous score 0.0–1.0 (not binary pass/fail), the one required field
//! - Confidence score (0.0–1.0)
//! - Detailed reasoning
//!
//! The verdict is read by [`read_judge_verdict`], whose contract is on
//! [`GradientResult`]; an output it cannot read is a
//! [`crate::domain::validation::JudgeFault`] naming the judge, not a fault in
//! the worker's output. A judge whose verdict cannot be read is run once more
//! as a fresh child execution with the same input; each fault publishes
//! [`crate::domain::events::ValidationEvent::JudgeFault`], and a second fault is
//! returned as the error, a `JudgeFault` saying the judge faulted twice (ADR-017's
//! Update of 2026-10-01). The supervisor ends the execution on it rather than
//! handing it to the worker as feedback.
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
    extract_json_from_text, read_judge_verdict, GradientResult, GradientValidator, JudgeFault,
    MultiJudgeConsensus, OutputGradientValidator, SystemGradientValidator, ValidationContext,
    ValidationPipeline, ValidationRequest, ValidatorEntry, ValidatorKind,
};
use crate::domain::workflow::{ConfidenceWeighting, ConsensusConfig, ConsensusStrategy};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;

pub struct ValidationService {
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    execution_service: Arc<dyn ExecutionService>,
    agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
}

impl ValidationService {
    pub fn new(
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
        execution_service: Arc<dyn ExecutionService>,
        agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
    ) -> Self {
        Self {
            event_bus,
            execution_service,
            agent_lifecycle_service,
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
        tenant_id: &TenantId,
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
            let lifecycle = self.agent_lifecycle_service.clone();
            let req = request.clone();
            let judge = *judge_id;
            let w = *weight;
            let timeout = timeout_seconds;
            let poll_interval = poll_interval_ms;
            let parent_id = execution_id;
            let tenant = tenant_id.clone();
            let event_bus = self.event_bus.clone();

            futures.push(tokio::spawn(async move {
                match Self::run_judge(
                    service,
                    lifecycle,
                    judge,
                    req,
                    parent_id,
                    tenant,
                    timeout,
                    poll_interval,
                    event_bus,
                )
                .await
                {
                    Ok((_agent_id, gradient_result)) => Ok((judge, gradient_result, w)),
                    Err(e) => Err(e),
                }
            }));
        }

        let mut results = Vec::new();
        let mut first_fault: Option<JudgeFault> = None;
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
                Ok(Err(e)) => {
                    tracing::warn!("Judge execution failed: {}", e);
                    keep_first_judge_fault(&mut first_fault, &e);
                }
                Err(e) => tracing::error!("Join error: {}", e),
            }
        }

        // Check minimum judges requirement
        if results.len() < config.min_judges_required {
            return Err(insufficient_judges_error(
                format!(
                    "Insufficient judges succeeded: {} of {} required (total: {})",
                    results.len(),
                    config.min_judges_required,
                    judges.len()
                ),
                first_fault,
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

    #[allow(clippy::too_many_arguments)]
    async fn run_judge(
        service: Arc<dyn ExecutionService>,
        lifecycle: Arc<dyn AgentLifecycleService>,
        judge_id: AgentId,
        request: ValidationRequest,
        parent_execution_id: ExecutionId,
        tenant_id: TenantId,
        timeout_seconds: u64,
        poll_interval_ms: u64,
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    ) -> Result<(AgentId, GradientResult)> {
        // Fetch the judge agent manifest to read its declared input_schema.
        // Use the system tenant so global judge agents (aegis-system scope) are always found.
        let judge_agent = lifecycle
            .get_agent_visible(&crate::domain::shared_kernel::TenantId::system(), judge_id)
            .await?;
        let judge_name = judge_agent.manifest.metadata.name.clone();

        // Build the execution input by mapping ValidationRequest fields to the judge's
        // declared spec.input_schema properties.  The mapping is canonical across all
        // four built-in judge agents; any property not covered falls back to
        // request.context[property_name] if present.
        let input = if let Some(schema) = &judge_agent.manifest.spec.input_schema {
            let properties = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();

            let mut payload = serde_json::Map::new();

            for prop_name in properties.keys() {
                let value: serde_json::Value = match prop_name.as_str() {
                    // content → primary output being evaluated
                    "generated_manifest" | "output" => {
                        serde_json::Value::String(request.content.clone())
                    }
                    "generated_workflow" => serde_json::Value::String(request.content.clone()),
                    // criteria → the user objective / task goal
                    "user_objective" | "task" => {
                        serde_json::Value::String(request.criteria.clone())
                    }
                    // explicit criteria pass-through
                    "criteria" => serde_json::Value::String(request.criteria.clone()),
                    // context-sourced fields
                    "deployment_result" | "tool_call_history" | "worker_mounts" => request
                        .context
                        .as_ref()
                        .and_then(|c| c.get(prop_name.as_str()))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    // validation_context is always the judge's own name
                    "validation_context" => serde_json::Value::String(judge_name.clone()),
                    // fallback: look up in request.context by property name
                    other => request
                        .context
                        .as_ref()
                        .and_then(|c| c.get(other))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                };
                payload.insert(prop_name.clone(), value);
            }

            ExecutionInput {
                intent: None,
                input: serde_json::Value::Object(payload),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            }
        } else {
            // No input_schema declared — pass content directly as a plain string.
            ExecutionInput {
                intent: Some(request.criteria.clone()),
                input: serde_json::Value::String(request.content.clone()),
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            }
        };

        // Spawn as a child execution (ADR-016: judges are isolated peer agents),
        // once more on an unreadable verdict (ADR-017's Update of 2026-10-01).
        let result = run_judge_with_one_rerun(JudgeRun {
            service: service.as_ref(),
            tenant_id: &tenant_id,
            judge_id,
            judge_name: &judge_name,
            input,
            parent_execution_id,
            timeout_seconds,
            poll_interval_ms,
            event_bus: Some(event_bus.as_ref()),
        })
        .await?;
        Ok((judge_id, result))
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

// ── Running a judge (ADR-016) and its one re-run on a fault (ADR-017) ─────────

/// One judge to run as a child execution of the execution it validates.
struct JudgeRun<'a> {
    service: &'a dyn ExecutionService,
    tenant_id: &'a TenantId,
    judge_id: AgentId,
    judge_name: &'a str,
    input: ExecutionInput,
    parent_execution_id: ExecutionId,
    timeout_seconds: u64,
    poll_interval_ms: u64,
    /// Where each [`JudgeFault`] is published; `None` publishes nothing.
    event_bus: Option<&'a crate::infrastructure::event_bus::EventBus>,
}

/// Start the judge as a fresh child execution with `input`, wait for it, and
/// read its last output as a verdict. The outer error is the run itself failing
/// (start, poll, timeout, the judge failed or cancelled); the inner is a verdict
/// that cannot be read.
async fn run_judge_once(
    run: &JudgeRun<'_>,
    input: ExecutionInput,
) -> Result<std::result::Result<GradientResult, JudgeFault>> {
    let exec_id = run
        .service
        .start_child_execution(run.judge_id, input, run.parent_execution_id)
        .await?;

    let max_attempts = calculate_max_attempts(run.timeout_seconds, run.poll_interval_ms)?;
    let mut attempts = 0;
    loop {
        if attempts >= max_attempts {
            return Err(anyhow!(
                "Judge '{}' timed out after {} seconds",
                run.judge_name,
                run.timeout_seconds
            ));
        }
        let exec = run
            .service
            .get_execution_for_tenant(run.tenant_id, exec_id)
            .await?;
        match exec.status {
            ExecutionStatus::Completed => {
                let last_iter = exec
                    .iterations()
                    .last()
                    .ok_or_else(|| anyhow!("Judge completed but has no iterations"))?;
                let output_str = last_iter.output.as_deref().unwrap_or_default();
                return Ok(read_judge_verdict(run.judge_name, output_str));
            }
            ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                return Err(anyhow!(
                    "Judge '{}' execution failed or was cancelled",
                    run.judge_name
                ));
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(run.poll_interval_ms)).await;
                attempts += 1;
            }
        }
    }
}

/// Run a judge; when its verdict cannot be read, run it once more as a fresh
/// child execution with the same input (ADR-017's Update of 2026-10-01). Every
/// fault publishes [`crate::domain::events::ValidationEvent::JudgeFault`]. A
/// second fault is returned as the error: a [`JudgeFault`] naming the judge and
/// saying it faulted twice, which the supervisor ends the execution on.
async fn run_judge_with_one_rerun(run: JudgeRun<'_>) -> Result<GradientResult> {
    let first = match run_judge_once(&run, run.input.clone()).await? {
        Ok(verdict) => return Ok(verdict),
        Err(fault) => fault,
    };
    publish_judge_fault(&run, &first);
    tracing::warn!(
        judge_agent = %first.judge_agent,
        reason = %first.reason,
        "Judge verdict cannot be read — running the judge once more"
    );

    match run_judge_once(&run, run.input.clone()).await? {
        Ok(verdict) => Ok(verdict),
        Err(second) => {
            publish_judge_fault(&run, &second);
            Err(anyhow::Error::new(JudgeFault {
                reason: format!(
                    "it faulted twice, on two fresh runs; the first: {}; the second: {}",
                    first.reason, second.reason
                ),
                judge_agent: second.judge_agent,
                output: second.output,
            }))
        }
    }
}

fn publish_judge_fault(run: &JudgeRun<'_>, fault: &JudgeFault) {
    if let Some(event_bus) = run.event_bus {
        event_bus.publish_execution_event(crate::domain::events::ExecutionEvent::Validation(
            crate::domain::events::ValidationEvent::JudgeFault {
                execution_id: run.parent_execution_id,
                judge_agent: fault.judge_agent.clone(),
                reason: fault.reason.clone(),
                judge_output: fault.output.clone(),
                faulted_at: chrono::Utc::now(),
            },
        ));
    }
}

/// Keep the first [`JudgeFault`] among the judges that failed, so a shortfall
/// of judges that faulted is reported as the judge fault it is.
fn keep_first_judge_fault(first_fault: &mut Option<JudgeFault>, error: &anyhow::Error) {
    if first_fault.is_none() {
        *first_fault = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<JudgeFault>())
            .cloned();
    }
}

/// Too few judges succeeded. When one of them faulted, the error carries that
/// [`JudgeFault`] beneath the shortfall, so the supervisor can tell it from a
/// failed validation of the worker's output.
fn insufficient_judges_error(message: String, first_fault: Option<JudgeFault>) -> anyhow::Error {
    match first_fault {
        Some(fault) => anyhow::Error::new(fault).context(message),
        None => anyhow!(message),
    }
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
/// The judge agent is identified by name via [`AgentLifecycleService::lookup_agent_for_tenant`].
/// It is spawned via [`ExecutionService::start_child_execution`] and polled for
/// completion.  Its last output is read as a verdict by [`read_judge_verdict`].
pub struct SemanticAgentValidator {
    judge_agent_name: String,
    criteria: String,
    timeout_seconds: u64,
    poll_interval_ms: u64,
    agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
    execution_service: Arc<dyn ExecutionService>,
    event_bus: Option<Arc<crate::infrastructure::event_bus::EventBus>>,
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
            event_bus: None,
            parent_execution_id: config.parent_execution_id,
            tenant_id: config.tenant_id,
        }
    }

    /// Publish each [`JudgeFault`] of this validator's judge on `event_bus`.
    pub fn with_event_bus(
        mut self,
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    ) -> Self {
        self.event_bus = Some(event_bus);
        self
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
            .lookup_agent_visible_for_tenant(&self.tenant_id, &self.judge_agent_name)
            .await?
            .ok_or_else(|| anyhow!("Judge agent '{}' not found", self.judge_agent_name))?;

        let generation_evidence = extract_json_from_text(&ctx.output)
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
            .or_else(|| serde_json::from_str::<serde_json::Value>(&ctx.output).ok());
        // Use the live trajectory threaded through ValidationContext to avoid the
        // DB fetch race where store_iteration_trajectory may not yet be visible
        // when this validator runs.
        let tool_audit_history = ctx.tool_trajectory.clone();
        let current_iter = self
            .execution_service
            .get_execution_for_tenant(&self.tenant_id, self.parent_execution_id)
            .await
            .ok()
            .and_then(|execution| execution.current_iteration().cloned());
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
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        };

        // 3. Run the judge as a child execution, once more on an unreadable verdict.
        run_judge_with_one_rerun(JudgeRun {
            service: self.execution_service.as_ref(),
            tenant_id: &self.tenant_id,
            judge_id,
            judge_name: &self.judge_agent_name,
            input,
            parent_execution_id: self.parent_execution_id,
            timeout_seconds: self.timeout_seconds,
            poll_interval_ms: self.poll_interval_ms,
            event_bus: self.event_bus.as_deref(),
        })
        .await
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
        let mut judge_ids: Vec<(AgentId, String, f64)> = Vec::new();
        for name in &self.judges {
            let id = self
                .agent_lifecycle_service
                .lookup_agent_visible_for_tenant(&self.tenant_id, name)
                .await?
                .ok_or_else(|| anyhow!("Judge agent '{name}' not found"))?;
            judge_ids.push((id, name.clone(), 1.0)); // Equal weight by default.
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
        for (judge_id, judge_name, weight) in &judge_ids {
            let svc = self.execution_service.clone();
            let payload = input_payload.clone();
            let jid = *judge_id;
            let judge_name = judge_name.clone();
            let w = *weight;
            let parent_id = self.parent_execution_id;
            let timeout = self.timeout_seconds;
            let poll_interval = self.poll_interval_ms;
            let tenant = self.tenant_id.clone();
            let event_bus = self.event_bus.clone();

            futures.push(tokio::spawn(async move {
                let exec_input = ExecutionInput {
                    intent: None,
                    input: payload,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                };
                let result = run_judge_with_one_rerun(JudgeRun {
                    service: svc.as_ref(),
                    tenant_id: &tenant,
                    judge_id: jid,
                    judge_name: &judge_name,
                    input: exec_input,
                    parent_execution_id: parent_id,
                    timeout_seconds: timeout,
                    poll_interval_ms: poll_interval,
                    event_bus: Some(event_bus.as_ref()),
                })
                .await?;
                Ok::<(AgentId, GradientResult, f64), anyhow::Error>((jid, result, w))
            }));
        }

        // 4. Collect results.
        let mut results: Vec<(AgentId, GradientResult, f64)> = Vec::new();
        let mut first_fault: Option<JudgeFault> = None;
        for future in futures {
            match future.await {
                Ok(Ok(triple)) => results.push(triple),
                Ok(Err(e)) => {
                    tracing::warn!("MultiJudge: judge failed: {}", e);
                    keep_first_judge_fault(&mut first_fault, &e);
                }
                Err(e) => tracing::error!("MultiJudge: join error: {}", e),
            }
        }

        if results.len() < self.min_judges_required {
            return Err(insufficient_judges_error(
                format!(
                    "MultiJudge: insufficient judges succeeded: {} of {} required (total: {})",
                    results.len(),
                    self.min_judges_required,
                    self.judges.len()
                ),
                first_fault,
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
                    validator: Box::new(
                        SemanticAgentValidator::new(
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
                        )
                        .with_event_bus(event_bus.clone()),
                    ),
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
