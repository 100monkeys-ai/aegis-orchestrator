//! Workflow YAML Parser
//!
//! This module provides infrastructure for parsing workflow YAML manifests
//! into domain objects.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Parse external YAML → Domain objects
//! - **Anti-Corruption:** Translates YAML schema to domain model
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: WorkflowMetadataYaml,
    pub spec: WorkflowSpecYaml,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowMetadataYaml {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpecYaml {
    pub initial_state: String,
    #[serde(default)]
    pub context: HashMap<String, serde_json::Value>,
    pub states: HashMap<String, WorkflowStateYaml>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStateYaml {
    #[serde(flatten)]
    pub kind: StateKindYaml,
    pub transitions: Vec<TransitionRuleYaml>,
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StateKindYaml {
    Agent {
        agent: String,
        input: String,
        #[serde(default)]
        isolation: Option<IsolationMode>,
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
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelAgentConfigYaml {
    pub agent: String,
    pub input: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
}

fn default_weight() -> f64 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusConfigYaml {
    pub strategy: ConsensusStrategy,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub agreement: Option<f64>,
    #[serde(default)]
    pub n: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRuleYaml {
    #[serde(flatten)]
    pub condition: TransitionConditionYaml,
    pub target: String,
    #[serde(default)]
    pub feedback: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum TransitionConditionYaml {
    Always,
    OnSuccess,
    OnFailure,
    ExitCode0,
    ExitCodeNonZero,
    ExitCode { value: i32 },
    ScoreAbove { threshold: f64 },
    ScoreBelow { threshold: f64 },
    ScoreBetween { min: f64, max: f64 },
    ConfidenceAbove { threshold: f64 },
    Consensus { threshold: f64, agreement: f64 },
    AllApproved,
    AnyRejected,
    InputEquals { value: String },
    InputEqualsYes,
    InputEqualsNo,
    Custom { expression: String },
}

// ============================================================================
// Parser
// ============================================================================

/// Workflow parser (Infrastructure service)
pub struct WorkflowParser;

impl WorkflowParser {
    /// Parse a workflow manifest from YAML file
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<Workflow, WorkflowParseError> {
        let content = fs::read_to_string(path.as_ref()).map_err(|e| {
            WorkflowParseError::IoError {
                path: path.as_ref().display().to_string(),
                error: e.to_string(),
            }
        })?;

        Self::parse_yaml(&content)
    }

    /// Parse a workflow manifest from YAML string
    pub fn parse_yaml(yaml: &str) -> Result<Workflow, WorkflowParseError> {
        let manifest: WorkflowManifest = serde_yaml::from_str(yaml)
            .map_err(|e| WorkflowParseError::YamlError(e.to_string()))?;

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
            WorkflowParseError::ValidationError(format!("Invalid workflow name: {}", e))
        })?;

        let metadata = WorkflowMetadata {
            name: manifest.metadata.name,
            version: manifest.metadata.version,
            description: manifest.metadata.description,
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
                .map(|t| Self::convert_transition(t))
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
            } => StateKind::Agent {
                agent,
                input,
                isolation,
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
            StateKindYaml::ParallelAgents { agents, consensus } => {
                let agent_configs = agents
                    .into_iter()
                    .map(|a| ParallelAgentConfig {
                        agent: a.agent,
                        input: a.input,
                        weight: a.weight,
                    })
                    .collect();

                let consensus_config = ConsensusConfig {
                    strategy: consensus.strategy,
                    threshold: consensus.threshold,
                    agreement: consensus.agreement,
                    n: consensus.n,
                };

                StateKind::ParallelAgents {
                    agents: agent_configs,
                    consensus: consensus_config,
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
            TransitionConditionYaml::ExitCode0 => TransitionCondition::ExitCode0,
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
                        .map(|t| Self::transition_to_yaml(t))
                        .collect(),
                    timeout: state.timeout,
                },
            );
        }

        let spec = WorkflowSpecYaml {
            initial_state: workflow.spec.initial_state.as_str().to_string(),
            context: workflow.spec.context.clone(),
            states,
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
            } => StateKindYaml::Agent {
                agent: agent.clone(),
                input: input.clone(),
                isolation: *isolation,
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
            StateKind::ParallelAgents { agents, consensus } => StateKindYaml::ParallelAgents {
                agents: agents
                    .iter()
                    .map(|a| ParallelAgentConfigYaml {
                        agent: a.agent.clone(),
                        input: a.input.clone(),
                        weight: a.weight,
                    })
                    .collect(),
                consensus: ConsensusConfigYaml {
                    strategy: consensus.strategy,
                    threshold: consensus.threshold,
                    agreement: consensus.agreement,
                    n: consensus.n,
                },
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
            TransitionCondition::ExitCode0 => TransitionConditionYaml::ExitCode0,
            TransitionCondition::ExitCodeNonZero => TransitionConditionYaml::ExitCodeNonZero,
            TransitionCondition::ExitCode { value } => {
                TransitionConditionYaml::ExitCode { value: *value }
            }
            TransitionCondition::ScoreAbove { threshold } => {
                TransitionConditionYaml::ScoreAbove {
                    threshold: *threshold,
                }
            }
            TransitionCondition::ScoreBelow { threshold } => {
                TransitionConditionYaml::ScoreBelow {
                    threshold: *threshold,
                }
            }
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
        assert!(result.is_ok());

        let workflow = result.unwrap();
        assert_eq!(workflow.metadata.name, "test-workflow");
        assert_eq!(workflow.spec.states.len(), 2);
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
        assert!(matches!(
            result,
            Err(WorkflowParseError::InvalidApiVersion { .. })
        ));
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

        let workflow = WorkflowParser::parse_yaml(yaml).unwrap();
        let yaml_out = WorkflowParser::to_yaml(&workflow).unwrap();
        let workflow2 = WorkflowParser::parse_yaml(&yaml_out).unwrap();

        assert_eq!(workflow.metadata.name, workflow2.metadata.name);
        assert_eq!(workflow.spec.states.len(), workflow2.spec.states.len());
    }
}
