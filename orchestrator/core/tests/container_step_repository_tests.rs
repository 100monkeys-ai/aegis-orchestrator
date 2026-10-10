// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A ContainerRun step that mounts its run's repository (AEGIS ADR-141 F6)
//! and one that reaches package registries through the node's egress network
//! for steps (F7), as the step use case hands them to the container runner.
//!
//! | Scenario | Test |
//! |---|---|
//! | `repository` mounted through the gateway | `a_step_naming_repository_mounts_the_runs_working_tree_read_write_at_its_path` |
//! | `repository:<label>` | `a_step_naming_a_label_mounts_that_repository` |
//! | A run holding none | `a_step_naming_the_repository_of_a_run_holding_none_is_refused_before_any_container` |
//! | A label the run does not hold | `a_step_naming_a_label_the_run_does_not_hold_is_refused_before_any_container` |
//! | The run's person and entries | `the_runs_repositories_are_read_from_its_workflow_execution_as_its_person` |
//! | `network_mode: egress` | `egress_runs_the_step_on_the_nodes_step_network` |
//! | No egress network | `egress_on_a_node_with_no_step_network_is_refused_before_any_container` |
//! | Another network mode | `another_network_mode_reaches_the_runner_as_given` |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use aegis_orchestrator_core::application::git_repo_service::{
    PreparedRepository, RunMount, RunRepositories, RunRepositoryError,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::run_container_step::{
    RunContainerStepError, RunContainerStepInput, RunContainerStepUseCase, StepRepositories,
    WorkflowRunRepositories,
};
use aegis_orchestrator_core::domain::agent::ImagePullPolicy;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::git_repo::{GitRepoBindingId, RunRepository};
use aegis_orchestrator_core::domain::repository::WorkflowExecutionRepository;
use aegis_orchestrator_core::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
};
use aegis_orchestrator_core::domain::shared_kernel::VolumeId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::volume::{AccessMode, FilerEndpoint, VolumeMount};
use aegis_orchestrator_core::domain::workflow::{
    ContainerVolumeMount, StateName, WorkflowExecution,
};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

const REMOTE: &str = "/aegis/seaweedfs/volumes/forge/repo";

/// What the runner saw of one step: its configuration, and the gateway's
/// registration of each volume it was handed, read while the step ran.
struct Seen {
    config: ContainerStepConfig,
    registered: Vec<Option<(String, PathBuf, bool)>>,
}

/// Records each step and answers exit 0.
struct RecordingRunner {
    registry: NfsVolumeRegistry,
    seen: Mutex<Vec<Seen>>,
}

impl RecordingRunner {
    fn new(registry: NfsVolumeRegistry) -> Arc<Self> {
        Arc::new(Self {
            registry,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn runs(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl ContainerStepRunner for RecordingRunner {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        let registered = config
            .volumes
            .iter()
            .map(|v| {
                let id = uuid::Uuid::parse_str(&v.name).ok()?;
                let ctx = self.registry.lookup(VolumeId(id))?;
                Some((
                    ctx.remote_path.clone(),
                    ctx.mount_point.clone(),
                    !ctx.policy.write.is_empty(),
                ))
            })
            .collect();
        self.seen.lock().unwrap().push(Seen { config, registered });
        Ok(ContainerStepResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 1,
        })
    }
}

/// The run's repositories, as fixed mounts by label.
struct FixedRepositories(Vec<(String, VolumeId)>);

#[async_trait]
impl StepRepositories for FixedRepositories {
    async fn mounts_of_run(
        &self,
        _tenant_id: &TenantId,
        _run: uuid::Uuid,
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        Ok(self
            .0
            .iter()
            .map(|(label, volume)| RunMount {
                label: label.clone(),
                mount: VolumeMount::new(
                    *volume,
                    PathBuf::from(format!("/workspace/{label}")),
                    AccessMode::ReadWrite,
                    FilerEndpoint::new("http://filer:8888").unwrap(),
                    REMOTE.to_string(),
                ),
            })
            .collect())
    }
}

fn input(volumes: Vec<ContainerVolumeMount>, network_mode: Option<&str>) -> RunContainerStepInput {
    RunContainerStepInput {
        execution_id: ExecutionId::new(),
        state_name: StateName::new("EXECUTE_TESTS").unwrap(),
        name: "suite".to_string(),
        image: "docker.io/library/python:3.11-slim".to_string(),
        image_pull_policy: ImagePullPolicy::IfNotPresent,
        command: vec!["pytest".to_string()],
        env: HashMap::new(),
        workdir: Some("/workspace/app".to_string()),
        volumes,
        resources: None,
        registry_credentials: None,
        max_attempts: 1,
        shell: false,
        read_only_root_filesystem: false,
        run_as_user: None,
        network_mode: network_mode.map(str::to_string),
        workflow_execution_id: Some(uuid::Uuid::new_v4()),
        tenant_id: TenantId::consumer(),
    }
}

fn volume(name: &str, path: &str) -> ContainerVolumeMount {
    ContainerVolumeMount {
        name: name.to_string(),
        mount_path: path.to_string(),
        read_only: false,
    }
}

fn refusal(result: Result<impl Sized, RunContainerStepError>) -> String {
    match result {
        Err(RunContainerStepError::Refused(sentence)) => sentence,
        Err(other) => panic!("the step failed, not refused: {other}"),
        Ok(_) => panic!("the step ran"),
    }
}

#[tokio::test]
async fn a_step_naming_repository_mounts_the_runs_working_tree_read_write_at_its_path() {
    let registry = NfsVolumeRegistry::new();
    let runner = RecordingRunner::new(registry.clone());
    let volume_id = VolumeId(uuid::Uuid::new_v4());
    let use_case = RunContainerStepUseCase::new(runner.clone());
    use_case.set_run_repositories(
        Arc::new(FixedRepositories(vec![("app".to_string(), volume_id)])),
        registry.clone(),
    );

    let output = use_case
        .run(input(vec![volume("repository", "/workspace/app")], None))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));
    assert_eq!(output.exit_code, 0);

    let seen = runner.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let handed: Vec<(String, String, bool)> = seen[0]
        .config
        .volumes
        .iter()
        .map(|v| (v.name.clone(), v.mount_path.clone(), v.read_only))
        .collect();
    assert_eq!(
        handed,
        vec![(volume_id.0.to_string(), "/workspace/app".to_string(), false)],
        "the runner was not handed the repository's volume at the step's path"
    );
    assert_eq!(
        seen[0].registered,
        vec![Some((
            REMOTE.to_string(),
            PathBuf::from("/workspace/app"),
            true
        ))],
        "the gateway did not hold the working tree, read-write, while the step ran"
    );
    assert!(
        registry.lookup(volume_id).is_none(),
        "the step's registration of the repository outlived the step"
    );
}

#[tokio::test]
async fn a_step_naming_a_label_mounts_that_repository() {
    let registry = NfsVolumeRegistry::new();
    let runner = RecordingRunner::new(registry.clone());
    let first = VolumeId(uuid::Uuid::new_v4());
    let second = VolumeId(uuid::Uuid::new_v4());
    let use_case = RunContainerStepUseCase::new(runner.clone());
    use_case.set_run_repositories(
        Arc::new(FixedRepositories(vec![
            ("app".to_string(), first),
            ("lib".to_string(), second),
        ])),
        registry,
    );

    use_case
        .run(input(vec![volume("repository:lib", "/src/lib")], None))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));
    let seen = runner.seen.lock().unwrap();
    assert_eq!(seen[0].config.volumes[0].name, second.0.to_string());
    assert_eq!(seen[0].config.volumes[0].mount_path, "/src/lib");
}

#[tokio::test]
async fn a_step_naming_the_repository_of_a_run_holding_none_is_refused_before_any_container() {
    let registry = NfsVolumeRegistry::new();
    let runner = RecordingRunner::new(registry.clone());
    let use_case = RunContainerStepUseCase::new(runner.clone());
    use_case.set_run_repositories(Arc::new(FixedRepositories(Vec::new())), registry);

    let sentence = refusal(
        use_case
            .run(input(vec![volume("repository", "/workspace/app")], None))
            .await,
    );
    assert_eq!(
        sentence,
        "this step names the run's repository and the run holds none"
    );
    assert_eq!(runner.runs(), 0, "a container was started");
}

#[tokio::test]
async fn a_step_naming_a_label_the_run_does_not_hold_is_refused_before_any_container() {
    let registry = NfsVolumeRegistry::new();
    let runner = RecordingRunner::new(registry.clone());
    let use_case = RunContainerStepUseCase::new(runner.clone());
    use_case.set_run_repositories(
        Arc::new(FixedRepositories(vec![(
            "app".to_string(),
            VolumeId(uuid::Uuid::new_v4()),
        )])),
        registry,
    );

    let sentence = refusal(
        use_case
            .run(input(vec![volume("repository:other", "/src")], None))
            .await,
    );
    assert_eq!(
        sentence,
        "this step names the run's repository and the run holds no repository 'other'"
    );
    assert_eq!(runner.runs(), 0, "a container was started");
}

/// Records the arguments of each mount request.
#[derive(Default)]
struct RecordingRunRepositories {
    asked: Mutex<Vec<(Option<String>, uuid::Uuid, Vec<RunRepository>)>>,
}

#[async_trait]
impl RunRepositories for RecordingRunRepositories {
    async fn prepare_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        Ok(entries.to_vec())
    }

    async fn mounts_for_run(
        &self,
        _tenant_id: &TenantId,
        person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        self.asked
            .lock()
            .unwrap()
            .push((person.map(str::to_string), run, entries.to_vec()));
        Ok(Vec::new())
    }

    fn release_run(&self, _run: uuid::Uuid) {}

    fn take_prepared(&self, _run: uuid::Uuid) -> Vec<PreparedRepository> {
        Vec::new()
    }
}

#[tokio::test]
async fn the_runs_repositories_are_read_from_its_workflow_execution_as_its_person() {
    let tenant = TenantId::consumer();
    let workflow = WorkflowParser::parse_yaml(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: one-step
spec:
  initial_state: A
  states:
    A:
      kind: System
      command: "true"
      transitions: []
"#,
    )
    .unwrap();
    let binding = GitRepoBindingId(uuid::Uuid::new_v4());
    let run = ExecutionId::new();
    let mut execution = WorkflowExecution::new(
        &workflow,
        run,
        json!({ "repositories": [{ "binding_id": binding.0.to_string(), "branch": "aegis/forge" }] }),
    );
    execution.tenant_id = tenant.clone();
    execution.initiating_user_sub = Some("the-person".to_string());
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    executions
        .save_for_tenant(&tenant, &execution)
        .await
        .unwrap();
    let repositories = Arc::new(RecordingRunRepositories::default());

    let reader = WorkflowRunRepositories::new(executions, repositories.clone());
    let mounts = reader.mounts_of_run(&tenant, run.0).await.unwrap();
    assert!(mounts.is_empty());

    let asked = repositories.asked.lock().unwrap();
    assert_eq!(
        *asked,
        vec![(
            Some("the-person".to_string()),
            run.0,
            vec![RunRepository {
                binding_id: binding,
                branch: Some("aegis/forge".to_string()),
                author: None,
                label: None,
                git_ref: None,
                started_from: None,
            }]
        )]
    );
}

#[tokio::test]
async fn egress_runs_the_step_on_the_nodes_step_network() {
    let runner = RecordingRunner::new(NfsVolumeRegistry::new());
    let use_case = RunContainerStepUseCase::new(runner.clone())
        .with_egress_network(Some("aegis-step-egress".to_string()));

    use_case
        .run(input(Vec::new(), Some("egress")))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));
    let seen = runner.seen.lock().unwrap();
    assert_eq!(
        seen[0].config.network_mode.as_deref(),
        Some("aegis-step-egress"),
        "the step did not run on the step network"
    );
}

#[tokio::test]
async fn egress_on_a_node_with_no_step_network_is_refused_before_any_container() {
    let runner = RecordingRunner::new(NfsVolumeRegistry::new());
    let use_case = RunContainerStepUseCase::new(runner.clone());

    let sentence = refusal(use_case.run(input(Vec::new(), Some("egress"))).await);
    assert_eq!(sentence, "this node has no egress network for steps");
    assert_eq!(runner.runs(), 0, "a container was started");
}

#[tokio::test]
async fn another_network_mode_reaches_the_runner_as_given() {
    let runner = RecordingRunner::new(NfsVolumeRegistry::new());
    let use_case = RunContainerStepUseCase::new(runner.clone())
        .with_egress_network(Some("aegis-step-egress".to_string()));

    use_case
        .run(input(Vec::new(), Some("none")))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));
    use_case
        .run(input(Vec::new(), None))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));
    let seen = runner.seen.lock().unwrap();
    assert_eq!(seen[0].config.network_mode.as_deref(), Some("none"));
    assert_eq!(seen[1].config.network_mode, None);
}
