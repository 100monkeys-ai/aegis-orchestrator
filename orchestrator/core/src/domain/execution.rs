// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Execution Domain Aggregate (BC-2, ADR-005)
//!
//! Defines the `Execution` aggregate root and the `Iteration` entity that
//! together implement the **100monkeys Algorithm** iterative refinement loop:
//!
//! ```text
//! start_execution
//!   ▼
//! Iteration 1: generate → execute → evaluate
//!   ├─ success → ExecutionCompleted
//!   └─ failure → RefinementApplied → Iteration 2 → … → Iteration N
//!                                                    └─ max_iterations reached → ExecutionFailed
//! ```
//!
//! ## Aggregate Invariants (see AGENTS.md §Execution Aggregate)
//!
//! - An `Execution` must have at least 1 `Iteration`.
//! - At most `max_iterations` iterations (default 10).
//! - Only one `Iteration` may be `Running` at a time.
//! - Iteration numbers are sequential starting from 1.
//!
//! ## Recursive Execution
//!
//! Agents may spawn child agents. `Execution.depth` tracks the nesting level;
//! `MAX_RECURSIVE_DEPTH` (defined in this module) prevents infinite recursion.
//!
//! See ADR-005 (Iterative Execution Strategy), AGENTS.md §Execution Context.

use crate::domain::agent::AgentId;
use crate::domain::tenant::TenantId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ============================================================================
// Execution Hierarchy (Recursive Execution Tracking)
// ============================================================================

/// Maximum recursive depth for nested agent executions
///
/// This prevents infinite recursion when agents call other agents.
/// Example execution tree:
///
/// ```text
/// Depth 0: User Agent (generates code)
/// Depth 1: ├─ Validation Agent (validates code)
/// Depth 2: │  └─ Meta-Validation Agent (validates validator's reasoning)
/// Depth 3: │     └─ Super-Meta Agent (validates meta-validator) [MAX DEPTH]
/// ```
pub const MAX_RECURSIVE_DEPTH: u8 = 3;

/// Recursive execution tracking
///
/// Tracks parent-child execution relationships for nested agent calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionHierarchy {
    /// Parent execution ID (None for root executions)
    pub parent_execution_id: Option<ExecutionId>,

    /// Recursive depth (0 for root, 1 for child, 2 for grandchild, etc.)
    pub depth: u8,

    /// Execution path from root (list of execution IDs)
    pub path: Vec<ExecutionId>,

    /// Optional swarm ID linking this execution to a multi-agent swarm (ADR-039).
    /// Uses raw UUID to avoid circular dependency with the swarm crate.
    #[serde(default)]
    pub swarm_id: Option<uuid::Uuid>,
}

impl Default for ExecutionHierarchy {
    fn default() -> Self {
        // Use a synthetic root ID for default initialization.
        let temp_id = ExecutionId::new();
        Self::root(temp_id)
    }
}

impl ExecutionHierarchy {
    /// Create a root execution hierarchy (no parent)
    pub fn root(execution_id: ExecutionId) -> Self {
        Self {
            parent_execution_id: None,
            depth: 0,
            path: vec![execution_id],
            swarm_id: None,
        }
    }

    /// Create a child execution hierarchy
    pub fn child(
        parent: &ExecutionHierarchy,
        child_execution_id: ExecutionId,
    ) -> Result<Self, String> {
        let new_depth = parent.depth + 1;

        if new_depth > MAX_RECURSIVE_DEPTH {
            return Err(format!(
                "Maximum recursive depth ({MAX_RECURSIVE_DEPTH}) exceeded. Cannot create child execution."
            ));
        }

        let mut path = parent.path.clone();
        path.push(child_execution_id);

        Ok(Self {
            parent_execution_id: Some(parent.path[parent.path.len() - 1]),
            depth: new_depth,
            path,
            swarm_id: parent.swarm_id,
        })
    }

    /// Check if this execution can spawn a child
    pub fn can_spawn_child(&self) -> bool {
        self.depth < MAX_RECURSIVE_DEPTH
    }

    /// Get the root execution ID
    pub fn root_id(&self) -> ExecutionId {
        self.path[0]
    }

    /// Get the immediate parent execution ID
    pub fn parent_id(&self) -> Option<ExecutionId> {
        self.parent_execution_id
    }

    /// Associate this hierarchy with a swarm.
    pub fn with_swarm_id(mut self, swarm_id: uuid::Uuid) -> Self {
        self.swarm_id = Some(swarm_id);
        self
    }
}

// ============================================================================
// Execution Entity
// ============================================================================

pub use crate::domain::shared_kernel::ExecutionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    #[serde(default)]
    pub tenant_id: TenantId,
    pub status: ExecutionStatus,
    pub iterations: Vec<Iteration>,
    pub max_iterations: u8,
    pub input: ExecutionInput,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,

    /// Hierarchical execution tracking for nested agent calls
    /// Enables judge-calling-judge and agent composition patterns
    #[serde(default)]
    pub hierarchy: ExecutionHierarchy,

    /// Container user ID for permission squashing (ADR-036)
    /// Default: 1000 (standard non-root user)
    #[serde(default = "default_container_uid")]
    pub container_uid: u32,

    /// Container group ID for permission squashing (ADR-036)
    /// Default: 1000 (standard non-root group)
    #[serde(default = "default_container_gid")]
    pub container_gid: u32,

    /// Security context name governing tool access for this execution (ADR-083).
    #[serde(default = "default_security_context_name")]
    pub security_context_name: String,
}

fn default_container_uid() -> u32 {
    1000
}

fn default_container_gid() -> u32 {
    1000
}

fn default_security_context_name() -> String {
    "aegis-system-operator".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInput {
    /// Optional free-text override used by the natural-language dispatch path.
    /// Steers the LLM prompt directly. Complementary to `input`, not an
    /// alternative — when an agent declares `input_schema`, callers pass typed
    /// data via `input`; `intent` may be omitted or used alongside it.
    pub intent: Option<String>,
    /// Typed input data for the agent, validated against the agent's
    /// `input_schema` when one is declared. Supplies structured data to the
    /// prompt template context via `{{input}}` / `{{input.KEY}}` dot-notation
    /// (ADR-092).
    pub input: serde_json::Value,
    /// Workspace volume ID provisioned by the workflow orchestrator. When set,
    /// the runtime registers this pre-existing volume against the execution so
    /// fs.* tools can write to the shared workspace (ADR-087).
    pub workspace_volume_id: Option<crate::domain::shared_kernel::VolumeId>,
    /// Mount path for the workspace volume. Defaults to /workspace.
    pub workspace_volume_mount_path: Option<std::path::PathBuf>,
    /// NFS remote path for the workspace volume in SeaweedFS.
    /// When set, used directly for NFS registration instead of constructing a path.
    pub workspace_remote_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Iteration {
    pub number: u8,
    pub status: IterationStatus,
    pub action: String,
    pub output: Option<String>,
    pub validation_results: Option<ValidationResults>,
    pub error: Option<IterationError>,
    pub code_changes: Option<CodeDiff>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub llm_interactions: Vec<LlmInteraction>,
    #[serde(default)]
    pub trajectory: Option<Vec<TrajectoryStep>>,
    /// Tool names that were blocked by policy during this iteration.
    #[serde(default)]
    pub policy_violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStep {
    pub tool_name: String,
    pub arguments_json: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmInteraction {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: DateTime<Utc>,
}

use crate::domain::validation::ValidationResults;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IterationStatus {
    Running,
    Success,
    Failed,
    Refining,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationError {
    pub message: String,
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeDiff {
    pub file_path: String,
    pub diff: String,
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("Max iterations reached")]
    MaxIterationsReached,
    #[error("Execution is not running")]
    NotRunning,
    #[error("Iteration {0} not found")]
    IterationNotFound(u8),
    #[error("Maximum recursive execution depth exceeded: {0}")]
    MaxDepthExceeded(String),
    #[error("Agent manifest is missing spec.task")]
    MissingPromptTemplate,
    #[error("Failed to render prompt template: {0}")]
    PromptRenderFailed(String),
    #[error("Failed to extract user input from execution input: {0}")]
    InvalidExecutionInput(String),
    #[error(
        "Cross-tenant spawn forbidden: parent tenant '{parent_tenant}' cannot spawn child in tenant '{child_tenant}'"
    )]
    CrossTenantSpawnForbidden {
        parent_tenant: String,
        child_tenant: String,
    },
    #[error(
        "Cross-tenant access forbidden: caller tenant '{caller_tenant}' cannot execute agent owned by tenant '{requested_agent_tenant}'"
    )]
    CrossTenantAccessForbidden {
        requested_agent_tenant: String,
        caller_tenant: String,
    },
}

impl Execution {
    pub fn new(
        agent_id: AgentId,
        input: ExecutionInput,
        max_iterations: u8,
        security_context_name: String,
    ) -> Self {
        Self::new_with_id(
            ExecutionId::new(),
            agent_id,
            input,
            max_iterations,
            security_context_name,
        )
    }

    /// Create an execution with a pre-assigned ID (used for cluster forwarding).
    /// The execution_id is imported from the originating node to preserve tracing correlation.
    pub fn new_with_id(
        id: ExecutionId,
        agent_id: AgentId,
        input: ExecutionInput,
        max_iterations: u8,
        security_context_name: String,
    ) -> Self {
        Self {
            id,
            agent_id,
            tenant_id: TenantId::default(),
            status: ExecutionStatus::Pending,
            iterations: Vec::new(),
            max_iterations,
            input,
            started_at: Utc::now(),
            ended_at: None,
            error: None,
            container_uid: 1000,
            container_gid: 1000,
            hierarchy: ExecutionHierarchy::root(id),
            security_context_name,
        }
    }

    /// Create a child execution (for nested agent calls like judges)
    pub fn new_child(
        agent_id: AgentId,
        input: ExecutionInput,
        max_iterations: u8,
        parent: &Execution,
    ) -> Result<Self, ExecutionError> {
        let child_id = ExecutionId::new();
        let hierarchy = ExecutionHierarchy::child(&parent.hierarchy, child_id)
            .map_err(ExecutionError::MaxDepthExceeded)?;

        Ok(Self {
            id: child_id,
            agent_id,
            tenant_id: parent.tenant_id.clone(),
            status: ExecutionStatus::Pending,
            iterations: Vec::new(),
            max_iterations,
            input,
            started_at: Utc::now(),
            ended_at: None,
            error: None,
            container_uid: 1000,
            container_gid: 1000,
            hierarchy,
            security_context_name: parent.security_context_name.clone(),
        })
    }

    /// Check if this execution can spawn a child agent (for recursive calls)
    pub fn can_spawn_child(&self) -> bool {
        self.hierarchy.can_spawn_child()
    }

    /// Get execution depth (0 = root, 1 = child, 2 = grandchild, etc.)
    pub fn depth(&self) -> u8 {
        self.hierarchy.depth
    }

    /// Get parent execution ID if this is a child execution
    pub fn parent_id(&self) -> Option<ExecutionId> {
        self.hierarchy.parent_id()
    }

    pub fn start(&mut self) {
        self.status = ExecutionStatus::Running;
    }

    pub fn add_llm_interaction(
        &mut self,
        iteration_number: u8,
        interaction: LlmInteraction,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.llm_interactions.push(interaction);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    pub fn iterations(&self) -> &[Iteration] {
        &self.iterations
    }

    pub fn start_iteration(&mut self, action: String) -> Result<&mut Iteration, ExecutionError> {
        if self.iterations.len() as u8 >= self.max_iterations {
            return Err(ExecutionError::MaxIterationsReached);
        }

        let iteration = Iteration {
            number: (self.iterations.len() + 1) as u8,
            status: IterationStatus::Running,
            action,
            output: None,
            validation_results: None,
            error: None,
            code_changes: None,
            started_at: Utc::now(),
            ended_at: None,
            llm_interactions: Vec::new(),
            trajectory: None,
            policy_violations: Vec::new(),
        };

        self.iterations.push(iteration);
        Ok(self.iterations.last_mut().unwrap())
    }

    pub fn complete_iteration(&mut self, output: String) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.status = IterationStatus::Success;
            iter.output = Some(output);
            iter.ended_at = Some(Utc::now());
        }
    }

    pub fn store_validation_results(
        &mut self,
        iteration_number: u8,
        results: ValidationResults,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.validation_results = Some(results);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    /// Append a policy-blocked tool name to the current iteration.
    pub fn add_policy_violation(&mut self, tool_name: String) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.policy_violations.push(tool_name);
        }
    }

    pub fn store_iteration_trajectory(
        &mut self,
        iteration_number: u8,
        trajectory: Vec<TrajectoryStep>,
    ) -> Result<(), ExecutionError> {
        if let Some(iter) = self
            .iterations
            .iter_mut()
            .find(|i| i.number == iteration_number)
        {
            iter.trajectory = Some(trajectory);
            Ok(())
        } else {
            Err(ExecutionError::IterationNotFound(iteration_number))
        }
    }

    pub fn fail_iteration(&mut self, error: IterationError) {
        if let Some(iter) = self.iterations.last_mut() {
            iter.status = IterationStatus::Failed;
            iter.error = Some(error);
            iter.ended_at = Some(Utc::now());
        }
    }

    pub fn complete(&mut self) {
        self.status = ExecutionStatus::Completed;
        self.ended_at = Some(Utc::now());
    }

    pub fn fail(&mut self, reason: String) {
        self.status = ExecutionStatus::Failed;
        self.error = Some(reason);
        self.ended_at = Some(Utc::now());
    }

    /// Check if execution is completed (success, failure, or cancellation)
    pub fn is_completed(&self) -> bool {
        matches!(
            self.status,
            ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
        )
    }

    /// Get the current (most recent) iteration
    pub fn current_iteration(&self) -> Option<&Iteration> {
        self.iterations.last()
    }

    /// Get the total number of attempts (iterations)
    pub fn total_attempts(&self) -> u8 {
        self.iterations.len() as u8
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInfo {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    #[serde(default)]
    pub tenant_id: TenantId,
    pub status: ExecutionStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl From<Execution> for ExecutionInfo {
    fn from(exec: Execution) -> Self {
        Self {
            id: exec.id,
            agent_id: exec.agent_id,
            tenant_id: exec.tenant_id,
            status: exec.status,
            started_at: exec.started_at,
            ended_at: exec.ended_at,
            error: exec.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;

    fn make_input(intent: &str) -> ExecutionInput {
        ExecutionInput {
            intent: Some(intent.to_string()),
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
        }
    }

    // ── ExecutionId ───────────────────────────────────────────────────────────

    #[test]
    fn test_execution_id_new_unique() {
        let a = ExecutionId::new();
        let b = ExecutionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn test_execution_id_display() {
        let id = ExecutionId::new();
        let s = format!("{id}");
        assert_eq!(s, id.0.to_string());
    }

    #[test]
    fn test_execution_id_default() {
        let a = ExecutionId::default();
        let b = ExecutionId::default();
        assert_ne!(a, b);
    }

    // ── Execution state machine ───────────────────────────────────────────────

    #[test]
    fn test_new_execution_is_pending() {
        let exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        assert_eq!(exec.status, ExecutionStatus::Pending);
        assert_eq!(exec.depth(), 0);
        assert!(exec.parent_id().is_none());
        assert!(exec.can_spawn_child());
        assert_eq!(exec.container_uid, 1000);
        assert_eq!(exec.container_gid, 1000);
    }

    #[test]
    fn test_execution_start_changes_status() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        assert_eq!(exec.status, ExecutionStatus::Running);
    }

    #[test]
    fn test_execution_complete() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.complete();
        assert_eq!(exec.status, ExecutionStatus::Completed);
        assert!(exec.ended_at.is_some());
    }

    #[test]
    fn test_execution_fail() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.fail("something broke".to_string());
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert_eq!(exec.error.as_deref(), Some("something broke"));
        assert!(exec.ended_at.is_some());
    }

    // ── Iteration management ──────────────────────────────────────────────────

    #[test]
    fn test_start_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        let iter = exec.start_iteration("generate".to_string()).unwrap();
        assert_eq!(iter.number, 1);
        assert_eq!(iter.status, IterationStatus::Running);
        assert_eq!(iter.action, "generate");
    }

    #[test]
    fn test_complete_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        exec.complete_iteration("result".to_string());
        let iter = &exec.iterations()[0];
        assert_eq!(iter.status, IterationStatus::Success);
        assert_eq!(iter.output.as_deref(), Some("result"));
    }

    #[test]
    fn test_fail_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        exec.fail_iteration(IterationError {
            message: "compile error".to_string(),
            details: Some("syntax".to_string()),
        });
        let iter = &exec.iterations()[0];
        assert_eq!(iter.status, IterationStatus::Failed);
        assert!(iter.error.is_some());
    }

    #[test]
    fn test_max_iterations_enforced() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            2,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("iter1".to_string()).unwrap();
        exec.complete_iteration("out1".to_string());
        exec.start_iteration("iter2".to_string()).unwrap();
        exec.complete_iteration("out2".to_string());
        let err = exec.start_iteration("iter3".to_string()).unwrap_err();
        assert!(matches!(err, ExecutionError::MaxIterationsReached));
    }

    #[test]
    fn test_store_validation_results() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        let results = ValidationResults {
            system: None,
            output: None,
            semantic: None,
            gradient: None,
            consensus: None,
        };
        exec.store_validation_results(1, results).unwrap();
        assert!(exec.iterations()[0].validation_results.is_some());
    }

    #[test]
    fn test_store_validation_results_wrong_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("validate".to_string()).unwrap();
        let results = ValidationResults {
            system: None,
            output: None,
            semantic: None,
            gradient: None,
            consensus: None,
        };
        let err = exec.store_validation_results(99, results).unwrap_err();
        assert!(matches!(err, ExecutionError::IterationNotFound(99)));
    }

    #[test]
    fn test_add_llm_interaction() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        let interaction = LlmInteraction {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            prompt: "write hello world".to_string(),
            response: "print('hello')".to_string(),
            timestamp: chrono::Utc::now(),
        };
        exec.add_llm_interaction(1, interaction).unwrap();
        assert_eq!(exec.iterations()[0].llm_interactions.len(), 1);
    }

    #[test]
    fn test_add_llm_interaction_wrong_iteration() {
        let mut exec = Execution::new(
            AgentId::new(),
            make_input("task"),
            5,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        let interaction = LlmInteraction {
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            prompt: "prompt".to_string(),
            response: "response".to_string(),
            timestamp: chrono::Utc::now(),
        };
        let err = exec.add_llm_interaction(99, interaction).unwrap_err();
        assert!(matches!(err, ExecutionError::IterationNotFound(99)));
    }

    // ── ExecutionHierarchy ────────────────────────────────────────────────────

    #[test]
    fn test_hierarchy_root() {
        let id = ExecutionId::new();
        let h = ExecutionHierarchy::root(id);
        assert_eq!(h.depth, 0);
        assert!(h.parent_id().is_none());
        assert!(h.can_spawn_child());
        assert_eq!(h.root_id(), id);
    }

    #[test]
    fn test_hierarchy_child() {
        let root_id = ExecutionId::new();
        let root_h = ExecutionHierarchy::root(root_id);

        let child_id = ExecutionId::new();
        let child_h = ExecutionHierarchy::child(&root_h, child_id).unwrap();

        assert_eq!(child_h.depth, 1);
        assert_eq!(child_h.parent_id(), Some(root_id));
        assert_eq!(child_h.root_id(), root_id);
    }

    #[test]
    fn test_hierarchy_depth_limit() {
        let root_id = ExecutionId::new();
        let mut h = ExecutionHierarchy::root(root_id);

        // Go up to MAX_RECURSIVE_DEPTH
        for _ in 0..MAX_RECURSIVE_DEPTH {
            let child_id = ExecutionId::new();
            h = ExecutionHierarchy::child(&h, child_id).unwrap();
        }
        assert_eq!(h.depth, MAX_RECURSIVE_DEPTH);
        assert!(!h.can_spawn_child());

        // One more should fail
        let err = ExecutionHierarchy::child(&h, ExecutionId::new());
        assert!(err.is_err());
    }

    // ── ExecutionInfo ─────────────────────────────────────────────────────────

    #[test]
    fn test_execution_info_from_execution() {
        let agent_id = AgentId::new();
        let mut exec = Execution::new(
            agent_id,
            make_input("task"),
            3,
            "aegis-system-operator".to_string(),
        );
        exec.start();
        exec.complete();

        let info = ExecutionInfo::from(exec.clone());
        assert_eq!(info.id, exec.id);
        assert_eq!(info.agent_id, agent_id);
        assert_eq!(info.status, ExecutionStatus::Completed);
        assert!(info.ended_at.is_some());
        assert!(info.error.is_none());
    }
}
