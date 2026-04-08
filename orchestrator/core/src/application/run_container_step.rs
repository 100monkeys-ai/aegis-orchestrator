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
    /// If true, the container's root filesystem is mounted read-only (ADR-087 D5).
    pub read_only_root_filesystem: bool,
    /// User the container process runs as, e.g. "65534:65534" (ADR-087 D5).
    pub run_as_user: Option<String>,
    /// Docker network mode override, e.g. "none" (ADR-087 D5).
    pub network_mode: Option<String>,
    /// Workflow execution UUID that owns the workspace volume.
    pub workflow_execution_id: Option<uuid::Uuid>,
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
/// Event publishing is handled by [`crate::infrastructure::container_step_runner::ContainerStepRunnerImpl`]
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
                read_only_root_filesystem: input.read_only_root_filesystem,
                run_as_user: input.run_as_user.clone(),
                network_mode: input.network_mode.clone(),
                workflow_execution_id: input.workflow_execution_id,
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

        Err(ContainerStepError::DockerError(format!(
            "retry loop exhausted without returning (max_attempts={max_attempts})"
        )))
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
                        // ParallelContainerRun steps do not carry per-step security overrides.
                        read_only_root_filesystem: false,
                        run_as_user: None,
                        network_mode: None,
                        workflow_execution_id: None,
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
                "ParallelContainerRun strategy {completion:?} not met: {succeeded}/{step_count} steps succeeded"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::ImagePullPolicy;
    use crate::domain::events::ContainerRunEvent;
    use crate::domain::execution::ExecutionId;
    use crate::domain::runtime::{
        ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
    };
    use crate::domain::workflow::{ContainerRunConfig, ParallelCompletionStrategy, StateName};
    use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventReceiver};
    use async_trait::async_trait;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    enum StubOutcome {
        Success(ContainerStepResult),
        Error(ContainerStepError),
    }

    #[derive(Default)]
    struct StubContainerStepRunner {
        outcomes: Mutex<HashMap<String, VecDeque<StubOutcome>>>,
        recorded_configs: Mutex<Vec<ContainerStepConfig>>,
    }

    impl StubContainerStepRunner {
        fn with_step_outcomes(
            step_outcomes: impl IntoIterator<Item = (String, Vec<StubOutcome>)>,
        ) -> Self {
            let outcomes = step_outcomes
                .into_iter()
                .map(|(name, outcomes)| (name, outcomes.into()))
                .collect();
            Self {
                outcomes: Mutex::new(outcomes),
                recorded_configs: Mutex::new(Vec::new()),
            }
        }

        fn recorded_configs(&self) -> Vec<ContainerStepConfig> {
            self.recorded_configs.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ContainerStepRunner for StubContainerStepRunner {
        async fn run_step(
            &self,
            config: ContainerStepConfig,
        ) -> Result<ContainerStepResult, ContainerStepError> {
            self.recorded_configs.lock().unwrap().push(config.clone());

            let mut outcomes = self.outcomes.lock().unwrap();
            let queue = outcomes
                .get_mut(&config.name)
                .unwrap_or_else(|| panic!("missing stub outcomes for step '{}'", config.name));

            match queue.pop_front() {
                Some(StubOutcome::Success(result)) => Ok(result),
                Some(StubOutcome::Error(error)) => Err(error),
                None => panic!("no remaining stub outcomes for step '{}'", config.name),
            }
        }
    }

    fn make_input(name: &str) -> RunContainerStepInput {
        RunContainerStepInput {
            execution_id: ExecutionId::new(),
            state_name: StateName::new("BUILD").unwrap(),
            name: name.to_string(),
            image: "rust:1.88".to_string(),
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            command: vec!["cargo".to_string(), "test".to_string()],
            env: HashMap::new(),
            workdir: Some("/workspace".to_string()),
            volumes: Vec::new(),
            resources: None,
            registry_credentials: None,
            max_attempts: 1,
            shell: false,
            read_only_root_filesystem: false,
            run_as_user: None,
            network_mode: None,
            workflow_execution_id: None,
        }
    }

    fn make_step(name: &str, command: &[&str], shell: bool) -> ContainerRunConfig {
        ContainerRunConfig {
            name: name.to_string(),
            image: "rust:1.88".to_string(),
            command: command.iter().map(|part| part.to_string()).collect(),
            env: HashMap::new(),
            workdir: Some("/workspace".to_string()),
            volumes: Vec::new(),
            resources: None,
            registry_credentials: None,
            shell,
        }
    }

    fn ok_result(exit_code: i32) -> ContainerStepResult {
        ContainerStepResult {
            exit_code,
            stdout: "ok".to_string(),
            stderr: String::new(),
            duration_ms: 25,
        }
    }

    async fn recv_parallel_aggregated_event(receiver: &mut EventReceiver) -> ContainerRunEvent {
        match receiver.recv().await.unwrap() {
            DomainEvent::ContainerRun(event) => event,
            other => panic!("expected container run event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_wraps_shell_commands_with_sh_c() {
        let runner = Arc::new(StubContainerStepRunner::with_step_outcomes([(
            "build".to_string(),
            vec![StubOutcome::Success(ok_result(0))],
        )]));
        let use_case = RunContainerStepUseCase::new(runner.clone());
        let mut input = make_input("build");
        input.shell = true;
        input.command = vec!["echo".to_string(), "hello world".to_string()];

        let output = use_case.execute(input).await.unwrap();

        assert_eq!(output.exit_code, 0);
        let recorded = runner.recorded_configs();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].command,
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hello world".to_string()
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn execute_retries_until_successful_attempt() {
        let runner = Arc::new(StubContainerStepRunner::with_step_outcomes([(
            "build".to_string(),
            vec![
                StubOutcome::Error(ContainerStepError::DockerError("first failure".to_string())),
                StubOutcome::Success(ok_result(0)),
            ],
        )]));
        let use_case = Arc::new(RunContainerStepUseCase::new(runner.clone()));
        let mut input = make_input("build");
        input.max_attempts = 2;

        let handle = tokio::spawn({
            let use_case = use_case.clone();
            async move { use_case.execute(input).await }
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        let output = handle.await.unwrap().unwrap();

        assert_eq!(output.attempts, 2);
        assert_eq!(runner.recorded_configs().len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn execute_returns_last_error_after_max_attempts() {
        let runner = Arc::new(StubContainerStepRunner::with_step_outcomes([(
            "build".to_string(),
            vec![
                StubOutcome::Error(ContainerStepError::DockerError("first".to_string())),
                StubOutcome::Error(ContainerStepError::DockerError("second".to_string())),
                StubOutcome::Error(ContainerStepError::TimeoutExpired { timeout_secs: 30 }),
            ],
        )]));
        let use_case = Arc::new(RunContainerStepUseCase::new(runner.clone()));
        let mut input = make_input("build");
        input.max_attempts = 3;

        let handle = tokio::spawn({
            let use_case = use_case.clone();
            async move { use_case.execute(input).await }
        });

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        let error = match handle.await.unwrap() {
            Ok(output) => panic!(
                "expected retries to fail after max attempts, got exit_code={}",
                output.exit_code
            ),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            ContainerStepError::TimeoutExpired { timeout_secs: 30 }
        ));
        assert_eq!(runner.recorded_configs().len(), 3);
    }

    #[tokio::test]
    async fn parallel_all_succeed_returns_success_and_publishes_aggregate_event() {
        let runner = Arc::new(StubContainerStepRunner::with_step_outcomes([
            ("lint".to_string(), vec![StubOutcome::Success(ok_result(0))]),
            ("test".to_string(), vec![StubOutcome::Success(ok_result(0))]),
        ]));
        let single_use_case = Arc::new(RunContainerStepUseCase::new(runner));
        let event_bus = Arc::new(EventBus::new(8));
        let parallel_use_case =
            RunParallelContainerStepsUseCase::new(single_use_case, event_bus.clone());
        let execution_id = ExecutionId::new();
        let state_name = StateName::new("CI").unwrap();
        let mut receiver = event_bus.subscribe();

        let output = parallel_use_case
            .execute(
                execution_id,
                state_name.clone(),
                vec![
                    make_step("lint", &["cargo", "fmt", "--check"], false),
                    make_step("test", &["cargo", "test"], false),
                ],
                ParallelCompletionStrategy::AllSucceed,
                ImagePullPolicy::IfNotPresent,
            )
            .await
            .unwrap();

        assert_eq!(output.succeeded, 2);
        assert_eq!(output.failed, 0);
        assert_eq!(output.strategy, ParallelCompletionStrategy::AllSucceed);

        let event = recv_parallel_aggregated_event(&mut receiver).await;
        assert!(matches!(
            event,
            ContainerRunEvent::ParallelContainerRunAggregated {
                execution_id: id,
                state_name: ref state,
                total_steps: 2,
                succeeded: 2,
                failed: 0,
                ref strategy,
                ..
            } if id == execution_id && state == state_name.as_str() && strategy == "all_succeed"
        ));
    }

    #[tokio::test]
    async fn parallel_any_succeed_accepts_non_zero_exit_when_one_step_succeeds() {
        let runner = Arc::new(StubContainerStepRunner::with_step_outcomes([
            ("pass".to_string(), vec![StubOutcome::Success(ok_result(0))]),
            (
                "warn".to_string(),
                vec![StubOutcome::Success(ok_result(17))],
            ),
        ]));
        let single_use_case = Arc::new(RunContainerStepUseCase::new(runner));
        let parallel_use_case =
            RunParallelContainerStepsUseCase::new(single_use_case, Arc::new(EventBus::new(8)));

        let output = parallel_use_case
            .execute(
                ExecutionId::new(),
                StateName::new("CI").unwrap(),
                vec![
                    make_step("pass", &["cargo", "check"], false),
                    make_step("warn", &["cargo", "clippy"], false),
                ],
                ParallelCompletionStrategy::AnySucceed,
                ImagePullPolicy::IfNotPresent,
            )
            .await
            .unwrap();

        assert_eq!(output.succeeded, 1);
        assert_eq!(output.failed, 1);
        assert_eq!(output.results.len(), 2);
    }

    #[tokio::test]
    async fn parallel_best_effort_returns_success_even_when_a_step_errors() {
        let runner = Arc::new(StubContainerStepRunner::with_step_outcomes([
            ("pass".to_string(), vec![StubOutcome::Success(ok_result(0))]),
            (
                "fail".to_string(),
                vec![StubOutcome::Error(ContainerStepError::ResourceExhausted {
                    detail: "oom-killed".to_string(),
                })],
            ),
        ]));
        let single_use_case = Arc::new(RunContainerStepUseCase::new(runner));
        let parallel_use_case =
            RunParallelContainerStepsUseCase::new(single_use_case, Arc::new(EventBus::new(8)));

        let output = parallel_use_case
            .execute(
                ExecutionId::new(),
                StateName::new("CI").unwrap(),
                vec![
                    make_step("pass", &["cargo", "check"], false),
                    make_step("fail", &["cargo", "test"], false),
                ],
                ParallelCompletionStrategy::BestEffort,
                ImagePullPolicy::IfNotPresent,
            )
            .await
            .unwrap();

        assert_eq!(output.succeeded, 1);
        assert_eq!(output.failed, 1);
        assert_eq!(output.strategy, ParallelCompletionStrategy::BestEffort);
        assert!(output.results.iter().any(|result| result.outcome.is_err()));
    }
}
