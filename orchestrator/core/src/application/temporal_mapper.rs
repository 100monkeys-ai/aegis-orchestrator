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

use crate::domain::workflow::*;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    pub name: String,
    pub version: String,
    pub initial_state: String,
    pub context: HashMap<String, serde_json::Value>,
    pub states: HashMap<String, TemporalWorkflowState>,
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
    pub isolation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,

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

    // Transitions
    pub transitions: Vec<TemporalTransitionRule>,
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
    /// let temporal_def = TemporalWorkflowMapper::to_temporal_definition(&workflow)?;
    /// ```
    pub fn to_temporal_definition(workflow: &Workflow) -> Result<TemporalWorkflowDefinition> {
        // Map states
        let mut temporal_states = HashMap::new();
        for (state_name, state) in &workflow.spec.states {
            let temporal_state = Self::map_workflow_state(state)?;
            temporal_states.insert(state_name.as_str().to_string(), temporal_state);
        }

        Ok(TemporalWorkflowDefinition {
            workflow_id: workflow.id.to_string(),
            name: workflow.metadata.name.clone(),
            version: workflow.metadata.version.clone().unwrap_or_else(|| "1.0.0".to_string()),
            initial_state: workflow.spec.initial_state.as_str().to_string(),
            context: workflow.spec.context.clone(),
            states: temporal_states,
        })
    }

    /// Map WorkflowState to TemporalWorkflowState
    fn map_workflow_state(state: &WorkflowState) -> Result<TemporalWorkflowState> {
        let (kind, agent, input, isolation, command, env, workdir, prompt, default_response, agents, consensus) =
            match &state.kind {
                StateKind::Agent {
                    agent,
                    input,
                    isolation,
                } => (
                    "Agent".to_string(),
                    Some(agent.clone()),
                    Some(input.clone()),
                    isolation.map(|i| Self::map_isolation_mode(i)),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),

                StateKind::System {
                    command,
                    env,
                    workdir,
                } => (
                    "System".to_string(),
                    None,
                    None,
                    None,
                    Some(command.clone()),
                    Some(env.clone()),
                    workdir.clone(),
                    None,
                    None,
                    None,
                    None,
                ),

                StateKind::Human {
                    prompt,
                    default_response,
                } => (
                    "Human".to_string(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(prompt.clone()),
                    default_response.clone(),
                    None,
                    None,
                ),

                StateKind::ParallelAgents { agents, consensus } => {
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

                    (
                        "ParallelAgents".to_string(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(temporal_agents),
                        Some(temporal_consensus),
                    )
                }
            };

        // Map transitions
        let transitions = state
            .transitions
            .iter()
            .map(|tr| Self::map_transition_rule(tr))
            .collect::<Result<Vec<_>>>()?;

        Ok(TemporalWorkflowState {
            kind,
            agent,
            input,
            isolation,
            timeout: state.timeout.map(|d| format!("{}s", d.as_secs())),
            command,
            env,
            workdir,
            prompt,
            default_response,
            agents,
            consensus,
            transitions,
        })
    }

    /// Map TransitionRule to TemporalTransitionRule
    fn map_transition_rule(rule: &TransitionRule) -> Result<TemporalTransitionRule> {
        let (condition, threshold, min, max, exit_code, value, expression) = match &rule.condition {
            TransitionCondition::Always => ("always".to_string(), None, None, None, None, None, None),
            TransitionCondition::OnSuccess => ("on_success".to_string(), None, None, None, None, None, None),
            TransitionCondition::OnFailure => ("on_failure".to_string(), None, None, None, None, None, None),
            TransitionCondition::ExitCodeZero => ("exit_code_zero".to_string(), None, None, None, None, None, None),
            TransitionCondition::ExitCodeNonZero => {
                ("exit_code_non_zero".to_string(), None, None, None, None, None, None)
            }
            TransitionCondition::ExitCode { value: v } => {
                ("exit_code".to_string(), None, None, None, Some(*v), None, None)
            }
            TransitionCondition::ScoreAbove { threshold: t } => {
                ("score_above".to_string(), Some(*t), None, None, None, None, None)
            }
            TransitionCondition::ScoreBelow { threshold: t } => {
                ("score_below".to_string(), Some(*t), None, None, None, None, None)
            }
            TransitionCondition::ScoreBetween { min: mi, max: ma } => {
                ("score_between".to_string(), None, Some(*mi), Some(*ma), None, None, None)
            }
            TransitionCondition::ConfidenceAbove { threshold: t } => {
                ("confidence_above".to_string(), Some(*t), None, None, None, None, None)
            }
            TransitionCondition::Consensus { threshold: t, agreement: a } => {
                ("consensus".to_string(), Some(*t), Some(*a), None, None, None, None)
            }
            TransitionCondition::AllApproved => ("all_approved".to_string(), None, None, None, None, None, None),
            TransitionCondition::AnyRejected => ("any_rejected".to_string(), None, None, None, None, None, None),
            TransitionCondition::InputEquals { value: v } => {
                ("input_equals".to_string(), None, None, None, None, Some(v.clone()), None)
            }
            TransitionCondition::InputEqualsYes => {
                ("input_equals_yes".to_string(), None, None, None, None, None, None)
            }
            TransitionCondition::InputEqualsNo => {
                ("input_equals_no".to_string(), None, None, None, None, None, None)
            }
            TransitionCondition::Custom { expression: expr } => {
                ("custom".to_string(), None, None, None, None, None, Some(expr.clone()))
            }
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
                        .with_context(|| format!("Invalid template in state {}: {}", state_name, input))?;
                }
                StateKind::System { env, .. } => {
                    for (key, value) in env {
                        handlebars
                            .render_template(value, &serde_json::json!({}))
                            .with_context(|| {
                                format!("Invalid template in state {} env {}: {}", state_name, key, value)
                            })?;
                    }
                }
                StateKind::ParallelAgents { agents, .. } => {
                    for agent in agents {
                        handlebars
                            .render_template(&agent.input, &serde_json::json!({}))
                            .with_context(|| {
                                format!("Invalid template in parallel agent {}: {}", agent.agent, agent.input)
                            })?;
                    }
                }
                _ => {}
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
                    isolation: None,
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
            },
            WorkflowSpec {
                initial_state: StateName::new("START").unwrap(),
                context: HashMap::new(),
                states,
            },
        )
        .unwrap();

        let temporal_def = TemporalWorkflowMapper::to_temporal_definition(&workflow).unwrap();

        assert_eq!(temporal_def.name, "test-workflow");
        assert_eq!(temporal_def.initial_state, "START");
        assert_eq!(temporal_def.states.len(), 2);
        assert_eq!(temporal_def.states.get("START").unwrap().kind, "Agent");
        assert_eq!(temporal_def.states.get("END").unwrap().kind, "System");
    }
}
