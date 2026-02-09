// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::execution::ExecutionInput;
use crate::domain::runtime::{AgentRuntime, InstanceId, TaskInput, RuntimeError, RuntimeConfig};
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
    
    // New methods for instance lifecycle
    async fn on_instance_spawned(&self, iteration: u8, instance_id: &InstanceId);
    async fn on_instance_terminated(&self, iteration: u8, instance_id: &InstanceId);
}

pub struct Supervisor {
    runtime: Arc<dyn AgentRuntime>,
    judge: Arc<dyn EvaluationEngine>,
}

impl Supervisor {
    pub fn new(runtime: Arc<dyn AgentRuntime>, judge: Arc<dyn EvaluationEngine>) -> Self {
        Self {
            runtime,
            judge,
        }
    }

    /// Run the 100monkeys loop with fresh instances per iteration
    /// 
    /// This method spawns a NEW runtime instance for each iteration attempt,
    /// ensuring complete isolation between iterations. Each instance is
    /// terminated after the iteration completes (success or failure).
    /// 
    /// # Arguments
    /// * `runtime_config` - Configuration for spawning runtime instances
    /// * `input` - Execution input with intent/payload
    /// * `max_retries` - Maximum number of iteration attempts (from manifest)
    /// * `observer` - Observer for iteration lifecycle events
    pub async fn run_loop(
        &self, 
        runtime_config: RuntimeConfig, 
        input: ExecutionInput,
        max_retries: u32,
        observer: Arc<dyn SupervisorObserver>
    ) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        let original_intent = input.intent.clone().unwrap_or_default();
        // TODO: Handle payload merge into context if needed
        
        // Track iteration history for context in subsequent attempts
        let mut iteration_history: Vec<serde_json::Value> = Vec::new();

        while attempts < max_retries {
            attempts += 1;
            info!("Starting iteration {}/{}", attempts, max_retries);
            observer.on_iteration_start(attempts as u8, &original_intent).await;

            // SPAWN FRESH INSTANCE for this iteration
            info!("Spawning fresh runtime instance for iteration {}", attempts);
            
            let mut current_config = runtime_config.clone();
            current_config.env.insert("AEGIS_ITERATION".to_string(), attempts.to_string());
            
            // Inject iteration history as JSON for bootstrap.py to use
            if !iteration_history.is_empty() {
                let history_json = serde_json::to_string(&iteration_history)
                    .unwrap_or_else(|_| "[]".to_string());
                current_config.env.insert("AEGIS_ITERATION_HISTORY".to_string(), history_json);
            }
            
            let instance_id = match self.runtime.spawn(current_config).await {
                Ok(id) => {
                    observer.on_instance_spawned(attempts as u8, &id).await;
                    id
                },
                Err(e) => {
                    let error_msg = format!("Failed to spawn instance: {}", e);
                    warn!("{}", error_msg);
                    observer.on_iteration_fail(attempts as u8, &error_msg).await;
                    
                    // Record spawn failure in history
                    iteration_history.push(serde_json::json!({
                        "iteration": attempts,
                        "error": error_msg
                    }));
                    
                    continue; // Try next iteration
                }
            };

            let task_input = TaskInput {
                prompt: original_intent.clone(),
                context: std::collections::HashMap::new(),
            };

            // Execute task in the fresh instance
            let execution_result = self.runtime.execute(&instance_id, task_input).await;
            
            // ALWAYS terminate the instance after execution (success or failure)
            let terminate_result = self.runtime.terminate(&instance_id).await;
            if let Err(e) = terminate_result {
                warn!("Failed to terminate instance {}: {}", instance_id.as_str(), e);
            } else {
                observer.on_instance_terminated(attempts as u8, &instance_id).await;
            }

            // Process execution result
            let output = match execution_result {
                Ok(out) => out,
                Err(e) => {
                    let error_msg = format!("Execution failed: {}", e);
                    warn!("{}", error_msg);
                    observer.on_iteration_fail(attempts as u8, &error_msg).await;
                    
                    // Record execution failure in history
                    iteration_history.push(serde_json::json!({
                        "iteration": attempts,
                        "error": error_msg
                    }));
                    
                    continue;
                }
            };

            let stdout = output.result.to_string(); 
            let stderr = output.logs.join("\n");
            
            observer.on_console_output(attempts as u8, "stdout", &stdout).await;
            if !stderr.is_empty() {
                observer.on_console_output(attempts as u8, "stderr", &stderr).await;
            }

            // Evaluate
            let valid_res = self.judge.evaluate(&stdout, output.exit_code, &stderr).await
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
            
            // Record validation failure in history
            let mut history_entry = serde_json::json!({
                "iteration": attempts,
                "output": stdout,
                "error": valid_res.errors.join("; ")
            });
            
            // Include feedback from judge if available
            if let Some(ref feedback) = valid_res.feedback {
                history_entry["feedback"] = serde_json::json!(feedback);
            }
            
            iteration_history.push(history_entry);
        }

        Err(RuntimeError::ExecutionFailed("Max retries exceeded".to_string()))
    }
}
