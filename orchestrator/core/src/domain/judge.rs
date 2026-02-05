use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub success: bool,
    pub errors: Vec<String>,
    pub feedback: Option<String>,
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Validation execution failed: {0}")]
    ExecutionError(String),
}

#[async_trait]
pub trait EvaluationEngine: Send + Sync {
    /// Validates the output of an execution iteration
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str) -> Result<ValidationResult, ValidationError>;
}

pub struct BasicJudge;

#[async_trait]
impl EvaluationEngine for BasicJudge {
    async fn evaluate(&self, _output: &str, exit_code: i64, stderr: &str) -> Result<ValidationResult, ValidationError> {
        let mut errors = Vec::new();
        
        if exit_code != 0 {
            errors.push(format!("Process exited with code {}", exit_code));
        }
        
        if !stderr.is_empty() {
            errors.push(format!("Stderr is not empty: {}", stderr));
        }

        if errors.is_empty() {
            Ok(ValidationResult {
                success: true,
                errors: vec![],
                feedback: None,
            })
        } else {
            Ok(ValidationResult {
                success: false,
                errors,
                feedback: Some("Execution failed. Please fix the errors listed.".to_string()),
            })
        }
    }
}
