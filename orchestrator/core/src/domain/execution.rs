// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::domain::agent::AgentId;

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
}

impl Default for ExecutionHierarchy {
    fn default() -> Self {
        // Create a temporary execution ID for default
        // This will be overridden when Execution is created
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
        }
    }
    
    /// Create a child execution hierarchy
    pub fn child(parent: &ExecutionHierarchy, child_execution_id: ExecutionId) -> Result<Self, String> {
        let new_depth = parent.depth + 1;
        
        if new_depth > MAX_RECURSIVE_DEPTH {
            return Err(format!(
                "Maximum recursive depth ({}) exceeded. Cannot create child execution.",
                MAX_RECURSIVE_DEPTH
            ));
        }
        
        let mut path = parent.path.clone();
        path.push(child_execution_id);
        
        Ok(Self {
            parent_execution_id: Some(parent.path[parent.path.len() - 1]),
            depth: new_depth,
            path,
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
}

// ============================================================================
// Execution Entity
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionId(pub Uuid);

impl ExecutionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub agent_id: AgentId,
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
}

fn default_container_uid() -> u32 {
    1000
}

fn default_container_gid() -> u32 {
    1000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionInput {
    pub intent: Option<String>,
    pub payload: serde_json::Value,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmInteraction {
    pub provider: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: DateTime<Utc>,
}

use crate::domain::validation::{GradientResult, MultiJudgeConsensus};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResults {
    pub system: Option<SystemValidationResult>,
    pub output: Option<OutputValidationResult>,
    pub semantic: Option<SemanticValidationResult>,
    pub gradient: Option<GradientResult>,
    pub consensus: Option<MultiJudgeConsensus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemValidationResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputValidationResult {
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticValidationResult {
    pub success: bool,
    pub score: f64,
    pub reasoning: String,
}

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
    #[error("Agent manifest is missing prompt_template in spec.task")]
    MissingPromptTemplate,
    #[error("Failed to render prompt template: {0}")]
    PromptRenderFailed(String),
    #[error("Failed to extract user input from execution payload: {0}")]
    InvalidExecutionInput(String),
}

impl Execution {
    pub fn new(agent_id: AgentId, input: ExecutionInput, max_iterations: u8) -> Self {
        let id = ExecutionId::new();
        Self {
            id,
            agent_id,
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
            .map_err(|e| ExecutionError::MaxDepthExceeded(e))?;
        
        Ok(Self {
            id: child_id,
            agent_id,
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

    pub fn add_llm_interaction(&mut self, iteration_number: u8, interaction: LlmInteraction) -> Result<(), ExecutionError> {
        if let Some(iter) = self.iterations.iter_mut().find(|i| i.number == iteration_number) {
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
    
    pub fn store_validation_results(&mut self, iteration_number: u8, results: ValidationResults) -> Result<(), ExecutionError> {
        if let Some(iter) = self.iterations.iter_mut().find(|i| i.number == iteration_number) {
            iter.validation_results = Some(results);
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
            payload: serde_json::json!({}),
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
        let s = format!("{}", id);
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
        let exec = Execution::new(AgentId::new(), make_input("task"), 5);
        assert_eq!(exec.status, ExecutionStatus::Pending);
        assert_eq!(exec.depth(), 0);
        assert!(exec.parent_id().is_none());
        assert!(exec.can_spawn_child());
        assert_eq!(exec.container_uid, 1000);
        assert_eq!(exec.container_gid, 1000);
    }

    #[test]
    fn test_execution_start_changes_status() {
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
        exec.start();
        assert_eq!(exec.status, ExecutionStatus::Running);
    }

    #[test]
    fn test_execution_complete() {
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
        exec.start();
        exec.complete();
        assert_eq!(exec.status, ExecutionStatus::Completed);
        assert!(exec.ended_at.is_some());
    }

    #[test]
    fn test_execution_fail() {
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
        exec.start();
        exec.fail("something broke".to_string());
        assert_eq!(exec.status, ExecutionStatus::Failed);
        assert_eq!(exec.error.as_deref(), Some("something broke"));
        assert!(exec.ended_at.is_some());
    }

    // ── Iteration management ──────────────────────────────────────────────────

    #[test]
    fn test_start_iteration() {
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
        exec.start();
        let iter = exec.start_iteration("generate".to_string()).unwrap();
        assert_eq!(iter.number, 1);
        assert_eq!(iter.status, IterationStatus::Running);
        assert_eq!(iter.action, "generate");
    }

    #[test]
    fn test_complete_iteration() {
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
        exec.start();
        exec.start_iteration("generate".to_string()).unwrap();
        exec.complete_iteration("result".to_string());
        let iter = &exec.iterations()[0];
        assert_eq!(iter.status, IterationStatus::Success);
        assert_eq!(iter.output.as_deref(), Some("result"));
    }

    #[test]
    fn test_fail_iteration() {
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
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
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 2);
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
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
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
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
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
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
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
        let mut exec = Execution::new(AgentId::new(), make_input("task"), 5);
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
        let mut exec = Execution::new(agent_id, make_input("task"), 3);
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
