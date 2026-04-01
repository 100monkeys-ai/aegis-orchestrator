// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Workflow YAML Parser
//!
//! Infrastructure-layer service that parses workflow YAML manifests into
//! validated domain objects, and serialises domain objects back to YAML.
//!
//! # Architectural Context
//!
//! - **Bounded Context:** Workflow Orchestration Context (BC-3)
//! - **Layer:** Infrastructure
//! - **Purpose:** Anti-Corruption Layer — translates external YAML schema to/from domain model
//! - **Related ADRs:** ADR-015 (Workflow Engine Architecture), ADR-031 (Handlebars Template Engine)
//!
//! # Design Principles
//!
//! 1. **Anti-Corruption:** YAML schema structs (`WorkflowManifest`, `StateKindYaml`, etc.) never
//!    bleed into the domain layer; only `Workflow` domain types cross the boundary.
//! 2. **Single Responsibility:** Parse and serialize only — no side effects beyond file I/O.
//! 3. **Fail Fast:** Full manifest validation before returning a `Workflow` domain object.
//! 4. **Round-trip Safe:** `parse_yaml(to_yaml(w)) ≡ w` for all valid workflows.
//!
//! # Code Quality Principles
//!
//! - All public APIs return `Result` — no panics in production paths.
//! - Errors carry full context (path, YAML snippet, field name).
//! - Tests validate both happy-path and error-path behaviour with explicit assertions.
//!
//! # Manifest Format
//!
//! ```yaml
//! apiVersion: 100monkeys.ai/v1
//! kind: Workflow
//! metadata:
//!   name: my-workflow
//!   version: "1.0.0"
//! spec:
//!   initial_state: START
//!   states:
//!     START:
//!       kind: Agent
//!       agent: coder-v1
//!       input: "{{workflow.task}}"
//!       transitions:
//!         - condition: always
//!           target: END
//!     END:
//!       kind: System
//!       command: finalize
//!       transitions: []
//! ```

use crate::domain::workflow::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

// ============================================================================
// YAML Schema (External Representation)
// ============================================================================

/// External YAML representation of a workflow manifest
///
/// This struct matches the YAML schema exactly. It is then converted
/// to the domain Workflow object with validation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: WorkflowMetadataYaml,
    pub spec: WorkflowSpecYaml,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowMetadataYaml {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowSpecYaml {
    pub initial_state: String,
    #[serde(default)]
    pub context: HashMap<String, serde_json::Value>,
    pub states: HashMap<String, WorkflowStateYaml>,
    /// Workflow-level storage configuration (WORKFLOW_MANIFEST_SPEC_V1 §spec.storage)
    #[serde(default)]
    pub storage: crate::domain::workflow::WorkflowStorageSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowStateYaml {
    #[serde(flatten)]
    pub kind: StateKindYaml,
    pub transitions: Vec<TransitionRuleYaml>,
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum StateKindYaml {
    Agent {
        agent: String,
        input: String,
        #[serde(default)]
        isolation: Option<IsolationMode>,
        /// Judge agents declared per-state (ADR-016 / ADR-017)
        #[serde(default)]
        judges: Vec<JudgeConfigYaml>,
        /// Maximum inner-loop iterations for this state (overrides global default)
        #[serde(default)]
        max_iterations: Option<u32>,
        /// Optional pre-execution validator agent ID (ADR-049 Pillar 1)
        #[serde(default)]
        pre_execution_validator: Option<String>,
    },
    System {
        command: String,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        workdir: Option<String>,
    },
    Human {
        prompt: String,
        #[serde(default)]
        default_response: Option<String>,
    },
    ParallelAgents {
        agents: Vec<ParallelAgentConfigYaml>,
        consensus: ConsensusConfigYaml,
        /// External judge agents for validating combined parallel output (ADR-016)
        #[serde(default)]
        judges_for_parallel: Vec<JudgeConfigYaml>,
    },
    /// Deterministic CI/CD container step — no LLM loop (ADR-050)
    ContainerRun {
        name: String,
        image: String,
        #[serde(default)]
        image_pull_policy: Option<crate::domain::agent::ImagePullPolicy>,
        #[serde(default)]
        command: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        workdir: Option<String>,
        #[serde(default)]
        volumes: Vec<crate::domain::workflow::ContainerVolumeMount>,
        #[serde(default)]
        resources: Option<crate::domain::workflow::ContainerResources>,
        #[serde(default)]
        registry_credentials: Option<String>,
        #[serde(default)]
        retry: Option<crate::domain::workflow::RetryConfig>,
        #[serde(default)]
        shell: bool,
    },
    /// Parallel deterministic container steps — no LLM loop (ADR-050)
    ParallelContainerRun {
        steps: Vec<crate::domain::workflow::ContainerRunConfig>,
        completion: crate::domain::workflow::ParallelCompletionStrategy,
    },
    /// Invoke a child workflow — blocking or fire-and-forget (ADR-065)
    #[serde(rename = "Subworkflow")]
    Subworkflow {
        /// The workflow to invoke (name or UUID)
        workflow_id: String,
        /// Execution mode: "blocking" or "fire_and_forget"
        mode: String,
        /// Blackboard key for child result (required for blocking, forbidden for fire_and_forget)
        #[serde(default)]
        result_key: Option<String>,
        /// Optional Handlebars input template
        #[serde(default)]
        input: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ParallelAgentConfigYaml {
    pub agent: String,
    pub input: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
}

fn default_weight() -> f64 {
    1.0
}

fn default_timeout() -> u64 {
    60
}

fn default_poll_interval() -> u64 {
    500
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConsensusConfigYaml {
    pub strategy: ConsensusStrategy,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default, alias = "min_agreement_confidence")]
    pub agreement: Option<f64>,
    #[serde(default)]
    pub n: Option<usize>,
    #[serde(default = "default_min_judges")]
    pub min_judges_required: Option<usize>,
    #[serde(default)]
    pub confidence_weighting: Option<crate::domain::workflow::ConfidenceWeighting>,
}

fn default_min_judges() -> Option<usize> {
    Some(1)
}

/// YAML representation of a judge agent configuration (ADR-016 / ADR-017)
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JudgeConfigYaml {
    pub agent_id: String,
    #[serde(default)]
    pub input_template: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TransitionRuleYaml {
    #[serde(flatten)]
    pub condition: TransitionConditionYaml,
    pub target: String,
    #[serde(default)]
    pub feedback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum TransitionConditionYaml {
    Always,
    OnSuccess,
    OnFailure,
    ExitCodeZero,
    ExitCodeNonZero,
    ExitCode {
        value: i32,
    },
    ScoreAbove {
        threshold: f64,
    },
    ScoreBelow {
        threshold: f64,
    },
    ScoreBetween {
        min: f64,
        max: f64,
    },
    ConfidenceAbove {
        threshold: f64,
    },
    /// Both validation score AND confidence are above the same threshold (ADR-049)
    ScoreAndConfidenceAbove {
        threshold: f64,
    },
    Consensus {
        threshold: f64,
        agreement: f64,
    },
    AllApproved,
    AnyRejected,
    InputEquals {
        value: String,
    },
    InputEqualsYes,
    InputEqualsNo,
    Custom {
        expression: String,
    },
}

// ============================================================================
// Parser
// ============================================================================

/// Workflow parser (Infrastructure service)
pub struct WorkflowParser;

impl WorkflowParser {
    /// Parse a workflow manifest from YAML file
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Workflow, WorkflowParseError> {
        let content =
            fs::read_to_string(path.as_ref()).map_err(|e| WorkflowParseError::IoError {
                path: path.as_ref().display().to_string(),
                error: e.to_string(),
            })?;

        Self::parse_yaml(&content)
    }

    /// Parse a workflow manifest from YAML string
    pub fn parse_yaml(yaml: &str) -> Result<Workflow, WorkflowParseError> {
        let manifest: WorkflowManifest =
            serde_yaml::from_str(yaml).map_err(|e| WorkflowParseError::YamlError(e.to_string()))?;

        Self::validate_and_convert(manifest)
    }

    /// Validate manifest and convert to domain object
    fn validate_and_convert(manifest: WorkflowManifest) -> Result<Workflow, WorkflowParseError> {
        // Validate apiVersion
        if manifest.api_version != "100monkeys.ai/v1" {
            return Err(WorkflowParseError::InvalidApiVersion {
                expected: "100monkeys.ai/v1".to_string(),
                got: manifest.api_version,
            });
        }

        // Validate kind
        if manifest.kind != "Workflow" {
            return Err(WorkflowParseError::InvalidKind {
                expected: "Workflow".to_string(),
                got: manifest.kind,
            });
        }

        // Validate and convert metadata
        WorkflowMetadata::validate_name(&manifest.metadata.name).map_err(|e| {
            WorkflowParseError::ValidationError(format!("Invalid workflow name: {e}"))
        })?;

        let metadata = WorkflowMetadata {
            name: manifest.metadata.name,
            version: manifest.metadata.version,
            description: manifest.metadata.description,
            tags: manifest.metadata.tags,
            labels: manifest.metadata.labels,
            annotations: manifest.metadata.annotations,
        };

        // Convert states
        let mut states = HashMap::new();
        for (state_name_str, state_yaml) in manifest.spec.states {
            let state_name = StateName::new(state_name_str)
                .map_err(|e| WorkflowParseError::ValidationError(e.to_string()))?;

            let kind = Self::convert_state_kind(state_yaml.kind)?;

            let transitions = state_yaml
                .transitions
                .into_iter()
                .map(Self::convert_transition)
                .collect::<Result<Vec<_>, _>>()?;

            states.insert(
                state_name,
                WorkflowState {
                    kind,
                    transitions,
                    timeout: state_yaml.timeout,
                },
            );
        }

        // Convert spec
        let initial_state = StateName::new(manifest.spec.initial_state)
            .map_err(|e| WorkflowParseError::ValidationError(e.to_string()))?;

        let spec = WorkflowSpec {
            initial_state,
            context: manifest.spec.context,
            states,
            storage: manifest.spec.storage,
        };

        // Create and validate workflow
        Workflow::new(metadata, spec)
            .map_err(|e| WorkflowParseError::ValidationError(e.to_string()))
    }

    fn convert_state_kind(yaml: StateKindYaml) -> Result<StateKind, WorkflowParseError> {
        Ok(match yaml {
            StateKindYaml::Agent {
                agent,
                input,
                isolation,
                judges,
                max_iterations,
                pre_execution_validator,
            } => StateKind::Agent {
                agent,
                input,
                isolation,
                judges: judges
                    .into_iter()
                    .map(|j| crate::domain::workflow::JudgeConfig {
                        agent_id: j.agent_id,
                        input_template: j.input_template,
                        weight: j.weight,
                    })
                    .collect(),
                max_iterations,
                pre_execution_validator,
            },
            StateKindYaml::System {
                command,
                env,
                workdir,
            } => StateKind::System {
                command,
                env,
                workdir,
            },
            StateKindYaml::Human {
                prompt,
                default_response,
            } => StateKind::Human {
                prompt,
                default_response,
            },
            StateKindYaml::ParallelAgents {
                agents,
                consensus,
                judges_for_parallel,
            } => {
                let agent_configs = agents
                    .into_iter()
                    .map(|a| ParallelAgentConfig {
                        agent: a.agent,
                        input: a.input,
                        weight: a.weight,
                        timeout_seconds: a.timeout_seconds,
                        poll_interval_ms: a.poll_interval_ms,
                    })
                    .collect();

                let consensus_config = ConsensusConfig {
                    strategy: consensus.strategy,
                    threshold: consensus.threshold,
                    min_agreement_confidence: consensus.agreement,
                    n: consensus.n,
                    min_judges_required: consensus.min_judges_required.unwrap_or(1),
                    confidence_weighting: consensus.confidence_weighting,
                };

                let judges_for_parallel_configs = judges_for_parallel
                    .into_iter()
                    .map(|j| crate::domain::workflow::JudgeConfig {
                        agent_id: j.agent_id,
                        input_template: j.input_template,
                        weight: j.weight,
                    })
                    .collect();

                StateKind::ParallelAgents {
                    agents: agent_configs,
                    consensus: consensus_config,
                    judges_for_parallel: judges_for_parallel_configs,
                }
            }
            StateKindYaml::ContainerRun {
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
            } => StateKind::ContainerRun {
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
            },
            StateKindYaml::ParallelContainerRun { steps, completion } => {
                StateKind::ParallelContainerRun { steps, completion }
            }
            StateKindYaml::Subworkflow {
                workflow_id,
                mode,
                result_key,
                input,
            } => {
                let mode = match mode.as_str() {
                    "blocking" => crate::domain::workflow::SubworkflowMode::Blocking,
                    "fire_and_forget" => crate::domain::workflow::SubworkflowMode::FireAndForget,
                    other => {
                        return Err(WorkflowParseError::ValidationError(format!(
                            "Invalid subworkflow mode '{other}': expected 'blocking' or 'fire_and_forget'"
                        )));
                    }
                };
                StateKind::Subworkflow {
                    workflow_id,
                    mode,
                    result_key,
                    input,
                }
            }
        })
    }

    fn convert_transition(yaml: TransitionRuleYaml) -> Result<TransitionRule, WorkflowParseError> {
        let target = StateName::new(yaml.target)
            .map_err(|e| WorkflowParseError::ValidationError(e.to_string()))?;

        let condition = Self::convert_condition(yaml.condition)?;

        Ok(TransitionRule {
            condition,
            target,
            feedback: yaml.feedback,
        })
    }

    fn convert_condition(
        yaml: TransitionConditionYaml,
    ) -> Result<TransitionCondition, WorkflowParseError> {
        Ok(match yaml {
            TransitionConditionYaml::Always => TransitionCondition::Always,
            TransitionConditionYaml::OnSuccess => TransitionCondition::OnSuccess,
            TransitionConditionYaml::OnFailure => TransitionCondition::OnFailure,
            TransitionConditionYaml::ExitCodeZero => TransitionCondition::ExitCodeZero,
            TransitionConditionYaml::ExitCodeNonZero => TransitionCondition::ExitCodeNonZero,
            TransitionConditionYaml::ExitCode { value } => TransitionCondition::ExitCode { value },
            TransitionConditionYaml::ScoreAbove { threshold } => {
                TransitionCondition::ScoreAbove { threshold }
            }
            TransitionConditionYaml::ScoreBelow { threshold } => {
                TransitionCondition::ScoreBelow { threshold }
            }
            TransitionConditionYaml::ScoreBetween { min, max } => {
                TransitionCondition::ScoreBetween { min, max }
            }
            TransitionConditionYaml::ConfidenceAbove { threshold } => {
                TransitionCondition::ConfidenceAbove { threshold }
            }
            TransitionConditionYaml::ScoreAndConfidenceAbove { threshold } => {
                TransitionCondition::ScoreAndConfidenceAbove { threshold }
            }
            TransitionConditionYaml::Consensus {
                threshold,
                agreement,
            } => TransitionCondition::Consensus {
                threshold,
                agreement,
            },
            TransitionConditionYaml::AllApproved => TransitionCondition::AllApproved,
            TransitionConditionYaml::AnyRejected => TransitionCondition::AnyRejected,
            TransitionConditionYaml::InputEquals { value } => {
                TransitionCondition::InputEquals { value }
            }
            TransitionConditionYaml::InputEqualsYes => TransitionCondition::InputEqualsYes,
            TransitionConditionYaml::InputEqualsNo => TransitionCondition::InputEqualsNo,
            TransitionConditionYaml::Custom { expression } => {
                TransitionCondition::Custom { expression }
            }
        })
    }

    /// Serialize a workflow back to YAML
    pub fn to_yaml(workflow: &Workflow) -> Result<String, WorkflowParseError> {
        // Convert domain model back to YAML representation
        let manifest = Self::workflow_to_manifest(workflow);
        serde_yaml::to_string(&manifest).map_err(|e| WorkflowParseError::YamlError(e.to_string()))
    }

    fn workflow_to_manifest(workflow: &Workflow) -> WorkflowManifest {
        let metadata = WorkflowMetadataYaml {
            name: workflow.metadata.name.clone(),
            version: workflow.metadata.version.clone(),
            description: workflow.metadata.description.clone(),
            tags: workflow.metadata.tags.clone(),
            labels: workflow.metadata.labels.clone(),
            annotations: workflow.metadata.annotations.clone(),
        };

        let mut states = HashMap::new();
        for (state_name, state) in &workflow.spec.states {
            states.insert(
                state_name.as_str().to_string(),
                WorkflowStateYaml {
                    kind: Self::state_kind_to_yaml(&state.kind),
                    transitions: state
                        .transitions
                        .iter()
                        .map(Self::transition_to_yaml)
                        .collect(),
                    timeout: state.timeout,
                },
            );
        }

        let spec = WorkflowSpecYaml {
            initial_state: workflow.spec.initial_state.as_str().to_string(),
            context: workflow.spec.context.clone(),
            states,
            storage: workflow.spec.storage.clone(),
        };

        WorkflowManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Workflow".to_string(),
            metadata,
            spec,
        }
    }

    fn state_kind_to_yaml(kind: &StateKind) -> StateKindYaml {
        match kind {
            StateKind::Agent {
                agent,
                input,
                isolation,
                judges,
                max_iterations,
                pre_execution_validator,
            } => StateKindYaml::Agent {
                agent: agent.clone(),
                input: input.clone(),
                isolation: *isolation,
                judges: judges
                    .iter()
                    .map(|j| JudgeConfigYaml {
                        agent_id: j.agent_id.clone(),
                        input_template: j.input_template.clone(),
                        weight: j.weight,
                    })
                    .collect(),
                max_iterations: *max_iterations,
                pre_execution_validator: pre_execution_validator.clone(),
            },
            StateKind::System {
                command,
                env,
                workdir,
            } => StateKindYaml::System {
                command: command.clone(),
                env: env.clone(),
                workdir: workdir.clone(),
            },
            StateKind::Human {
                prompt,
                default_response,
            } => StateKindYaml::Human {
                prompt: prompt.clone(),
                default_response: default_response.clone(),
            },
            StateKind::ParallelAgents {
                agents,
                consensus,
                judges_for_parallel,
            } => StateKindYaml::ParallelAgents {
                agents: agents
                    .iter()
                    .map(|a| ParallelAgentConfigYaml {
                        agent: a.agent.clone(),
                        input: a.input.clone(),
                        weight: a.weight,
                        timeout_seconds: a.timeout_seconds,
                        poll_interval_ms: a.poll_interval_ms,
                    })
                    .collect(),
                consensus: ConsensusConfigYaml {
                    strategy: consensus.strategy,
                    threshold: consensus.threshold,
                    agreement: consensus.min_agreement_confidence,
                    n: consensus.n,
                    min_judges_required: Some(consensus.min_judges_required),
                    confidence_weighting: consensus.confidence_weighting.clone(),
                },
                judges_for_parallel: judges_for_parallel
                    .iter()
                    .map(|j| JudgeConfigYaml {
                        agent_id: j.agent_id.clone(),
                        input_template: j.input_template.clone(),
                        weight: j.weight,
                    })
                    .collect(),
            },
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
            } => StateKindYaml::ContainerRun {
                name: name.clone(),
                image: image.clone(),
                image_pull_policy: *image_pull_policy,
                command: command.clone(),
                env: env.clone(),
                workdir: workdir.clone(),
                volumes: volumes.clone(),
                resources: resources.clone(),
                registry_credentials: registry_credentials.clone(),
                retry: retry.clone(),
                shell: *shell,
            },
            StateKind::ParallelContainerRun { steps, completion } => {
                StateKindYaml::ParallelContainerRun {
                    steps: steps.clone(),
                    completion: *completion,
                }
            }
            StateKind::Subworkflow {
                workflow_id,
                mode,
                result_key,
                input,
            } => StateKindYaml::Subworkflow {
                workflow_id: workflow_id.clone(),
                mode: match mode {
                    crate::domain::workflow::SubworkflowMode::Blocking => "blocking".to_string(),
                    crate::domain::workflow::SubworkflowMode::FireAndForget => {
                        "fire_and_forget".to_string()
                    }
                },
                result_key: result_key.clone(),
                input: input.clone(),
            },
        }
    }

    fn transition_to_yaml(transition: &TransitionRule) -> TransitionRuleYaml {
        TransitionRuleYaml {
            condition: Self::condition_to_yaml(&transition.condition),
            target: transition.target.as_str().to_string(),
            feedback: transition.feedback.clone(),
        }
    }

    fn condition_to_yaml(condition: &TransitionCondition) -> TransitionConditionYaml {
        match condition {
            TransitionCondition::Always => TransitionConditionYaml::Always,
            TransitionCondition::OnSuccess => TransitionConditionYaml::OnSuccess,
            TransitionCondition::OnFailure => TransitionConditionYaml::OnFailure,
            TransitionCondition::ExitCodeZero => TransitionConditionYaml::ExitCodeZero,
            TransitionCondition::ExitCodeNonZero => TransitionConditionYaml::ExitCodeNonZero,
            TransitionCondition::ExitCode { value } => {
                TransitionConditionYaml::ExitCode { value: *value }
            }
            TransitionCondition::ScoreAbove { threshold } => TransitionConditionYaml::ScoreAbove {
                threshold: *threshold,
            },
            TransitionCondition::ScoreBelow { threshold } => TransitionConditionYaml::ScoreBelow {
                threshold: *threshold,
            },
            TransitionCondition::ScoreBetween { min, max } => {
                TransitionConditionYaml::ScoreBetween {
                    min: *min,
                    max: *max,
                }
            }
            TransitionCondition::ConfidenceAbove { threshold } => {
                TransitionConditionYaml::ConfidenceAbove {
                    threshold: *threshold,
                }
            }
            TransitionCondition::Consensus {
                threshold,
                agreement,
            } => TransitionConditionYaml::Consensus {
                threshold: *threshold,
                agreement: *agreement,
            },
            TransitionCondition::AllApproved => TransitionConditionYaml::AllApproved,
            TransitionCondition::AnyRejected => TransitionConditionYaml::AnyRejected,
            TransitionCondition::InputEquals { value } => TransitionConditionYaml::InputEquals {
                value: value.clone(),
            },
            TransitionCondition::InputEqualsYes => TransitionConditionYaml::InputEqualsYes,
            TransitionCondition::InputEqualsNo => TransitionConditionYaml::InputEqualsNo,
            TransitionCondition::Custom { expression } => TransitionConditionYaml::Custom {
                expression: expression.clone(),
            },
            TransitionCondition::ScoreAndConfidenceAbove { threshold } => {
                TransitionConditionYaml::ScoreAndConfidenceAbove {
                    threshold: *threshold,
                }
            }
        }
    }
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum WorkflowParseError {
    #[error("IO error reading {path}: {error}")]
    IoError { path: String, error: String },

    #[error("YAML parse error: {0}")]
    YamlError(String),

    #[error("Invalid API version: expected '{expected}', got '{got}'")]
    InvalidApiVersion { expected: String, got: String },

    #[error("Invalid kind: expected '{expected}', got '{got}'")]
    InvalidKind { expected: String, got: String },

    #[error("Validation error: {0}")]
    ValidationError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_workflow() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: test-workflow
  version: "1.0.0"
spec:
  initial_state: START
  states:
    START:
      kind: Agent
      agent: coder-v1
      input: "{{workflow.task}}"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: echo "done"
      transitions: []
"#;

        let result = WorkflowParser::parse_yaml(yaml);
        assert!(
            result.is_ok(),
            "simple workflow YAML failed to parse: {:?}",
            result.err()
        );
        let workflow = result.expect("simple workflow YAML should parse successfully");
        assert_eq!(
            workflow.metadata.name, "test-workflow",
            "workflow name did not match expected value"
        );
        assert_eq!(
            workflow.spec.states.len(),
            2,
            "expected exactly 2 states in the parsed workflow"
        );
    }

    #[test]
    fn test_invalid_api_version() {
        let yaml = r#"
apiVersion: invalid/v1
kind: Workflow
metadata:
  name: test
spec:
  initial_state: START
  states:
    START:
      kind: System
      command: echo
      transitions: []
"#;

        let result = WorkflowParser::parse_yaml(yaml);
        assert!(
            result.is_err(),
            "expected parse to fail for invalid apiVersion, but it succeeded"
        );
        assert!(
            matches!(result, Err(WorkflowParseError::InvalidApiVersion { .. })),
            "expected InvalidApiVersion error variant, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_workflow_round_trip() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: roundtrip-test
spec:
  initial_state: START
  states:
    START:
      kind: System
      command: echo "hello"
      transitions: []
"#;

        let workflow =
            WorkflowParser::parse_yaml(yaml).expect("round-trip source YAML should parse");
        let yaml_out = WorkflowParser::to_yaml(&workflow)
            .expect("workflow domain object should serialize to YAML");
        let workflow2 = WorkflowParser::parse_yaml(&yaml_out)
            .expect("re-serialized YAML should parse without error");

        assert_eq!(
            workflow.metadata.name, workflow2.metadata.name,
            "workflow name must survive round-trip serialization"
        );
        assert_eq!(
            workflow.spec.states.len(),
            workflow2.spec.states.len(),
            "state count must survive round-trip serialization"
        );
    }

    #[test]
    fn test_parse_container_run_state() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: ci-build
  version: "1.0.0"
spec:
  initial_state: build
  storage:
    shared_volumes:
      - name: cargo-cache
        storage_class: persistent
        size_limit_bytes: 2147483648
  states:
    build:
      kind: ContainerRun
      name: cargo-build
      image: "rust:1.75-alpine"
      command: ["cargo", "build", "--release"]
      env:
        CARGO_HOME: /cargo-cache
      workdir: /workspace
      volumes:
        - name: cargo-cache
          mount_path: /cargo-cache
          read_only: false
      resources:
        cpu: 2000
        memory: "4Gi"
        timeout: 10m
      retry:
        max_attempts: 2
        backoff: "5s"
      shell: false
      transitions: []
"#;

        let result = WorkflowParser::parse_yaml(yaml);
        assert!(result.is_ok(), "parse failed: {:?}", result.err());

        let workflow = result.unwrap();
        assert_eq!(workflow.metadata.name, "ci-build");
        assert_eq!(workflow.spec.storage.shared_volumes.len(), 1);
        assert_eq!(workflow.spec.storage.shared_volumes[0].name, "cargo-cache");

        let build_state = workflow
            .spec
            .states
            .values()
            .next()
            .expect("expected one state");

        match &build_state.kind {
            crate::domain::workflow::StateKind::ContainerRun {
                name,
                image,
                command,
                volumes,
                resources,
                retry,
                shell,
                ..
            } => {
                assert_eq!(name, "cargo-build");
                assert_eq!(image, "rust:1.75-alpine");
                assert_eq!(command, &["cargo", "build", "--release"]);
                assert_eq!(volumes.len(), 1);
                assert_eq!(volumes[0].name, "cargo-cache");
                assert_eq!(volumes[0].mount_path, "/cargo-cache");
                assert!(!volumes[0].read_only);
                let res = resources.as_ref().expect("resources should be present");
                assert_eq!(res.cpu, Some(2000));
                assert_eq!(res.memory.as_deref(), Some("4Gi"));
                let r = retry.as_ref().expect("retry should be present");
                assert_eq!(r.max_attempts, 2);
                assert_eq!(r.backoff.as_deref(), Some("5s"));
                assert!(!shell);
            }
            other => panic!("expected ContainerRun, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_parallel_container_run() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: parallel-tests
spec:
  initial_state: test
  states:
    test:
      kind: ParallelContainerRun
      completion: all_succeed
      steps:
        - name: unit-tests
          image: "rust:1.75-alpine"
          command: ["cargo", "test", "--lib"]
          shell: false
        - name: integration-tests
          image: "rust:1.75-alpine"
          command: ["cargo", "test", "--test", "*"]
          shell: false
      transitions: []
"#;

        let result = WorkflowParser::parse_yaml(yaml);
        assert!(result.is_ok(), "parse failed: {:?}", result.err());

        let workflow = result.unwrap();
        let state = workflow.spec.states.values().next().unwrap();

        match &state.kind {
            crate::domain::workflow::StateKind::ParallelContainerRun { steps, completion } => {
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].name, "unit-tests");
                assert_eq!(steps[1].name, "integration-tests");
                assert_eq!(
                    *completion,
                    crate::domain::workflow::ParallelCompletionStrategy::AllSucceed
                );
            }
            other => panic!("expected ParallelContainerRun, got {other:?}"),
        }
    }

    #[test]
    fn test_container_run_round_trip() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: container-roundtrip
spec:
  initial_state: build
  storage:
    shared_volumes:
      - name: workspace
        storage_class: ephemeral
  states:
    build:
      kind: ContainerRun
      name: docker-build
      image: "docker:24-dind"
      command: ["docker", "build", "-t", "myapp:latest", "."]
      env:
        DOCKER_BUILDKIT: "1"
      volumes:
        - name: workspace
          mount_path: /workspace
      shell: false
      transitions: []
"#;

        let workflow = WorkflowParser::parse_yaml(yaml).unwrap();
        let yaml_out = WorkflowParser::to_yaml(&workflow).unwrap();
        let workflow2 = WorkflowParser::parse_yaml(&yaml_out).unwrap();

        assert_eq!(workflow.metadata.name, workflow2.metadata.name);
        assert_eq!(
            workflow.spec.storage.shared_volumes.len(),
            workflow2.spec.storage.shared_volumes.len()
        );
        assert_eq!(
            workflow.spec.storage.shared_volumes[0].name,
            workflow2.spec.storage.shared_volumes[0].name
        );
        assert_eq!(workflow.spec.states.len(), workflow2.spec.states.len());

        // Verify ContainerRun fields survive the round-trip
        let state1 = workflow.spec.states.values().next().unwrap();
        let state2 = workflow2.spec.states.values().next().unwrap();
        match (&state1.kind, &state2.kind) {
            (
                crate::domain::workflow::StateKind::ContainerRun {
                    name: n1,
                    image: i1,
                    volumes: v1,
                    ..
                },
                crate::domain::workflow::StateKind::ContainerRun {
                    name: n2,
                    image: i2,
                    volumes: v2,
                    ..
                },
            ) => {
                assert_eq!(n1, n2);
                assert_eq!(i1, i2);
                assert_eq!(v1.len(), v2.len());
            }
            _ => panic!("both states should be ContainerRun after round-trip"),
        }
    }

    #[test]
    fn test_spec_volumes_validation_rejects_undeclared_mount() {
        // volume name in ContainerVolumeMount does not exist in spec.storage.shared_volumes
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: bad-volume-ref
spec:
  initial_state: build
  storage:
    shared_volumes:
      - name: declared-volume
        storage_class: ephemeral
  states:
    build:
      kind: ContainerRun
      name: step
      image: "alpine:3"
      command: ["ls"]
      volumes:
        - name: undeclared-volume
          mount_path: /data
      shell: false
      transitions: []
"#;

        let result = WorkflowParser::parse_yaml(yaml);
        assert!(
            result.is_err(),
            "Expected parse to fail for undeclared volume mount"
        );
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("undeclared-volume"),
            "Error message should mention the undeclared volume name, got: {err_str}"
        );
    }

    #[test]
    fn test_subworkflow_round_trip() {
        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: parent-workflow
  version: "1.0.0"
spec:
  initial_state: TRIGGER_CHILD
  states:
    TRIGGER_CHILD:
      kind: Subworkflow
      workflow_id: child-workflow
      mode: blocking
      result_key: child_output
      input: "{{workflow.context.task}}"
      transitions:
        - condition: on_success
          target: DONE
    DONE:
      kind: System
      command: "echo done"
      transitions: []
"#;
        let workflow = WorkflowParser::parse_yaml(yaml).expect("Should parse Subworkflow YAML");
        assert_eq!(workflow.metadata.name, "parent-workflow");

        // Verify Subworkflow state
        let state = workflow
            .spec
            .states
            .get(&crate::domain::workflow::StateName::new("TRIGGER_CHILD").unwrap())
            .expect("TRIGGER_CHILD state should exist");
        match &state.kind {
            StateKind::Subworkflow {
                workflow_id,
                mode,
                result_key,
                input,
            } => {
                assert_eq!(workflow_id, "child-workflow");
                assert_eq!(*mode, crate::domain::workflow::SubworkflowMode::Blocking);
                assert_eq!(result_key.as_deref(), Some("child_output"));
                assert_eq!(input.as_deref(), Some("{{workflow.context.task}}"));
            }
            other => panic!("Expected Subworkflow, got {:?}", other),
        }

        // Round-trip: convert back to YAML and re-parse
        let yaml_out = WorkflowParser::to_yaml(&workflow).expect("Should serialize to YAML");
        let reparsed = WorkflowParser::parse_yaml(&yaml_out).expect("Should re-parse from YAML");
        assert_eq!(reparsed.metadata.name, "parent-workflow");
    }
}
