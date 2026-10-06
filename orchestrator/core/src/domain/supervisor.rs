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
//! | judge fault (unreadable verdict, twice) | Mark `Failed` with the fault as reason, no feedback |
//!
//! Before any of these, a declared output that is missing, short or of the
//! wrong prefix fails the iteration with that sentence as the next
//! iteration's feedback, with or without a pipeline; past max retries the
//! execution fails with it (ADR-005, Update of 2026-10-06, O2).
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

use crate::domain::agent::DeclaredOutput;
use crate::domain::execution::{ExecutionId, ExecutionInput, ProducedFile, TrajectoryStep};
use crate::domain::repository::ExecutionRepository;
use crate::domain::runtime::{AgentRuntime, InstanceId, RuntimeConfig, RuntimeError, TaskInput};
use crate::domain::validation::{
    JudgeFault, ValidationContext, ValidationPipeline, ValidationResults,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// RAII guard that ensures a spawned container is terminated even if the
/// surrounding code panics or takes an unexpected error path.  Create one
/// right after `runtime.spawn()` succeeds; call [`ContainerGuard::defuse`]
/// before an intentional termination (or when keeping a container for
/// debugging) so the guard does not double-terminate.
struct ContainerGuard {
    inner: Option<(Arc<dyn AgentRuntime>, InstanceId)>,
}

impl ContainerGuard {
    fn new(runtime: Arc<dyn AgentRuntime>, instance_id: InstanceId) -> Self {
        Self {
            inner: Some((runtime, instance_id)),
        }
    }

    /// Disarm the guard so it will **not** terminate the container on drop.
    fn defuse(&mut self) {
        self.inner.take();
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        if let Some((runtime, instance_id)) = self.inner.take() {
            warn!(
                instance_id = %instance_id.as_str(),
                "ContainerGuard dropping — terminating leaked container"
            );
            // We may be dropping from a non-async context (e.g. panic unwind),
            // so spawn a blocking-compatible task to perform the cleanup.
            tokio::spawn(async move {
                if let Err(e) = runtime.terminate(&instance_id).await {
                    warn!(
                        instance_id = %instance_id.as_str(),
                        error = %e,
                        "ContainerGuard failed to terminate leaked container"
                    );
                }
            });
        }
    }
}

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

    Err(format!("Invalid duration format: {s}"))
}

/// Global default execution timeout when the manifest specifies none.
/// 30 minutes is generous enough for complex tasks while preventing indefinite runs.
pub const DEFAULT_EXECUTION_TIMEOUT_SECONDS: u64 = 1800;

/// Default bound on one iteration when the manifest's
/// `spec.execution.iteration_timeout` is absent or unparseable: time for the
/// iteration's whole inner tool loop, many model calls and tool rounds.
pub const DEFAULT_ITERATION_TIMEOUT_SECONDS: u64 = 600;

/// The bound on one iteration of an agent with this execution strategy: its
/// `iteration_timeout`, or [`DEFAULT_ITERATION_TIMEOUT_SECONDS`]. The supervisor
/// enforces it on each iteration by terminating the container; the agent's
/// bootstrap holds no clock of its own (ADR-040).
pub fn iteration_timeout(execution: &crate::domain::agent::ExecutionStrategy) -> Duration {
    execution
        .iteration_timeout
        .as_deref()
        .and_then(|s| parse_duration(s).ok())
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_ITERATION_TIMEOUT_SECONDS))
}

#[async_trait]
pub trait SupervisorObserver: Send + Sync {
    async fn on_iteration_start(&self, iteration: u8, prompt: &str);
    async fn on_console_output(&self, iteration: u8, stream: &str, content: &str);
    async fn on_iteration_complete(&self, iteration: u8, result: &str, exit_code: i64);
    async fn on_iteration_fail(&self, iteration: u8, error: &str);

    // Instance lifecycle events
    async fn on_instance_spawned(&self, iteration: u8, instance_id: &InstanceId);
    async fn on_instance_terminated(&self, iteration: u8, instance_id: &InstanceId);

    /// Called after gradient validation completes for a successful iteration (ADR-017).
    async fn on_validation_complete(
        &self,
        iteration: u8,
        results: &ValidationResults,
        passed: bool,
    );

    /// Called when every declared output of an iteration was found in the
    /// volume, before the iteration counts as completed, with what was found
    /// (ADR-005, Update of 2026-10-06, O3).
    async fn on_outputs_verified(&self, _iteration: u8, _produced: &[ProducedFile]) {}

    /// Called instead of [`Self::on_iteration_complete`] when a declared
    /// output is missing, short or of the wrong prefix: the iteration answered
    /// `output` and fails with `reason` (O2). An observer that keeps a record
    /// stores the output before failing the iteration.
    async fn on_outputs_missing(&self, iteration: u8, _output: &str, reason: &str) {
        self.on_iteration_fail(iteration, reason).await;
    }
}

/// Where a declared output is read: one volume of the execution and the path
/// inside that volume, rooted at `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLocation {
    pub execution_id: ExecutionId,
    pub workflow_execution_id: Option<uuid::Uuid>,
    pub volume_id: crate::domain::volume::VolumeId,
    pub path_in_volume: String,
    pub container_uid: u32,
    pub container_gid: u32,
}

/// What a [`DeclaredOutputReader`] found at an [`OutputLocation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputHead {
    /// The file's whole size in bytes.
    pub size_bytes: u64,
    /// Its first bytes: as many as were asked for, fewer when it is shorter.
    pub head: Vec<u8>,
    /// Its content type.
    pub content_type: String,
}

/// Reads a declared output from an execution's volume (ADR-005, Update of
/// 2026-10-06, O2): the port the supervisor checks outputs through.
#[async_trait]
pub trait DeclaredOutputReader: Send + Sync {
    /// The regular file at `location`, with its first `head_len` bytes;
    /// `Ok(None)` when no regular file is there; `Err` with the reason when
    /// the volume cannot be read.
    async fn read_head(
        &self,
        location: &OutputLocation,
        head_len: usize,
    ) -> Result<Option<OutputHead>, String>;
}

/// The volume of `config` a container path lies in, by the longest mount
/// point that is the path or one of its directories.
fn output_location(config: &RuntimeConfig, path: &str) -> Option<OutputLocation> {
    config
        .volumes
        .iter()
        .filter_map(|mount| {
            let mount_point = mount.mount_point.to_string_lossy();
            let mount_point = mount_point.trim_end_matches('/');
            let rest = path.strip_prefix(mount_point)?;
            if !rest.is_empty() && !rest.starts_with('/') {
                return None;
            }
            Some((mount_point.len(), mount.volume_id, rest.to_string()))
        })
        .max_by_key(|(length, _, _)| *length)
        .map(|(_, volume_id, rest)| OutputLocation {
            execution_id: config.execution_id,
            workflow_execution_id: config.workflow_execution_id,
            volume_id,
            path_in_volume: if rest.is_empty() {
                "/".to_string()
            } else {
                rest
            },
            container_uid: config.container_uid,
            container_gid: config.container_gid,
        })
}

/// Check every declared output against the execution's volumes (ADR-005,
/// Update of 2026-10-06, O2): the files found, or every failure sentence
/// joined by "; ". A node with no reader cannot check, and fails each output.
pub async fn check_declared_outputs(
    outputs: &[DeclaredOutput],
    config: &RuntimeConfig,
    reader: Option<&dyn DeclaredOutputReader>,
) -> Result<Vec<ProducedFile>, String> {
    let mut produced = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for output in outputs {
        let path = output.path.as_str();
        let Some(reader) = reader else {
            failures.push(format!(
                "declared output {path} cannot be checked: this node has no volume reader"
            ));
            continue;
        };
        let Some(location) = output_location(config, path) else {
            failures.push(format!("declared output {path} does not exist"));
            continue;
        };
        let head_len = output.magic.as_ref().map_or(0, String::len);
        match reader.read_head(&location, head_len).await {
            Err(reason) => failures.push(format!(
                "declared output {path} cannot be checked: {reason}"
            )),
            Ok(None) => failures.push(format!("declared output {path} does not exist")),
            Ok(Some(found)) => {
                let mut whole = true;
                if let Some(min_bytes) = output.min_bytes {
                    if found.size_bytes < min_bytes {
                        failures.push(format!(
                            "declared output {path} is {} bytes, under min_bytes {min_bytes}",
                            found.size_bytes
                        ));
                        whole = false;
                    }
                }
                if let Some(magic) = &output.magic {
                    if !found.head.starts_with(magic.as_bytes()) {
                        failures.push(format!(
                            "declared output {path} does not start with \"{magic}\""
                        ));
                        whole = false;
                    }
                }
                if whole {
                    produced.push(ProducedFile {
                        path: path.to_string(),
                        size_bytes: found.size_bytes,
                        content_type: found.content_type,
                    });
                }
            }
        }
    }
    if failures.is_empty() {
        Ok(produced)
    } else {
        Err(failures.join("; "))
    }
}

#[derive(Clone)]
pub struct Supervisor {
    runtime: Arc<dyn AgentRuntime>,
    /// Optional execution repository used to fetch the stored inner-loop trajectory
    /// after a container iteration completes.  When set, the supervisor populates
    /// `ValidationContext::tool_trajectory` from the persisted trajectory rather than
    /// leaving it empty.
    execution_repository: Option<Arc<dyn ExecutionRepository>>,
    /// Reads declared outputs from an execution's volume (ADR-005, Update of
    /// 2026-10-06, O2). Without one, every declared output fails as
    /// unreadable: an output is never passed unchecked.
    output_reader: Option<Arc<dyn DeclaredOutputReader>>,
}

impl Supervisor {
    pub fn new(runtime: Arc<dyn AgentRuntime>) -> Self {
        Self {
            runtime,
            execution_repository: None,
            output_reader: None,
        }
    }

    /// Attach the reader declared outputs are checked through.
    pub fn with_output_reader(mut self, reader: Arc<dyn DeclaredOutputReader>) -> Self {
        self.output_reader = Some(reader);
        self
    }

    /// Attach an execution repository so the supervisor can fetch the inner-loop
    /// trajectory for each iteration and thread it through `ValidationContext`.
    pub fn with_execution_repository(mut self, repo: Arc<dyn ExecutionRepository>) -> Self {
        self.execution_repository = Some(repo);
        self
    }

    /// Run the 100monkeys loop with fresh instances per iteration
    ///
    /// This method spawns a NEW runtime instance for each iteration attempt,
    /// ensuring complete isolation between iterations. Each instance is
    /// terminated after the iteration completes (success or failure).
    ///
    /// ## Gradient Validation (ADR-005, ADR-017)
    ///
    /// When a `validation_pipeline` is provided, iteration output is evaluated
    /// by the pipeline after each runtime-success.  If the output is rejected
    /// (score below threshold), the iteration is added to history and the loop
    /// continues to the next attempt.  Without a pipeline the first runtime-success
    /// is returned immediately — the original behaviour.
    ///
    /// ## Timeout Enforcement
    ///
    /// The overall execution is bounded by `runtime_config.resources.timeout_seconds`
    /// (falling back to [`DEFAULT_EXECUTION_TIMEOUT_SECONDS`] when unset). Each
    /// individual iteration is bounded by [`iteration_timeout`] (the manifest's
    /// `iteration_timeout`, or [`DEFAULT_ITERATION_TIMEOUT_SECONDS`]). On either
    /// bound the iteration's container is terminated; the agent's bootstrap
    /// holds no clock of its own (ADR-040).
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
    /// * `validation_pipeline` - Optional gradient validation pipeline (ADR-017)
    pub async fn run_loop(
        &self,
        runtime_config: RuntimeConfig,
        input: ExecutionInput,
        max_retries: u32,
        observer: Arc<dyn SupervisorObserver>,
        cancellation_token: CancellationToken,
        validation_pipeline: Option<Arc<ValidationPipeline>>,
    ) -> Result<String, RuntimeError> {
        let overall_timeout_secs = runtime_config
            .resources
            .timeout_seconds
            .unwrap_or(DEFAULT_EXECUTION_TIMEOUT_SECONDS);
        let overall_timeout = Duration::from_secs(overall_timeout_secs);

        // Per-iteration timeout: explicitly configured in manifest, or
        // DEFAULT_ITERATION_TIMEOUT_SECONDS. This ensures each iteration has
        // sufficient time for LLM calls + tool invocations.
        let per_iteration_timeout = iteration_timeout(&runtime_config.execution);

        info!(
            overall_timeout_secs = overall_timeout_secs,
            per_iteration_timeout_secs = per_iteration_timeout.as_secs(),
            max_retries = max_retries,
            "Starting supervisor loop with timeout enforcement"
        );

        let current_instance = Arc::new(Mutex::new(None));

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
                validation_pipeline,
                current_instance.clone(),
                self.execution_repository.clone(),
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
                if let Some(instance_id) = current_instance.lock().await.take() {
                    if let Err(error) = self.runtime.terminate(&instance_id).await {
                        warn!(
                            instance_id = %instance_id.as_str(),
                            error = %error,
                            "Failed to terminate instance after overall timeout"
                        );
                    }
                }
                Err(RuntimeError::TimedOut(overall_timeout_secs))
            }
        }
    }

    /// Inner implementation of the 100monkeys loop, run under a `tokio::time::timeout`
    /// wrapper by [`Supervisor::run_loop`].
    #[allow(clippy::too_many_arguments)]
    async fn run_loop_inner(
        &self,
        runtime_config: RuntimeConfig,
        input: ExecutionInput,
        max_retries: u32,
        observer: Arc<dyn SupervisorObserver>,
        cancellation_token: CancellationToken,
        per_iteration_timeout: Duration,
        validation_pipeline: Option<Arc<ValidationPipeline>>,
        current_instance: Arc<Mutex<Option<InstanceId>>>,
        execution_repository: Option<Arc<dyn ExecutionRepository>>,
    ) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        let original_intent = input.intent.clone().unwrap_or_default();
        let execution_context = Self::extract_execution_context(&input.input);

        // Extract the execution ID from the runtime environment so we can look up the
        // stored inner-loop trajectory after each container iteration completes.
        let execution_id_for_trajectory: Option<ExecutionId> = runtime_config
            .env
            .get("AEGIS_EXECUTION_ID")
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(ExecutionId);

        // Track iteration history for context in subsequent attempts
        let mut iteration_history: Vec<serde_json::Value> = Vec::new();

        // Why the last iteration's declared outputs failed it, if they did:
        // the reason the execution ends with past max retries (O2).
        let mut outputs_failure: Option<String> = None;

        while attempts < max_retries {
            outputs_failure = None;
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
                    *current_instance.lock().await = Some(id.clone());
                    observer.on_instance_spawned(attempts as u8, &id).await;
                    id
                }
                Err(e) => {
                    let error_msg = format!("Failed to spawn instance: {e}");
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

            // RAII guard: if we panic or hit an unexpected error path between
            // here and the explicit terminate/retain below, this guard ensures
            // the container is cleaned up.
            let mut container_guard =
                ContainerGuard::new(self.runtime.clone(), instance_id.clone());

            let task_input = TaskInput {
                prompt: original_intent.clone(),
                context: execution_context.clone(),
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
                    // Defuse the guard — we terminate explicitly here.
                    container_guard.defuse();
                    // Terminate the instance before returning
                    if let Some(instance_id) = current_instance.lock().await.take() {
                        let _ = self.runtime.terminate(&instance_id).await;
                        observer.on_instance_terminated(attempts as u8, &instance_id).await;
                    }
                    return Err(RuntimeError::Cancelled);
                }
            };

            // Terminate the instance after execution (unless keep_on_failure is set)
            let should_terminate = if keep_on_failure {
                execution_result.is_ok()
            } else {
                true
            };

            if !should_terminate {
                // Preserve the failed container for debugging, but stop tracking it as active.
                // Defuse the guard so it does not terminate the debug container.
                container_guard.defuse();
                let _ = current_instance.lock().await.take();
            }

            if should_terminate {
                // Defuse the guard — we are about to terminate explicitly.
                container_guard.defuse();
                let terminate_result = self.runtime.terminate(&instance_id).await;
                if let Err(e) = terminate_result {
                    warn!(
                        "Failed to terminate instance {}: {}",
                        instance_id.as_str(),
                        e
                    );
                } else {
                    let _ = current_instance.lock().await.take();
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
                    let error_msg = format!("Execution failed: {e}");
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

            // Unwrap Value::String to avoid double-encoding: when the agent's bootstrap
            // returns a plain string, `serde_json::Value::String(s).to_string()` produces
            // `"\"...escaped...\""` — the outer quotes plus JSON-escaped newlines/quotes.
            // Validators (OutputGradientValidator, strip_code_fence) and bootstrap.py's
            // clean_str all then receive a mangled string that starts with `"` rather than
            // the raw content, causing JSON parse errors at column 1.
            let stdout = match output.result {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            let stderr = output.logs.join("\n");

            observer
                .on_console_output(attempts as u8, "stdout", &stdout)
                .await;
            if !stderr.is_empty() {
                observer
                    .on_console_output(attempts as u8, "stderr", &stderr)
                    .await;
            }

            // Before the iteration counts as completed, its declared outputs
            // must be in the volume (ADR-005, Update of 2026-10-06, O2), with
            // or without a validation pipeline.
            let declared_outputs = &runtime_config.execution.outputs;
            if !declared_outputs.is_empty() {
                match check_declared_outputs(
                    declared_outputs,
                    &runtime_config,
                    self.output_reader.as_deref(),
                )
                .await
                {
                    Ok(produced) => {
                        observer
                            .on_outputs_verified(attempts as u8, &produced)
                            .await;
                    }
                    Err(reason) => {
                        warn!(
                            iteration = attempts,
                            reason = %reason,
                            "Declared outputs missing — failing the iteration"
                        );
                        observer
                            .on_outputs_missing(attempts as u8, &stdout, &reason)
                            .await;
                        iteration_history.push(serde_json::json!({
                            "iteration": attempts,
                            "output": stdout,
                            "exit_code": output.exit_code,
                            "validation_failed": true,
                            "validation_reason": reason,
                            "feedback": reason
                        }));
                        outputs_failure = Some(reason);
                        continue;
                    }
                }
            }

            // Iteration completed without runtime errors — run gradient validation (ADR-017).
            info!("Iteration {} completed", attempts);
            observer
                .on_iteration_complete(attempts as u8, &stdout, output.exit_code)
                .await;

            if let Some(ref pipeline) = validation_pipeline {
                // Filter out internal bootstrap debug logs so they don't penalize the validation score
                let validation_stderr = output
                    .logs
                    .iter()
                    .filter(|line| !line.starts_with("[BOOTSTRAP "))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");

                // Fetch the inner-loop trajectory stored by the dispatch gateway handler
                // during this iteration.  By the time execute() returns the container has
                // already exited, which means bootstrap.py has already received the Final
                // response and store_iteration_trajectory has already completed.
                let tool_trajectory: Vec<TrajectoryStep> = if let (Some(ref repo), Some(exec_id)) =
                    (&execution_repository, execution_id_for_trajectory)
                {
                    match repo.find_by_id_unscoped(exec_id).await {
                        Ok(Some(exec)) => exec
                            .iterations()
                            .iter()
                            .find(|it| it.number == attempts as u8)
                            .and_then(|it| it.trajectory.clone())
                            .unwrap_or_default(),
                        Ok(None) => {
                            warn!(
                                execution_id = %exec_id.0,
                                "Execution not found when fetching trajectory for ValidationContext"
                            );
                            vec![]
                        }
                        Err(e) => {
                            warn!(
                                execution_id = %exec_id.0,
                                error = %e,
                                "Failed to fetch trajectory for ValidationContext"
                            );
                            vec![]
                        }
                    }
                } else {
                    vec![]
                };

                let ctx = ValidationContext {
                    task: original_intent.clone(),
                    output: stdout.clone(),
                    exit_code: output.exit_code,
                    stderr: validation_stderr,
                    worker_mounts: runtime_config
                        .volumes
                        .iter()
                        .map(|m| m.mount_point.to_string_lossy().to_string())
                        .collect(),
                    policy_violations: vec![],
                    tool_trajectory,
                };
                match pipeline.validate(&ctx).await {
                    Ok(pipeline_result) => {
                        observer
                            .on_validation_complete(
                                attempts as u8,
                                &pipeline_result.results,
                                pipeline_result.passed,
                            )
                            .await;
                        if pipeline_result.passed {
                            iteration_history.push(serde_json::json!({
                                "iteration": attempts,
                                "output": stdout,
                                "exit_code": output.exit_code
                            }));
                            return Ok(stdout);
                        }
                        let blocking_reason = pipeline_result
                            .blocking_reason
                            .unwrap_or_else(|| "validation failed".to_string());
                        warn!(
                            iteration = attempts,
                            reason = %blocking_reason,
                            "Validation pipeline rejected iteration — retrying"
                        );
                        // Serialize the full GradientResult so bootstrap.py injects the
                        // complete judge response (score, confidence, reasoning, signals)
                        // into the next iteration's prompt. Re-serialisation is fine here —
                        // the LLM doesn't care about field ordering, and GradientResult has
                        // a metadata HashMap catchall so no judge-emitted data is lost.
                        let feedback = pipeline_result
                            .results
                            .gradient
                            .as_ref()
                            .and_then(|g| serde_json::to_string_pretty(g).ok())
                            .unwrap_or_else(|| blocking_reason.clone());
                        iteration_history.push(serde_json::json!({
                            "iteration": attempts,
                            "output": stdout,
                            "exit_code": output.exit_code,
                            "validation_failed": true,
                            "validation_reason": blocking_reason,
                            "feedback": feedback
                        }));
                        continue;
                    }
                    Err(e)
                        if e.chain()
                            .any(|cause| cause.downcast_ref::<JudgeFault>().is_some()) =>
                    {
                        // A judge whose verdict cannot be read, after its one re-run,
                        // is the judge's fault, not the worker's (ADR-017's Update of
                        // 2026-10-01): the execution ends failed with the fault as its
                        // reason, the worker is handed no feedback on its own output,
                        // and no further iteration is spent on it.
                        let reason = format!("{e:#}");
                        warn!(
                            iteration = attempts,
                            reason = %reason,
                            "Judge fault — ending the execution as failed"
                        );
                        return Err(RuntimeError::ExecutionFailed(reason));
                    }
                    Err(e) => {
                        let reason = format!("validation error: {e}");
                        warn!(
                            iteration = attempts,
                            error = %e,
                            "Validation pipeline error — treating iteration as failed"
                        );
                        iteration_history.push(serde_json::json!({
                            "iteration": attempts,
                            "output": stdout,
                            "exit_code": output.exit_code,
                            "validation_failed": true,
                            "validation_reason": reason,
                            // Surface the error as feedback so bootstrap.py injects it
                            // into the next iteration's prompt. Without this key the agent
                            // sees no feedback at all when validation times out or errors.
                            "feedback": reason
                        }));
                        continue;
                    }
                }
            } else {
                // No validation pipeline: return the first runtime-success.
                // Workflow-driven iteration is handled by WorkflowEngine.tick() — see ADR-015.
                iteration_history.push(serde_json::json!({
                    "iteration": attempts,
                    "output": stdout,
                    "exit_code": output.exit_code
                }));
                return Ok(stdout);
            }
        }

        Err(RuntimeError::ExecutionFailed(match outputs_failure {
            Some(reason) => format!("Max retries exceeded: {reason}"),
            None => "Max retries exceeded".to_string(),
        }))
    }

    fn extract_execution_context(
        payload: &serde_json::Value,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        let serde_json::Value::Object(map) = payload else {
            return std::collections::HashMap::new();
        };

        map.get("context_overrides")
            .and_then(|value| value.as_object())
            .map(|context| {
                context
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::{InstanceStatus, ResourceLimits, TaskOutput};
    use crate::domain::validation::{
        GradientResult, GradientValidator, ValidatorEntry, ValidatorKind,
    };
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn iteration_timeout_defaults_to_600_seconds() {
        let strategy = crate::domain::agent::ExecutionStrategy::default();
        assert_eq!(iteration_timeout(&strategy), Duration::from_secs(600));
        assert_eq!(DEFAULT_ITERATION_TIMEOUT_SECONDS, 600);
    }

    #[test]
    fn iteration_timeout_reads_the_manifest_and_falls_back_on_garbage() {
        let mut strategy = crate::domain::agent::ExecutionStrategy {
            iteration_timeout: Some("15m".to_string()),
            ..Default::default()
        };
        assert_eq!(iteration_timeout(&strategy), Duration::from_secs(900));
        strategy.iteration_timeout = Some("120s".to_string());
        assert_eq!(iteration_timeout(&strategy), Duration::from_secs(120));
        strategy.iteration_timeout = Some("soon".to_string());
        assert_eq!(iteration_timeout(&strategy), Duration::from_secs(600));
    }

    // Test runtime for exercising supervisor behavior.
    struct TestRuntime {
        spawn_results: Arc<Mutex<Vec<Result<InstanceId, RuntimeError>>>>,
        execute_results: Arc<Mutex<Vec<Result<TaskOutput, RuntimeError>>>>,
        execute_inputs: Arc<Mutex<Vec<TaskInput>>>,
        terminate_calls: Arc<Mutex<Vec<InstanceId>>>,
        /// The environment each spawn was given, in spawn order.
        spawn_envs: Arc<Mutex<Vec<HashMap<String, String>>>>,
        /// Optional delay injected into `execute()` to simulate long-running work.
        execute_delay: Option<Duration>,
    }

    impl TestRuntime {
        fn new() -> Self {
            Self {
                spawn_results: Arc::new(Mutex::new(Vec::new())),
                execute_results: Arc::new(Mutex::new(Vec::new())),
                execute_inputs: Arc::new(Mutex::new(Vec::new())),
                terminate_calls: Arc::new(Mutex::new(Vec::new())),
                spawn_envs: Arc::new(Mutex::new(Vec::new())),
                execute_delay: None,
            }
        }

        fn with_spawn_success(self, count: usize) -> Self {
            let mut results = Vec::new();
            for i in 0..count {
                results.push(Ok(InstanceId::new(format!("instance-{i}"))));
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
                        trajectory: vec![],
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
    impl AgentRuntime for TestRuntime {
        async fn spawn(&self, config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
            self.spawn_envs.lock().await.push(config.env.clone());
            let mut results = self.spawn_results.lock().await;
            results.remove(0)
        }

        async fn execute(
            &self,
            _id: &InstanceId,
            input: TaskInput,
        ) -> Result<TaskOutput, RuntimeError> {
            if let Some(delay) = self.execute_delay {
                tokio::time::sleep(delay).await;
            }
            self.execute_inputs.lock().await.push(input);
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

    /// One iteration's verified outputs, as the observer was given them.
    type VerifiedOutputs = (u8, Vec<ProducedFile>);

    // Test observer that records callback invocations.
    #[derive(Default)]
    struct TestObserver {
        iteration_starts: Arc<Mutex<Vec<u8>>>,
        iteration_completes: Arc<Mutex<Vec<u8>>>,
        iteration_fails: Arc<Mutex<Vec<u8>>>,
        /// Each failed iteration's reason, in order.
        fail_reasons: Arc<Mutex<Vec<String>>>,
        /// The output kept by each iteration failed for its declared outputs.
        missing_outputs_output: Arc<Mutex<Vec<String>>>,
        /// What each iteration's verified outputs were.
        verified: Arc<Mutex<Vec<VerifiedOutputs>>>,
    }

    #[async_trait]
    impl SupervisorObserver for TestObserver {
        async fn on_iteration_start(&self, iteration: u8, _prompt: &str) {
            self.iteration_starts.lock().await.push(iteration);
        }

        async fn on_console_output(&self, _iteration: u8, _stream: &str, _content: &str) {}

        async fn on_iteration_complete(&self, iteration: u8, _result: &str, _exit_code: i64) {
            self.iteration_completes.lock().await.push(iteration);
        }

        async fn on_iteration_fail(&self, iteration: u8, error: &str) {
            self.iteration_fails.lock().await.push(iteration);
            self.fail_reasons.lock().await.push(error.to_string());
        }

        async fn on_outputs_verified(&self, iteration: u8, produced: &[ProducedFile]) {
            self.verified
                .lock()
                .await
                .push((iteration, produced.to_vec()));
        }

        async fn on_outputs_missing(&self, iteration: u8, output: &str, reason: &str) {
            self.missing_outputs_output
                .lock()
                .await
                .push(output.to_string());
            self.on_iteration_fail(iteration, reason).await;
        }

        async fn on_instance_spawned(&self, _iteration: u8, _instance_id: &InstanceId) {}

        async fn on_instance_terminated(&self, _iteration: u8, _instance_id: &InstanceId) {}

        async fn on_validation_complete(
            &self,
            _iteration: u8,
            _results: &ValidationResults,
            _passed: bool,
        ) {
        }
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
                tool_validation: None,
                delivery: None,
                outputs: Vec::new(),
            },
            volumes: Vec::new(),
            keep_container_on_failure: false,
            image: "python:3.12".to_string(),
            bootstrap_path: None,
            execution_id: crate::domain::execution::ExecutionId::new(),
            workflow_execution_id: None,
        }
    }

    fn create_test_input() -> ExecutionInput {
        ExecutionInput {
            intent: Some("Test task".to_string()),
            input: serde_json::json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_supervisor_success_first_iteration() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Success output".to_string()]),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
        // Value::String is unwrapped directly — no surrounding quotes
        assert_eq!(result.unwrap(), "Success output");

        // Verify observer was called correctly
        assert_eq!(observer.iteration_starts.lock().await.len(), 1);
        assert_eq!(observer.iteration_completes.lock().await.len(), 1);
        assert_eq!(observer.iteration_fails.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn test_supervisor_retries_on_spawn_failure() {
        let runtime = Arc::new(TestRuntime::new());

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
            trajectory: vec![],
        }));

        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(TestObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
        // Verify we had one failure and one success
        assert_eq!(observer.iteration_starts.lock().await.len(), 2);
        assert_eq!(observer.iteration_fails.lock().await.len(), 1);
        assert_eq!(observer.iteration_completes.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn test_supervisor_passes_context_overrides_to_runtime() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Success output".to_string()]),
        );
        let supervisor = Supervisor::new(runtime.clone());

        let result = supervisor
            .run_loop(
                create_test_config(),
                ExecutionInput {
                    intent: Some("Test task".to_string()),
                    input: serde_json::json!({
                        "context_overrides": {
                            "repo": "aegis",
                            "owner": "100monkeys"
                        }
                    }),
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                1,
                Arc::new(TestObserver::default()),
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
        let execute_inputs = runtime.execute_inputs.lock().await;
        assert_eq!(execute_inputs.len(), 1);
        assert_eq!(
            execute_inputs[0].context.get("repo"),
            Some(&serde_json::json!("aegis"))
        );
        assert_eq!(
            execute_inputs[0].context.get("owner"),
            Some(&serde_json::json!("100monkeys"))
        );
    }

    #[tokio::test]
    async fn test_supervisor_max_retries_exceeded() {
        let runtime = Arc::new(TestRuntime::new());

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
        let observer = Arc::new(TestObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
                None,
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
            TestRuntime::new()
                .with_spawn_success(2)
                .with_execute_success(vec!["Output1".to_string(), "Output2".to_string()]),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

        let _result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer,
                CancellationToken::new(),
                None,
            )
            .await;

        // Verify instance was terminated
        let terminate_calls = runtime.terminate_calls.lock().await;
        assert_eq!(terminate_calls.len(), 1);
        assert_eq!(terminate_calls[0].as_str(), "instance-0");
    }

    #[tokio::test]
    async fn test_supervisor_overall_timeout() {
        // Runtime that sleeps longer than the timeout allows.
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(3)
                .with_execute_success(vec![
                    "Output".to_string(),
                    "Output".to_string(),
                    "Output".to_string(),
                ])
                .with_execute_delay(Duration::from_secs(5)),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

        // Set a short timeout so it fires before execution completes.
        let mut config = create_test_config();
        config.resources.timeout_seconds = Some(1);

        let result = supervisor
            .run_loop(
                config,
                create_test_input(),
                3,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RuntimeError::TimedOut(1)));

        let terminate_calls = runtime.terminate_calls.lock().await;
        assert_eq!(terminate_calls.len(), 1);
        assert_eq!(terminate_calls[0].as_str(), "instance-0");
    }

    /// An iteration whose bootstrap never returns (it waits on the dispatch
    /// gateway with no timeout, AEGIS ADR-040) is ended at the iteration's
    /// bound by the supervisor, which terminates its container; the next
    /// iteration gets a fresh container and the same bound.
    #[tokio::test]
    async fn iteration_whose_bootstrap_never_returns_is_ended_at_the_iteration_bound() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(2)
                .with_execute_success(vec!["never".to_string(), "never".to_string()])
                .with_execute_delay(Duration::from_secs(3600)),
        );
        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

        let mut config = create_test_config();
        config.execution.iteration_timeout = Some("1s".to_string());
        config.resources.timeout_seconds = Some(60);

        let started = std::time::Instant::now();
        let result = supervisor
            .run_loop(
                config,
                create_test_input(),
                2,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;
        let elapsed = started.elapsed();

        assert!(
            matches!(result, Err(RuntimeError::ExecutionFailed(_))),
            "{result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "each iteration ends at its 1 s bound, not the execution's 60 s: {elapsed:?}"
        );
        assert_eq!(*observer.iteration_fails.lock().await, vec![1, 2]);
        let terminated: Vec<String> = runtime
            .terminate_calls
            .lock()
            .await
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        assert_eq!(terminated, vec!["instance-0", "instance-1"]);
    }

    #[tokio::test]
    async fn test_supervisor_cancellation() {
        // Runtime that sleeps long enough for cancellation to trigger.
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Output".to_string()])
                .with_execute_delay(Duration::from_secs(30)),
        );

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

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
            .run_loop(
                config,
                create_test_input(),
                3,
                observer.clone(),
                token,
                None,
            )
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
            TestRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec!["Success".to_string()]),
        );

        let supervisor = Supervisor::new(runtime);
        let observer = Arc::new(TestObserver::default());

        let config = create_test_config(); // timeout_seconds: None
        let result = supervisor
            .run_loop(
                config,
                create_test_input(),
                3,
                observer,
                CancellationToken::new(),
                None,
            )
            .await;

        assert!(result.is_ok());
    }

    // ── A judge fault is not the worker's failed validation (ADR-017) ─────────

    /// A validator that answers each call with the next scripted result.
    struct ScriptedValidator {
        results: std::sync::Mutex<Vec<anyhow::Result<GradientResult>>>,
    }

    #[async_trait]
    impl GradientValidator for ScriptedValidator {
        async fn validate(&self, _ctx: &ValidationContext) -> anyhow::Result<GradientResult> {
            self.results.lock().unwrap().remove(0)
        }
    }

    fn semantic_pipeline(results: Vec<anyhow::Result<GradientResult>>) -> Arc<ValidationPipeline> {
        Arc::new(ValidationPipeline::new(vec![ValidatorEntry {
            kind: ValidatorKind::Semantic,
            validator: Box::new(ScriptedValidator {
                results: std::sync::Mutex::new(results),
            }),
            min_score: 0.7,
            min_confidence: 0.0,
        }]))
    }

    fn verdict(score: f64, reasoning: &str) -> GradientResult {
        GradientResult {
            score,
            confidence: 0.9,
            reasoning: reasoning.to_string(),
            signals: vec![],
            metadata: HashMap::new(),
        }
    }

    fn faulted_twice() -> JudgeFault {
        JudgeFault {
            judge_agent: "haiku-judge-agent".to_string(),
            reason: "it faulted twice, on two fresh runs; the first: the output is not JSON; \
                     the second: the output is not JSON"
                .to_string(),
            output: "I think the answer is fine.".to_string(),
        }
    }

    async fn run_with_pipeline(
        runtime: Arc<TestRuntime>,
        observer: Arc<TestObserver>,
        pipeline: Arc<ValidationPipeline>,
    ) -> Result<String, RuntimeError> {
        Supervisor::new(runtime)
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer,
                CancellationToken::new(),
                Some(pipeline),
            )
            .await
    }

    /// Every way the run broke the judge-fault rule, so one red reports them all:
    /// a further worker iteration, feedback handed to the worker, and an ending
    /// that is not a failure saying judge fault and naming the judge.
    async fn judge_fault_complaints(
        runtime: &TestRuntime,
        observer: &TestObserver,
        result: Result<String, RuntimeError>,
    ) -> Vec<String> {
        let mut complaints = Vec::new();
        let worker_iterations = observer.iteration_starts.lock().await.len();
        if worker_iterations != 1 {
            complaints.push(format!(
                "a judge fault must consume no further worker iteration, but the worker ran {worker_iterations} times"
            ));
        }
        let handed_feedback = runtime
            .spawn_envs
            .lock()
            .await
            .iter()
            .filter(|env| env.contains_key("AEGIS_ITERATION_HISTORY"))
            .count();
        if handed_feedback != 0 {
            complaints.push(format!(
                "the judge fault must not be handed to the worker as feedback on its own output, but {handed_feedback} iterations were handed history"
            ));
        }
        match result {
            Err(RuntimeError::ExecutionFailed(reason))
                if reason.contains("judge fault") && reason.contains("haiku-judge-agent") => {}
            other => complaints.push(format!(
                "a judge fault must end the execution failed with a reason that says judge fault and names the judge, got: {other:?}"
            )),
        }
        complaints
    }

    /// The worker's output is fine; its judge's verdict could not be read twice.
    /// The execution ends failed with the judge fault as its reason, after one
    /// worker iteration, and no feedback is ever handed to the worker.
    #[tokio::test]
    async fn judge_fault_ends_the_execution_failed_without_feedback_or_another_iteration() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(3)
                .with_execute_success(vec!["the answer".to_string(); 3]),
        );
        let observer = Arc::new(TestObserver::default());
        let pipeline = semantic_pipeline(vec![
            Err(anyhow::Error::new(faulted_twice())),
            Err(anyhow::Error::new(faulted_twice())),
            Err(anyhow::Error::new(faulted_twice())),
        ]);

        let result = run_with_pipeline(runtime.clone(), observer.clone(), pipeline).await;

        let complaints = judge_fault_complaints(&runtime, &observer, result).await;
        assert!(complaints.is_empty(), "{}", complaints.join("; "));
    }

    /// The multi-judge shortfall carries the fault beneath its own message; the
    /// supervisor finds it there and ends the execution the same way.
    #[tokio::test]
    async fn judge_fault_beneath_a_multi_judge_shortfall_ends_the_execution_failed() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(3)
                .with_execute_success(vec!["the answer".to_string(); 3]),
        );
        let observer = Arc::new(TestObserver::default());
        let shortfall = || {
            Err(anyhow::Error::new(faulted_twice())
                .context("MultiJudge: insufficient judges succeeded: 0 of 1 required (total: 1)"))
        };
        let pipeline = semantic_pipeline(vec![shortfall(), shortfall(), shortfall()]);

        let result = run_with_pipeline(runtime.clone(), observer.clone(), pipeline).await;

        let complaints = judge_fault_complaints(&runtime, &observer, result).await;
        assert!(complaints.is_empty(), "{}", complaints.join("; "));
    }

    /// The control: a worker whose output the judge scores below the threshold
    /// still gets the verdict as feedback and another iteration.
    #[tokio::test]
    async fn ordinary_failed_validation_still_gets_feedback_and_another_iteration() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(2)
                .with_execute_success(vec!["a wrong answer".to_string(), "the answer".to_string()]),
        );
        let observer = Arc::new(TestObserver::default());
        let pipeline = semantic_pipeline(vec![
            Ok(verdict(0.2, "the answer is wrong")),
            Ok(verdict(1.0, "the answer is right")),
        ]);

        let result = run_with_pipeline(runtime.clone(), observer.clone(), pipeline).await;

        assert_eq!(result.unwrap(), "the answer");
        assert_eq!(
            observer.iteration_starts.lock().await.len(),
            2,
            "a failed validation of the worker's output earns another iteration"
        );
        let envs = runtime.spawn_envs.lock().await;
        let history = envs[1]
            .get("AEGIS_ITERATION_HISTORY")
            .expect("the second iteration is handed the first one's history");
        assert!(
            history.contains("\"validation_failed\":true")
                && history.contains("the answer is wrong"),
            "the worker is handed the judge's verdict as feedback, got: {history}"
        );
    }

    // ── Declared outputs (ADR-005, Update of 2026-10-06, O2 and O3) ──────────

    /// A volume double: the files it holds, by path inside the volume.
    struct VolumeDouble {
        files: HashMap<String, Vec<u8>>,
        reads: std::sync::Mutex<Vec<OutputLocation>>,
    }

    impl VolumeDouble {
        fn holding(files: &[(&str, &[u8])]) -> Arc<Self> {
            Arc::new(Self {
                files: files
                    .iter()
                    .map(|(path, bytes)| (path.to_string(), bytes.to_vec()))
                    .collect(),
                reads: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl DeclaredOutputReader for VolumeDouble {
        async fn read_head(
            &self,
            location: &OutputLocation,
            head_len: usize,
        ) -> Result<Option<OutputHead>, String> {
            self.reads.lock().unwrap().push(location.clone());
            Ok(self
                .files
                .get(&location.path_in_volume)
                .map(|bytes| OutputHead {
                    size_bytes: bytes.len() as u64,
                    head: bytes.iter().take(head_len).copied().collect(),
                    content_type: "application/pdf".to_string(),
                }))
        }
    }

    fn pdf_output() -> DeclaredOutput {
        DeclaredOutput {
            path: "/workspace/x.pdf".to_string(),
            min_bytes: Some(8),
            magic: Some("%PDF".to_string()),
        }
    }

    /// The test config with one volume mounted at `/workspace` and the
    /// given declared outputs.
    fn config_declaring(outputs: Vec<DeclaredOutput>) -> RuntimeConfig {
        let mut config = create_test_config();
        config.volumes = vec![crate::domain::volume::VolumeMount::new(
            crate::domain::volume::VolumeId::new(),
            std::path::PathBuf::from("/workspace"),
            crate::domain::volume::AccessMode::ReadWrite,
            crate::domain::volume::FilerEndpoint::new("http://localhost:8888").unwrap(),
            "/aegis/volumes/test/workspace".to_string(),
        )];
        config.execution.outputs = outputs;
        config
    }

    /// The model answers the path of a file it never wrote, `max_retries`
    /// times; `reader` is the volume, or none.
    async fn run_declaring(
        outputs: Vec<DeclaredOutput>,
        reader: Option<Arc<dyn DeclaredOutputReader>>,
        max_retries: u32,
        pipeline: Option<Arc<ValidationPipeline>>,
    ) -> (
        Arc<TestRuntime>,
        Arc<TestObserver>,
        Result<String, RuntimeError>,
    ) {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(max_retries as usize)
                .with_execute_success(vec!["/workspace/x.pdf".to_string(); max_retries as usize]),
        );
        let observer = Arc::new(TestObserver::default());
        let mut supervisor = Supervisor::new(runtime.clone());
        if let Some(reader) = reader {
            supervisor = supervisor.with_output_reader(reader);
        }
        let result = supervisor
            .run_loop(
                config_declaring(outputs),
                create_test_input(),
                max_retries,
                observer.clone(),
                CancellationToken::new(),
                pipeline,
            )
            .await;
        (runtime, observer, result)
    }

    /// Test 1. The model answers "/workspace/x.pdf" with no tool call and no
    /// file is there: each iteration fails with "declared output
    /// /workspace/x.pdf does not exist", keeps the model's text, hands the
    /// sentence to the next iteration as feedback, and past max retries the
    /// execution fails with it.
    #[tokio::test]
    async fn declared_output_missing_fails_each_iteration_and_the_execution_past_max_retries() {
        let sentence = "declared output /workspace/x.pdf does not exist";
        let (runtime, observer, result) = run_declaring(
            vec![pdf_output()],
            Some(VolumeDouble::holding(&[])),
            2,
            None,
        )
        .await;
        let mut complaints: Vec<String> = Vec::new();
        let reasons = observer.fail_reasons.lock().await.clone();
        if reasons != vec![sentence.to_string(), sentence.to_string()] {
            complaints.push(format!("the iterations failed with {reasons:?}"));
        }
        let completes = observer.iteration_completes.lock().await.clone();
        if !completes.is_empty() {
            complaints.push(format!("iterations {completes:?} counted as completed"));
        }
        let kept = observer.missing_outputs_output.lock().await.clone();
        if kept != vec!["/workspace/x.pdf".to_string(); 2] {
            complaints.push(format!("the failed iterations kept {kept:?}"));
        }
        let envs = runtime.spawn_envs.lock().await.clone();
        let history = envs
            .get(1)
            .and_then(|env| env.get("AEGIS_ITERATION_HISTORY"))
            .cloned()
            .unwrap_or_default();
        if !history.contains(&format!("\"feedback\":\"{sentence}\"")) {
            complaints.push(format!("iteration 2 was not told the sentence: {history}"));
        }
        match &result {
            Err(RuntimeError::ExecutionFailed(reason))
                if reason == &format!("Max retries exceeded: {sentence}") => {}
            other => complaints.push(format!("the execution ended {other:?}")),
        }
        println!("declared output missing: iteration reasons {reasons:?}; execution {result:?}");
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Test 2. The file is there: the execution completes, and the observer
    /// is given what was found before the iteration counts as completed.
    #[tokio::test]
    async fn declared_output_present_completes_with_produced_files() {
        let volume = VolumeDouble::holding(&[("/x.pdf", b"%PDF-1.7 a real document")]);
        let (_runtime, observer, result) =
            run_declaring(vec![pdf_output()], Some(volume.clone()), 2, None).await;
        let mut complaints: Vec<String> = Vec::new();
        if !matches!(&result, Ok(out) if out == "/workspace/x.pdf") {
            complaints.push(format!("the execution ended {result:?}"));
        }
        let verified = observer.verified.lock().await.clone();
        let expected = vec![(
            1u8,
            vec![ProducedFile {
                path: "/workspace/x.pdf".to_string(),
                size_bytes: 24,
                content_type: "application/pdf".to_string(),
            }],
        )];
        if verified != expected {
            complaints.push(format!("the verified outputs were {verified:?}"));
        }
        let reads = volume.reads.lock().unwrap().clone();
        if reads.len() != 1 || reads[0].path_in_volume != "/x.pdf" {
            complaints.push(format!("the volume was read at {reads:?}"));
        }
        println!("declared output present: execution {result:?}; produced_files {verified:?}");
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Test 3. A short file and a wrong prefix each give their own sentence,
    /// and a file failing both reports both, as does a second output.
    #[tokio::test]
    async fn short_and_wrong_prefix_outputs_give_their_own_sentences_and_all_are_reported() {
        let mut complaints: Vec<String> = Vec::new();
        let cases: [(&[u8], &str); 3] = [
            (
                b"%PDF",
                "declared output /workspace/x.pdf is 4 bytes, under min_bytes 8",
            ),
            (
                b"<html> not a pdf",
                "declared output /workspace/x.pdf does not start with \"%PDF\"",
            ),
            (
                b"PK",
                "declared output /workspace/x.pdf is 2 bytes, under min_bytes 8; \
                 declared output /workspace/x.pdf does not start with \"%PDF\"",
            ),
        ];
        for (bytes, expected) in cases {
            let (_r, observer, _result) = run_declaring(
                vec![pdf_output()],
                Some(VolumeDouble::holding(&[("/x.pdf", bytes)])),
                1,
                None,
            )
            .await;
            let reasons = observer.fail_reasons.lock().await.clone();
            if reasons != vec![expected.to_string()] {
                complaints.push(format!("for {bytes:?} the reasons were {reasons:?}"));
            }
        }
        let second = DeclaredOutput {
            path: "/workspace/out/y.csv".to_string(),
            min_bytes: None,
            magic: None,
        };
        let (_r, observer, _result) = run_declaring(
            vec![pdf_output(), second],
            Some(VolumeDouble::holding(&[])),
            1,
            None,
        )
        .await;
        let reasons = observer.fail_reasons.lock().await.clone();
        let both = "declared output /workspace/x.pdf does not exist; \
                    declared output /workspace/out/y.csv does not exist";
        if reasons != vec![both.to_string()] {
            complaints.push(format!("two missing outputs gave {reasons:?}"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Test 4. With a validation pipeline that would pass, a missing output
    /// still fails the iteration, and no judge is spent on it.
    #[tokio::test]
    async fn a_pipeline_that_would_pass_does_not_rescue_a_missing_output() {
        let pipeline = semantic_pipeline(vec![Ok(verdict(1.0, "looks done"))]);
        let (_r, observer, result) = run_declaring(
            vec![pdf_output()],
            Some(VolumeDouble::holding(&[])),
            1,
            Some(pipeline.clone()),
        )
        .await;
        let mut complaints: Vec<String> = Vec::new();
        match &result {
            Err(RuntimeError::ExecutionFailed(reason))
                if reason
                    == "Max retries exceeded: declared output /workspace/x.pdf does not exist" => {}
            other => complaints.push(format!("the execution ended {other:?}")),
        }
        let reasons = observer.fail_reasons.lock().await.clone();
        if reasons != vec!["declared output /workspace/x.pdf does not exist".to_string()] {
            complaints.push(format!("the iteration failed with {reasons:?}"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Test 5. A node with no reader cannot check a declared output, and
    /// never passes it unchecked.
    #[tokio::test]
    async fn a_node_with_no_reader_fails_every_declared_output_as_unchecked() {
        let (_r, observer, result) = run_declaring(vec![pdf_output()], None, 1, None).await;
        let sentence =
            "declared output /workspace/x.pdf cannot be checked: this node has no volume reader";
        let mut complaints: Vec<String> = Vec::new();
        let reasons = observer.fail_reasons.lock().await.clone();
        if reasons != vec![sentence.to_string()] {
            complaints.push(format!("the iteration failed with {reasons:?}"));
        }
        match &result {
            Err(RuntimeError::ExecutionFailed(reason))
                if reason == &format!("Max retries exceeded: {sentence}") => {}
            other => complaints.push(format!("the execution ended {other:?}")),
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
