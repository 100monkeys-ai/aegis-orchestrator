// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A workflow that works on a fixed number of repositories (AEGIS ADR-141
//! F2), and a ContainerRun step that names the run's repository as a volume
//! (F6), as the manifest and the start see them.
//!
//! | Scenario | Test |
//! |---|---|
//! | `spec.repositories` parsed and written back | `spec_repositories_is_read_from_the_manifest_and_written_back` |
//! | A start naming no repository | `a_start_naming_fewer_repositories_than_the_workflow_works_on_is_refused` |
//! | A start naming two | `a_start_naming_more_repositories_than_the_workflow_works_on_is_refused_before_any_is_held` |
//! | A start naming one | `a_start_naming_the_number_the_workflow_works_on_passes_the_count` |
//! | No count declared | `a_workflow_declaring_no_count_starts_with_any_number` |
//! | `repository` as a step's volume | `a_container_step_may_name_the_runs_repository_beside_declared_volumes` |

use aegis_orchestrator_core::application::git_repo_service::{
    PreparedRepository, RunMount, RunRepositories, RunRepositoryError,
};
use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::domain::execution::ExecutionError;
use aegis_orchestrator_core::domain::git_repo::RunRepository;
use aegis_orchestrator_core::domain::repository::WorkflowRepository;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const FIRST: &str = "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e";
const SECOND: &str = "8a1d2c3b-4e5f-4a6b-9c7d-0e1f2a3b4c5d";

/// A two-state workflow, `spec.repositories` set when `count` is given.
fn manifest(name: &str, count: Option<u32>) -> String {
    let repositories = count
        .map(|n| format!("  repositories: {n}\n"))
        .unwrap_or_default();
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "1.0.0"
spec:
  initial_state: START
{repositories}  states:
    START:
      kind: System
      command: "echo ok"
      transitions:
        - condition: always
          target: END
    END:
      kind: System
      command: "echo done"
      transitions: []
"#
    )
}

#[derive(Default)]
struct RecordingEngine {
    starts: Mutex<usize>,
}

#[async_trait]
impl WorkflowEnginePort for RecordingEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(&self, _params: StartWorkflowParams<'_>) -> anyhow::Result<String> {
        *self.starts.lock().unwrap() += 1;
        Ok("recorded-run-id".to_string())
    }
}

/// Records every preparation it is asked for, and holds nothing.
#[derive(Default)]
struct RecordingRepositories {
    prepared: Mutex<Vec<usize>>,
}

#[async_trait]
impl RunRepositories for RecordingRepositories {
    async fn prepare_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        self.prepared.lock().unwrap().push(entries.len());
        Ok(entries.to_vec())
    }

    async fn mounts_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        _entries: &[RunRepository],
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        Ok(Vec::new())
    }

    fn release_run(&self, _run: uuid::Uuid) {}

    fn take_prepared(&self, _run: uuid::Uuid) -> Vec<PreparedRepository> {
        Vec::new()
    }
}

struct Start {
    use_case: StandardStartWorkflowExecutionUseCase,
    engine: Arc<RecordingEngine>,
    repositories: Arc<RecordingRepositories>,
    tenant: TenantId,
}

async fn start_for(yaml: &str) -> Start {
    let workflow = WorkflowParser::parse_yaml(yaml).expect("the manifest parses");
    let tenant = TenantId::consumer();
    let workflows = Arc::new(InMemoryWorkflowRepository::new());
    workflows.save_for_tenant(&tenant, &workflow).await.unwrap();
    let engine = Arc::new(RecordingEngine::default());
    let use_case = StandardStartWorkflowExecutionUseCase::new(
        workflows,
        Arc::new(InMemoryWorkflowExecutionRepository::new()),
        Arc::new(tokio::sync::RwLock::new(Some(
            engine.clone() as Arc<dyn WorkflowEnginePort>
        ))),
        Arc::new(EventBus::new(8)),
    );
    let repositories = Arc::new(RecordingRepositories::default());
    use_case.set_repositories(repositories.clone());
    Start {
        use_case,
        engine,
        repositories,
        tenant,
    }
}

impl Start {
    async fn run(&self, name: &str, input: Value) -> anyhow::Result<()> {
        self.use_case
            .start_execution(StartWorkflowExecutionRequest {
                workflow_id: name.to_string(),
                input,
                blackboard: None,
                version: None,
                tenant_id: Some(self.tenant.clone()),
                security_context_name: None,
                intent: None,
            })
            .await
            .map(|_| ())
    }
}

/// The sentence a start was refused with (`ExecutionError::Refused`).
fn sentence(refused: &anyhow::Error) -> String {
    match refused.downcast_ref::<ExecutionError>() {
        Some(ExecutionError::Refused(sentence)) => sentence.clone(),
        _ => panic!("the start failed, not refused: {refused:#}"),
    }
}

fn entries(ids: &[&str]) -> Value {
    Value::Array(ids.iter().map(|id| json!({ "binding_id": id })).collect())
}

#[test]
fn spec_repositories_is_read_from_the_manifest_and_written_back() {
    let workflow =
        WorkflowParser::parse_yaml(&manifest("one-repository", Some(1))).expect("parses");
    let written = WorkflowParser::to_yaml(&workflow).expect("writes");
    let again = WorkflowParser::parse_yaml(&written).expect("parses again");
    let as_json = serde_json::to_value(&again.spec).expect("the spec serialises");
    assert_eq!(
        as_json.get("repositories"),
        Some(&json!(1)),
        "spec.repositories was not kept through a parse and a write: {written}"
    );

    let none = WorkflowParser::parse_yaml(&manifest("no-count", None)).expect("parses");
    let as_json = serde_json::to_value(&none.spec).expect("the spec serialises");
    assert_eq!(as_json.get("repositories"), None);
}

#[tokio::test]
async fn a_start_naming_fewer_repositories_than_the_workflow_works_on_is_refused() {
    let start = start_for(&manifest("the-forge", Some(1))).await;
    let refused = start
        .run("the-forge", json!({ "task": "x" }))
        .await
        .expect_err("a start naming no repository started");
    assert_eq!(
        sentence(&refused),
        "workflow 'the-forge' works on exactly 1 repository; this run names 0"
    );
    assert_eq!(
        *start.engine.starts.lock().unwrap(),
        0,
        "Temporal was started"
    );
}

#[tokio::test]
async fn a_start_naming_more_repositories_than_the_workflow_works_on_is_refused_before_any_is_held()
{
    let start = start_for(&manifest("the-forge", Some(1))).await;
    let refused = start
        .run(
            "the-forge",
            json!({ "task": "x", "repositories": entries(&[FIRST, SECOND]) }),
        )
        .await
        .expect_err("a start naming two repositories started");
    assert_eq!(
        sentence(&refused),
        "workflow 'the-forge' works on exactly 1 repository; this run names 2"
    );
    assert!(
        start.repositories.prepared.lock().unwrap().is_empty(),
        "a repository was prepared before the count was checked"
    );
    assert_eq!(
        *start.engine.starts.lock().unwrap(),
        0,
        "Temporal was started"
    );
}

#[tokio::test]
async fn a_start_naming_the_number_the_workflow_works_on_passes_the_count() {
    let start = start_for(&manifest("the-forge", Some(1))).await;
    start
        .run(
            "the-forge",
            json!({ "task": "x", "repositories": entries(&[FIRST]) }),
        )
        .await
        .unwrap_or_else(|e| panic!("a start naming one repository was refused: {e:#}"));
    assert_eq!(*start.repositories.prepared.lock().unwrap(), vec![1]);
}

#[tokio::test]
async fn a_workflow_declaring_no_count_starts_with_any_number() {
    let start = start_for(&manifest("free-count", None)).await;
    start
        .run("free-count", json!({}))
        .await
        .unwrap_or_else(|e| panic!("refused with no repository: {e:#}"));
    start
        .run(
            "free-count",
            json!({ "repositories": entries(&[FIRST, SECOND]) }),
        )
        .await
        .unwrap_or_else(|e| panic!("refused with two repositories: {e:#}"));
}

#[test]
fn a_container_step_may_name_the_runs_repository_beside_declared_volumes() {
    let yaml = r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: tests-on-the-clone
  version: "1.0.0"
spec:
  initial_state: TEST
  repositories: 1
  storage:
    shared_volumes:
      - name: cache
        storage_class: ephemeral
  states:
    TEST:
      kind: ContainerRun
      name: suite
      image: "docker.io/library/python:3.11-slim"
      command: ["pytest"]
      volumes:
        - name: repository
          mount_path: /workspace/app
        - name: repository:app
          mount_path: /workspace/again
        - name: cache
          mount_path: /cache
      network_mode: egress
      transitions: []
"#;
    WorkflowParser::parse_yaml(yaml)
        .unwrap_or_else(|e| panic!("a step naming the run's repository was refused: {e}"));

    let undeclared = yaml.replace(
        "- name: cache\n          mount_path: /cache",
        "- name: other\n          mount_path: /cache",
    );
    let refused = WorkflowParser::parse_yaml(&undeclared)
        .expect_err("an undeclared volume beside the repository was accepted");
    assert!(
        refused.to_string().contains("'other'"),
        "the refusal did not name the undeclared volume: {refused}"
    );
}
