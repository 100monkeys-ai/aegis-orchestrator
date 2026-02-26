// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Execution Supervisor Domain Service — BC-2 (ADR-005)
//!
//! Orchestrates the top-level execution loop for the **100monkeys Algorithm**:
//! evaluate each `ValidationResult` returned by judge agents, decide whether
//! to continue iterating (refine → retry) or terminate (success/exhausted),
//! and apply refinement code diffs between iterations.
//!
//! Called from `StandardExecutionService` after each iteration completes.
//!
//! ## Loop Decision Table
//! | Validation Score | Action |
//! |-----------------|--------|
//! | ≥ success threshold | Mark `Success`, stop loop |
//! | < threshold, iterations remaining | Apply `Refinement`, continue |
//! | < threshold, max iterations reached | Mark `Failed` |
//!
//! See ADR-005 (Iterative Execution Strategy).

// ============================================================================
// ADR-005: Iterative Execution Strategy (100monkeys Algorithm)
// ============================================================================
// This module implements the core 100monkeys iterative refinement loop:
// Generate → Execute → Evaluate → Refine (repeat up to max_iterations)
//
// Status: Phase 1 Core Implementation (in progress)
// The loop is functional but may require refinement in Phase 2 for:
// - Enhanced error classification (move beyond simple parsing failures)
// - Smarter code mutation strategies (integrate with Cortex learning)
// - Dynamic iteration prioritization (ADR-017 Gradient Validation)
//
// See: adrs/005-iterative-execution-strategy.md
// ============================================================================

use crate::domain::execution::ExecutionInput;
use crate::domain::runtime::{AgentRuntime, InstanceId, RuntimeConfig, RuntimeError, TaskInput};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use async_trait::async_trait;

/// Parse human-readable duration strings like "30s", "5m", "1h"
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();

    // Try parsing with duration_str crate if available, or implement basic parsing
    if let Ok(seconds) = s.trim_end_matches('s').parse::<u64>() {
        return Ok(Duration::from_secs(seconds));
    }
    if let Ok(minutes) = s.trim_end_matches('m').parse::<u64>() {
        return Ok(Duration::from_secs(minutes * 60));
    }
    if let Ok(hours) = s.trim_end_matches('h').parse::<u64>() {
        return Ok(Duration::from_secs(hours * 3600));
    }

    Err(format!("Invalid duration format: {}", s))
}

/// Global default execution timeout when the manifest specifies none.
/// 30 minutes is generous enough for complex tasks while preventing indefinite runs.
pub const DEFAULT_EXECUTION_TIMEOUT_SECONDS: u64 = 1800;

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
        Self { runtime }
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
    /// ## Timeout Enforcement
    ///
    /// The overall execution is bounded by `runtime_config.resources.timeout_seconds`
    /// (falling back to [`DEFAULT_EXECUTION_TIMEOUT_SECONDS`] when unset). Each
    /// individual iteration is bounded by `timeout / max_retries` to ensure the
    /// loop cannot monopolise the full deadline on a single hanging iteration.
    ///
    /// ## Cancellation
    ///
    /// The `cancellation_token` is checked before each iteration and via
    /// `tokio::select!` during execution. When cancelled, the current container
    /// is terminated and [`RuntimeError::Cancelled`] is returned.
    ///
    /// # Arguments
    /// * `runtime_config` - Configuration for spawning runtime instances
    /// * `input` - Execution input with intent/payload
    /// * `max_retries` - Maximum number of iteration attempts (from manifest)
    /// * `observer` - Observer for iteration lifecycle events
    /// * `cancellation_token` - Token to cooperatively cancel the execution
    pub async fn run_loop(
        &self,
        runtime_config: RuntimeConfig,
        input: ExecutionInput,
        max_retries: u32,
        observer: Arc<dyn SupervisorObserver>,
        cancellation_token: CancellationToken,
    ) -> Result<String, RuntimeError> {
        let overall_timeout_secs = runtime_config
            .resources
            .timeout_seconds
            .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS);
        let overall_timeout = Duration::from_secs(overall_timeout_secs);

        // Per-iteration timeout: explicitly configured in manifest, or 300 seconds default.
        // This ensures each iteration has sufficient time for LLM calls + tool invocations.
        let per_iteration_timeout = runtime_config
            .execution
            .iteration_timeout
            .as_deref()
            .and_then(|s| parse_duration(s).ok())
            .unwrap_or_else(|| Duration::from_secs(300));

        info!(
            overall_timeout_secs = overall_timeout_secs,
            per_iteration_timeout_secs = per_iteration_timeout.as_secs(),
            max_retries = max_retries,
            "Starting supervisor loop with timeout enforcement"
        );

        // Wrap the entire loop in an overall deadline
        match tokio::time::timeout(
            overall_timeout,
            self.run_loop_inner(
                runtime_config,
                input,
                max_retries,
                observer,
                cancellation_token,
                per_iteration_timeout,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => {
                warn!(
                    timeout_seconds = overall_timeout_secs,
                    "Execution timed out — overall deadline exceeded"
                );
                Err(RuntimeError::TimedOut(overall_timeout_secs))
            }
        }
    }

    /// Inner implementation of the 100monkeys loop, run under a `tokio::time::timeout`
    /// wrapper by [`Supervisor::run_loop`].
    async fn run_loop_inner(
        &self,
        runtime_config: RuntimeConfig,
        input: ExecutionInput,
        max_retries: u32,
        observer: Arc<dyn SupervisorObserver>,
        cancellation_token: CancellationToken,
        per_iteration_timeout: Duration,
    ) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        let original_intent = input.intent.clone().unwrap_or_default();
        // Payload merge into context is handled by ExecutionService::prepare_execution_input

        // Track iteration history for context in subsequent attempts
        let mut iteration_history: Vec<serde_json::Value> = Vec::new();

        while attempts < max_retries {
            // Check cancellation before each iteration
            if cancellation_token.is_cancelled() {
                info!("Execution cancelled before iteration {}", attempts + 1);
                return Err(RuntimeError::Cancelled);
            }

            attempts += 1;
            info!("Starting iteration {}/{}", attempts, max_retries);
            observer
                .on_iteration_start(attempts as u8, &original_intent)
                .await;

            // SPAWN FRESH INSTANCE for this iteration
            info!("Spawning fresh runtime instance for iteration {}", attempts);

            let mut current_config = runtime_config.clone();
            current_config
                .env
                .insert("AEGIS_ITERATION".to_string(), attempts.to_string());

            // Inject iteration history as JSON for bootstrap.py to use
            if !iteration_history.is_empty() {
                let history_json =
                    serde_json::to_string(&iteration_history).unwrap_or_else(|_| "[]".to_string());
                current_config
                    .env
                    .insert("AEGIS_ITERATION_HISTORY".to_string(), history_json);
            }

            // Save keep_container flag before moving config
            let keep_on_failure = current_config.keep_container_on_failure;

            let instance_id = match self.runtime.spawn(current_config).await {
                Ok(id) => {
                    observer.on_instance_spawned(attempts as u8, &id).await;
                    id
                }
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

            // Execute task with per-iteration timeout and cancellation support
            let execution_result = tokio::select! {
                result = tokio::time::timeout(per_iteration_timeout, self.runtime.execute(&instance_id, task_input)) => {
                    match result {
                        Ok(inner) => inner,
                        Err(_elapsed) => {
                            warn!(
                                iteration = attempts,
                                timeout_secs = per_iteration_timeout.as_secs(),
                                "Iteration timed out"
                            );
                            Err(RuntimeError::TimedOut(per_iteration_timeout.as_secs()))
                        }
                    }
                }
                _ = cancellation_token.cancelled() => {
                    info!(iteration = attempts, "Execution cancelled during iteration");
                    // Terminate the instance before returning
                    let _ = self.runtime.terminate(&instance_id).await;
                    observer.on_instance_terminated(attempts as u8, &instance_id).await;
                    return Err(RuntimeError::Cancelled);
                }
            };

            // Terminate the instance after execution (unless keep_on_failure is set)
            let should_terminate = if keep_on_failure {
                execution_result.is_ok()
            } else {
                true
            };

            if should_terminate {
                let terminate_result = self.runtime.terminate(&instance_id).await;
                if let Err(e) = terminate_result {
                    warn!(
                        "Failed to terminate instance {}: {}",
                        instance_id.as_str(),
                        e
                    );
                } else {
                    observer
                        .on_instance_terminated(attempts as u8, &instance_id)
                        .await;
                }
            } else {
                info!(
                    "Keeping failed container {} alive for debugging (manual cleanup required: docker rm -f {})",
                    instance_id.as_str(),
                    instance_id.as_str()
                );
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

            observer
                .on_console_output(attempts as u8, "stdout", &stdout)
                .await;
            if !stderr.is_empty() {
                observer
                    .on_console_output(attempts as u8, "stderr", &stderr)
                    .await;
            }

            // SUCCESS: iteration completed without runtime errors
            // NOTE: We do NOT validate output here. Validation is the workflow's job.
            // If the workflow wants validation, it should spawn a judge agent.
            info!("Iteration {} completed", attempts);
            observer
                .on_iteration_complete(attempts as u8, &stdout, output.exit_code)
                .await;

            // Record this iteration in history for context in future attempts
            iteration_history.push(serde_json::json!({
                "iteration": attempts,
                "output": stdout,
                "exit_code": output.exit_code
            }));

            // For now, we return the first successful execution.
            // Workflow-driven iteration is handled by WorkflowEngine.tick() which
            // controls when to invoke Supervisor.run_loop(). See ADR-015.
            return Ok(stdout);
        }

        Err(RuntimeError::ExecutionFailed(
            "Max retries exceeded".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{InstanceStatus, ResourceLimits, TaskOutput};
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    // Mock runtime for testing
    struct MockRuntime {
        spawn_results: Arc<Mutex<Vec<Result<InstanceId, RuntimeError>>>>,
        execute_results: Arc<Mutex<Vec<Result<TaskOutput, RuntimeError>>>>,
        terminate_calls: Arc<Mutex<Vec<InstanceId>>>,
        /// Optional delay injected into `execute()` to simulate long-running work.
        execute_delay: Option<Duration>,
    }

    impl MockRuntime {
        fn new() -> Self {
            Self {
                spawn_results: Arc::new(Mutex::new(Vec::new())),
                execute_results: Arc::new(Mutex::new(Vec::new())),
                terminate_calls: Arc::new(Mutex::new(Vec::new())),
                execute_delay: None,
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
            let results = outputs
                .into_iter()
                .map(|output| {
                    Ok(TaskOutput {
                        result: serde_json::Value::String(output),
                        logs: vec![],
                        tool_calls: vec![],
                        exit_code: 0,
                    })
                })
                .collect();
            Self {
                execute_results: Arc::new(Mutex::new(results)),
                ..self
            }
        }

        fn with_execute_delay(self, delay: Duration) -> Self {
            Self {
                execute_delay: Some(delay),
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

        async fn execute(
            &self,
            _id: &InstanceId,
            _input: TaskInput,
        ) -> Result<TaskOutput, RuntimeError> {
            if let Some(delay) = self.execute_delay {
                tokio::time::sleep(delay).await;
            }
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
            image_pull_policy: crate::domain::agent::ImagePullPolicy::IfNotPresent,
            container_uid: 1000,
            container_gid: 1000,
            resources: ResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
                disk_bytes: None,
                timeout_seconds: None,
            },
            execution: crate::domain::agent::ExecutionStrategy {
                mode: crate::domain::agent::ExecutionMode::Iterative,
                max_retries: 5,
                iteration_timeout: None, // Use default 300s
                llm_timeout_seconds: 300,
                validation: None,
                delivery: None,
            },
            volumes: Vec::new(),
            keep_container_on_failure: false,
            image: "python:3.12".to_string(),
            bootstrap_path: None,
            execution_id: crate::domain::execution::ExecutionId::new(),
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
                .with_execute_success(vec!["Success output".to_string()]),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(MockObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
            )
            .await;

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
        let runtime = Arc::new(MockRuntime::new());

        // Setup: first spawn fails, second succeeds
        runtime
            .spawn_results
            .lock()
            .await
            .push(Err(RuntimeError::SpawnFailed("Network error".to_string())));
        runtime
            .spawn_results
            .lock()
            .await
            .push(Ok(InstanceId::new("instance-1".to_string())));
        runtime.execute_results.lock().await.push(Ok(TaskOutput {
            result: serde_json::Value::String("Success".to_string()),
            logs: vec![],
            tool_calls: vec![],
            exit_code: 0,
        }));

        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(MockObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
            )
            .await;

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
            runtime
                .spawn_results
                .lock()
                .await
                .push(Err(RuntimeError::SpawnFailed(
                    "Resource exhausted".to_string(),
                )));
        }

        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(MockObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RuntimeError::ExecutionFailed(_)
        ));

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
                .with_execute_success(vec!["Output1".to_string(), "Output2".to_string()]),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(MockObserver::default());

        let _result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer,
                CancellationToken::new(),
            )
            .await;

        // Verify instance was terminated
        let terminate_calls = runtime.terminate_calls.lock().await;
        assert_eq!(terminate_calls.len(), 1);
        assert_eq!(terminate_calls[0].as_str(), "instance-0");
    }

    #[tokio::test]
    async fn test_supervisor_overall_timeout() {
        // Mock runtime that sleeps longer than the timeout allows
        let runtime = Arc::new(
            MockRuntime::new()
                .with_spawn_success(3)
                .with_execute_success(vec![
                    "Output".to_string(),
                    "Output".to_string(),
                    "Output".to_string(),
                ])
                .with_execute_delay(Duration::from_secs(5)),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(MockObserver::default());

        // Set a very short timeout so it fires before the mock completes
        let mut config = create_test_config();
        config.resources.timeout_seconds = Some(1);

        let result = supervisor
            .run_loop(
                config,
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RuntimeError::TimedOut(1)));
    }

    #[tokio::test]
    async fn test_supervisor_cancellation() {
        // Mock runtime that sleeps long enough for us to cancel
        let runtime = Arc::new(
            MockRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Output".to_string()])
                .with_execute_delay(Duration::from_secs(30)),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(MockObserver::default());

        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            token_clone.cancel();
        });

        let mut config = create_test_config();
        config.resources.timeout_seconds = Some(60); // long timeout — cancellation should fire first

        let result = supervisor
            .run_loop(config, create_test_input(), 3, observer.clone(), token)
            .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RuntimeError::Cancelled));

        // Instance should have been terminated
        let terminate_calls = runtime.terminate_calls.lock().await;
        assert_eq!(terminate_calls.len(), 1);
    }

    #[tokio::test]
    async fn test_supervisor_uses_default_timeout_when_none() {
        // Verify that when timeout_seconds is None, the supervisor still proceeds
        // (using DEFAULT_EXECUTION_TIMEOUT_SECONDS) and completes normally.
        let runtime = Arc::new(
            MockRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Success".to_string()]),
        );

        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(MockObserver::default());

        let config = create_test_config(); // timeout_seconds: None
        let result = supervisor
            .run_loop(
                config,
                create_test_input(),
                3,
                observer,
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_ok());
    }
}
