// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Temporal Workflow Mapper
//!
//! Anti-Corruption Layer that translates AEGIS Workflow domain objects
//! to Temporal workflow definitions (JSON format for TypeScript worker).
//!
//! # Architectural Role
//!
//! This mapper sits in the **Application Layer** and serves as the boundary between:
//! - **Domain Layer**: Pure business logic (Workflow aggregate, validation)
//! - **Infrastructure Layer**: Temporal execution engine (TypeScript worker)
//!
//! # DDD Pattern: Anti-Corruption Layer
//!
//! The domain model has NO knowledge of Temporal. This mapper ensures that:
//! 1. Domain invariants are preserved
//! 2. Temporal-specific concepts don't leak into domain
//! 3. Infrastructure can be swapped without touching domain
//!
//! # Related ADR
//!
//! - ADR-022: Temporal Workflow Engine Integration
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Implements internal responsibilities for temporal mapper

use crate::domain::tenant::TenantId;
use crate::domain::workflow::*;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DEFAULT_WORKFLOW_VERSION: &str = "1.0.0";

// ============================================================================
// Temporal Workflow Definition (Target Format)
// ============================================================================

/// Temporal Workflow Definition in JSON format
///
/// This structure is sent to the TypeScript Temporal Worker via HTTP API.
/// The worker dynamically generates TypeScript workflow functions from this definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalWorkflowDefinition {
    pub workflow_id: String,
    pub tenant_id: String,
    pub name: String,
    pub version: String,
    pub initial_state: String,
    pub context: HashMap<String, serde_json::Value>,
    pub states: HashMap<String, TemporalWorkflowState>,
    /// Full storage configuration forwarded to the TypeScript worker.
    /// Includes workspace provisioning and shared volume declarations (WORKFLOW_MANIFEST_SPEC_V1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec_storage: Option<crate::domain::workflow::WorkflowStorageSpec>,
    /// Visibility scope for this workflow definition (ADR-076).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Individual state in Temporal workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalWorkflowState {
    pub kind: String, // "Agent" | "System" | "Human" | "ParallelAgents"

    // Agent-specific fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,

    // Agent inner-loop validation (ADR-016 / ADR-017)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judges: Option<Vec<TemporalJudgeConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_execution_validator: Option<String>,

    // System-specific fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,

    // Human-specific fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_response: Option<String>,

    // ParallelAgents-specific fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<TemporalParallelAgentConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consensus: Option<TemporalConsensusConfig>,
    /// External judge agents for ParallelAgents consensus validation (ADR-016)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judges_for_parallel: Option<Vec<TemporalJudgeConfig>>,

    // ContainerRun-specific fields (ADR-050)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_image_pull_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_workdir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_volumes: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_resources: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_registry_credentials: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_retry: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_run_shell: Option<bool>,

    // ParallelContainerRun-specific fields (ADR-050)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_container_steps: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_container_completion: Option<String>,

    // ── Subworkflow-specific (ADR-065) ──────────────────────────────
    /// Child workflow identifier (name or UUID)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subworkflow_id: Option<String>,

    /// Execution mode: "blocking" or "fire_and_forget"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subworkflow_mode: Option<String>,

    /// Blackboard key for child result (blocking mode only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subworkflow_result_key: Option<String>,

    /// Optional Handlebars input template for the child workflow
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subworkflow_input: Option<String>,

    // Output handler (ADR-103)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_handler: Option<serde_json::Value>,

    // Transitions
    pub transitions: Vec<TemporalTransitionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalJudgeConfig {
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_template: Option<String>,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalParallelAgentConfig {
    pub agent: String,
    pub input: String,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalConsensusConfig {
    pub strategy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agreement: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemporalTransitionRule {
    pub condition: String,
    pub target: Option<String>, // null for terminal states
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

// ============================================================================
// Temporal Workflow Mapper
// ============================================================================

pub struct TemporalWorkflowMapper;

impl TemporalWorkflowMapper {
    /// Convert AEGIS Workflow domain object to Temporal workflow definition
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let workflow = Workflow::new(metadata, spec)?;
    /// let temporal_def = TemporalWorkflowMapper::to_temporal_definition(
    ///     &workflow,
    ///     &TenantId::consumer(),
    /// )?;
    /// ```
    pub fn to_temporal_definition(
        workflow: &Workflow,
        tenant_id: &TenantId,
    ) -> Result<TemporalWorkflowDefinition> {
        // Map states
        let mut temporal_states = HashMap::new();
        for (state_name, state) in &workflow.spec.states {
            let temporal_state = Self::map_workflow_state(state)?;
            temporal_states.insert(state_name.as_str().to_string(), temporal_state);
        }

        Ok(TemporalWorkflowDefinition {
            workflow_id: workflow.id.to_string(),
            tenant_id: tenant_id.as_str().to_string(),
            name: workflow.metadata.name.clone(),
            version: workflow
                .metadata
                .version
                .clone()
                .unwrap_or_else(|| DEFAULT_WORKFLOW_VERSION.to_string()),
            initial_state: workflow.spec.initial_state.as_str().to_string(),
            context: workflow.spec.context.clone(),
            states: temporal_states,
            spec_storage: if workflow.spec.storage.workspace.is_some()
                || !workflow.spec.storage.shared_volumes.is_empty()
            {
                Some(workflow.spec.storage.clone())
            } else {
                None
            },
            scope: Some(match &workflow.scope {
                WorkflowScope::Global => "global".to_string(),
                WorkflowScope::Tenant => "tenant".to_string(),
            }),
        })
    }

    /// Map WorkflowState to TemporalWorkflowState
    fn map_workflow_state(state: &WorkflowState) -> Result<TemporalWorkflowState> {
        // Map transitions — common to all state kinds
        let transitions = state
            .transitions
            .iter()
            .map(Self::map_transition_rule)
            .collect::<Result<Vec<_>>>()?;

        let timeout = state.timeout.map(|d| format!("{}s", d.as_secs()));

        match &state.kind {
            StateKind::Agent {
                agent,
                input,
                intent,
                isolation,
                judges,
                max_iterations,
                pre_execution_validator,
                output_handler,
            } => {
                let mapped_judges = if judges.is_empty() {
                    None
                } else {
                    Some(
                        judges
                            .iter()
                            .map(|j| TemporalJudgeConfig {
                                agent_id: j.agent_id.clone(),
                                input_template: j.input_template.clone(),
                                weight: j.weight,
                            })
                            .collect(),
                    )
                };

                Ok(TemporalWorkflowState {
                    kind: "Agent".to_string(),
                    agent: Some(agent.clone()),
                    input: Some(input.clone()),
                    intent: intent.clone(),
                    isolation: isolation.map(Self::map_isolation_mode),
                    timeout,
                    judges: mapped_judges,
                    max_iterations: *max_iterations,
                    pre_execution_validator: pre_execution_validator.clone(),
                    command: None,
                    env: None,
                    workdir: None,
                    prompt: None,
                    default_response: None,
                    agents: None,
                    consensus: None,
                    judges_for_parallel: None,
                    container_run_name: None,
                    container_run_image: None,
                    container_run_image_pull_policy: None,
                    container_run_command: None,
                    container_run_env: None,
                    container_run_workdir: None,
                    container_run_volumes: None,
                    container_run_resources: None,
                    container_run_registry_credentials: None,
                    container_run_retry: None,
                    container_run_shell: None,
                    parallel_container_steps: None,
                    parallel_container_completion: None,
                    subworkflow_id: None,
                    subworkflow_mode: None,
                    subworkflow_result_key: None,
                    subworkflow_input: None,
                    output_handler: output_handler
                        .as_ref()
                        .map(|h| serde_json::to_value(h).unwrap_or(serde_json::Value::Null)),
                    transitions,
                })
            }

            StateKind::System {
                command,
                env,
                workdir,
            } => Ok(TemporalWorkflowState {
                kind: "System".to_string(),
                agent: None,
                input: None,
                intent: None,
                isolation: None,
                timeout,
                judges: None,
                max_iterations: None,
                pre_execution_validator: None,
                command: Some(command.clone()),
                env: Some(env.clone()),
                workdir: workdir.clone(),
                prompt: None,
                default_response: None,
                agents: None,
                consensus: None,
                judges_for_parallel: None,
                container_run_name: None,
                container_run_image: None,
                container_run_image_pull_policy: None,
                container_run_command: None,
                container_run_env: None,
                container_run_workdir: None,
                container_run_volumes: None,
                container_run_resources: None,
                container_run_registry_credentials: None,
                container_run_retry: None,
                container_run_shell: None,
                parallel_container_steps: None,
                parallel_container_completion: None,
                subworkflow_id: None,
                subworkflow_mode: None,
                subworkflow_result_key: None,
                subworkflow_input: None,
                output_handler: None,
                transitions,
            }),

            StateKind::Human {
                prompt,
                default_response,
            } => Ok(TemporalWorkflowState {
                kind: "Human".to_string(),
                agent: None,
                input: None,
                intent: None,
                isolation: None,
                timeout,
                judges: None,
                max_iterations: None,
                pre_execution_validator: None,
                command: None,
                env: None,
                workdir: None,
                prompt: Some(prompt.clone()),
                default_response: default_response.clone(),
                agents: None,
                consensus: None,
                judges_for_parallel: None,
                container_run_name: None,
                container_run_image: None,
                container_run_image_pull_policy: None,
                container_run_command: None,
                container_run_env: None,
                container_run_workdir: None,
                container_run_volumes: None,
                container_run_resources: None,
                container_run_registry_credentials: None,
                container_run_retry: None,
                container_run_shell: None,
                parallel_container_steps: None,
                parallel_container_completion: None,
                subworkflow_id: None,
                subworkflow_mode: None,
                subworkflow_result_key: None,
                subworkflow_input: None,
                output_handler: None,
                transitions,
            }),

            StateKind::ParallelAgents {
                agents,
                consensus,
                judges_for_parallel,
                output_handler,
            } => {
                let temporal_agents = agents
                    .iter()
                    .map(|a| TemporalParallelAgentConfig {
                        agent: a.agent.clone(),
                        input: a.input.clone(),
                        weight: a.weight,
                    })
                    .collect();

                let temporal_consensus = TemporalConsensusConfig {
                    strategy: Self::map_consensus_strategy(consensus.strategy),
                    threshold: consensus.threshold,
                    agreement: consensus.min_agreement_confidence,
                    n: consensus.n,
                };

                let mapped_judges_for_parallel = if judges_for_parallel.is_empty() {
                    None
                } else {
                    Some(
                        judges_for_parallel
                            .iter()
                            .map(|j| TemporalJudgeConfig {
                                agent_id: j.agent_id.clone(),
                                input_template: j.input_template.clone(),
                                weight: j.weight,
                            })
                            .collect(),
                    )
                };

                Ok(TemporalWorkflowState {
                    kind: "ParallelAgents".to_string(),
                    agent: None,
                    input: None,
                    isolation: None,
                    timeout,
                    judges: None,
                    max_iterations: None,
                    pre_execution_validator: None,
                    command: None,
                    env: None,
                    workdir: None,
                    prompt: None,
                    default_response: None,
                    agents: Some(temporal_agents),
                    consensus: Some(temporal_consensus),
                    judges_for_parallel: mapped_judges_for_parallel,
                    container_run_name: None,
                    container_run_image: None,
                    container_run_image_pull_policy: None,
                    container_run_command: None,
                    container_run_env: None,
                    container_run_workdir: None,
                    container_run_volumes: None,
                    container_run_resources: None,
                    container_run_registry_credentials: None,
                    container_run_retry: None,
                    container_run_shell: None,
                    parallel_container_steps: None,
                    parallel_container_completion: None,
                    subworkflow_id: None,
                    subworkflow_mode: None,
                    subworkflow_result_key: None,
                    subworkflow_input: None,
                    output_handler: output_handler
                        .as_ref()
                        .map(|h| serde_json::to_value(h).unwrap_or(serde_json::Value::Null)),
                    transitions,
                })
            }

            StateKind::ContainerRun {
                name,
                image,
                image_pull_policy,
                command,
                env,
                workdir,
                volumes,
                resources,
                registry_credentials,
                retry,
                shell,
                output_handler,
            } => {
                let pull_policy_str = image_pull_policy.as_ref().map(|p| match p {
                    crate::domain::agent::ImagePullPolicy::Always => "always".to_string(),
                    crate::domain::agent::ImagePullPolicy::IfNotPresent => {
                        "if_not_present".to_string()
                    }
                    crate::domain::agent::ImagePullPolicy::Never => "never".to_string(),
                });

                Ok(TemporalWorkflowState {
                    kind: "ContainerRun".to_string(),
                    agent: None,
                    input: None,
                    isolation: None,
                    timeout,
                    judges: None,
                    max_iterations: None,
                    pre_execution_validator: None,
                    command: None,
                    env: None,
                    workdir: None,
                    prompt: None,
                    default_response: None,
                    agents: None,
                    consensus: None,
                    judges_for_parallel: None,
                    container_run_name: Some(name.clone()),
                    container_run_image: Some(image.clone()),
                    container_run_image_pull_policy: pull_policy_str,
                    container_run_command: Some(command.clone()),
                    container_run_env: Some(env.clone()),
                    container_run_workdir: workdir.clone(),
                    container_run_volumes: Some(
                        serde_json::to_value(volumes).unwrap_or(serde_json::json!([])),
                    ),
                    container_run_resources: resources
                        .as_ref()
                        .map(|r| serde_json::to_value(r).unwrap_or(serde_json::json!({}))),
                    container_run_registry_credentials: registry_credentials.clone(),
                    container_run_retry: retry
                        .as_ref()
                        .map(|r| serde_json::to_value(r).unwrap_or(serde_json::json!({}))),
                    container_run_shell: Some(*shell),
                    parallel_container_steps: None,
                    parallel_container_completion: None,
                    subworkflow_id: None,
                    subworkflow_mode: None,
                    subworkflow_result_key: None,
                    subworkflow_input: None,
                    output_handler: output_handler
                        .as_ref()
                        .map(|h| serde_json::to_value(h).unwrap_or(serde_json::Value::Null)),
                    transitions,
                })
            }

            StateKind::ParallelContainerRun { steps, completion } => {
                let completion_str = match completion {
                    ParallelCompletionStrategy::AllSucceed => "all_succeed",
                    ParallelCompletionStrategy::AnySucceed => "any_succeed",
                    ParallelCompletionStrategy::BestEffort => "best_effort",
                }
                .to_string();

                Ok(TemporalWorkflowState {
                    kind: "ParallelContainerRun".to_string(),
                    agent: None,
                    input: None,
                    isolation: None,
                    timeout,
                    judges: None,
                    max_iterations: None,
                    pre_execution_validator: None,
                    command: None,
                    env: None,
                    workdir: None,
                    prompt: None,
                    default_response: None,
                    agents: None,
                    consensus: None,
                    judges_for_parallel: None,
                    container_run_name: None,
                    container_run_image: None,
                    container_run_image_pull_policy: None,
                    container_run_command: None,
                    container_run_env: None,
                    container_run_workdir: None,
                    container_run_volumes: None,
                    container_run_resources: None,
                    container_run_registry_credentials: None,
                    container_run_retry: None,
                    container_run_shell: None,
                    parallel_container_steps: Some(
                        serde_json::to_value(steps).unwrap_or(serde_json::json!([])),
                    ),
                    parallel_container_completion: Some(completion_str),
                    subworkflow_id: None,
                    subworkflow_mode: None,
                    subworkflow_result_key: None,
                    subworkflow_input: None,
                    output_handler: None,
                    transitions,
                })
            }

            StateKind::Subworkflow {
                workflow_id,
                mode,
                result_key,
                input,
            } => {
                let mode_str = match mode {
                    crate::domain::workflow::SubworkflowMode::Blocking => "blocking",
                    crate::domain::workflow::SubworkflowMode::FireAndForget => "fire_and_forget",
                };
                Ok(TemporalWorkflowState {
                    kind: "Subworkflow".to_string(),
                    // Agent fields
                    agent: None,
                    input: None,
                    isolation: None,
                    timeout: state.timeout.map(|d| format!("{}s", d.as_secs())),
                    judges: None,
                    max_iterations: None,
                    pre_execution_validator: None,
                    // System fields
                    command: None,
                    env: None,
                    workdir: None,
                    // Human fields
                    prompt: None,
                    default_response: None,
                    // ParallelAgents fields
                    agents: None,
                    consensus: None,
                    judges_for_parallel: None,
                    // ContainerRun fields
                    container_run_name: None,
                    container_run_image: None,
                    container_run_image_pull_policy: None,
                    container_run_command: None,
                    container_run_env: None,
                    container_run_workdir: None,
                    container_run_volumes: None,
                    container_run_resources: None,
                    container_run_registry_credentials: None,
                    container_run_retry: None,
                    container_run_shell: None,
                    // ParallelContainerRun fields
                    parallel_container_steps: None,
                    parallel_container_completion: None,
                    // Subworkflow fields (ADR-065)
                    subworkflow_id: Some(workflow_id.clone()),
                    subworkflow_mode: Some(mode_str.to_string()),
                    subworkflow_result_key: result_key.clone(),
                    subworkflow_input: input.clone(),
                    // Output handler (ADR-103): Subworkflow states do not carry an output_handler
                    output_handler: None,
                    // Transitions
                    transitions,
                })
            }
        }
    }

    /// Map TransitionRule to TemporalTransitionRule
    fn map_transition_rule(rule: &TransitionRule) -> Result<TemporalTransitionRule> {
        let (condition, threshold, min, max, exit_code, value, expression) = match &rule.condition {
            TransitionCondition::Always => {
                ("always".to_string(), None, None, None, None, None, None)
            }
            TransitionCondition::OnSuccess => {
                ("on_success".to_string(), None, None, None, None, None, None)
            }
            TransitionCondition::OnFailure => {
                ("on_failure".to_string(), None, None, None, None, None, None)
            }
            TransitionCondition::ExitCodeZero => (
                "exit_code_zero".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::ExitCodeNonZero => (
                "exit_code_non_zero".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::ExitCode { value: v } => (
                "exit_code".to_string(),
                None,
                None,
                None,
                Some(*v),
                None,
                None,
            ),
            TransitionCondition::ScoreAbove { threshold: t } => (
                "score_above".to_string(),
                Some(*t),
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::ScoreBelow { threshold: t } => (
                "score_below".to_string(),
                Some(*t),
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::ScoreBetween { min: mi, max: ma } => (
                "score_between".to_string(),
                None,
                Some(*mi),
                Some(*ma),
                None,
                None,
                None,
            ),
            TransitionCondition::ConfidenceAbove { threshold: t } => (
                "confidence_above".to_string(),
                Some(*t),
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::ScoreAndConfidenceAbove { threshold: t } => (
                "score_and_confidence_above".to_string(),
                Some(*t),
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::Consensus {
                threshold: t,
                agreement: a,
            } => (
                "consensus".to_string(),
                Some(*t),
                Some(*a),
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::AllApproved => (
                "all_approved".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::AnyRejected => (
                "any_rejected".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::InputEquals { value: v } => (
                "input_equals".to_string(),
                None,
                None,
                None,
                None,
                Some(v.clone()),
                None,
            ),
            TransitionCondition::InputEqualsYes => (
                "input_equals_yes".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::InputEqualsNo => (
                "input_equals_no".to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
            TransitionCondition::Custom { expression: expr } => (
                "custom".to_string(),
                None,
                None,
                None,
                None,
                None,
                Some(expr.clone()),
            ),
        };

        Ok(TemporalTransitionRule {
            condition,
            target: Some(rule.target.as_str().to_string()),
            threshold,
            min,
            max,
            exit_code,
            value,
            expression,
            feedback: rule.feedback.clone(),
        })
    }

    /// Map IsolationMode to string
    fn map_isolation_mode(mode: IsolationMode) -> String {
        match mode {
            IsolationMode::Inherit => "inherit".to_string(),
            IsolationMode::Firecracker => "firecracker".to_string(),
            IsolationMode::Docker => "docker".to_string(),
            IsolationMode::Process => "process".to_string(),
        }
    }

    /// Map ConsensusStrategy to string
    fn map_consensus_strategy(strategy: ConsensusStrategy) -> String {
        match strategy {
            ConsensusStrategy::WeightedAverage => "weighted_average".to_string(),
            ConsensusStrategy::Majority => "majority_vote".to_string(),
            ConsensusStrategy::Unanimous => "unanimous".to_string(),
            ConsensusStrategy::BestOfN => "best_of_n".to_string(),
        }
    }

    /// Validate Handlebars templates in workflow
    ///
    /// This ensures all templates can be parsed before sending to TypeScript worker
    pub fn validate_templates(workflow: &Workflow) -> Result<()> {
        let handlebars = handlebars::Handlebars::new();

        for (state_name, state) in &workflow.spec.states {
            match &state.kind {
                StateKind::Agent { input, .. } => {
                    handlebars
                        .render_template(input, &serde_json::json!({}))
                        .with_context(|| {
                            format!("Invalid template in state {state_name}: {input}")
                        })?;
                }
                StateKind::System { env, .. } => {
                    for (key, value) in env {
                        handlebars
                            .render_template(value, &serde_json::json!({}))
                            .with_context(|| {
                                format!("Invalid template in state {state_name} env {key}: {value}")
                            })?;
                    }
                }
                StateKind::ParallelAgents { agents, .. } => {
                    for agent in agents {
                        handlebars
                            .render_template(&agent.input, &serde_json::json!({}))
                            .with_context(|| {
                                format!(
                                    "Invalid template in parallel agent {}: {}",
                                    agent.agent, agent.input
                                )
                            })?;
                    }
                }
                StateKind::ContainerRun { env, .. } => {
                    for (key, value) in env {
                        handlebars
                            .render_template(value, &serde_json::json!({}))
                            .with_context(|| {
                                format!(
                                    "Invalid template in ContainerRun state {state_name} env {key}: {value}"
                                )
                            })?;
                    }
                }
                StateKind::ParallelContainerRun { steps, .. } => {
                    for step in steps {
                        for (key, value) in &step.env {
                            handlebars
                                .render_template(value, &serde_json::json!({}))
                                .with_context(|| {
                                    format!(
                                        "Invalid template in ParallelContainerRun state {} step {} env {}: {}",
                                        state_name, step.name, key, value
                                    )
                                })?;
                        }
                    }
                }
                StateKind::Human { .. } => {
                    // Human states have no Handlebars templates to validate.
                }
                StateKind::Subworkflow { input, .. } => {
                    if let Some(tmpl) = input {
                        handlebars
                            .render_template(tmpl, &serde_json::json!({}))
                            .with_context(|| {
                                format!(
                                    "Invalid template in Subworkflow state {state_name} input: {tmpl}"
                                )
                            })?;
                    }
                }
            }
        }

        Ok(())
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_simple_workflow() {
        let mut states = HashMap::new();
        states.insert(
            StateName::new("START").unwrap(),
            WorkflowState {
                kind: StateKind::Agent {
                    agent: "test-agent".to_string(),
                    input: "Hello {{task}}".to_string(),
                    intent: None,
                    isolation: None,
                    judges: vec![],
                    max_iterations: None,
                    pre_execution_validator: None,
                    output_handler: None,
                },
                transitions: vec![TransitionRule {
                    condition: TransitionCondition::Always,
                    target: StateName::new("END").unwrap(),
                    feedback: None,
                }],
                timeout: None,
            },
        );
        states.insert(
            StateName::new("END").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo done".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );

        let workflow = Workflow::new(
            WorkflowMetadata {
                name: "test-workflow".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                labels: HashMap::new(),
                annotations: HashMap::new(),
                input_schema: None,
            },
            WorkflowSpec {
                initial_state: StateName::new("START").unwrap(),
                context: HashMap::new(),
                states,
                storage: Default::default(),
            },
        )
        .unwrap();

        let temporal_def =
            TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::consumer())
                .unwrap();

        assert_eq!(temporal_def.name, "test-workflow");
        assert_eq!(temporal_def.tenant_id, TenantId::consumer().to_string());
        assert_eq!(temporal_def.initial_state, "START");
        assert_eq!(temporal_def.states.len(), 2);
        assert_eq!(temporal_def.states.get("START").unwrap().kind, "Agent");
        assert_eq!(temporal_def.states.get("END").unwrap().kind, "System");
    }

    #[test]
    fn test_container_run_image_pull_policy_maps_to_snake_case() {
        let mut states = HashMap::new();
        states.insert(
            StateName::new("BUILD").unwrap(),
            WorkflowState {
                kind: StateKind::ContainerRun {
                    name: "build".to_string(),
                    image: "rust:1.75".to_string(),
                    image_pull_policy: Some(crate::domain::agent::ImagePullPolicy::IfNotPresent),
                    command: vec!["cargo".to_string(), "build".to_string()],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![],
                    resources: None,
                    registry_credentials: None,
                    retry: None,
                    shell: false,
                    output_handler: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );

        let workflow = Workflow::new(
            WorkflowMetadata {
                name: "container-policy-map".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                labels: HashMap::new(),
                annotations: HashMap::new(),
                input_schema: None,
            },
            WorkflowSpec {
                initial_state: StateName::new("BUILD").unwrap(),
                context: HashMap::new(),
                states,
                storage: Default::default(),
            },
        )
        .unwrap();

        let temporal_def =
            TemporalWorkflowMapper::to_temporal_definition(&workflow, &TenantId::consumer())
                .unwrap();
        assert_eq!(
            temporal_def
                .states
                .get("BUILD")
                .unwrap()
                .container_run_image_pull_policy
                .as_deref(),
            Some("if_not_present")
        );
    }
}
