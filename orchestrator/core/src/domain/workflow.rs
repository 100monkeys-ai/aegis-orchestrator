//! Workflow Domain Model
//!
//! This module defines the core domain entities and value objects for the Workflow Engine.
//! Workflows are declarative finite state machines (FSM) that orchestrate agent executions.
//!
//! # Architectural Context
//!
//! - **Bounded Context:** Execution Context
//! - **Aggregate Root:** Workflow
//! - **Related ADRs:** ADR-015 (Workflow Engine Architecture)
//! - **Specification:** WORKFLOW_MANIFEST_SPEC.md
//!
//! # Design Principles
//!
//! 1. **Immutability:** Workflow definitions are immutable once loaded
//! 2. **Domain-Driven:** Uses ubiquitous language (State, Transition, Blackboard)
//! 3. **Type Safety:** Strongly typed transitions and conditions
//! 4. **Self-Validating:** Constructors enforce invariants

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use uuid::Uuid;
use crate::domain::execution::{ExecutionId, ExecutionStatus};

// ============================================================================
// Value Objects: Identifiers
// ============================================================================

/// Unique identifier for a Workflow definition
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkflowId(pub Uuid);

impl WorkflowId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for WorkflowId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique name for a state within a workflow (e.g., "GENERATE", "VALIDATE")
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateName(String);

impl StateName {
    /// Create a new StateName with validation
    ///
    /// # Validation Rules
    /// - Must not be empty
    /// - Recommended: UPPERCASE_WITH_UNDERSCORES
    pub fn new(name: impl Into<String>) -> Result<Self, WorkflowError> {
        let name = name.into();
        if name.is_empty() {
            return Err(WorkflowError::InvalidStateName("State name cannot be empty".to_string()));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StateName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Aggregate Root: Workflow
// ============================================================================

/// Workflow Aggregate Root
///
/// Represents a complete workflow definition with metadata and state machine specification.
/// Workflows are immutable once created.
///
/// # Invariants
/// - Must have at least one state
/// - initial_state must reference an existing state
/// - All transition targets must reference existing states
/// - State names must be unique
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: WorkflowId,
    pub metadata: WorkflowMetadata,
    pub spec: WorkflowSpec,
    pub created_at: DateTime<Utc>,
}

impl Workflow {
    /// Create a new Workflow with validation
    pub fn new(metadata: WorkflowMetadata, spec: WorkflowSpec) -> Result<Self, WorkflowError> {
        // Validate: At least one state
        if spec.states.is_empty() {
            return Err(WorkflowError::NoStates);
        }

        // Validate: initial_state exists
        if !spec.states.contains_key(&spec.initial_state) {
            return Err(WorkflowError::InitialStateNotFound(
                spec.initial_state.clone(),
            ));
        }

        // Validate: All transition targets exist
        for (state_name, state) in &spec.states {
            for transition in &state.transitions {
                if !spec.states.contains_key(&transition.target) {
                    return Err(WorkflowError::TransitionTargetNotFound {
                        from_state: state_name.clone(),
                        target: transition.target.clone(),
                    });
                }
            }
        }

        Ok(Self {
            id: WorkflowId::new(),
            metadata,
            spec,
            created_at: Utc::now(),
        })
    }

    /// Get the initial state
    pub fn initial_state(&self) -> &WorkflowState {
        self.spec
            .states
            .get(&self.spec.initial_state)
            .expect("Invariant: initial_state must exist")
    }

    /// Get a state by name
    pub fn get_state(&self, name: &StateName) -> Option<&WorkflowState> {
        self.spec.states.get(name)
    }

    /// Check if a state is terminal (no outgoing transitions)
    pub fn is_terminal_state(&self, name: &StateName) -> bool {
        self.get_state(name)
            .map(|state| state.transitions.is_empty())
            .unwrap_or(false)
    }
}

// ============================================================================
// Entities: Workflow Components
// ============================================================================

/// Workflow metadata (Kubernetes-style)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowMetadata {
    /// Unique workflow name (DNS label format: lowercase, alphanumeric, hyphens)
    pub name: String,

    /// Optional semantic version
    pub version: Option<String>,

    /// Human-readable description
    pub description: Option<String>,

    /// Key-value labels for categorization
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Arbitrary annotations (non-identifying metadata)
    #[serde(default)]
    pub annotations: HashMap<String, String>,
}

impl WorkflowMetadata {
    /// Validate workflow name (DNS label format)
    pub fn validate_name(name: &str) -> Result<(), WorkflowError> {
        if name.is_empty() || name.len() > 63 {
            return Err(WorkflowError::InvalidWorkflowName(
                "Name must be 1-63 characters".to_string(),
            ));
        }

        // Must be lowercase alphanumeric + hyphens
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(WorkflowError::InvalidWorkflowName(
                "Name must be lowercase alphanumeric + hyphens".to_string(),
            ));
        }

        // Must start and end with alphanumeric
        if !name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
            || !name
                .chars()
                .last()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false)
        {
            return Err(WorkflowError::InvalidWorkflowName(
                "Name must start and end with alphanumeric".to_string(),
            ));
        }

        Ok(())
    }
}

/// Workflow specification (state machine definition)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSpec {
    /// Name of the initial state
    pub initial_state: StateName,

    /// Global context available to all states
    #[serde(default)]
    pub context: HashMap<String, serde_json::Value>,

    /// State machine definition (state_name -> state)
    pub states: HashMap<StateName, WorkflowState>,
}

/// Individual state in the workflow FSM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    /// State kind (Agent, System, Human, ParallelAgents)
    pub kind: StateKind,

    /// Transition rules (evaluated in order)
    pub transitions: Vec<TransitionRule>,

    /// Optional timeout for this state
    #[serde(default)]
    #[serde(with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

// ============================================================================
// Value Objects: State Kinds
// ============================================================================

/// State kind determines how the state is executed
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "PascalCase")]
pub enum StateKind {
    /// Execute an agent
    Agent {
        /// Agent identifier (name or ID)
        agent: String,

        /// Input template (Handlebars syntax)
        input: String,

        /// Optional isolation override
        #[serde(default)]
        isolation: Option<IsolationMode>,
    },

    /// Execute system command
    System {
        /// Shell command to execute
        command: String,

        /// Environment variables (supports templates)
        #[serde(default)]
        env: HashMap<String, String>,

        /// Working directory
        #[serde(default)]
        workdir: Option<String>,
    },

    /// Pause for human input
    Human {
        /// Prompt shown to human
        prompt: String,

        /// Default response if timeout
        #[serde(default)]
        default_response: Option<String>,
    },

    /// Execute multiple agents in parallel
    ParallelAgents {
        /// List of agents to execute
        agents: Vec<ParallelAgentConfig>,

        /// Consensus configuration
        consensus: ConsensusConfig,
    },
}

/// Isolation mode for agent execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMode {
    /// Inherit from node configuration
    Inherit,
    /// Force Firecracker micro-VM
    Firecracker,
    /// Force Docker container
    Docker,
    /// No isolation (dangerous, for debugging only)
    Process,
}

impl Default for IsolationMode {
    fn default() -> Self {
        Self::Inherit
    }
}

/// Configuration for a single agent in parallel execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelAgentConfig {
    /// Agent identifier
    pub agent: String,

    /// Input template
    pub input: String,

    /// Weight for consensus (default: 1.0)
    #[serde(default = "default_weight")]
    pub weight: f64,

    /// Timeout in seconds for this agent execution (default: 60)
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,

    /// Poll interval in milliseconds (default: 500)
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

/// Consensus strategy for parallel agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsensusConfig {
    /// Consensus strategy
    pub strategy: ConsensusStrategy,

    /// Minimum score threshold (for score-based strategies)
    #[serde(default)]
    pub threshold: Option<f64>,

    /// Minimum agreement confidence (0.0-1.0)
    /// Renamed from 'agreement' for clarity
    #[serde(default, alias = "agreement")]
    pub min_agreement_confidence: Option<f64>,

    /// For BestOfN strategy
    #[serde(default)]
    pub n: Option<usize>,

    /// Minimum number of judges that must succeed (default: 1)
    #[serde(default = "default_min_judges")]
    pub min_judges_required: usize,

    /// Confidence weighting factors for consensus calculation
    #[serde(default)]
    pub confidence_weighting: Option<ConfidenceWeighting>,
}

fn default_min_judges() -> usize {
    1
}

/// Weights for combining agreement and self-confidence in consensus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceWeighting {
    /// Weight for agreement among judges (default: 0.7)
    #[serde(default = "default_agreement_factor")]
    pub agreement_factor: f64,

    /// Weight for judges' self-confidence (default: 0.3)
    #[serde(default = "default_self_confidence_factor")]
    pub self_confidence_factor: f64,
}

impl Default for ConfidenceWeighting {
    fn default() -> Self {
        Self {
            agreement_factor: 0.7,
            self_confidence_factor: 0.3,
        }
    }
}

impl ConfidenceWeighting {
    /// Validate that weights sum to approximately 1.0
    pub fn validate(&self) -> Result<(), String> {
        let sum = self.agreement_factor + self.self_confidence_factor;
        if (sum - 1.0).abs() > 0.01 {
            return Err(format!(
                "Confidence weights must sum to 1.0, got {:.2} (agreement) + {:.2} (self-confidence) = {:.2}",
                self.agreement_factor, self.self_confidence_factor, sum
            ));
        }
        if self.agreement_factor < 0.0 || self.agreement_factor > 1.0 {
            return Err(format!("Agreement factor must be between 0.0 and 1.0, got {}", self.agreement_factor));
        }
        if self.self_confidence_factor < 0.0 || self.self_confidence_factor > 1.0 {
            return Err(format!("Self-confidence factor must be between 0.0 and 1.0, got {}", self.self_confidence_factor));
        }
        Ok(())
    }
}

fn default_agreement_factor() -> f64 {
    0.7
}

fn default_self_confidence_factor() -> f64 {
    0.3
}

/// Consensus strategy for aggregating parallel agent results
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsensusStrategy {
    /// Weighted average of scores
    WeightedAverage,
    /// Simple majority vote
    Majority,
    /// All agents must agree
    Unanimous,
    /// Take best N results
    BestOfN,
}

// ============================================================================
// Value Objects: Transitions
// ============================================================================

/// Transition rule from one state to another
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRule {
    /// Condition to evaluate
    pub condition: TransitionCondition,

    /// Target state name
    pub target: StateName,

    /// Optional feedback message (available as {{state.feedback}})
    #[serde(default)]
    pub feedback: Option<String>,
}

/// Condition types for state transitions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum TransitionCondition {
    /// Always transition (unconditional)
    Always,

    /// Transition if state succeeded
    OnSuccess,

    /// Transition if state failed
    OnFailure,

    /// System state exit code is 0
    ExitCodeZero,

    /// System state exit code is non-zero
    ExitCodeNonZero,

    /// System state exit code equals specific value
    ExitCode { value: i32 },

    /// Validation score above threshold
    ScoreAbove { threshold: f64 },

    /// Validation score below threshold
    ScoreBelow { threshold: f64 },

    /// Validation score between min and max
    ScoreBetween { min: f64, max: f64 },

    /// Confidence above threshold
    ConfidenceAbove { threshold: f64 },

    /// Consensus reached (for ParallelAgents)
    Consensus {
        threshold: f64,
        agreement: f64,
    },

    /// All parallel agents approved
    AllApproved,

    /// Any parallel agent rejected
    AnyRejected,

    /// Human input equals specific value
    InputEquals { value: String },

    /// Human input equals "yes"
    InputEqualsYes,

    /// Human input equals "no"
    InputEqualsNo,

    /// Custom boolean expression (Handlebars)
    Custom { expression: String },
}

// ============================================================================
// Value Objects: Blackboard (Shared Context)
// ============================================================================

/// Blackboard: Shared mutable state for workflow execution
///
/// The blackboard stores execution context that can be read/written by states.
/// Values are accessible via Handlebars templates: {{blackboard.key}}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Blackboard {
    data: HashMap<String, serde_json::Value>,
}

impl Blackboard {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Get a value from the blackboard
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.data.get(key)
    }

    /// Set a value in the blackboard
    pub fn set(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.data.insert(key.into(), value);
    }

    /// Remove a value from the blackboard
    pub fn remove(&mut self, key: &str) -> Option<serde_json::Value> {
        self.data.remove(key)
    }

    /// Check if a key exists
    pub fn contains_key(&self, key: &str) -> bool {
        self.data.contains_key(key)
    }

    /// Get all data as a reference
    pub fn data(&self) -> &HashMap<String, serde_json::Value> {
        &self.data
    }

    /// Clear all data
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Merge another blackboard into this one
    pub fn merge(&mut self, other: &Blackboard) {
        for (key, value) in &other.data {
            self.data.insert(key.clone(), value.clone());
        }
    }
}

// ============================================================================
// Entities: Workflow Execution State
// ============================================================================

/// Execution instance of a workflow (runtime state)
///
/// This is NOT part of the Workflow aggregate - it's part of the Execution aggregate.
/// Stored here for convenience but belongs to Execution Context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    /// Unique execution ID
    pub id: ExecutionId,

    /// Workflow being executed
    pub workflow_id: WorkflowId,

    /// Execution status
    pub status: ExecutionStatus,

    /// Current state name
    pub current_state: StateName,

    /// Shared context (blackboard)
    pub blackboard: Blackboard,

    /// Input parameters
    pub input: serde_json::Value,

    /// State outputs (state_name -> output_json)
    pub state_outputs: HashMap<StateName, serde_json::Value>,

    /// Execution started at
    pub started_at: DateTime<Utc>,

    /// Last state transition at
    pub last_transition_at: DateTime<Utc>,
}

impl WorkflowExecution {
    pub fn new(workflow: &Workflow, id: ExecutionId, input: serde_json::Value) -> Self {
        let initial_state = workflow.spec.initial_state.clone();
        let now = Utc::now();

        Self {
            id,
            workflow_id: workflow.id,
            status: ExecutionStatus::Running,
            current_state: initial_state,
            blackboard: Blackboard::new(),
            input,
            state_outputs: HashMap::new(),
            started_at: now,
            last_transition_at: now,
        }
    }

    /// Transition to a new state
    pub fn transition_to(&mut self, target: StateName) {
        self.current_state = target;
        self.last_transition_at = Utc::now();
    }

    /// Record output from a state
    pub fn record_state_output(&mut self, state: StateName, output: serde_json::Value) {
        self.state_outputs.insert(state, output);
    }

    /// Get output from a previous state
    pub fn get_state_output(&self, state: &StateName) -> Option<&serde_json::Value> {
        self.state_outputs.get(state)
    }
}

// ============================================================================
// Domain Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("Workflow must have at least one state")]
    NoStates,

    #[error("Initial state '{0}' not found in workflow states")]
    InitialStateNotFound(StateName),

    #[error("Transition target '{target}' not found (from state '{from_state}')")]
    TransitionTargetNotFound {
        from_state: StateName,
        target: StateName,
    },

    #[error("Invalid workflow name: {0}")]
    InvalidWorkflowName(String),

    #[error("Invalid state name: {0}")]
    InvalidStateName(String),

    #[error("State '{0}' not found in workflow")]
    StateNotFound(StateName),

    #[error("Workflow execution error: {0}")]
    ExecutionError(String),

    #[error("Invalid API version: expected '100monkeys.ai/v1', got '{0}'")]
    InvalidApiVersion(String),

    #[error("Invalid kind: expected 'Workflow', got '{0}'")]
    InvalidKind(String),

    #[error("Template rendering error: {0}")]
    TemplateError(String),

    #[error("Timeout exceeded for state '{0}'")]
    TimeoutExceeded(StateName),
}

// ============================================================================
// Domain Services (if needed)
// ============================================================================

/// Domain service for workflow validation
pub struct WorkflowValidator;

impl WorkflowValidator {
    /// Validate workflow for circular references (simple cycle detection)
    pub fn check_for_cycles(workflow: &Workflow) -> Result<(), WorkflowError> {
        // Simple DFS to detect cycles
        fn visit(
            current: &StateName,
            workflow: &Workflow,
            visited: &mut HashMap<StateName, bool>,
            rec_stack: &mut HashMap<StateName, bool>,
        ) -> bool {
            visited.insert(current.clone(), true);
            rec_stack.insert(current.clone(), true);

            if let Some(state) = workflow.get_state(current) {
                for transition in &state.transitions {
                    if !visited.get(&transition.target).copied().unwrap_or(false) {
                        if visit(&transition.target, workflow, visited, rec_stack) {
                            return true;
                        }
                    } else if rec_stack.get(&transition.target).copied().unwrap_or(false) {
                        return true; // Cycle detected
                    }
                }
            }

            rec_stack.insert(current.clone(), false);
            false
        }

        let mut visited = HashMap::new();
        let mut rec_stack = HashMap::new();

        if visit(
            &workflow.spec.initial_state,
            workflow,
            &mut visited,
            &mut rec_stack,
        ) {
            return Err(WorkflowError::ExecutionError(
                "Circular reference detected in workflow".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workflow_id_creation() {
        let id1 = WorkflowId::new();
        let id2 = WorkflowId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_state_name_validation() {
        assert!(StateName::new("GENERATE").is_ok());
        assert!(StateName::new("").is_err());
    }

    #[test]
    fn test_workflow_name_validation() {
        assert!(WorkflowMetadata::validate_name("my-workflow").is_ok());
        assert!(WorkflowMetadata::validate_name("100monkeys-classic").is_ok());
        assert!(WorkflowMetadata::validate_name("My-Workflow").is_err()); // Uppercase
        assert!(WorkflowMetadata::validate_name("my_workflow").is_err()); // Underscore
        assert!(WorkflowMetadata::validate_name("-invalid").is_err()); // Starts with hyphen
    }

    #[test]
    fn test_blackboard_operations() {
        let mut blackboard = Blackboard::new();
        
        blackboard.set("iteration", serde_json::json!(1));
        assert_eq!(blackboard.get("iteration"), Some(&serde_json::json!(1)));
        
        blackboard.set("iteration", serde_json::json!(2));
        assert_eq!(blackboard.get("iteration"), Some(&serde_json::json!(2)));
        
        blackboard.remove("iteration");
        assert_eq!(blackboard.get("iteration"), None);
    }

    #[test]
    fn test_workflow_validation_no_states() {
        let metadata = WorkflowMetadata {
            name: "test".to_string(),
            version: None,
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        };

        let spec = WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states: HashMap::new(),
        };

        let result = Workflow::new(metadata, spec);
        assert!(matches!(result, Err(WorkflowError::NoStates)));
    }

    #[test]
    fn test_workflow_validation_initial_state_not_found() {
        let metadata = WorkflowMetadata {
            name: "test".to_string(),
            version: None,
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
        };

        let mut states = HashMap::new();
        states.insert(
            StateName::new("OTHER").unwrap(),
            WorkflowState {
                kind: StateKind::System {
                    command: "echo".to_string(),
                    env: HashMap::new(),
                    workdir: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );

        let spec = WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states,
        };

        let result = Workflow::new(metadata, spec);
        assert!(matches!(
            result,
            Err(WorkflowError::InitialStateNotFound(_))
        ));
    }
}
