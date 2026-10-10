// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Run Container Step Use Cases — BC-3 CI/CD (ADR-050)
//!
//! Application-layer orchestration for `ContainerRun` and `ParallelContainerRun`
//! workflow states. Delegates execution to the [`ContainerStepRunner`] domain trait
//! and applies retry logic and parallel completion strategies.

use crate::application::git_repo_service::{RunMount, RunRepositories, RunRepositoryError};
use crate::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use crate::domain::agent::ImagePullPolicy;
use crate::domain::events::ContainerRunEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::FsalAccessPolicy;
use crate::domain::repository::WorkflowExecutionRepository;
use crate::domain::runtime::{ContainerStepConfig, ContainerStepError, ContainerStepRunner};
use crate::domain::shared_kernel::VolumeId;
use crate::domain::workflow::{run_repository_volume, RUN_REPOSITORY_VOLUME};
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
    /// The tenant of the workflow execution the step runs in, in which its
    /// repositories are read (AEGIS ADR-141 F6).
    pub tenant_id: crate::domain::tenant::TenantId,
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
    /// The runs' repositories a step may mount, and the gateway's registry
    /// it mounts them through (AEGIS ADR-141 F6). Unset: no step may.
    repositories: std::sync::OnceLock<(Arc<dyn StepRepositories>, NfsVolumeRegistry)>,
    /// The node's network for steps that reach public hosts (core's
    /// `spec.storage.git.step_network`, AEGIS ADR-141 F7, ADR-136 G12).
    egress_network: Option<String>,
}

/// The network mode a ContainerRun state names to reach package registries
/// (AEGIS ADR-141 F7).
pub const EGRESS_NETWORK_MODE: &str = "egress";

/// The refusal of `network_mode: egress` on a node with no step network
/// (AEGIS ADR-141 F7).
pub const NO_EGRESS_NETWORK: &str = "this node has no egress network for steps";

/// Why a step was not run: a refusal before any container, in its own
/// sentence (AEGIS ADR-141 F6, F7), or the step's own failure.
#[derive(Debug, thiserror::Error)]
pub enum RunContainerStepError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Step(#[from] ContainerStepError),
}

/// The repositories a workflow run holds, as a ContainerRun step of it
/// mounts them (AEGIS ADR-141 F6).
#[async_trait::async_trait]
pub trait StepRepositories: Send + Sync {
    /// The working-tree mounts of the repositories the workflow execution
    /// `run` of `tenant_id` holds, in the order its start named them.
    async fn mounts_of_run(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        run: uuid::Uuid,
    ) -> Result<Vec<RunMount>, RunRepositoryError>;
}

/// [`StepRepositories`] read from the workflow execution's record: its
/// `repositories`, as its person, through the git repository service's
/// [`RunRepositories`], which checks and holds them as for an agent state.
pub struct WorkflowRunRepositories {
    workflow_executions: Arc<dyn WorkflowExecutionRepository>,
    repositories: Arc<dyn RunRepositories>,
}

impl WorkflowRunRepositories {
    pub fn new(
        workflow_executions: Arc<dyn WorkflowExecutionRepository>,
        repositories: Arc<dyn RunRepositories>,
    ) -> Self {
        Self {
            workflow_executions,
            repositories,
        }
    }
}

#[async_trait::async_trait]
impl StepRepositories for WorkflowRunRepositories {
    async fn mounts_of_run(
        &self,
        tenant_id: &crate::domain::tenant::TenantId,
        run: uuid::Uuid,
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        use crate::domain::git_repo::{parse_run_repositories, REPOSITORIES_INPUT_KEY};
        let Some(execution) = self
            .workflow_executions
            .find_by_id_for_tenant(tenant_id, ExecutionId(run))
            .await
            .map_err(|e| RunRepositoryError::Failed(e.into()))?
        else {
            return Ok(Vec::new());
        };
        let entries = match execution.input.get(REPOSITORIES_INPUT_KEY) {
            None => Vec::new(),
            Some(value) => parse_run_repositories(value)
                .map_err(|sentence| RunRepositoryError::Refused(sentence.to_string()))?,
        };
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        self.repositories
            .mounts_for_run(
                tenant_id,
                execution.initiating_user_sub.as_deref(),
                run,
                &entries,
            )
            .await
    }
}

/// The refusal of a step naming the run's repository when its run holds
/// none (AEGIS ADR-141 F6).
pub const STEP_RUN_HOLDS_NONE: &str = "this step names the run's repository and the run holds none";

/// The refusal of a step naming a repository by a label its run does not
/// hold (AEGIS ADR-141 F6).
pub fn step_run_holds_no_label(label: &str) -> String {
    format!("this step names the run's repository and the run holds no repository '{label}'")
}

/// The refusal of a step naming the run's repository on a node with no git
/// repository service: the sentence a run's start answers there.
const NO_GIT_REPOSITORY_SERVICE: &str =
    "this node has no git repository service, so a run cannot be given repositories";

/// A volume registration the step replaced, put back when the step ends.
type Replaced = (
    VolumeId,
    Option<crate::infrastructure::nfs::server::NfsVolumeContext>,
);

impl RunContainerStepUseCase {
    pub fn new(runner: Arc<dyn ContainerStepRunner>) -> Self {
        Self {
            runner,
            repositories: std::sync::OnceLock::new(),
            egress_network: None,
        }
    }

    /// Let a step mount its run's repository (AEGIS ADR-141 F6), registered
    /// with `registry`, the gateway agent states mount through. Wired once
    /// at startup, after the git repository service exists; a second call
    /// is ignored.
    pub fn set_run_repositories(
        &self,
        repositories: Arc<dyn StepRepositories>,
        registry: NfsVolumeRegistry,
    ) {
        let _ = self.repositories.set((repositories, registry));
    }

    /// The node's network for `network_mode: egress` steps (AEGIS ADR-141
    /// F7); `None` refuses them.
    pub fn with_egress_network(mut self, network: Option<String>) -> Self {
        self.egress_network = network;
        self
    }

    /// Run one ContainerRun state's step: its run's repository resolved to
    /// a mount through the gateway (F6) and `network_mode: egress` to the
    /// node's step network (F7), each refused before any container when it
    /// cannot be, then [`Self::execute`].
    pub async fn run(
        &self,
        mut input: RunContainerStepInput,
    ) -> Result<RunContainerStepOutput, RunContainerStepError> {
        if input.network_mode.as_deref() == Some(EGRESS_NETWORK_MODE) {
            let Some(network) = &self.egress_network else {
                return Err(RunContainerStepError::Refused(
                    NO_EGRESS_NETWORK.to_string(),
                ));
            };
            input.network_mode = Some(network.clone());
        }
        let replaced = self.mount_run_repositories(&mut input).await?;
        let result = self.execute(input).await;
        if let Some((_, registry)) = self.repositories.get() {
            for (volume_id, found) in replaced {
                match found {
                    Some(context) => registry.register(VolumeRegistration {
                        volume_id: context.volume_id,
                        execution_id: context.execution_id,
                        workflow_execution_id: context.workflow_execution_id,
                        container_uid: context.container_uid,
                        container_gid: context.container_gid,
                        policy: context.policy,
                        mount_point: context.mount_point,
                        remote_path: context.remote_path,
                    }),
                    None => registry.deregister(volume_id),
                }
            }
        }
        Ok(result?)
    }

    /// Each volume entry naming the run's repository (AEGIS ADR-141 F6)
    /// becomes its working tree's volume, registered with the gateway agent
    /// states mount through at the entry's `mount_path`, for the step's
    /// length. Answers the registrations replaced, to be put back.
    async fn mount_run_repositories(
        &self,
        input: &mut RunContainerStepInput,
    ) -> Result<Vec<Replaced>, RunContainerStepError> {
        if !input
            .volumes
            .iter()
            .any(|v| run_repository_volume(&v.name).is_some())
        {
            return Ok(Vec::new());
        }
        let Some((repositories, registry)) = self.repositories.get() else {
            return Err(RunContainerStepError::Refused(
                NO_GIT_REPOSITORY_SERVICE.to_string(),
            ));
        };
        let Some(run) = input.workflow_execution_id else {
            return Err(RunContainerStepError::Refused(
                STEP_RUN_HOLDS_NONE.to_string(),
            ));
        };
        let mounts = repositories
            .mounts_of_run(&input.tenant_id, run)
            .await
            .map_err(|e| match e {
                RunRepositoryError::Refused(sentence) => RunContainerStepError::Refused(sentence),
                RunRepositoryError::Failed(e) => {
                    RunContainerStepError::Step(ContainerStepError::VolumeMountFailed {
                        volume: RUN_REPOSITORY_VOLUME.to_string(),
                        error: e.to_string(),
                    })
                }
            })?;
        // Every entry is resolved before any is registered: a refusal
        // leaves the gateway as it was.
        let mut chosen = Vec::new();
        for (index, volume) in input.volumes.iter().enumerate() {
            let Some(label) = run_repository_volume(&volume.name) else {
                continue;
            };
            let mount = match label {
                None => mounts.first().ok_or_else(|| {
                    RunContainerStepError::Refused(STEP_RUN_HOLDS_NONE.to_string())
                })?,
                Some(label) => mounts.iter().find(|m| m.label == label).ok_or_else(|| {
                    RunContainerStepError::Refused(if mounts.is_empty() {
                        STEP_RUN_HOLDS_NONE.to_string()
                    } else {
                        step_run_holds_no_label(label)
                    })
                })?,
            };
            chosen.push((index, mount.mount.clone()));
        }
        let mut replaced = Vec::with_capacity(chosen.len());
        for (index, mount) in chosen {
            let volume = &mut input.volumes[index];
            replaced.push((mount.volume_id, registry.lookup(mount.volume_id)));
            registry.register(VolumeRegistration {
                volume_id: mount.volume_id,
                execution_id: input.execution_id,
                workflow_execution_id: Some(run),
                container_uid: 1000,
                container_gid: 1000,
                policy: FsalAccessPolicy {
                    read: vec!["/*".to_string()],
                    write: if volume.read_only {
                        Vec::new()
                    } else {
                        vec!["/*".to_string()]
                    },
                },
                mount_point: std::path::PathBuf::from(&volume.mount_path),
                remote_path: mount.remote_path.clone(),
            });
            info!(
                execution_id = %input.execution_id,
                step_name = %input.name,
                volume_id = %mount.volume_id,
                mount_path = %volume.mount_path,
                "A step mounts its run's repository"
            );
            volume.name = mount.volume_id.0.to_string();
        }
        Ok(replaced)
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
                entrypoint: None,
                command,
                stdin: None,
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
                files: Vec::new(),
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
                        tenant_id: crate::domain::tenant::TenantId::default(),
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
            tenant_id: crate::domain::tenant::TenantId::default(),
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
