use crate::domain::execution::{ExecutionInput, Iteration, ValidationResults, SystemValidationResult, OutputValidationResult, SemanticValidationResult};
use crate::domain::runtime::{AgentRuntime, InstanceId, TaskInput, RuntimeError, RuntimeConfig};
use crate::domain::judge::EvaluationEngine;
use std::sync::Arc;
use tokio::time::Duration;
use tracing::{info, warn, error};

pub struct Supervisor {
    runtime: Arc<dyn AgentRuntime>,
    judge: Arc<dyn EvaluationEngine>,
    max_retries: u32,
}

impl Supervisor {
    pub fn new(runtime: Arc<dyn AgentRuntime>, judge: Arc<dyn EvaluationEngine>) -> Self {
        Self {
            runtime,
            judge,
            max_retries: 5,
        }
    }

    pub async fn run_loop(&self, instance_id: &InstanceId, input: ExecutionInput) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        let mut prompt = input.intent.clone().unwrap_or_default();
        // TODO: Handle payload merge into context if needed

        while attempts < self.max_retries {
            attempts += 1;
            info!("Starting iteration {}/{}", attempts, self.max_retries);

            let task_input = TaskInput {
                prompt: prompt.clone(),
                context: std::collections::HashMap::new(),
            };

            // Execute
            let output = self.runtime.execute(instance_id, task_input).await?;
            let stdout = output.result.to_string(); 
            let stderr = output.logs.join("\n");
            
            // Evaluate
            let valid_res = self.judge.evaluate(&stdout, 0, &stderr).await
                .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

            if valid_res.success {
                info!("Iteration {} succeeded", attempts);
                return Ok(stdout);
            }

            warn!("Iteration {} failed: {:?}", attempts, valid_res.errors);
            
            // Refine
            if let Some(feedback) = valid_res.feedback {
                prompt = format!("Previous attempt failed with errors: {:?}\nFeedback: {}\nPlease fix the code and retry.\nOriginal Request: {}", valid_res.errors, feedback, prompt);
            }
        }

        Err(RuntimeError::ExecutionFailed("Max retries exceeded".to_string()))
    }
}
