use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::domain::agent::AgentId;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: ExecutionId,
    pub agent_id: AgentId,
    pub status: ExecutionStatus,
    iterations: Vec<Iteration>,
    pub max_iterations: u8,
    pub input: ExecutionInput,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResults {
    pub system: Option<SystemValidationResult>,
    pub output: Option<OutputValidationResult>,
    pub semantic: Option<SemanticValidationResult>,
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
}

impl Execution {
    pub fn new(agent_id: AgentId, input: ExecutionInput, max_iterations: u8) -> Self {
        Self {
            id: ExecutionId::new(),
            agent_id,
            status: ExecutionStatus::Pending,
            iterations: Vec::new(),
            max_iterations,
            input,
            started_at: Utc::now(),
            ended_at: None,
        }
    }

    pub fn start(&mut self) {
        self.status = ExecutionStatus::Running;
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

    pub fn fail(&mut self) {
        self.status = ExecutionStatus::Failed;
        self.ended_at = Some(Utc::now());
    }
}

// Trait moved to domain/repository.rs
