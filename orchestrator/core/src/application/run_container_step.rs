// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Run Container Step Use Cases — BC-3 CI/CD (ADR-050)
//!
//! Application-layer orchestration for `ContainerRun` and `ParallelContainerRun`
//! workflow states. Delegates execution to the [`ContainerStepRunner`] domain trait
//! and applies retry logic and parallel completion strategies.

use crate::domain::agent::ImagePullPolicy;
use crate::domain::events::ContainerRunEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::runtime::{ContainerStepConfig, ContainerStepError, ContainerStepRunner};
use crate::domain::workflow::{ContainerRunConfig, ParallelCompletionStrategy, StateName};
use crate::infrastructure::event_bus::EventBus;
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

// ─── RunContainerStepUseCase ──────────────────────────────────────────────────

/// Input for a single container step execution.
pub struct RunContainerStepInput {
    pub execution_id: ExecutionId,
    pub state_name: StateName,
    /// Logical name for the step (used in events and logs).
    pub name: String,
    pub image: String,
    pub image_pull_policy: ImagePullPolicy,
    pub command: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub workdir: Option<String>,
    pub volumes: Vec<crate::domain::workflow::ContainerVolumeMount>,
    pub resources: Option<crate::domain::workflow::ContainerResources>,
    pub registry_credentials: Option<String>,
    /// Maximum number of attempts (1 = no retry).
    pub max_attempts: u32,
    pub shell: bool,
}

/// Output from a single container step execution.
pub struct RunContainerStepOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    /// Number of attempts consumed (useful for telemetry).
    pub attempts: u32,
}

/// Use case: run a single `ContainerRun` workflow state, applying retry logic.
///
/// Event publishing is handled by [`crate::infrastructure::container_step_runner::DockerContainerStepRunner`]
/// at the infrastructure layer. [`RunParallelContainerStepsUseCase`] holds its own `event_bus`
/// for the aggregated parallel completion event.
pub struct RunContainerStepUseCase {
    runner: Arc<dyn ContainerStepRunner>,
}

impl RunContainerStepUseCase {
    pub fn new(runner: Arc<dyn ContainerStepRunner>) -> Self {
        Self { runner }
    }

    pub async fn execute(
        &self,
        input: RunContainerStepInput,
    ) -> Result<RunContainerStepOutput, ContainerStepError> {
        let max_attempts = input.max_attempts.max(1);

        for attempt in 1..=max_attempts {
            debug!(
                execution_id = %input.execution_id,
                state_name = %input.state_name,
                step_name = %input.name,
                attempt = attempt,
                max_attempts = max_attempts,
                "Attempting container step"
            );

            // Apply sh -c wrapping when shell mode is requested (ADR-050).
            // ContainerStepConfig carries the resolved argv only — no shell flag.
            let command = if input.shell {
                vec!["sh".to_string(), "-c".to_string(), input.command.join(" ")]
            } else {
                input.command.clone()
            };

            let config = ContainerStepConfig {
                name: input.name.clone(),
                image: input.image.clone(),
                image_pull_policy: input.image_pull_policy,
                command,
                env: input.env.clone(),
                workdir: input.workdir.clone(),
                volumes: input.volumes.clone(),
                resources: input.resources.clone(),
                registry_credentials: input.registry_credentials.clone(),
                execution_id: input.execution_id,
                state_name: input.state_name.clone(),
            };

            match self.runner.run_step(config).await {
                Ok(result) => {
                    info!(
                        execution_id = %input.execution_id,
                        step_name = %input.name,
                        exit_code = result.exit_code,
                        attempts = attempt,
                        "Container step succeeded"
                    );
                    return Ok(RunContainerStepOutput {
                        exit_code: result.exit_code,
                        stdout: result.stdout,
                        stderr: result.stderr,
                        duration_ms: result.duration_ms,
                        attempts: attempt,
                    });
                }
                Err(e) if attempt < max_attempts => {
                    warn!(
                        execution_id = %input.execution_id,
                        step_name = %input.name,
                        attempt = attempt,
                        max_attempts = max_attempts,
                        error = %e,
                        "Container step attempt failed; will retry"
                    );
                    // Simple linear backoff: 1s * attempt number (capped at 30s)
                    let backoff_secs = (attempt as u64).min(30);
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                }
                Err(e) => {
                    warn!(
                        execution_id = %input.execution_id,
                        step_name = %input.name,
                        attempts = attempt,
                        error = %e,
                        "Container step failed after all attempts"
                    );
                    return Err(e);
                }
            }
        }

        // SAFETY: Unreachable in practice — the `for` loop over `1..=max_attempts` covers all
        // attempts, and the final attempt's `Err(e)` arm (above) always executes `return Err(e)`.
        // `max_attempts` is clamped to ≥ 1 at construction, so the loop body runs at least once.
        unreachable!(
            "retry loop exhausted without returning — \
             max_attempts({max_attempts}) invariant violated"
        )
    }
}

// ─── RunParallelContainerStepsUseCase ─────────────────────────────────────────

/// Output from a parallel container step execution.
pub struct RunParallelContainerStepsOutput {
    pub results: Vec<ParallelStepResult>,
    pub succeeded: u32,
    pub failed: u32,
    pub strategy: ParallelCompletionStrategy,
}

pub struct ParallelStepResult {
    pub name: String,
    pub outcome: Result<RunContainerStepOutput, ContainerStepError>,
}

/// Use case: run multiple container steps in parallel with a completion strategy.
///
/// - `AllSucceed`: returns success only if every step exits with code 0.
/// - `AnySucceed`: returns success if at least one step exits with code 0.
/// - `BestEffort`: always returns success; caller inspects individual results.
pub struct RunParallelContainerStepsUseCase {
    single_use_case: Arc<RunContainerStepUseCase>,
    event_bus: Arc<EventBus>,
}

impl RunParallelContainerStepsUseCase {
    pub fn new(single_use_case: Arc<RunContainerStepUseCase>, event_bus: Arc<EventBus>) -> Self {
        Self {
            single_use_case,
            event_bus,
        }
    }

    pub async fn execute(
        &self,
        execution_id: ExecutionId,
        state_name: StateName,
        steps: Vec<ContainerRunConfig>,
        completion: ParallelCompletionStrategy,
        global_image_pull_policy: ImagePullPolicy,
    ) -> Result<RunParallelContainerStepsOutput, ContainerStepError> {
        use futures::future::join_all;

        let step_count = steps.len() as u32;

        info!(
            execution_id = %execution_id,
            state_name = %state_name,
            step_count = step_count,
            strategy = ?completion,
            "Executing parallel container steps"
        );

        // Spawn all steps concurrently.
        let futures: Vec<_> = steps
            .into_iter()
            .map(|step| {
                let uc = Arc::clone(&self.single_use_case);
                let eid = execution_id;
                let sn = state_name.clone();
                let pull_policy = global_image_pull_policy;

                async move {
                    let name = step.name.clone();
                    let input = RunContainerStepInput {
                        execution_id: eid,
                        state_name: sn,
                        name: step.name,
                        image: step.image,
                        // ContainerRunConfig has no per-step image_pull_policy;
                        // use the workflow-level policy passed by the caller.
                        image_pull_policy: pull_policy,
                        command: step.command,
                        // env and volumes are plain HashMap/Vec (not Option) on ContainerRunConfig.
                        env: step.env,
                        workdir: step.workdir,
                        volumes: step.volumes,
                        resources: step.resources,
                        registry_credentials: step.registry_credentials,
                        // ContainerRunConfig has no retry field; single attempt per step.
                        max_attempts: 1,
                        // shell is a plain bool (not Option) on ContainerRunConfig.
                        shell: step.shell,
                    };
                    let outcome = uc.execute(input).await;
                    ParallelStepResult { name, outcome }
                }
            })
            .collect();

        let results = join_all(futures).await;

        let succeeded = results
            .iter()
            .filter(|r| matches!(&r.outcome, Ok(o) if o.exit_code == 0))
            .count() as u32;
        let failed = step_count - succeeded;

        // Publish aggregated event.
        let strategy = match completion {
            ParallelCompletionStrategy::AllSucceed => "all_succeed",
            ParallelCompletionStrategy::AnySucceed => "any_succeed",
            ParallelCompletionStrategy::BestEffort => "best_effort",
        };
        self.event_bus.publish_container_run_event(
            ContainerRunEvent::ParallelContainerRunAggregated {
                execution_id,
                state_name: state_name.to_string(),
                total_steps: step_count,
                succeeded,
                failed,
                strategy: strategy.to_string(),
                aggregated_at: Utc::now(),
            },
        );

        info!(
            execution_id = %execution_id,
            state_name = %state_name,
            succeeded = succeeded,
            failed = failed,
            strategy = ?completion,
            "Parallel container steps aggregated"
        );

        // Apply completion strategy to determine success or failure.
        let strategy_met = match completion {
            ParallelCompletionStrategy::AllSucceed => failed == 0,
            ParallelCompletionStrategy::AnySucceed => succeeded > 0,
            ParallelCompletionStrategy::BestEffort => true,
        };

        if !strategy_met {
            // Return the first failure's error as the representative error.
            if let Some(failed_result) = results.iter().find(|r| r.outcome.is_err()) {
                if let Err(ref e) = failed_result.outcome {
                    return Err(match e {
                        ContainerStepError::ImagePullFailed { image, error } => {
                            ContainerStepError::ImagePullFailed {
                                image: image.clone(),
                                error: error.clone(),
                            }
                        }
                        ContainerStepError::TimeoutExpired { timeout_secs } => {
                            ContainerStepError::TimeoutExpired {
                                timeout_secs: *timeout_secs,
                            }
                        }
                        ContainerStepError::VolumeMountFailed { volume, error } => {
                            ContainerStepError::VolumeMountFailed {
                                volume: volume.clone(),
                                error: error.clone(),
                            }
                        }
                        ContainerStepError::ResourceExhausted { detail } => {
                            ContainerStepError::ResourceExhausted {
                                detail: detail.clone(),
                            }
                        }
                        ContainerStepError::DockerError(msg) => {
                            ContainerStepError::DockerError(msg.clone())
                        }
                    });
                }
            }
            // All failed with non-zero exit codes (Ok results with exit_code != 0).
            return Err(ContainerStepError::DockerError(format!(
                "ParallelContainerRun strategy {:?} not met: {}/{} steps succeeded",
                completion, succeeded, step_count
            )));
        }

        Ok(RunParallelContainerStepsOutput {
            results,
            succeeded,
            failed,
            strategy: completion,
        })
    }
}
