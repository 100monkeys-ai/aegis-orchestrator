// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
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
//! - **Specification:** WORKFLOW_MANIFEST_SPEC_V1.md
//!
//! # Design Principles
//!
//! 1. **Immutability:** Workflow definitions are immutable once loaded
//! 2. **Domain-Driven:** Uses ubiquitous language (State, Transition, Blackboard)
//! 3. **Type Safety:** Strongly typed transitions and conditions
//! 4. **Self-Validating:** Constructors enforce invariants
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements internal responsibilities for workflow
//!
//! # Code Quality Principles
//!
//! - Keep workflow definitions immutable after validation.
//! - Enforce FSM invariants in constructors and transitions, not in callers.
//! - Avoid transport, persistence, or runtime-specific concerns in the domain model.

pub use crate::domain::shared_kernel::WorkflowId;

use crate::domain::agent::ImagePullPolicy;
use crate::domain::execution::{ExecutionId, ExecutionStatus};
use crate::domain::tenant::TenantId;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

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
            return Err(WorkflowError::InvalidStateName(
                "State name cannot be empty".to_string(),
            ));
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

/// Visibility scope for a workflow definition (ADR-076).
///
/// Scope determines which principals can discover and execute a workflow.
/// Scope is mutable via explicit promote/demote operations (not via save).
///
/// Hierarchy (broadest to narrowest): Global > Tenant > User
///
/// Invariants:
/// - `Global` workflows MUST have `tenant_id == TenantId::system()`.
/// - `Tenant` workflows have no `owner_user_id`.
/// - `User` workflows MUST have a non-None `owner_user_id`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkflowScope {
    /// Platform-wide: visible to all tenants, managed by operators.
    /// Stored with `tenant_id = "aegis-system"`.
    Global,

    /// Tenant-wide: visible to all users within the owning tenant.
    #[default]
    Tenant,

    /// User-private: visible only to the owning user within their tenant.
    User {
        /// OIDC `sub` claim identifying the owning user.
        owner_user_id: String,
    },
}

impl WorkflowScope {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            WorkflowScope::Global => "global",
            WorkflowScope::Tenant => "tenant",
            WorkflowScope::User { .. } => "user",
        }
    }

    /// Extract the owner_user_id if this is a User scope, else None.
    pub fn owner_user_id(&self) -> Option<&str> {
        match self {
            WorkflowScope::User { owner_user_id } => Some(owner_user_id.as_str()),
            _ => None,
        }
    }
}

impl std::fmt::Display for WorkflowScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_db_str())
    }
}

impl std::str::FromStr for WorkflowScope {
    type Err = WorkflowError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "global" => Ok(WorkflowScope::Global),
            "tenant" => Ok(WorkflowScope::Tenant),
            _ => Err(WorkflowError::InvalidScope(s.to_string())),
        }
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
    #[serde(default)]
    pub tenant_id: TenantId,
    #[serde(default)]
    pub scope: WorkflowScope,
    pub metadata: WorkflowMetadata,
    pub spec: WorkflowSpec,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
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

        // Validate: Container state invariants (ADR-050)
        for (state_name, state) in &spec.states {
            match &state.kind {
                StateKind::ContainerRun {
                    name,
                    image,
                    command,
                    volumes,
                    ..
                } => {
                    if name.trim().is_empty() {
                        return Err(WorkflowError::InvalidContainerState {
                            state: state_name.clone(),
                            detail: "ContainerRun.name cannot be empty".to_string(),
                        });
                    }
                    if image.trim().is_empty() {
                        return Err(WorkflowError::InvalidContainerState {
                            state: state_name.clone(),
                            detail: "ContainerRun.image cannot be empty".to_string(),
                        });
                    }
                    if command.is_empty() {
                        return Err(WorkflowError::InvalidContainerState {
                            state: state_name.clone(),
                            detail: "ContainerRun.command must contain at least one token"
                                .to_string(),
                        });
                    }

                    for mount in volumes {
                        if mount.name.trim().is_empty() {
                            return Err(WorkflowError::InvalidContainerState {
                                state: state_name.clone(),
                                detail: "ContainerRun volume name cannot be empty".to_string(),
                            });
                        }
                        if mount.mount_path.trim().is_empty() || !mount.mount_path.starts_with('/')
                        {
                            return Err(WorkflowError::InvalidContainerState {
                                state: state_name.clone(),
                                detail: format!(
                                    "ContainerRun mount_path '{}' must be an absolute path",
                                    mount.mount_path
                                ),
                            });
                        }
                    }
                }
                StateKind::ParallelContainerRun { steps, .. } => {
                    if steps.is_empty() {
                        return Err(WorkflowError::InvalidContainerState {
                            state: state_name.clone(),
                            detail: "ParallelContainerRun.steps must contain at least one step"
                                .to_string(),
                        });
                    }

                    let mut step_names = std::collections::HashSet::new();
                    for step in steps {
                        if step.name.trim().is_empty() {
                            return Err(WorkflowError::InvalidContainerState {
                                state: state_name.clone(),
                                detail: "ParallelContainerRun step name cannot be empty"
                                    .to_string(),
                            });
                        }
                        if !step_names.insert(step.name.clone()) {
                            return Err(WorkflowError::InvalidContainerState {
                                state: state_name.clone(),
                                detail: format!(
                                    "ParallelContainerRun step name '{}' must be unique",
                                    step.name
                                ),
                            });
                        }
                        if step.image.trim().is_empty() {
                            return Err(WorkflowError::InvalidContainerState {
                                state: state_name.clone(),
                                detail: format!(
                                    "ParallelContainerRun step '{}' has empty image",
                                    step.name
                                ),
                            });
                        }
                        if step.command.is_empty() {
                            return Err(WorkflowError::InvalidContainerState {
                                state: state_name.clone(),
                                detail: format!(
                                    "ParallelContainerRun step '{}' command must contain at least one token",
                                    step.name
                                ),
                            });
                        }
                        for mount in &step.volumes {
                            if mount.name.trim().is_empty() {
                                return Err(WorkflowError::InvalidContainerState {
                                    state: state_name.clone(),
                                    detail: format!(
                                        "ParallelContainerRun step '{}' has empty volume name",
                                        step.name
                                    ),
                                });
                            }
                            if mount.mount_path.trim().is_empty()
                                || !mount.mount_path.starts_with('/')
                            {
                                return Err(WorkflowError::InvalidContainerState {
                                    state: state_name.clone(),
                                    detail: format!(
                                        "ParallelContainerRun step '{}' has non-absolute mount_path '{}'",
                                        step.name, mount.mount_path
                                    ),
                                });
                            }
                        }
                    }
                }
                StateKind::Subworkflow {
                    workflow_id,
                    mode,
                    result_key,
                    ..
                } => {
                    if workflow_id.trim().is_empty() {
                        return Err(WorkflowError::InvalidSubworkflowState {
                            state: state_name.clone(),
                            detail: "Subworkflow.workflow_id cannot be empty".to_string(),
                        });
                    }
                    match mode {
                        SubworkflowMode::Blocking => {
                            if result_key.is_none() {
                                return Err(WorkflowError::InvalidSubworkflowState {
                                    state: state_name.clone(),
                                    detail: "Subworkflow in blocking mode requires a result_key"
                                        .to_string(),
                                });
                            }
                        }
                        SubworkflowMode::FireAndForget => {
                            if result_key.is_some() {
                                return Err(WorkflowError::InvalidSubworkflowState {
                                    state: state_name.clone(),
                                    detail:
                                        "Subworkflow in fire_and_forget mode must not specify result_key"
                                            .to_string(),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Validate: All ContainerVolumeMount names resolve to declared spec.storage.shared_volumes
        // (only enforced when spec.storage.shared_volumes is non-empty — opt-in per ADR-050)
        if !spec.storage.shared_volumes.is_empty() {
            let declared: std::collections::HashSet<&str> = spec
                .storage
                .shared_volumes
                .iter()
                .map(|v| v.name.as_str())
                .collect();

            for (state_name, state) in &spec.states {
                let mounts: Vec<&str> = match &state.kind {
                    StateKind::ContainerRun { volumes, .. } => {
                        volumes.iter().map(|m| m.name.as_str()).collect()
                    }
                    StateKind::ParallelContainerRun { steps, .. } => steps
                        .iter()
                        .flat_map(|s| s.volumes.iter().map(|m| m.name.as_str()))
                        .collect(),
                    _ => vec![],
                };

                for mount_name in mounts {
                    if !declared.contains(mount_name) {
                        return Err(WorkflowError::UndeclaredVolume {
                            state: state_name.clone(),
                            volume_name: mount_name.to_string(),
                        });
                    }
                }
            }
        }

        Ok(Self {
            id: WorkflowId::new(),
            tenant_id: TenantId::default(),
            scope: WorkflowScope::default(),
            metadata,
            spec,
            created_at: Utc::now(),
            updated_at: None,
        })
    }

    /// Return all judge-agent references used anywhere in the workflow.
    ///
    /// This returns a single, deduplicated list of agent identifiers drawn from:
    /// - state-level semantic judges (`judges` on `StateKind::Agent`),
    /// - pre-execution validators (`pre_execution_validator` on `StateKind::Agent`),
    /// - judges for parallel outputs (`judges_for_parallel` on `StateKind::ParallelAgents`).
    ///
    /// The different judge roles are *not* distinguished in the result; callers only
    /// get the unique set of agent IDs, sorted to keep dependency checks deterministic.
    pub fn referenced_judge_agents(&self) -> Vec<String> {
        let mut judge_names = BTreeSet::new();

        for state in self.spec.states.values() {
            match &state.kind {
                StateKind::Agent {
                    judges,
                    pre_execution_validator,
                    ..
                } => {
                    for judge in judges {
                        judge_names.insert(judge.agent_id.clone());
                    }
                    if let Some(judge) = pre_execution_validator {
                        judge_names.insert(judge.clone());
                    }
                }
                StateKind::ParallelAgents {
                    judges_for_parallel,
                    ..
                } => {
                    for judge in judges_for_parallel {
                        judge_names.insert(judge.agent_id.clone());
                    }
                }
                _ => {}
            }
        }

        judge_names.into_iter().collect()
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

    /// Free-form tags for categorization and discovery
    #[serde(default)]
    pub tags: Vec<String>,

    /// Key-value labels for categorization
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Arbitrary annotations (non-identifying metadata)
    #[serde(default)]
    pub annotations: HashMap<String, String>,

    /// Optional JSON Schema describing workflow execution inputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
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

/// Storage class for a workflow-level volume declaration
///
/// Workflows may declare named volumes in `spec.volumes`; these are referenced
/// by `ContainerVolumeMount.name` inside `ContainerRun` and `ParallelContainerRun` states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStorageClass {
    /// Ephemeral volume — auto-cleaned after workflow execution ends
    #[default]
    Ephemeral,
    /// Persistent volume — survives workflow execution boundaries
    Persistent,
}

/// Declarative volume specification at the workflow level (ADR-050)
///
/// Volumes declared here are matched by name in `ContainerVolumeMount` entries.
/// Volume provisioning itself is handled by the Storage Gateway Context (ADR-036/ADR-032);
/// this declaration is the intent layer consumed by the manifest parser and Temporal mapper.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowVolumeSpec {
    /// Logical name referenced by `ContainerVolumeMount.name`
    pub name: String,

    /// Storage class governing lifecycle (default: ephemeral)
    #[serde(default)]
    pub storage_class: WorkflowStorageClass,

    /// Optional hard quota in bytes enforced by the Storage Gateway
    #[serde(default)]
    pub size_limit_bytes: Option<u64>,
}

/// Workflow-level storage configuration (WORKFLOW_MANIFEST_SPEC_V1 §spec.storage).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct WorkflowStorageSpec {
    /// Default shared workspace volume, auto-created at workflow start if defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkflowWorkspaceSpec>,

    /// Additional named shared volumes accessible by multiple states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_volumes: Vec<WorkflowVolumeSpec>,
}

/// Specifies how a workflow execution manages its workspace volume (ADR-087).
/// Follows the spec.storage.workspace schema from WORKFLOW_MANIFEST_SPEC_V1.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowWorkspaceSpec {
    /// Storage class: "ephemeral" (auto-deleted after TTL) or "persistent" (manual deletion).
    #[serde(default)]
    pub storage_class: WorkflowStorageClass,

    /// Time-to-live in hours for ephemeral workspaces.
    #[serde(default = "default_workspace_ttl_hours")]
    pub ttl_hours: u32,

    /// Maximum size in megabytes.
    #[serde(default = "default_workspace_size_limit_mb")]
    pub size_limit_mb: u64,

    /// Pre-created volume UUID (persistent only). Rejected for Free tier at policy layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_id: Option<String>,

    /// Blackboard key for the resolved volume ID.
    #[serde(default = "default_workspace_blackboard_key")]
    pub blackboard_key: String,
}

fn default_workspace_ttl_hours() -> u32 {
    1
}

fn default_workspace_size_limit_mb() -> u64 {
    256
}

fn default_workspace_blackboard_key() -> String {
    "workspace_volume_id".to_string()
}

/// Input schema for the aegis.execute.intent tool (ADR-087).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentExecutionInput {
    pub intent: String,
    #[serde(default)]
    pub inputs: serde_json::Value,
    #[serde(default)]
    pub volume_id: Option<String>,
    #[serde(default)]
    pub language: ExecutionLanguage,
    #[serde(default)]
    pub timeout_seconds: Option<u32>,
}

/// Supported execution languages for the intent-to-execution pipeline (ADR-087).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionLanguage {
    #[default]
    Python,
    JavaScript,
    Bash,
}

impl ExecutionLanguage {
    pub fn container_image(&self) -> &'static str {
        match self {
            Self::Python => "python:3.11-slim",
            Self::JavaScript => "node:20-slim",
            Self::Bash => "ubuntu:22.04",
        }
    }
    pub fn runner_command(&self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::JavaScript => "node",
            Self::Bash => "bash",
        }
    }
    pub fn file_extension(&self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::JavaScript => "js",
            Self::Bash => "sh",
        }
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

    /// Workflow-level storage configuration (WORKFLOW_MANIFEST_SPEC_V1 §spec.storage).
    ///
    /// Named volumes for `ContainerRun` and `ParallelContainerRun` states are declared
    /// under `storage.shared_volumes`.
    #[serde(default, skip_serializing_if = "is_default_storage")]
    pub storage: WorkflowStorageSpec,
}

fn is_default_storage(s: &WorkflowStorageSpec) -> bool {
    s.workspace.is_none() && s.shared_volumes.is_empty()
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

// ─── CI/CD Container Value Objects (ADR-050) ─────────────────────────────────

// ContainerVolumeMount and ContainerResources are owned by BC-2 (Execution/Runtime)
// and re-exported here for backward compatibility with existing consumers.
pub use crate::domain::runtime::{ContainerResources, ContainerVolumeMount};

/// Retry configuration for a ContainerRun step
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RetryConfig {
    /// Maximum number of additional attempts after the first (0 = no retries)
    #[serde(default)]
    pub max_attempts: u32,

    /// Initial backoff between retries as a human-readable duration (e.g., "5s", "1m")
    #[serde(default)]
    pub backoff: Option<String>,
}

/// Configuration for a single step in ParallelContainerRun
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContainerRunConfig {
    /// Unique label for this step; used as the blackboard key for its output
    pub name: String,

    /// Container image reference (RegistryImage or StandardRuntime alias)
    pub image: String,

    /// Shell command or argv to execute inside the container
    pub command: Vec<String>,

    /// Environment variables (Handlebars templates evaluated against the blackboard)
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory inside the container (default: /workspace)
    #[serde(default)]
    pub workdir: Option<String>,

    /// Volume mounts — accessed via the NFS Server Gateway (ADR-036)
    #[serde(default)]
    pub volumes: Vec<ContainerVolumeMount>,

    /// CPU, memory, and timeout resource limits
    #[serde(default)]
    pub resources: Option<ContainerResources>,

    /// Secret-backend path for registry credentials (e.g. "secret:cicd/docker-registry")
    #[serde(default)]
    pub registry_credentials: Option<String>,

    /// When true the command is joined with spaces and executed via `sh -c`
    #[serde(default)]
    pub shell: bool,
}

/// Aggregation strategy for a ParallelContainerRun state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParallelCompletionStrategy {
    /// All steps must exit with code 0 — any non-zero exit causes the state to fail
    AllSucceed,
    /// At least one step must exit with code 0 — all-failed causes the state to fail
    AnySucceed,
    /// Always succeeds as a state; individual step failures are reflected in output
    BestEffort,
}

/// Execution mode for a subworkflow invocation (ADR-065)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubworkflowMode {
    /// Parent waits for child workflow to complete before continuing
    Blocking,
    /// Parent continues immediately; child runs independently
    FireAndForget,
}

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

        /// Judge agents that validate each iteration output (ADR-016 / ADR-017)
        #[serde(default)]
        judges: Vec<JudgeConfig>,

        /// Maximum iterations for the 100monkeys inner refinement loop
        /// (overrides global default of 10)
        #[serde(default)]
        max_iterations: Option<u32>,

        /// Optional judge agent ID used as a **pre-execution** semantic tool validator
        /// (ADR-049 Pillar 1). This runs before tool calls are executed to validate the
        /// proposed tool usage and is distinct from `judges`, which evaluate iteration
        /// outputs after execution in the inner refinement loop.
        #[serde(default)]
        pre_execution_validator: Option<String>,
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

        /// External judge agents for validating the combined parallel output (ADR-016)
        /// These must be distinct from the workers in `agents` (agents cannot judge themselves)
        #[serde(default)]
        judges_for_parallel: Vec<JudgeConfig>,
    },

    /// Execute a deterministic command in an isolated container without an LLM loop (ADR-050)
    ///
    /// Suitable for CI/CD steps such as `cargo build`, `docker push`, `npm test`.
    /// No bootstrap.py is injected; no AEGIS_ORCHESTRATOR_URL is set.
    /// Volumes are accessed via the NFS Server Gateway (ADR-036).
    ContainerRun {
        /// Human-readable label for this step (used in events and Synapse UI)
        name: String,

        /// Container image reference — RegistryImage or StandardRuntime alias
        /// (e.g., "rust:1.75-alpine", "ghcr.io/myorg/builder:v2")
        image: String,

        /// Image pull policy (default: IfNotPresent)
        #[serde(default)]
        image_pull_policy: Option<ImagePullPolicy>,

        /// Argv to execute inside the container
        command: Vec<String>,

        /// Environment variables (Handlebars templates evaluated against the blackboard)
        #[serde(default)]
        env: HashMap<String, String>,

        /// Working directory inside the container (default: /workspace)
        #[serde(default)]
        workdir: Option<String>,

        /// Volume mounts — accessed via the NFS Server Gateway (ADR-036)
        #[serde(default)]
        volumes: Vec<ContainerVolumeMount>,

        /// CPU, memory, and timeout resource limits
        #[serde(default)]
        resources: Option<ContainerResources>,

        /// Secret-backend path for private registry credentials (ADR-034, ADR-045)
        #[serde(default)]
        registry_credentials: Option<String>,

        /// Retry configuration (overrides Temporal activity default)
        #[serde(default)]
        retry: Option<RetryConfig>,

        /// When true the command entries are joined and passed to `sh -c`
        #[serde(default)]
        shell: bool,
    },

    /// Execute multiple container steps concurrently within a single workflow state (ADR-050)
    ///
    /// Results are aggregated according to `completion`. Each step's output is
    /// stored on the blackboard under `STATE_NAME.output.<step_name>`.
    ParallelContainerRun {
        /// Ordered list of container steps to run concurrently
        steps: Vec<ContainerRunConfig>,

        /// How to aggregate the individual step results into a state outcome
        completion: ParallelCompletionStrategy,
    },

    /// Invoke another workflow as a child execution (ADR-065)
    ///
    /// In `Blocking` mode the parent waits for the child to complete and writes
    /// the child result to `parent.blackboard[result_key]`. In `FireAndForget`
    /// mode the parent continues immediately and `result_key` must be `None`.
    Subworkflow {
        /// The workflow to invoke (name or UUID)
        workflow_id: String,

        /// Execution mode: blocking (wait) or fire-and-forget (continue)
        mode: SubworkflowMode,

        /// Blackboard key where child result is stored (required for blocking, forbidden for fire-and-forget)
        #[serde(default)]
        result_key: Option<String>,

        /// Optional input template (Handlebars) evaluated against the parent blackboard
        #[serde(default)]
        input: Option<String>,
    },
}

/// Isolation mode for agent execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMode {
    /// Inherit from node configuration
    #[default]
    Inherit,
    /// Force Firecracker micro-VM
    Firecracker,
    /// Force Docker container
    Docker,
    /// No isolation (dangerous, for debugging only)
    Process,
}

/// Configuration for a single judge agent (ADR-016, ADR-017, ADR-049)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeConfig {
    /// Agent identifier for this judge
    pub agent_id: String,

    /// Optional Handlebars input template (overrides default judge prompt)
    #[serde(default)]
    pub input_template: Option<String>,

    /// Consensus weight for this judge (default: 1.0)
    #[serde(default = "default_weight")]
    pub weight: f64,
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
            return Err(format!(
                "Agreement factor must be between 0.0 and 1.0, got {}",
                self.agreement_factor
            ));
        }
        if self.self_confidence_factor < 0.0 || self.self_confidence_factor > 1.0 {
            return Err(format!(
                "Self-confidence factor must be between 0.0 and 1.0, got {}",
                self.self_confidence_factor
            ));
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

    /// Both validation score AND confidence are above the same threshold (ADR-049)
    ScoreAndConfidenceAbove { threshold: f64 },

    /// Consensus reached (for ParallelAgents)
    Consensus { threshold: f64, agreement: f64 },

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
// Value Objects: Blackboard (Boundary Object)
// ============================================================================

/// Blackboard: Boundary object for workflow execution context.
///
/// The live execution context is owned by the TypeScript `aegis_workflow` Temporal worker
/// and persisted by Temporal's event-sourcing. Rust interacts with the blackboard at two
/// boundary points only — it never mutates the blackboard during execution.
///
/// - **Input (seed):** `StartWorkflowExecutionRequest.blackboard` supplies initial key-value
///   context (e.g. `judges`, `validation_threshold`) before the TypeScript worker starts.
///   Values are forwarded in the `StartWorkflowExecution` Temporal payload and accessible
///   inside the worker via Handlebars templates: `{{blackboard.key}}`.
///
/// - **Output (capture):** `TemporalEventListener` reads the `final_blackboard` from
///   `WorkflowExecutionCompleted` events and stores it on `WorkflowExecution` in PostgreSQL
///   for audit and Cortex learning.
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

    /// Reconstruct blackboard from JSONB (for persistence)
    pub fn from_json(json: &serde_json::Value) -> anyhow::Result<Self> {
        match json {
            serde_json::Value::Object(obj) => {
                let mut data = HashMap::new();
                for (k, v) in obj {
                    data.insert(k.clone(), v.clone());
                }
                Ok(Self { data })
            }
            _ => Ok(Self::new()),
        }
    }

    /// Serialize blackboard to JSONB (for persistence)
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.data).unwrap_or(serde_json::json!({}))
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

    /// Tenant that owns this execution
    #[serde(default)]
    pub tenant_id: TenantId,

    /// Execution status
    pub status: ExecutionStatus,

    /// Current state name
    pub current_state: StateName,

    /// Blackboard state: seeded from `StartWorkflowExecutionRequest` before the Temporal worker
    /// starts; overwritten with the final snapshot captured from `WorkflowExecutionCompleted`.
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
            tenant_id: TenantId::default(),
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
// Audit Trail Records
// ============================================================================

/// A single persisted event from a workflow execution — part of the
/// append-only audit trail written by `WorkflowExecutionRepository::append_event`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowExecutionEventRecord {
    /// Monotonically increasing sequence number within this execution.
    pub sequence: i64,
    /// Discriminant string matching the `WorkflowEvent` variant name
    /// (e.g. `"WorkflowStateEntered"`, `"WorkflowIterationCompleted"`).
    pub event_type: String,
    /// State name if the event is state-scoped (e.g. `WorkflowStateEntered`).
    pub state_name: Option<String>,
    /// Iteration number if the event is iteration-scoped.
    pub iteration_number: Option<u8>,
    /// Full event payload (serialised `WorkflowEvent` variant).
    pub payload: serde_json::Value,
    /// Wall-clock time the event was recorded.
    pub recorded_at: chrono::DateTime<chrono::Utc>,
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

    #[error(
        "Volume '{volume_name}' referenced in state '{state}' is not declared in spec.storage.shared_volumes"
    )]
    UndeclaredVolume {
        state: StateName,
        volume_name: String,
    },

    #[error("Invalid container state configuration in state '{state}': {detail}")]
    InvalidContainerState { state: StateName, detail: String },

    #[error("Invalid subworkflow state configuration in state '{state}': {detail}")]
    InvalidSubworkflowState { state: StateName, detail: String },

    #[error("Invalid workflow scope: {0}")]
    InvalidScope(String),
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
    fn test_score_and_confidence_above_roundtrip() {
        // Ensure ScoreAndConfidenceAbove can be serialized/deserialized and is
        // distinct from ScoreAbove and ConfidenceAbove (regression guard for
        // a previously reported TypeScript evaluateCondition dead-code issue).
        let condition = TransitionCondition::ScoreAndConfidenceAbove { threshold: 0.85 };
        let json = serde_json::to_string(&condition).unwrap();
        assert!(
            json.contains("score_and_confidence_above"),
            "wrong tag: {json}"
        );
        assert!(json.contains("0.85"), "threshold missing: {json}");

        let round_tripped: TransitionCondition = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(
                round_tripped,
                TransitionCondition::ScoreAndConfidenceAbove { .. }
            ),
            "Round-trip produced wrong variant"
        );
        if let TransitionCondition::ScoreAndConfidenceAbove { threshold } = round_tripped {
            assert!((threshold - 0.85).abs() < f64::EPSILON);
        }
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
    fn test_referenced_judge_agents_collects_all_judge_variants() {
        let mut states = std::collections::HashMap::new();
        states.insert(
            StateName::new("START").unwrap(),
            WorkflowState {
                kind: StateKind::Agent {
                    agent: "builder".to_string(),
                    input: "{{workflow.task}}".to_string(),
                    isolation: None,
                    judges: vec![
                        JudgeConfig {
                            agent_id: "judge-b".to_string(),
                            input_template: None,
                            weight: 1.0,
                        },
                        JudgeConfig {
                            agent_id: "judge-a".to_string(),
                            input_template: None,
                            weight: 1.0,
                        },
                        JudgeConfig {
                            agent_id: "judge-b".to_string(),
                            input_template: None,
                            weight: 0.5,
                        },
                    ],
                    max_iterations: None,
                    pre_execution_validator: Some("validator-z".to_string()),
                },
                transitions: vec![],
                timeout: None,
            },
        );
        states.insert(
            StateName::new("PARALLEL").unwrap(),
            WorkflowState {
                kind: StateKind::ParallelAgents {
                    agents: vec![ParallelAgentConfig {
                        agent: "worker".to_string(),
                        input: "{{workflow.task}}".to_string(),
                        weight: 1.0,
                        timeout_seconds: 60,
                        poll_interval_ms: 500,
                    }],
                    consensus: ConsensusConfig {
                        strategy: ConsensusStrategy::WeightedAverage,
                        threshold: None,
                        min_agreement_confidence: None,
                        n: None,
                        min_judges_required: 1,
                        confidence_weighting: None,
                    },
                    judges_for_parallel: vec![
                        JudgeConfig {
                            agent_id: "judge-c".to_string(),
                            input_template: None,
                            weight: 1.0,
                        },
                        JudgeConfig {
                            agent_id: "judge-a".to_string(),
                            input_template: None,
                            weight: 1.0,
                        },
                    ],
                },
                transitions: vec![],
                timeout: None,
            },
        );

        let workflow = Workflow {
            id: WorkflowId::new(),
            tenant_id: TenantId::default(),
            scope: WorkflowScope::default(),
            metadata: WorkflowMetadata {
                name: "test".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                tags: vec![],
                labels: std::collections::HashMap::new(),
                annotations: std::collections::HashMap::new(),
                input_schema: None,
            },
            spec: WorkflowSpec {
                initial_state: StateName::new("START").unwrap(),
                context: std::collections::HashMap::new(),
                states,
                storage: Default::default(),
            },
            created_at: Utc::now(),
            updated_at: None,
        };

        assert_eq!(
            workflow.referenced_judge_agents(),
            vec![
                "judge-a".to_string(),
                "judge-b".to_string(),
                "judge-c".to_string(),
                "validator-z".to_string()
            ]
        );
    }

    #[test]
    fn test_workflow_validation_no_states() {
        let metadata = WorkflowMetadata {
            name: "test".to_string(),
            version: None,
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };

        let spec = WorkflowSpec {
            initial_state: StateName::new("START").unwrap(),
            context: HashMap::new(),
            states: HashMap::new(),
            storage: Default::default(),
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
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
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
            storage: Default::default(),
        };

        let result = Workflow::new(metadata, spec);
        assert!(matches!(
            result,
            Err(WorkflowError::InitialStateNotFound(_))
        ));
    }

    #[test]
    fn test_container_run_requires_non_empty_command() {
        let metadata = WorkflowMetadata {
            name: "container-command-required".to_string(),
            version: None,
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("BUILD").unwrap(),
            WorkflowState {
                kind: StateKind::ContainerRun {
                    name: "build".to_string(),
                    image: "rust:1.75".to_string(),
                    image_pull_policy: None,
                    command: vec![],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![],
                    resources: None,
                    registry_credentials: None,
                    retry: None,
                    shell: false,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("BUILD").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };

        let result = Workflow::new(metadata, spec);
        assert!(matches!(
            result,
            Err(WorkflowError::InvalidContainerState { .. })
        ));
    }

    #[test]
    fn test_parallel_container_run_rejects_duplicate_step_names() {
        let metadata = WorkflowMetadata {
            name: "parallel-duplicate-steps".to_string(),
            version: None,
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("TEST").unwrap(),
            WorkflowState {
                kind: StateKind::ParallelContainerRun {
                    steps: vec![
                        ContainerRunConfig {
                            name: "lint".to_string(),
                            image: "rust:1.75".to_string(),
                            command: vec!["cargo".to_string(), "clippy".to_string()],
                            env: HashMap::new(),
                            workdir: None,
                            volumes: vec![],
                            resources: None,
                            registry_credentials: None,
                            shell: false,
                        },
                        ContainerRunConfig {
                            name: "lint".to_string(),
                            image: "rust:1.75".to_string(),
                            command: vec!["cargo".to_string(), "fmt".to_string()],
                            env: HashMap::new(),
                            workdir: None,
                            volumes: vec![],
                            resources: None,
                            registry_credentials: None,
                            shell: false,
                        },
                    ],
                    completion: ParallelCompletionStrategy::AllSucceed,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("TEST").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };

        let result = Workflow::new(metadata, spec);
        assert!(matches!(
            result,
            Err(WorkflowError::InvalidContainerState { .. })
        ));
    }

    #[test]
    fn test_container_mount_path_must_be_absolute() {
        let metadata = WorkflowMetadata {
            name: "container-mount-path".to_string(),
            version: None,
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("BUILD").unwrap(),
            WorkflowState {
                kind: StateKind::ContainerRun {
                    name: "build".to_string(),
                    image: "rust:1.75".to_string(),
                    image_pull_policy: None,
                    command: vec!["cargo".to_string(), "build".to_string()],
                    env: HashMap::new(),
                    workdir: None,
                    volumes: vec![ContainerVolumeMount {
                        name: "workspace".to_string(),
                        mount_path: "workspace".to_string(),
                        read_only: false,
                    }],
                    resources: None,
                    registry_credentials: None,
                    retry: None,
                    shell: false,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("BUILD").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };

        let result = Workflow::new(metadata, spec);
        assert!(matches!(
            result,
            Err(WorkflowError::InvalidContainerState { .. })
        ));
    }

    #[test]
    fn test_subworkflow_blocking_requires_result_key() {
        let metadata = WorkflowMetadata {
            name: "test-subworkflow".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("TRIGGER").unwrap(),
            WorkflowState {
                kind: StateKind::Subworkflow {
                    workflow_id: "child-workflow".to_string(),
                    mode: SubworkflowMode::Blocking,
                    result_key: None, // Missing — should fail
                    input: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("TRIGGER").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };
        let result = Workflow::new(metadata, spec);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("result_key"),
            "Expected result_key error, got: {err}"
        );
    }

    #[test]
    fn test_subworkflow_fire_and_forget_rejects_result_key() {
        let metadata = WorkflowMetadata {
            name: "test-subworkflow-faf".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("TRIGGER").unwrap(),
            WorkflowState {
                kind: StateKind::Subworkflow {
                    workflow_id: "child-workflow".to_string(),
                    mode: SubworkflowMode::FireAndForget,
                    result_key: Some("should_fail".to_string()), // Present — should fail
                    input: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("TRIGGER").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };
        let result = Workflow::new(metadata, spec);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must not specify result_key"),
            "Expected rejection error, got: {err}"
        );
    }

    #[test]
    fn test_subworkflow_valid_blocking() {
        let metadata = WorkflowMetadata {
            name: "test-subworkflow-ok".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("TRIGGER").unwrap(),
            WorkflowState {
                kind: StateKind::Subworkflow {
                    workflow_id: "child-workflow".to_string(),
                    mode: SubworkflowMode::Blocking,
                    result_key: Some("child_result".to_string()),
                    input: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("TRIGGER").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };
        let result = Workflow::new(metadata, spec);
        assert!(result.is_ok());
    }

    #[test]
    fn test_subworkflow_valid_fire_and_forget() {
        let metadata = WorkflowMetadata {
            name: "test-subworkflow-faf-ok".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            tags: vec![],
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
        };
        let mut states = HashMap::new();
        states.insert(
            StateName::new("TRIGGER").unwrap(),
            WorkflowState {
                kind: StateKind::Subworkflow {
                    workflow_id: "child-workflow".to_string(),
                    mode: SubworkflowMode::FireAndForget,
                    result_key: None,
                    input: None,
                },
                transitions: vec![],
                timeout: None,
            },
        );
        let spec = WorkflowSpec {
            initial_state: StateName::new("TRIGGER").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
        };
        let result = Workflow::new(metadata, spec);
        assert!(result.is_ok());
    }
}
