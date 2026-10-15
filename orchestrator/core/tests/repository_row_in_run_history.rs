// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A workflow run's repository row is a row of the run
//!
//! AEGIS ADR-141's Update of 2026-10-15 (2): when a workflow's first agent
//! state publishes the run's `RepositoryPrepared`, the persister records the
//! row under the agent state's execution and under the workflow execution,
//! so the run's logs route (which reads only the workflow execution's rows)
//! carries it. The start goes through the execution service's own start
//! path, with the persister listening on the same bus.
//!
//! | Scenario | Test |
//! |---|---|
//! | A workflow's agent state prepares the run's repository | `a_workflow_agent_states_repository_row_is_stored_under_the_state_and_the_run` |
//! | A root agent run's row stays its own | `a_root_runs_repository_row_is_stored_under_its_execution_alone` |

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use aegis_orchestrator_core::application::execution::{ExecutionService, StandardExecutionService};
use aegis_orchestrator_core::application::execution_event_persister::ExecutionEventPersister;
use aegis_orchestrator_core::application::git_repo_service::{
    PreparedRepository, RunMount, RunRepositories, RunRepositoryError,
};
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::agent::Agent;
use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionInput, ExecutionStatus};
use aegis_orchestrator_core::domain::git_repo::{default_work_branch, RunRepository, TreeReset};
use aegis_orchestrator_core::domain::repository::{
    AgentRepository, RepositoryError, WorkflowExecutionRepository,
};
use aegis_orchestrator_core::domain::runtime::{
    AgentRuntime, InstanceId, InstanceStatus, RuntimeConfig, RuntimeError, TaskInput, TaskOutput,
};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::supervisor::Supervisor;
use aegis_orchestrator_core::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeMount, VolumeOwnership,
};
use aegis_orchestrator_core::domain::workflow::{
    WorkflowExecution, WorkflowExecutionEventRecord, WorkflowId,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryWorkflowExecutionRepository,
};

const BINDING: &str = "7d1c2b3a-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
const STARTED_FROM: &str = "0123456789abcdef0123456789abcdef01234567";

// ===========================================================================
// Stand-ins
// ===========================================================================

/// The workflow executions, kept in memory, with every event row appended
/// to them kept under its execution, as the `execution_events` table does.
struct WorkflowRows {
    executions: InMemoryWorkflowExecutionRepository,
    rows: Mutex<Vec<(ExecutionId, WorkflowExecutionEventRecord)>>,
}

impl WorkflowRows {
    fn new() -> Self {
        Self {
            executions: InMemoryWorkflowExecutionRepository::new(),
            rows: Mutex::new(Vec::new()),
        }
    }

    /// The event types stored under `id`, in sequence order.
    fn types_under(&self, id: ExecutionId) -> Vec<String> {
        let mut rows: Vec<(i64, String)> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(of, _)| *of == id)
            .map(|(_, record)| (record.sequence, record.event_type.clone()))
            .collect();
        rows.sort();
        rows.into_iter().map(|(_, event_type)| event_type).collect()
    }

    /// The payload of the `repository_prepared` row under `id`, if any.
    fn prepared_under(&self, id: ExecutionId) -> Option<serde_json::Value> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|(of, record)| *of == id && record.event_type == "repository_prepared")
            .map(|(_, record)| record.payload.clone())
    }

    /// Wait until `n` rows are stored under `id` (the persister writes in
    /// its own task).
    async fn wait_for(&self, id: ExecutionId, n: usize) {
        for _ in 0..400 {
            if self.types_under(id).len() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

#[async_trait]
impl WorkflowExecutionRepository for WorkflowRows {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        self.executions.save_for_tenant(tenant_id, execution).await
    }
    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<WorkflowExecution>, RepositoryError> {
        self.executions.find_by_id_for_tenant(tenant_id, id).await
    }
    async fn find_active_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions.find_active_for_tenant(tenant_id).await
    }
    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: WorkflowId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions
            .find_by_workflow_for_tenant(tenant_id, workflow_id, limit, offset)
            .await
    }
    async fn list_paginated_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions
            .list_paginated_for_tenant(tenant_id, limit, offset)
            .await
    }
    async fn list_paginated_all(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.executions.list_paginated_all(limit, offset).await
    }
    async fn count_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: WorkflowId,
    ) -> Result<i64, RepositoryError> {
        self.executions
            .count_by_workflow_for_tenant(tenant_id, workflow_id)
            .await
    }
    async fn update_temporal_linkage_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        temporal_workflow_id: &str,
        temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        self.executions
            .update_temporal_linkage_for_tenant(
                tenant_id,
                execution_id,
                temporal_workflow_id,
                temporal_run_id,
            )
            .await
    }
    async fn append_event(
        &self,
        execution_id: ExecutionId,
        sequence_number: i64,
        event_type: String,
        payload: serde_json::Value,
        iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        self.rows.lock().unwrap().push((
            execution_id,
            WorkflowExecutionEventRecord {
                sequence: sequence_number,
                event_type,
                state_name: None,
                iteration_number,
                payload,
                recorded_at: chrono::Utc::now(),
            },
        ));
        Ok(())
    }
    async fn find_events_by_execution(
        &self,
        id: ExecutionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
        let mut rows: Vec<WorkflowExecutionEventRecord> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|(of, _)| *of == id)
            .map(|(_, record)| record.clone())
            .collect();
        rows.sort_by_key(|record| record.sequence);
        Ok(rows.into_iter().skip(offset).take(limit).collect())
    }
    async fn find_tenant_id_by_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        self.executions.find_tenant_id_by_execution(id).await
    }
}

/// A run's repositories as the git repository service answers them: each
/// entry checked out on its branch, one tree at `/workspace/app`, and one
/// preparation kept for the run until its first agent execution takes it.
struct OneRepository {
    volume: VolumeId,
    prepared: Mutex<Vec<uuid::Uuid>>,
}

impl OneRepository {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            volume: VolumeId::new(),
            prepared: Mutex::new(Vec::new()),
        })
    }

    /// The run was prepared before its first agent state started, as a
    /// workflow's start prepares it.
    fn prepared_before(&self, run: uuid::Uuid) {
        self.prepared.lock().unwrap().push(run);
    }
}

#[async_trait]
impl RunRepositories for OneRepository {
    async fn prepare_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        self.prepared.lock().unwrap().push(run);
        Ok(entries
            .iter()
            .map(|entry| RunRepository {
                branch: Some(
                    entry
                        .branch
                        .clone()
                        .unwrap_or_else(|| default_work_branch(run)),
                ),
                ..entry.clone()
            })
            .collect())
    }

    async fn mounts_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        _entries: &[RunRepository],
    ) -> Result<Vec<RunMount>, RunRepositoryError> {
        Ok(vec![RunMount {
            label: "app".to_string(),
            mount: VolumeMount::new(
                self.volume,
                PathBuf::from("/workspace/app"),
                AccessMode::ReadWrite,
                FilerEndpoint::new("http://filer:8888").unwrap(),
                "/aegis/seaweedfs/app/repo".to_string(),
            ),
        }])
    }

    fn release_run(&self, _run: uuid::Uuid) {}

    fn take_prepared(&self, run: uuid::Uuid) -> Vec<PreparedRepository> {
        let mut prepared = self.prepared.lock().unwrap();
        let Some(at) = prepared.iter().position(|of| *of == run) else {
            return Vec::new();
        };
        prepared.remove(at);
        vec![PreparedRepository {
            label: "app".to_string(),
            branch: default_work_branch(run),
            started_from: STARTED_FROM.to_string(),
            created: true,
            tree_reset: TreeReset::Hard,
            removed: vec!["test/".to_string()],
            removed_count: None,
            tests: Some(Vec::new()),
            prepared_at: chrono::Utc::now(),
        }]
    }
}

/// A runtime that starts no container: the supervisor's spawn fails in its
/// own task, after the start has answered and published its rows.
struct NoContainers;

#[async_trait]
impl AgentRuntime for NoContainers {
    async fn spawn(&self, _config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        Err(RuntimeError::SpawnFailed(
            "these tests start no container".to_string(),
        ))
    }
    async fn execute(
        &self,
        _id: &InstanceId,
        _input: TaskInput,
    ) -> Result<TaskOutput, RuntimeError> {
        unreachable!("no container is spawned")
    }
    async fn terminate(&self, _id: &InstanceId) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn status(&self, _id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
        unreachable!("no container is spawned")
    }
}

/// A volume service the start never needs: the agent declares no volume.
struct NoVolumes;

#[async_trait]
impl VolumeService for NoVolumes {
    async fn create_volume(
        &self,
        _name: String,
        _tenant_id: TenantId,
        _storage_class: StorageClass,
        _size_limit_mb: u64,
        _ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        unreachable!("the agent declares no volume")
    }
    async fn get_volume(&self, _id: VolumeId) -> anyhow::Result<Volume> {
        unreachable!("the agent declares no volume")
    }
    async fn list_volumes_by_tenant(&self, _tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn list_volumes_by_ownership(
        &self,
        _ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn attach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
        _mount_point: PathBuf,
        _access_mode: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        unreachable!("the agent declares no volume")
    }
    async fn detach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_volume(&self, _volume_id: VolumeId) -> anyhow::Result<()> {
        Ok(())
    }
    async fn get_volume_usage(&self, _volume_id: VolumeId) -> anyhow::Result<u64> {
        Ok(0)
    }
    async fn cleanup_expired_volumes(&self) -> anyhow::Result<usize> {
        Ok(0)
    }
    async fn create_volumes_for_execution(
        &self,
        _execution_id: ExecutionId,
        _tenant_id: TenantId,
        _volume_specs: &[aegis_orchestrator_core::domain::agent::VolumeSpec],
        _storage_mode: &str,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(vec![])
    }
    async fn persist_external_volume(
        &self,
        _volume_id: VolumeId,
        _name: String,
        _tenant_id: TenantId,
        _remote_path: String,
        _size_limit_bytes: u64,
        _ownership: VolumeOwnership,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// The run
// ===========================================================================

/// The execution service, its bus with the persister listening, the
/// workflow executions and their rows, and the agent a state runs.
struct Start {
    executions: StandardExecutionService,
    rows: Arc<WorkflowRows>,
    repositories: Arc<OneRepository>,
    tenant: TenantId,
    agent: Agent,
}

impl Start {
    async fn new() -> Self {
        let tenant = TenantId::consumer();
        let agent = Agent::new(
            serde_yaml::from_str(
                r#"apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: analyst
  version: "1.0.0"
spec:
  runtime:
    image: ghcr.io/example/analyst:latest
    isolation: docker
  task:
    instruction: "Run the analyst task"
"#,
            )
            .unwrap(),
        );
        let agents = Arc::new(InMemoryAgentRepository::new());
        agents.save_for_tenant(&tenant, &agent).await.unwrap();

        let rows = Arc::new(WorkflowRows::new());
        let bus = Arc::new(EventBus::new(256));
        let persister = Arc::new(ExecutionEventPersister::new(rows.clone(), bus.clone()));
        let _persisting = persister.start();

        let executions = StandardExecutionService::new(
            agents,
            Arc::new(NoVolumes),
            Arc::new(Supervisor::new(Arc::new(NoContainers))),
            Arc::new(InMemoryExecutionRepository::new()),
            bus,
            Arc::new(aegis_orchestrator_core::domain::node_config::NodeConfigManifest::default()),
        )
        .with_workflow_executions(rows.clone());
        let repositories = OneRepository::new();
        executions.set_repositories(repositories.clone() as Arc<dyn RunRepositories>);

        Self {
            executions,
            rows,
            repositories,
            tenant,
            agent,
        }
    }

    /// A running workflow execution of the tenant, its repository prepared
    /// as its start prepares it.
    async fn workflow_run(&self) -> uuid::Uuid {
        let workflow =
            aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser::parse_yaml(
                r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: forge
spec:
  initial_state: ANALYZE
  states:
    ANALYZE:
      kind: System
      command: "true"
      transitions: []
"#,
            )
            .unwrap();
        let run = uuid::Uuid::new_v4();
        let mut record = WorkflowExecution::new(
            &workflow,
            ExecutionId(run),
            serde_json::json!({ "repositories": self.entries(run) }),
        );
        record.tenant_id = self.tenant.clone();
        record.status = ExecutionStatus::Running;
        record.initiating_user_sub = Some("u-person".to_string());
        self.rows
            .save_for_tenant(&self.tenant, &record)
            .await
            .unwrap();
        self.repositories.prepared_before(run);
        run
    }

    /// The run's repositories as the prepared input carries them.
    fn entries(&self, run: uuid::Uuid) -> serde_json::Value {
        serde_json::json!([{
            "binding_id": BINDING,
            "branch": default_work_branch(run),
        }])
    }

    /// Start the agent with `input`, under `workflow_execution_id` when it
    /// is a workflow's agent state.
    async fn start(
        &self,
        input: serde_json::Value,
        workflow_execution_id: Option<uuid::Uuid>,
    ) -> ExecutionId {
        self.executions
            .start_execution(
                self.agent.id,
                ExecutionInput {
                    intent: Some("analyze".to_string()),
                    input,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id,
                    attachments: Vec::new(),
                },
                "aegis-system-operator".to_string(),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("the agent did not start: {e}"))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// The run `40e66741` of 2026-10-15: its `RepositoryPrepared` was stored
/// under its first agent state's execution only, and the run's logs route,
/// which reads the workflow execution's rows, never carried it. The row is
/// now stored under both, its payload the same row.
#[tokio::test]
async fn a_workflow_agent_states_repository_row_is_stored_under_the_state_and_the_run() {
    let fx = Start::new().await;
    let run = fx.workflow_run().await;
    let state = fx
        .start(
            serde_json::json!({
                "tenant_id": fx.tenant.as_str(),
                "repositories": fx.entries(run),
            }),
            Some(run),
        )
        .await;
    let workflow_execution = ExecutionId(run);
    fx.rows.wait_for(state, 2).await;
    fx.rows.wait_for(workflow_execution, 1).await;

    let under_state = fx.rows.types_under(state);
    let under_run = fx.rows.types_under(workflow_execution);
    println!("rows under the agent state {state}: {under_state:?}; under the workflow execution {run}: {under_run:?}");
    assert_eq!(
        under_state
            .iter()
            .filter(|event_type| *event_type == "repository_prepared")
            .count(),
        1,
        "the agent state's execution does not hold one repository_prepared row: {under_state:?}"
    );
    assert_eq!(
        under_run,
        vec!["repository_prepared".to_string()],
        "the workflow execution does not hold the run's repository_prepared row: {under_run:?}"
    );
    let payload = fx.rows.prepared_under(workflow_execution).unwrap();
    let row = &payload["RepositoryPrepared"];
    assert_eq!(row["execution_id"], serde_json::json!(run.to_string()));
    assert_eq!(
        row["agent_id"],
        serde_json::json!(fx.agent.id.0.to_string())
    );
    assert_eq!(row["label"], "app");
    assert_eq!(row["branch"], serde_json::json!(default_work_branch(run)));
    assert_eq!(row["started_from"], STARTED_FROM);
    assert_eq!(row["removed"], serde_json::json!(["test/"]));
}

/// A root agent run is its own run: its row is stored under its execution
/// and nowhere else.
#[tokio::test]
async fn a_root_runs_repository_row_is_stored_under_its_execution_alone() {
    let fx = Start::new().await;
    let root = fx
        .start(
            serde_json::json!({
                "tenant_id": fx.tenant.as_str(),
                "repositories": [{ "binding_id": BINDING }],
            }),
            None,
        )
        .await;
    fx.rows.wait_for(root, 2).await;

    let rows = fx.rows.rows.lock().unwrap().clone();
    let prepared: Vec<ExecutionId> = rows
        .iter()
        .filter(|(_, record)| record.event_type == "repository_prepared")
        .map(|(of, _)| *of)
        .collect();
    assert_eq!(
        prepared,
        vec![root],
        "the root run's repository row is not stored under its execution alone"
    );
}
