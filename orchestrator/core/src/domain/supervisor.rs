use crate::domain::execution::{Execution, ExecutionId, Iteration};
use crate::domain::runtime::{AgentRuntime, InstanceId, TaskInput, RuntimeError};
use crate::domain::judge::EvaluationEngine;
use crate::domain::agent::RuntimeConfig;
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

    pub async fn run_loop(&self, instance_id: &InstanceId, initial_prompt: String) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        let mut prompt = initial_prompt;

        while attempts < self.max_retries {
            attempts += 1;
            info!("Starting iteration {}/{}", attempts, self.max_retries);

            let input = TaskInput {
                prompt: prompt.clone(),
                context: std::collections::HashMap::new(),
            };

            // Execute
            let output = self.runtime.execute(instance_id, input).await?;
            let stdout = output.result.to_string(); // Simplified for now
            let stderr = output.logs.join("\n");
            
            // Evaluate
            // Note: Currently execute returns Result, so exit code implicit execution success needs detail
            // For now assuming success if execute returns Ok, need to enrich TaskOutput to include exit code
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
