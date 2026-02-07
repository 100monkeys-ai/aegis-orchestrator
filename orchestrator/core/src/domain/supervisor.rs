use crate::domain::execution::ExecutionInput;
use crate::domain::runtime::{AgentRuntime, InstanceId, TaskInput, RuntimeError};
use crate::domain::judge::EvaluationEngine;
use std::sync::Arc;
use tracing::{info, warn};

use async_trait::async_trait;

#[async_trait]
pub trait SupervisorObserver: Send + Sync {
    async fn on_iteration_start(&self, iteration: u8, prompt: &str);
    async fn on_console_output(&self, iteration: u8, stream: &str, content: &str);
    async fn on_iteration_complete(&self, iteration: u8, result: &str);
    async fn on_iteration_fail(&self, iteration: u8, error: &str);
}

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

    pub async fn run_loop(&self, instance_id: &InstanceId, input: ExecutionInput, observer: Arc<dyn SupervisorObserver>) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        let mut prompt = input.intent.clone().unwrap_or_default();
        // TODO: Handle payload merge into context if needed

        while attempts < self.max_retries {
            attempts += 1;
            info!("Starting iteration {}/{}", attempts, self.max_retries);
            observer.on_iteration_start(attempts as u8, &prompt).await;

            let task_input = TaskInput {
                prompt: prompt.clone(),
                context: std::collections::HashMap::new(),
            };

            // Execute
            let output = self.runtime.execute(instance_id, task_input).await?;
            let stdout = output.result.to_string(); 
            let stderr = output.logs.join("\n");
            
            observer.on_console_output(attempts as u8, "stdout", &stdout).await;
            if !stderr.is_empty() {
                observer.on_console_output(attempts as u8, "stderr", &stderr).await;
            }

            // Evaluate
            let valid_res = self.judge.evaluate(&stdout, 0, &stderr).await
                .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

            if valid_res.success {
                info!("Iteration {} succeeded", attempts);
                observer.on_iteration_complete(attempts as u8, &stdout).await;
                return Ok(stdout);
            }

            warn!("Iteration {} failed: {:?}", attempts, valid_res.errors);
            // Convert errors (Vec<String>) to single string for event
            let error_msg = format!("{:?}", valid_res.errors);
            observer.on_iteration_fail(attempts as u8, &error_msg).await;
            
            // Refine
            if let Some(feedback) = valid_res.feedback {
                prompt = format!("Previous attempt failed with errors: {:?}\nFeedback: {}\nPlease fix the code and retry.\nOriginal Request: {}", valid_res.errors, feedback, prompt);
            }
        }

        Err(RuntimeError::ExecutionFailed("Max retries exceeded".to_string()))
    }
}
