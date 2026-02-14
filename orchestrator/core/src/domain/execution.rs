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
