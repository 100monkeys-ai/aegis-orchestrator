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
use crate::domain::runtime::{
    with_exit_code, AgentRuntime, InstanceId, RuntimeConfig, RuntimeError, TaskInput,
};
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

/// The wait before the next try after a try whose execution failed:
/// 15 s after the first failed try, doubled for each try after it, never
/// more than 120 s. A failed execution is most often an upstream refusal
/// (a model provider's 503 once the model layer's own retries are spent),
/// and a try started at once meets the same burst again.
pub fn failed_execution_backoff(failed: u32) -> Duration {
    Duration::from_secs((15u64 << failed.saturating_sub(1).min(3)).min(120))
}

/// The execution's overall bound, as the loop sees it before each try.
#[derive(Debug, Clone, Copy)]
struct TryBound {
    /// When the bound runs out.
    deadline: tokio::time::Instant,
    /// The bound, in seconds.
    seconds: u64,
}

impl TryBound {
    /// The sentence the execution ends with when the time left under the
    /// bound is less than a try needs (AEGIS ADR-141, Update of 2026-10-15,
    /// clause (1)); `None` when a try fits.
    fn refuses_a_try(&self, try_needs: Duration) -> Option<String> {
        let left = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        (left < try_needs).then(|| {
            format!(
                "Execution ended: {} s left under the {} s bound, under the {} s a try needs",
                left.as_secs(),
                self.seconds,
                try_needs.as_secs()
            )
        })
    }
}

/// Whether iteration `iteration` of the execution is recorded exhausted
/// (AEGIS ADR-141, Update of 2026-10-14, R2): the inner loop records it when
/// the try's call budget or the try clock ends the try.
async fn recorded_exhausted(
    repository: Option<&Arc<dyn ExecutionRepository>>,
    execution_id: Option<ExecutionId>,
    iteration: u8,
) -> Option<crate::domain::execution::ExhaustedBy> {
    let (Some(repository), Some(execution_id)) = (repository, execution_id) else {
        return None;
    };
    match repository.find_by_id_unscoped(execution_id).await {
        Ok(Some(execution)) => execution
            .iterations()
            .iter()
            .find(|i| i.number == iteration)
            .and_then(|i| i.exhausted.as_ref())
            .map(|e| e.by),
        _ => None,
    }
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

    /// Called before [`Self::on_iteration_fail`] when the iteration ran out
    /// of its clock (AEGIS ADR-141, Update of 2026-10-14, R2): an observer
    /// that keeps a record stores the iteration as exhausted, with the
    /// history the next try is opened from.
    async fn on_iteration_exhausted(
        &self,
        _iteration: u8,
        _by: crate::domain::execution::ExhaustedBy,
    ) {
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
                        // Where it was read, so the download route reads
                        // this file and no other (ADR-005 I8).
                        volume_id: Some(location.volume_id),
                        path_in_volume: Some(location.path_in_volume.clone()),
                        declared: true,
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

/// Read every `/workspace` file `text` names that no output in `outputs`
/// declares, from the execution's volumes: each one found is a produced file
/// marked undeclared. One that is not there, or cannot be read, is left out
/// and fails nothing; a node with no reader reads none.
pub async fn read_named_files(
    text: &str,
    outputs: &[DeclaredOutput],
    config: &RuntimeConfig,
    reader: Option<&dyn DeclaredOutputReader>,
) -> Vec<ProducedFile> {
    let Some(reader) = reader else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for path in crate::domain::tool_requirement::workspace_files_named(text) {
        if outputs.iter().any(|output| output.path == path) {
            continue;
        }
        let Some(location) = output_location(config, path) else {
            continue;
        };
        if let Ok(Some(head)) = reader.read_head(&location, 0).await {
            found.push(ProducedFile {
                path: path.to_string(),
                size_bytes: head.size_bytes,
                content_type: head.content_type,
                volume_id: Some(location.volume_id),
                path_in_volume: Some(location.path_in_volume.clone()),
                declared: false,
            });
        }
    }
    found
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
        let bound = TryBound {
            deadline: tokio::time::Instant::now() + overall_timeout,
            seconds: overall_timeout_secs,
        };

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
                bound,
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
        bound: TryBound,
    ) -> Result<String, RuntimeError> {
        let mut attempts = 0;
        // The tries counted against `max_retries`: every try but one an
        // upstream refusal ended (AEGIS ADR-141, Update of 2026-10-15
        // `upstream-rate-limit-spends-no-try`, clause 2). `attempts` stays
        // the iteration's number.
        let mut counted: u32 = 0;
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

        // How the last iteration ended, in the person's words: the reason the
        // execution ends with past max retries.
        let mut last_end: Option<AttemptEnd> = None;

        while counted < max_retries {
            // Check cancellation before each iteration
            if cancellation_token.is_cancelled() {
                info!("Execution cancelled before iteration {}", attempts + 1);
                return Err(RuntimeError::Cancelled);
            }

            // The first try always starts; a later try starts only when it
            // fits under the execution's bound, and otherwise the execution
            // ends now rather than inside the try (AEGIS ADR-141, Update of
            // 2026-10-15, clause (1)). The last try was recorded exhausted
            // where it ended (R2).
            if attempts > 0 {
                if let Some(ended) = bound.refuses_a_try(per_iteration_timeout) {
                    warn!(iteration = attempts + 1, "{}", ended);
                    return Err(RuntimeError::ExecutionFailed(ended));
                }
            }

            attempts += 1;
            counted += 1;
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
                    last_end = Some(AttemptEnd::Cause(spawn_failure_cause(&e)));

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
                    if let RuntimeError::ExecutionFailedWithLogs { logs, .. } = &e {
                        if !logs.is_empty() {
                            observer
                                .on_console_output(attempts as u8, "stderr", &logs.join("\n"))
                                .await;
                        }
                    }
                    let error_msg = format!("Execution failed: {e}");
                    warn!("{}", error_msg);
                    // A try its clock ended is exhausted (AEGIS ADR-141,
                    // Update of 2026-10-14, R2); the inner loop records the
                    // tries its call budget or the try clock ended.
                    let exhausted_by = if matches!(e, RuntimeError::TimedOut(_)) {
                        observer
                            .on_iteration_exhausted(
                                attempts as u8,
                                crate::domain::execution::ExhaustedBy::Clock,
                            )
                            .await;
                        Some(crate::domain::execution::ExhaustedBy::Clock)
                    } else {
                        recorded_exhausted(
                            execution_repository.as_ref(),
                            execution_id_for_trajectory,
                            attempts as u8,
                        )
                        .await
                    };
                    let exhausted = exhausted_by.is_some();
                    // A try the provider's rate limit or unavailability ended
                    // is not counted; the wait below still comes before the
                    // next try, and the execution's bound still ends the run.
                    if exhausted_by == Some(crate::domain::execution::ExhaustedBy::Upstream) {
                        counted -= 1;
                        info!(
                            iteration = attempts,
                            "The try ended on an upstream refusal and is not counted"
                        );
                    }
                    observer.on_iteration_fail(attempts as u8, &error_msg).await;
                    last_end = Some(AttemptEnd::Cause(execution_failure_cause(&e)));

                    // Record execution failure in history; an exhausted try
                    // is not an item of it: the next try is opened from its
                    // constructed history instead.
                    if !exhausted {
                        iteration_history.push(serde_json::json!({
                            "iteration": attempts,
                            "error": error_msg
                        }));
                    }

                    if counted < max_retries {
                        let wait = failed_execution_backoff(attempts);
                        info!(
                            iteration = attempts,
                            wait_secs = wait.as_secs(),
                            "Waiting before the next try after a failed execution"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = cancellation_token.cancelled() => {
                                info!(
                                    iteration = attempts,
                                    "Execution cancelled while waiting before the next try"
                                );
                                return Err(RuntimeError::Cancelled);
                            }
                        }
                    }

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
            // or without a validation pipeline. A file the answer names that
            // no output declares is read too, and recorded undeclared when it
            // is there; its absence fails nothing (ADR-005 O8a).
            let declared_outputs = &runtime_config.execution.outputs;
            let declared_check = if declared_outputs.is_empty() {
                Ok(Vec::new())
            } else {
                check_declared_outputs(
                    declared_outputs,
                    &runtime_config,
                    self.output_reader.as_deref(),
                )
                .await
            };
            match declared_check {
                Ok(mut produced) => {
                    produced.extend(
                        read_named_files(
                            &stdout,
                            declared_outputs,
                            &runtime_config,
                            self.output_reader.as_deref(),
                        )
                        .await,
                    );
                    if !produced.is_empty() {
                        observer
                            .on_outputs_verified(attempts as u8, &produced)
                            .await;
                    }
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
                    last_end = Some(AttemptEnd::OutputsMissing(reason));
                    continue;
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
                        last_end = Some(AttemptEnd::Cause(format!(
                            "the last attempt's output was rejected: {blocking_reason}"
                        )));
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
                        last_end = Some(AttemptEnd::Cause(format!(
                            "the last attempt failed validation: {e:#}"
                        )));
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

        Err(RuntimeError::ExecutionFailed(max_retries_exceeded(
            attempts, last_end,
        )))
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

/// How the last iteration of a loop that ran out of retries ended.
enum AttemptEnd {
    /// Its declared outputs were missing: the sentence the execution has
    /// always ended with past max retries (ADR-005, Update of 2026-10-06, O2).
    OutputsMissing(String),
    /// Any other end, as a clause that completes "the last attempt …".
    Cause(String),
}

/// The error a loop that ran out of retries ends with: how many attempts it
/// made and how the last one ended, so the person reading the failure learns
/// why without reading the orchestrator's log.
fn max_retries_exceeded(attempts: u32, last_end: Option<AttemptEnd>) -> String {
    let plural = if attempts == 1 { "" } else { "s" };
    match last_end {
        Some(AttemptEnd::OutputsMissing(reason)) => format!("Max retries exceeded: {reason}"),
        Some(AttemptEnd::Cause(cause)) => {
            format!("Max retries exceeded after {attempts} attempt{plural}; {cause}")
        }
        None => "Max retries exceeded".to_string(),
    }
}

/// "the last attempt could not start: …", with the spawn's own error.
fn spawn_failure_cause(error: &RuntimeError) -> String {
    let detail = match error {
        RuntimeError::SpawnFailed(detail) => detail.clone(),
        other => other.to_string(),
    };
    format!("the last attempt could not start: {detail}")
}

/// At most this many lines of a failed process's standard error are named.
const STDERR_TAIL_LINES: usize = 5;
/// At most this many characters of a failed process's standard error are named.
const STDERR_TAIL_CHARS: usize = 2_000;

/// "the last attempt …" for an execution that returned an error: a timeout
/// names its bound; a failed process names its error line and the end of its
/// standard error, without the bootstrap's debug lines.
fn execution_failure_cause(error: &RuntimeError) -> String {
    match error {
        RuntimeError::TimedOut(seconds) => {
            format!("the last attempt ran past its time limit of {seconds} s and was stopped")
        }
        RuntimeError::ExecutionFailedWithLogs {
            error,
            logs,
            exit_code,
        } => {
            let tail = stderr_tail(logs);
            let named = with_exit_code(error, *exit_code);
            if tail.is_empty() || tail == *error {
                format!("the last attempt failed: {named}")
            } else {
                format!("the last attempt failed: {named}; the end of its standard error:\n{tail}")
            }
        }
        RuntimeError::ExecutionFailed(detail) => format!("the last attempt failed: {detail}"),
        other => format!("the last attempt failed: {other}"),
    }
}

/// The last lines of a process's standard error that are not the bootstrap's
/// own debug lines, bounded by [`STDERR_TAIL_LINES`] and [`STDERR_TAIL_CHARS`].
fn stderr_tail(logs: &[String]) -> String {
    let lines: Vec<&str> = logs
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("[BOOTSTRAP "))
        .collect();
    let tail = lines[lines.len().saturating_sub(STDERR_TAIL_LINES)..].join("\n");
    let chars = tail.chars().count();
    if chars <= STDERR_TAIL_CHARS {
        tail
    } else {
        let kept: String = tail.chars().skip(chars - STDERR_TAIL_CHARS).collect();
        format!("…{kept}")
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
        /// Each console output, as (iteration, stream, content), in order.
        console: Arc<Mutex<Vec<(u8, String, String)>>>,
        /// Each exhausted iteration and what ended it, in order.
        exhausted: Arc<Mutex<Vec<(u8, crate::domain::execution::ExhaustedBy)>>>,
    }

    #[async_trait]
    impl SupervisorObserver for TestObserver {
        async fn on_iteration_start(&self, iteration: u8, _prompt: &str) {
            self.iteration_starts.lock().await.push(iteration);
        }

        async fn on_console_output(&self, iteration: u8, stream: &str, content: &str) {
            self.console
                .lock()
                .await
                .push((iteration, stream.to_string(), content.to_string()));
        }

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

        async fn on_iteration_exhausted(
            &self,
            iteration: u8,
            by: crate::domain::execution::ExhaustedBy,
        ) {
            self.exhausted.lock().await.push((iteration, by));
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
                require_tool_call: false,
                max_tool_calls: crate::domain::agent::default_max_tool_calls(),
                done_when: None,
            },
            volumes: Vec::new(),
            keep_container_on_failure: false,
            image: "python:3.12".to_string(),
            bootstrap_path: None,
            execution_id: crate::domain::execution::ExecutionId::new(),
            workflow_execution_id: None,
            program_files: Vec::new(),
            program_input: None,
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

    /// A failed try's standard error reaches the execution's events as its
    /// console output, and the failure reason is the error line alone.
    #[tokio::test]
    async fn failed_try_stderr_reaches_console_output_and_reason_is_the_error() {
        let runtime = Arc::new(TestRuntime::new().with_spawn_success(1));
        runtime
            .execute_results
            .lock()
            .await
            .push(Err(RuntimeError::ExecutionFailedWithLogs {
                error: "RuntimeError: the model call failed".to_string(),
                logs: vec![
                    "[BOOTSTRAP DEBUG] execution_id=f0b6ddb5".to_string(),
                    "[BOOTSTRAP DEBUG] agent_id=2c54df3e".to_string(),
                    "RuntimeError: the model call failed".to_string(),
                ],
                exit_code: None,
            }));

        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());
        let _ = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                1,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;

        assert_eq!(
            *observer.console.lock().await,
            vec![(
                1u8,
                "stderr".to_string(),
                "[BOOTSTRAP DEBUG] execution_id=f0b6ddb5\n[BOOTSTRAP DEBUG] agent_id=2c54df3e\nRuntimeError: the model call failed".to_string()
            )]
        );
        assert_eq!(
            *observer.fail_reasons.lock().await,
            vec![
                "Execution failed: Failed to execute task: RuntimeError: the model call failed"
                    .to_string()
            ]
        );
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
        tokio::time::pause();
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

        let started = tokio::time::Instant::now();
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
            elapsed < Duration::from_secs(20),
            "each iteration ends at its 1 s bound, with the 15 s wait between the tries, not the execution's 60 s: {elapsed:?}"
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
        let reads = volume.reads.lock().unwrap().clone();
        let expected = vec![(
            1u8,
            vec![ProducedFile {
                path: "/workspace/x.pdf".to_string(),
                size_bytes: 24,
                content_type: "application/pdf".to_string(),
                volume_id: reads.first().map(|read| read.volume_id),
                path_in_volume: Some("/x.pdf".to_string()),
                declared: true,
            }],
        )];
        if verified != expected {
            complaints.push(format!("the verified outputs were {verified:?}"));
        }
        if reads.len() != 1 || reads[0].path_in_volume != "/x.pdf" {
            complaints.push(format!("the volume was read at {reads:?}"));
        }
        println!("declared output present: execution {result:?}; produced_files {verified:?}");
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// The model answers `answer` once; nothing else differs from
    /// [`run_declaring`].
    async fn run_answering(
        outputs: Vec<DeclaredOutput>,
        answer: &str,
        reader: Arc<dyn DeclaredOutputReader>,
    ) -> (Arc<TestObserver>, Result<String, RuntimeError>) {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(1)
                .with_execute_success(vec![answer.to_string()]),
        );
        let observer = Arc::new(TestObserver::default());
        let supervisor = Supervisor::new(runtime).with_output_reader(reader);
        let result = supervisor
            .run_loop(
                config_declaring(outputs),
                create_test_input(),
                1,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;
        (observer, result)
    }

    /// T7: an agent that declares no outputs answers the path of a file it
    /// wrote; the file is in its volume, so it is recorded as produced,
    /// marked undeclared, with where it was read.
    #[tokio::test]
    async fn a_file_the_answer_names_and_no_output_declares_is_recorded_undeclared() {
        let volume = VolumeDouble::holding(&[("/itinerary.md", b"# Van 1\n08:00 depot")]);
        let answer = "The itinerary is written to /workspace/itinerary.md.";
        let (observer, result) = run_answering(Vec::new(), answer, volume.clone()).await;
        let mut complaints: Vec<String> = Vec::new();
        if !matches!(&result, Ok(out) if out == answer) {
            complaints.push(format!("the execution ended {result:?}"));
        }
        let verified = observer.verified.lock().await.clone();
        let reads = volume.reads.lock().unwrap().clone();
        let expected = vec![(
            1u8,
            vec![ProducedFile {
                path: "/workspace/itinerary.md".to_string(),
                size_bytes: 19,
                content_type: "application/pdf".to_string(),
                volume_id: reads.first().map(|read| read.volume_id),
                path_in_volume: Some("/itinerary.md".to_string()),
                declared: false,
            }],
        )];
        println!("no declarations: recorded files {verified:?}");
        if verified != expected {
            complaints.push(format!(
                "the file the answer named was not recorded undeclared: the recorded files were {verified:?}"
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T8: the named file is not in the volume: the volume is asked, nothing
    /// is recorded, and the iteration completes; an undeclared file fails
    /// nothing.
    #[tokio::test]
    async fn a_named_file_that_is_absent_fails_nothing() {
        let volume = VolumeDouble::holding(&[]);
        let answer = "I wrote /workspace/itinerary.md";
        let (observer, result) = run_answering(Vec::new(), answer, volume.clone()).await;
        let mut complaints: Vec<String> = Vec::new();
        if !matches!(&result, Ok(out) if out == answer) {
            complaints.push(format!("the execution ended {result:?}"));
        }
        let fails = observer.fail_reasons.lock().await.clone();
        if !fails.is_empty() {
            complaints.push(format!("the iteration failed with {fails:?}"));
        }
        let completes = observer.iteration_completes.lock().await.clone();
        if completes != vec![1] {
            complaints.push(format!("the iterations completed were {completes:?}"));
        }
        let verified = observer.verified.lock().await.clone();
        if !verified.is_empty() {
            complaints.push(format!("an absent file was recorded: {verified:?}"));
        }
        let reads: Vec<String> = volume
            .reads
            .lock()
            .unwrap()
            .iter()
            .map(|read| read.path_in_volume.clone())
            .collect();
        println!("absent named file: execution {result:?}; volume read at {reads:?}");
        if reads != vec!["/itinerary.md".to_string()] {
            complaints.push(format!(
                "the volume was not asked for the named file: it was read at {reads:?}"
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// T9: a declared output and an undeclared file the answer names are
    /// recorded together, each once: the declared one marked declared and
    /// read once, though the answer names it too.
    #[tokio::test]
    async fn declared_and_undeclared_files_are_recorded_together_once_each() {
        let volume = VolumeDouble::holding(&[
            ("/x.pdf", b"%PDF-1.7 a real document"),
            ("/notes.md", b"notes"),
        ]);
        let answer = "Wrote /workspace/x.pdf and /workspace/notes.md, then /workspace/x.pdf again.";
        let (observer, result) = run_answering(vec![pdf_output()], answer, volume.clone()).await;
        let mut complaints: Vec<String> = Vec::new();
        if !matches!(&result, Ok(out) if out == answer) {
            complaints.push(format!("the execution ended {result:?}"));
        }
        let verified = observer.verified.lock().await.clone();
        let recorded: Vec<(String, bool)> = verified
            .iter()
            .flat_map(|(_, files)| files.iter().map(|f| (f.path.clone(), f.declared)))
            .collect();
        println!("declared and undeclared: recorded {recorded:?}");
        let expected = vec![
            ("/workspace/x.pdf".to_string(), true),
            ("/workspace/notes.md".to_string(), false),
        ];
        if verified.len() != 1 || recorded != expected {
            complaints.push(format!(
                "the declared and the undeclared file were not recorded once each: {verified:?}"
            ));
        }
        let pdf_reads = volume
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|read| read.path_in_volume == "/x.pdf")
            .count();
        if pdf_reads != 1 {
            complaints.push(format!("the declared output was read {pdf_reads} times"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// AEGIS ADR-005 I8 (P1): a produced file records the volume and the path
    /// inside it that the supervisor read it at, so the download route reads
    /// that file and no other.
    #[tokio::test]
    async fn a_produced_file_records_the_volume_and_path_it_was_read_at() {
        let volume = VolumeDouble::holding(&[("/x.pdf", b"%PDF-1.7 a real document")]);
        let (_runtime, observer, _result) =
            run_declaring(vec![pdf_output()], Some(volume.clone()), 2, None).await;
        let verified = observer.verified.lock().await.clone();
        let reads = volume.reads.lock().unwrap().clone();
        let read_at = reads.first().map(|read| read.volume_id);
        let recorded = verified
            .first()
            .and_then(|(_, files)| files.first())
            .map(|file| (file.volume_id, file.path_in_volume.clone()));
        println!("I8: read at volume {read_at:?}; recorded location {recorded:?}");
        assert!(read_at.is_some(), "the volume was never read");
        assert_eq!(
            recorded,
            Some((read_at, Some("/x.pdf".to_string()))),
            "I8: the produced file does not record the volume and path it was read at"
        );
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

    // ── The ending past max retries names how the last attempt ended ─────────

    /// Runs the loop and answers the error it ended with, or why it did not.
    async fn final_error(
        runtime: Arc<TestRuntime>,
        config: RuntimeConfig,
        max_retries: u32,
        pipeline: Option<Arc<ValidationPipeline>>,
    ) -> Result<String, String> {
        match Supervisor::new(runtime)
            .run_loop(
                config,
                create_test_input(),
                max_retries,
                Arc::new(TestObserver::default()),
                CancellationToken::new(),
                pipeline,
            )
            .await
        {
            Err(RuntimeError::ExecutionFailed(reason)) => Ok(reason),
            other => Err(format!("the execution ended {other:?}")),
        }
    }

    /// Every attempt that could not start: the ending names the attempts and
    /// the last spawn's error, in the singular for one attempt.
    #[tokio::test]
    async fn past_max_retries_after_spawn_errors_the_ending_names_the_last_spawn_error() {
        let mut complaints = Vec::new();
        for (attempts, expected) in [
            (
                3,
                "Max retries exceeded after 3 attempts; the last attempt could not start: \
                 Resource exhausted",
            ),
            (
                1,
                "Max retries exceeded after 1 attempt; the last attempt could not start: \
                 Resource exhausted",
            ),
        ] {
            let runtime = Arc::new(TestRuntime::new());
            for _ in 0..attempts {
                runtime
                    .spawn_results
                    .lock()
                    .await
                    .push(Err(RuntimeError::SpawnFailed(
                        "Resource exhausted".to_string(),
                    )));
            }
            match final_error(runtime, create_test_config(), attempts, None).await {
                Ok(reason) if reason == expected => {}
                Ok(reason) => complaints.push(format!("expected {expected:?}, got {reason:?}")),
                Err(other) => complaints.push(other),
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// Every attempt cut off at the iteration's bound: the ending says so,
    /// with the bound.
    #[tokio::test]
    async fn past_max_retries_after_iteration_timeouts_the_ending_names_the_bound() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(2)
                .with_execute_success(vec!["never".to_string(), "never".to_string()])
                .with_execute_delay(Duration::from_secs(3600)),
        );
        let mut config = create_test_config();
        config.execution.iteration_timeout = Some("1s".to_string());
        config.resources.timeout_seconds = Some(60);

        let reason = final_error(runtime, config, 2, None).await;

        assert_eq!(
            reason,
            Ok(
                "Max retries exceeded after 2 attempts; the last attempt ran past its time \
                limit of 1 s and was stopped"
                    .to_string()
            )
        );
    }

    /// Every attempt's process failed: the ending names the last one's error
    /// and the end of its standard error, without the bootstrap's debug lines;
    /// a process that wrote no error line is named by its exit code.
    #[tokio::test]
    async fn past_max_retries_after_failed_executions_the_ending_names_the_error_and_stderr_tail() {
        let traceback = RuntimeError::ExecutionFailedWithLogs {
            error: "ModuleNotFoundError: No module named 'requests'".to_string(),
            logs: vec![
                "[BOOTSTRAP DEBUG] execution_id=9ba643ba".to_string(),
                "Traceback (most recent call last):".to_string(),
                "  File \"/app/main.py\", line 1, in <module>".to_string(),
                "    import requests".to_string(),
                "ModuleNotFoundError: No module named 'requests'".to_string(),
                "[BOOTSTRAP DEBUG] exiting".to_string(),
            ],
            exit_code: None,
        };
        let killed = RuntimeError::ExecutionFailedWithLogs {
            error: "Bootstrap script exited with code 137".to_string(),
            logs: vec!["[BOOTSTRAP DEBUG] execution_id=9ba643ba".to_string()],
            exit_code: Some(137),
        };
        let mut complaints = Vec::new();
        for (last, expected) in [
            (
                traceback,
                "Max retries exceeded after 2 attempts; the last attempt failed: \
                 ModuleNotFoundError: No module named 'requests'; the end of its standard \
                 error:\nTraceback (most recent call last):\n  File \"/app/main.py\", line 1, \
                 in <module>\n    import requests\nModuleNotFoundError: No module named \
                 'requests'",
            ),
            (
                killed,
                "Max retries exceeded after 2 attempts; the last attempt failed: Bootstrap \
                 script exited with code 137",
            ),
        ] {
            let runtime = Arc::new(TestRuntime::new().with_spawn_success(2));
            {
                let mut results = runtime.execute_results.lock().await;
                results.push(Err(RuntimeError::ExecutionFailed(
                    "the first attempt's error".to_string(),
                )));
                results.push(Err(last));
            }
            match final_error(runtime, create_test_config(), 2, None).await {
                Ok(reason) if reason == expected => {}
                Ok(reason) => complaints.push(format!("expected {expected:?}, got {reason:?}")),
                Err(other) => complaints.push(other),
            }
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// A failed process whose exit code the runtime read is named by it in the
    /// ending, after its error line.
    #[tokio::test]
    async fn past_max_retries_after_a_failed_process_the_ending_names_its_exit_code() {
        let runtime = Arc::new(TestRuntime::new().with_spawn_success(2));
        {
            let mut results = runtime.execute_results.lock().await;
            results.push(Err(RuntimeError::ExecutionFailed(
                "the first attempt's error".to_string(),
            )));
            results.push(Err(RuntimeError::ExecutionFailedWithLogs {
                error: "ValueError: the input is not a route".to_string(),
                logs: vec!["ValueError: the input is not a route".to_string()],
                exit_code: Some(2),
            }));
        }
        let reason = final_error(runtime, create_test_config(), 2, None).await;
        assert_eq!(
            reason,
            Ok(
                "Max retries exceeded after 2 attempts; the last attempt failed: ValueError: \
                 the input is not a route (exited with code 2)"
                    .to_string()
            )
        );
    }

    /// Every attempt's validation errored, as when its judge ran out of time:
    /// the ending names the last validation error.
    #[tokio::test]
    async fn past_max_retries_after_validation_errors_the_ending_names_the_last_validation_error() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(3)
                .with_execute_success(vec!["the draft".to_string(); 3]),
        );
        let judge_failed = || {
            Err(anyhow::anyhow!(
                "Judge 'email-draft-judge' execution failed or was cancelled"
            ))
        };
        let pipeline = semantic_pipeline(vec![judge_failed(), judge_failed(), judge_failed()]);

        let reason = final_error(runtime, create_test_config(), 3, Some(pipeline)).await;

        assert_eq!(
            reason,
            Ok(
                "Max retries exceeded after 3 attempts; the last attempt failed validation: \
                Judge 'email-draft-judge' execution failed or was cancelled"
                    .to_string()
            )
        );
    }

    /// Every attempt's output was scored below the bar: the ending names the
    /// last verdict's reason.
    #[tokio::test]
    async fn past_max_retries_after_rejected_outputs_the_ending_names_the_last_verdict() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(3)
                .with_execute_success(vec!["the draft".to_string(); 3]),
        );
        let pipeline = semantic_pipeline(vec![
            Ok(verdict(0.1, "the draft is empty")),
            Ok(verdict(0.3, "the draft is short")),
            Ok(verdict(0.2, "the draft names no recipient")),
        ]);

        let reason = final_error(runtime, create_test_config(), 3, Some(pipeline)).await;

        assert_eq!(
            reason,
            Ok(
                "Max retries exceeded after 3 attempts; the last attempt's output was \
                rejected: Validation score 0.20 below minimum 0.70: the draft names no \
                recipient"
                    .to_string()
            )
        );
    }

    /// A runtime whose tries fail by their execution, as a model provider's
    /// 503 does once the model layer's own retries are spent, a set number
    /// of times before a try answers. The time of each spawn is kept.
    struct FailingThenAnsweringRuntime {
        failures_left: Mutex<u32>,
        spawn_times: Mutex<Vec<tokio::time::Instant>>,
    }

    impl FailingThenAnsweringRuntime {
        fn new(failures: u32) -> Self {
            Self {
                failures_left: Mutex::new(failures),
                spawn_times: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl AgentRuntime for FailingThenAnsweringRuntime {
        async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
            let mut times = self.spawn_times.lock().await;
            times.push(tokio::time::Instant::now());
            Ok(InstanceId::new(format!("instance-{}", times.len())))
        }

        async fn execute(
            &self,
            _id: &InstanceId,
            _input: TaskInput,
        ) -> Result<TaskOutput, RuntimeError> {
            let mut failures_left = self.failures_left.lock().await;
            if *failures_left > 0 {
                *failures_left -= 1;
                return Err(RuntimeError::ExecutionFailedWithLogs {
                    error: "RuntimeError: the model answered HTTP 503, code 3045".to_string(),
                    logs: vec![],
                    exit_code: None,
                });
            }
            Ok(TaskOutput {
                result: serde_json::Value::String("answered".to_string()),
                logs: vec![],
                tool_calls: vec![],
                exit_code: 0,
                trajectory: vec![],
            })
        }

        async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
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

    /// A try that fails by its execution is followed by a wait before the
    /// next try: 15 s after the first failure, 30 s after the second, so a
    /// burst of upstream refusals is not met again at once by every try.
    #[tokio::test]
    async fn a_failed_execution_waits_15_s_then_30_s_before_the_next_tries() {
        tokio::time::pause();
        let runtime = Arc::new(FailingThenAnsweringRuntime::new(2));
        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                5,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;

        assert_eq!(result.expect("the third try answers"), "answered");
        assert_eq!(*observer.iteration_fails.lock().await, vec![1, 2]);
        let times = runtime.spawn_times.lock().await.clone();
        assert_eq!(times.len(), 3, "three tries were spawned");
        let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps[0] >= Duration::from_secs(15) && gaps[1] >= Duration::from_secs(30),
            "the tries after a failed execution started {gaps:?} apart, expected at least 15 s then 30 s"
        );
    }

    /// Cancelling the execution while the supervisor waits after a failed
    /// execution ends it as cancelled, with no further try spawned.
    #[tokio::test]
    async fn cancelling_during_the_wait_after_a_failed_execution_returns_cancelled() {
        tokio::time::pause();
        let runtime = Arc::new(FailingThenAnsweringRuntime::new(1));
        let supervisor = Supervisor::new(runtime.clone());
        let observer = Arc::new(TestObserver::default());

        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            canceller.cancel();
        });

        let result = supervisor
            .run_loop(
                create_test_config(),
                create_test_input(),
                3,
                observer.clone(),
                token,
                None,
            )
            .await;

        assert!(
            matches!(result, Err(RuntimeError::Cancelled)),
            "cancelled during the wait after the failed try, the execution ended {result:?}"
        );
        assert_eq!(
            runtime.spawn_times.lock().await.len(),
            1,
            "no try is spawned after the cancel"
        );
    }

    /// AEGIS ADR-141, Update of 2026-10-14, R2: a try its clock ends is
    /// exhausted: the observer is told so before the failure, and the next
    /// try is handed no history item for it, so the agent runtime writes no
    /// "Previous Attempts" for it.
    #[tokio::test(start_paused = true)]
    async fn a_try_its_clock_ends_is_exhausted_and_hands_the_next_no_history() {
        let runtime = Arc::new(
            TestRuntime::new()
                .with_spawn_success(2)
                .with_execute_success(vec!["late".to_string(), "late".to_string()])
                .with_execute_delay(Duration::from_secs(5)),
        );
        let observer = Arc::new(TestObserver::default());
        let mut config = create_test_config();
        config.execution.iteration_timeout = Some("1s".to_string());
        let result = Supervisor::new(runtime.clone())
            .run_loop(
                config,
                create_test_input(),
                2,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;
        let mut complaints = Vec::new();
        if result.is_ok() {
            complaints.push(format!("the execution ended {result:?}"));
        }
        let exhausted = observer.exhausted.lock().await.clone();
        let expected = vec![
            (1, crate::domain::execution::ExhaustedBy::Clock),
            (2, crate::domain::execution::ExhaustedBy::Clock),
        ];
        if exhausted != expected {
            complaints.push(format!("the observer was told {exhausted:?}"));
        }
        let handed = runtime
            .spawn_envs
            .lock()
            .await
            .iter()
            .filter(|env| env.contains_key("AEGIS_ITERATION_HISTORY"))
            .count();
        if handed != 0 {
            complaints.push(format!("{handed} tries were handed a history item"));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }

    /// A runtime whose try records itself on the stored execution, as the
    /// inner loop does: the first try is recorded ended by an upstream rate
    /// limit and fails with the gateway's 429, the second fails by its own
    /// error, and the third answers.
    struct UpstreamRefusedThenFailingRuntime {
        repository: Arc<crate::infrastructure::repositories::InMemoryExecutionRepository>,
        execution_id: ExecutionId,
        spawn_times: Mutex<Vec<tokio::time::Instant>>,
    }

    #[async_trait]
    impl AgentRuntime for UpstreamRefusedThenFailingRuntime {
        async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
            let mut times = self.spawn_times.lock().await;
            times.push(tokio::time::Instant::now());
            Ok(InstanceId::new(format!("instance-{}", times.len())))
        }

        async fn execute(
            &self,
            _id: &InstanceId,
            _input: TaskInput,
        ) -> Result<TaskOutput, RuntimeError> {
            let mut execution = self
                .repository
                .find_by_id_unscoped(self.execution_id)
                .await
                .expect("the record reads")
                .expect("the record is stored");
            let try_number = {
                let iteration = execution
                    .start_iteration("try".to_string())
                    .expect("an iteration starts");
                if iteration.number == 1 {
                    iteration.exhausted = Some(crate::domain::execution::Exhaustion {
                        by: crate::domain::execution::ExhaustedBy::Upstream,
                        constructed: None,
                    });
                }
                iteration.number
            };
            self.repository
                .save_for_tenant(&crate::domain::tenant::TenantId::consumer(), &execution)
                .await
                .expect("the record is saved");
            match try_number {
                1 => Err(RuntimeError::ExecutionFailedWithLogs {
                    error: "RuntimeError: Orchestrator returned HTTP 429: upstream_rate_limit: Rate limit exceeded".to_string(),
                    logs: vec![],
                    exit_code: Some(1),
                }),
                2 => Err(RuntimeError::ExecutionFailedWithLogs {
                    error: "RuntimeError: the agent's own error".to_string(),
                    logs: vec![],
                    exit_code: Some(1),
                }),
                _ => Ok(TaskOutput {
                    result: serde_json::Value::String("answered".to_string()),
                    logs: vec![],
                    tool_calls: vec![],
                    exit_code: 0,
                    trajectory: vec![],
                }),
            }
        }

        async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
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

    /// AEGIS ADR-141, Update of 2026-10-15 `upstream-rate-limit-spends-no-try`,
    /// clause 2: a try recorded ended by an upstream rate limit is not counted
    /// against `max_retries`. With 2, the upstream-refused try and a failed
    /// try leave a third try, which starts after the failed-try wait.
    #[tokio::test(start_paused = true)]
    async fn a_try_ended_by_an_upstream_rate_limit_is_not_counted() {
        let repository =
            Arc::new(crate::infrastructure::repositories::InMemoryExecutionRepository::new());
        let execution = crate::domain::execution::Execution::new(
            crate::domain::agent::AgentId::new(),
            create_test_input(),
            10,
            "default".to_string(),
        );
        repository
            .save_for_tenant(&crate::domain::tenant::TenantId::consumer(), &execution)
            .await
            .expect("the record is saved");
        let runtime = Arc::new(UpstreamRefusedThenFailingRuntime {
            repository: repository.clone(),
            execution_id: execution.id,
            spawn_times: Mutex::new(Vec::new()),
        });
        let mut config = create_test_config();
        config
            .env
            .insert("AEGIS_EXECUTION_ID".to_string(), execution.id.0.to_string());
        let observer = Arc::new(TestObserver::default());

        let result = Supervisor::new(runtime.clone())
            .with_execution_repository(repository.clone())
            .run_loop(
                config,
                create_test_input(),
                2,
                observer.clone(),
                CancellationToken::new(),
                None,
            )
            .await;

        let mut complaints = Vec::new();
        match &result {
            Ok(answer) if answer == "answered" => {}
            other => complaints.push(format!("the execution ended {other:?}")),
        }
        let times = runtime.spawn_times.lock().await.clone();
        if times.len() != 3 {
            complaints.push(format!("{} tries were spawned, expected 3", times.len()));
        }
        if times.len() >= 2 && times[1] - times[0] < failed_execution_backoff(1) {
            complaints.push(format!(
                "the try after the upstream-refused one started {:?} after it, expected at least {:?}",
                times[1] - times[0],
                failed_execution_backoff(1)
            ));
        }
        assert!(complaints.is_empty(), "{}", complaints.join("\n"));
    }
}
