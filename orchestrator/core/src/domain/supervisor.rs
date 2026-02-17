// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::execution::ExecutionInput;
use crate::domain::runtime::{AgentRuntime, InstanceId, TaskInput, RuntimeError, RuntimeConfig};
use std::sync::Arc;
use tracing::{info, warn};

use async_trait::async_trait;

#[async_trait]
pub trait SupervisorObserver: Send + Sync {
    async fn on_iteration_start(&self, iteration: u8, prompt: &str);
    async fn on_console_output(&self, iteration: u8, stream: &str, content: &str);
    async fn on_iteration_complete(&self, iteration: u8, result: &str, exit_code: i64);
    async fn on_iteration_fail(&self, iteration: u8, error: &str);
    
    // New methods for instance lifecycle
    async fn on_instance_spawned(&self, iteration: u8, instance_id: &InstanceId);
    async fn on_instance_terminated(&self, iteration: u8, instance_id: &InstanceId);
}

pub struct Supervisor {
    runtime: Arc<dyn AgentRuntime>,
}

impl Supervisor {
    pub fn new(runtime: Arc<dyn AgentRuntime>) -> Self {
        Self {
            runtime,
        }
    }

    /// Run the 100monkeys loop with fresh instances per iteration
    /// 
    /// This method spawns a NEW runtime instance for each iteration attempt,
    /// ensuring complete isolation between iterations. Each instance is
    /// terminated after the iteration completes (success or failure).
    /// 
    /// NOTE: This supervisor is role-agnostic. It does NOT validate outputs.
    /// Validation is the responsibility of workflows that compose agents.
    /// This supervisor simply executes iterations and reports results.
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
        observer: Arc<dyn SupervisorObserver>,
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

            // SUCCESS: iteration completed without runtime errors
            // NOTE: We do NOT validate output here. Validation is the workflow's job.
            // If the workflow wants validation, it should spawn a judge agent.
            info!("Iteration {} completed", attempts);
            observer.on_iteration_complete(attempts as u8, &stdout, output.exit_code).await;
            
            // Record this iteration in history for context in future attempts
            iteration_history.push(serde_json::json!({
                "iteration": attempts,
                "output": stdout,
                "exit_code": output.exit_code
            }));
            
            // For now, we return the first successful execution.
            // TODO: In workflow mode, the workflow FSM decides when to stop iterating.
            return Ok(stdout);
        }

        Err(RuntimeError::ExecutionFailed("Max retries exceeded".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{TaskOutput, InstanceStatus, ResourceLimits};
    use tokio::sync::Mutex;
    use std::collections::HashMap;
    
    // Mock runtime for testing
    struct MockRuntime {
        spawn_results: Arc<Mutex<Vec<Result<InstanceId, RuntimeError>>>>,
        execute_results: Arc<Mutex<Vec<Result<TaskOutput, RuntimeError>>>>,
        terminate_calls: Arc<Mutex<Vec<InstanceId>>>,
    }
    
    impl MockRuntime {
        fn new() -> Self {
            Self {
                spawn_results: Arc::new(Mutex::new(Vec::new())),
                execute_results: Arc::new(Mutex::new(Vec::new())),
                terminate_calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
        
        fn with_spawn_success(self, count: usize) -> Self {
            let mut results = Vec::new();
            for i in 0..count {
                results.push(Ok(InstanceId::new(format!("instance-{}", i))));
            }
            Self {
                spawn_results: Arc::new(Mutex::new(results)),
                ..self
            }
        }
        
        fn with_execute_success(self, outputs: Vec<String>) -> Self {
            let results = outputs.into_iter().map(|output| {
                Ok(TaskOutput {
                    result: serde_json::Value::String(output),
                    logs: vec![],
                    tool_calls: vec![],
                    exit_code: 0,
                })
            }).collect();
            Self {
                execute_results: Arc::new(Mutex::new(results)),
                ..self
            }
        }
    }
    
    #[async_trait]
    impl AgentRuntime for MockRuntime {
        async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
            let mut results = self.spawn_results.lock().await;
            results.remove(0)
        }
        
        async fn execute(&self, _id: &InstanceId, _input: TaskInput) -> Result<TaskOutput, RuntimeError> {
            let mut results = self.execute_results.lock().await;
            results.remove(0)
        }
        
        async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError> {
            let mut calls = self.terminate_calls.lock().await;
            calls.push(id.clone());
            Ok(())
        }
        
        async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
            Ok(InstanceStatus {
                id: id.clone(),
                state: "running".to_string(),
                uptime_seconds: 0,
                memory_usage_mb: 0,
                cpu_usage_percent: 0.0,
            })
        }
    }
    
    // Mock observer for testing
    #[derive(Default)]
    struct MockObserver {
        iteration_starts: Arc<Mutex<Vec<u8>>>,
        iteration_completes: Arc<Mutex<Vec<u8>>>,
        iteration_fails: Arc<Mutex<Vec<u8>>>,
    }
    
    #[async_trait]
    impl SupervisorObserver for MockObserver {
        async fn on_iteration_start(&self, iteration: u8, _prompt: &str) {
            self.iteration_starts.lock().await.push(iteration);
        }
        
        async fn on_console_output(&self, _iteration: u8, _stream: &str, _content: &str) {}
        
        async fn on_iteration_complete(&self, iteration: u8, _result: &str, _exit_code: i64) {
            self.iteration_completes.lock().await.push(iteration);
        }
        
        async fn on_iteration_fail(&self, iteration: u8, _error: &str) {
            self.iteration_fails.lock().await.push(iteration);
        }
        
        async fn on_instance_spawned(&self, _iteration: u8, _instance_id: &InstanceId) {}
        
        async fn on_instance_terminated(&self, _iteration: u8, _instance_id: &InstanceId) {}
    }
    
    fn create_test_config() -> RuntimeConfig {
        RuntimeConfig {
            language: "python".to_string(),
            version: "3.12".to_string(),
            isolation: "process".to_string(),
            env: HashMap::new(),
            autopull: false,
            resources: ResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
                disk_bytes: None,
            },
        }
    }
    
    fn create_test_input() -> ExecutionInput {
        ExecutionInput {
            intent: Some("Test task".to_string()),
            payload: serde_json::json!({}),
        }
    }
    
    #[tokio::test]
    async fn test_supervisor_success_first_iteration() {
        let runtime = Arc::new(
            MockRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Success output".to_string()])
        );
        
        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(MockObserver::default());
        
        let result = supervisor.run_loop(create_test_config(), create_test_input(), 3, observer.clone()).await;
        
        assert!(result.is_ok());
        // The result is serialized as JSON string, so it includes quotes
        assert_eq!(result.unwrap(), "\"Success output\"");
        
        // Verify observer was called correctly
        assert_eq!(observer.iteration_starts.lock().await.len(), 1);
        assert_eq!(observer.iteration_completes.lock().await.len(), 1);
        assert_eq!(observer.iteration_fails.lock().await.len(), 0);
    }
    
    #[tokio::test]
    async fn test_supervisor_retries_on_spawn_failure() {
        let runtime = Arc::new(
            MockRuntime::new()
        );
        
        // Setup: first spawn fails, second succeeds
        runtime.spawn_results.lock().await.push(Err(RuntimeError::SpawnFailed("Network error".to_string())));
        runtime.spawn_results.lock().await.push(Ok(InstanceId::new("instance-1".to_string())));
        runtime.execute_results.lock().await.push(Ok(TaskOutput {
            result: serde_json::Value::String("Success".to_string()),
            logs: vec![],
            tool_calls: vec![],
            exit_code: 0,
        }));
        
        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(MockObserver::default());
        
        let result = supervisor.run_loop(create_test_config(), create_test_input(), 3, observer.clone()).await;
        
        assert!(result.is_ok());
        // Verify we had one failure and one success
        assert_eq!(observer.iteration_starts.lock().await.len(), 2);
        assert_eq!(observer.iteration_fails.lock().await.len(), 1);
        assert_eq!(observer.iteration_completes.lock().await.len(), 1);
    }
    
    #[tokio::test]
    async fn test_supervisor_max_retries_exceeded() {
        let runtime = Arc::new(MockRuntime::new());
        
        // All spawn attempts fail
        for _ in 0..3 {
            runtime.spawn_results.lock().await.push(
                Err(RuntimeError::SpawnFailed("Resource exhausted".to_string()))
            );
        }
        
        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(MockObserver::default());
        
        let result = supervisor.run_loop(create_test_config(), create_test_input(), 3, observer.clone()).await;
        
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RuntimeError::ExecutionFailed(_)));
        
        // Verify all attempts were made
        assert_eq!(observer.iteration_starts.lock().await.len(), 3);
        assert_eq!(observer.iteration_fails.lock().await.len(), 3);
        assert_eq!(observer.iteration_completes.lock().await.len(), 0);
    }
    
    #[tokio::test]
    async fn test_supervisor_terminates_instances() {
        let runtime = Arc::new(
            MockRuntime::new()
                .with_spawn_success(2)
                .with_execute_success(vec!["Output1".to_string(), "Output2".to_string()])
        );
        
        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(MockObserver::default());
        
        let _result = supervisor.run_loop(create_test_config(), create_test_input(), 3, observer).await;
        
        // Verify instance was terminated
        let terminate_calls = runtime.terminate_calls.lock().await;
        assert_eq!(terminate_calls.len(), 1);
        assert_eq!(terminate_calls[0].as_str(), "instance-0");
    }
}
