// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A workflow run's attachments (AEGIS ADR-143 S3 and S4): `aegis.workflow.run`
//! places them in the run's `input.attachments`, as `aegis.execute.intent`
//! does, and a ContainerRun step naming the volume `attachments` mounts,
//! read-only at its `mount_path`, a copy of each at its `name`, made before
//! any container into a volume owned by the workflow execution and mounted
//! through the gateway as `repository` is.
//!
//! | Scenario | Test |
//! |---|---|
//! | S3: the run's input carries them | `workflow_run_places_attachments_in_the_runs_input` |
//! | S4: the copy at each name, read-only | `a_step_naming_attachments_mounts_the_runs_files_read_only_at_their_names` |
//! | S4: the copy's volume | `the_steps_copy_is_a_volume_owned_by_the_workflow_execution_and_read_only_on_the_mount` |
//! | S4: another person's volume | `an_attachment_in_a_volume_not_the_runs_persons_is_refused_before_any_container` |
//! | S4: a run with none | `a_step_naming_attachments_in_a_run_carrying_none_is_refused_before_any_container` |
//! | `attachments` is never a declared volume | `a_container_step_may_name_attachments_beside_declared_volumes` |

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::file_operations_service::FileOperationsService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::run_container_step::{
    RunContainerStepError, RunContainerStepInput, RunContainerStepUseCase, WorkflowRunAttachments,
};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StartWorkflowExecutionRequest, StartWorkflowExecutionUseCase, StartedWorkflowExecution,
};
use aegis_orchestrator_core::application::tool_invocation_service::{
    ToolInvocationResult, ToolInvocationService,
};
use aegis_orchestrator_core::application::volume_manager::{StandardVolumeService, VolumeService};
use aegis_orchestrator_core::domain::agent::{
    Agent, AgentId, AgentManifest, AgentStatus, ImagePullPolicy,
};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::AegisFSAL;
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::repository::{
    AgentVersion, VolumeRepository, WorkflowExecutionRepository,
};
use aegis_orchestrator_core::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
};
use aegis_orchestrator_core::domain::security_context::{
    Capability, SecurityContext, SecurityContextMetadata, SecurityContextRepository,
};
use aegis_orchestrator_core::domain::shared_kernel::VolumeId;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::volume::{StorageClass, VolumeOwnership, VolumeStatus};
use aegis_orchestrator_core::domain::workflow::{
    ContainerVolumeMount, StateName, WorkflowExecution,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
    InMemoryWorkflowExecutionRepository,
};
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

const PERSON: &str = "the-person";
const CONTEXT: &str = "workflow-attachments-test";
const RECORDING: &[u8] = b"RIFF....WAVEfmt the recording";
const NOTES: &[u8] = b"the notes\n";

// ===========================================================================
// S3: aegis.workflow.run
// ===========================================================================

/// Keeps the request each start was given.
#[derive(Default)]
struct RecordingStart {
    requests: Mutex<Vec<StartWorkflowExecutionRequest>>,
}

#[async_trait]
impl StartWorkflowExecutionUseCase for RecordingStart {
    async fn start_execution_for_tenant(
        &self,
        _tenant_id: &TenantId,
        request: StartWorkflowExecutionRequest,
        _identity: Option<&UserIdentity>,
    ) -> Result<StartedWorkflowExecution> {
        let workflow_id = request.workflow_id.clone();
        self.requests.lock().unwrap().push(request);
        Ok(StartedWorkflowExecution {
            execution_id: ExecutionId::new().to_string(),
            workflow_id,
            temporal_run_id: "temporal-run".to_string(),
            status: "started".to_string(),
            started_at: chrono::Utc::now(),
        })
    }
}

#[tokio::test]
async fn workflow_run_places_attachments_in_the_runs_input() {
    let agent = agent();
    let agent_id = agent.id;
    let mut execution = Execution::new_with_id(
        ExecutionId::new(),
        agent_id,
        ExecutionInput {
            intent: None,
            input: json!({}),
            workspace_volume_id: None,
            workspace_volume_mount_path: None,
            workspace_remote_path: None,
            workflow_execution_id: None,
            attachments: Vec::new(),
        },
        5,
        CONTEXT.to_string(),
    );
    execution.tenant_id = TenantId::default();
    let caller = execution.id;
    let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
    security_context_repo
        .save(security_context())
        .await
        .unwrap();
    let storage_root = std::env::temp_dir().join(format!(
        "aegis-workflow-run-attachments-{}",
        uuid::Uuid::new_v4()
    ));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
        Arc::new(InMemoryVolumeRepository::new()),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let start = Arc::new(RecordingStart::default());
    let service = ToolInvocationService::new(
        Arc::new(InMemorySealSessionRepository::new()),
        security_context_repo,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(OneAgent(agent)),
        Arc::new(Executions(HashMap::from([(caller, execution)]))),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        Arc::new(EventBus::new(64)),
        None,
    )
    .with_workflow_execution(start.clone());

    let volume_id = uuid::Uuid::new_v4().to_string();
    let attachment = json!({
        "volume_id": volume_id,
        "path": "uploads/clip.wav",
        "name": "recording.wav",
        "mime_type": "audio/x-wav",
        "size": RECORDING.len(),
    });
    let answer = match service
        .invoke_tool_internal(
            &agent_id,
            caller,
            TenantId::default(),
            0,
            Vec::new(),
            "aegis.workflow.run".to_string(),
            json!({
                "name": "transcribe-audio",
                "input": { "language": "en" },
                "attachments": [attachment.clone()],
            }),
        )
        .await
    {
        Ok(ToolInvocationResult::Direct(value)) => value,
        other => panic!("aegis.workflow.run did not answer directly: {other:?}"),
    };
    assert_eq!(answer["status"], json!("started"), "{answer}");

    let requests = start.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "the workflow was not started once");
    assert_eq!(
        requests[0].input["attachments"],
        json!([attachment]),
        "the run's input does not carry the attachments: {}",
        requests[0].input
    );
    assert_eq!(requests[0].input["language"], json!("en"));
}

// ===========================================================================
// S4: the step's mount
// ===========================================================================

/// What the runner saw of one step, read while it ran.
struct Seen {
    config: ContainerStepConfig,
    /// Per volume handed: the gateway's mount point and whether it lets
    /// the step write.
    registered: Vec<Option<(PathBuf, bool)>>,
    /// Per volume handed: its record's ownership.
    ownership: Vec<Option<VolumeOwnership>>,
    /// Per volume handed: its files, by name, with their bytes.
    files: Vec<HashMap<String, Vec<u8>>>,
}

/// Records each step and answers exit 0.
struct RecordingRunner {
    registry: NfsVolumeRegistry,
    volumes: Arc<InMemoryVolumeRepository>,
    storage_root: PathBuf,
    seen: Mutex<Vec<Seen>>,
}

#[async_trait]
impl ContainerStepRunner for RecordingRunner {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        let mut registered = Vec::new();
        let mut ownership = Vec::new();
        let mut files = Vec::new();
        for v in &config.volumes {
            let id = uuid::Uuid::parse_str(&v.name).ok().map(VolumeId);
            registered.push(id.and_then(|id| {
                self.registry
                    .lookup(id)
                    .map(|ctx| (ctx.mount_point.clone(), !ctx.policy.write.is_empty()))
            }));
            let record = match id {
                Some(id) => self.volumes.find_by_id(id).await.unwrap(),
                None => None,
            };
            ownership.push(record.as_ref().map(|r| r.ownership.clone()));
            let mut found = HashMap::new();
            if let Some(record) = record {
                let remote = record
                    .to_mount(
                        PathBuf::from("/"),
                        aegis_orchestrator_core::domain::volume::AccessMode::ReadOnly,
                    )
                    .remote_path;
                let dir = self.storage_root.join(remote.trim_start_matches('/'));
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        found.insert(
                            entry.file_name().to_string_lossy().to_string(),
                            std::fs::read(entry.path()).unwrap(),
                        );
                    }
                }
            }
            files.push(found);
        }
        self.seen.lock().unwrap().push(Seen {
            config,
            registered,
            ownership,
            files,
        });
        Ok(ContainerStepResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 1,
        })
    }
}

/// The step use case wired with the real services over one local store.
struct Steps {
    tenant: TenantId,
    registry: NfsVolumeRegistry,
    volumes: Arc<InMemoryVolumeRepository>,
    volume_service: Arc<StandardVolumeService>,
    files: Arc<FileOperationsService>,
    executions: Arc<InMemoryWorkflowExecutionRepository>,
    runner: Arc<RecordingRunner>,
    use_case: RunContainerStepUseCase,
}

async fn steps() -> Steps {
    let tenant = TenantId::consumer();
    let storage_root =
        std::env::temp_dir().join(format!("aegis-step-attachments-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&storage_root).unwrap();
    let storage = Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap());
    let volumes = Arc::new(InMemoryVolumeRepository::new());
    let fsal = Arc::new(AegisFSAL::new(
        storage.clone(),
        volumes.clone(),
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoOpPublisher),
    ));
    let volume_service = Arc::new(
        StandardVolumeService::new(
            volumes.clone(),
            storage,
            Arc::new(EventBus::new(64)),
            "http://filer:8888".to_string(),
            "local_host",
        )
        .unwrap(),
    );
    let files = Arc::new(FileOperationsService::new(
        fsal.clone(),
        Arc::new(InMemoryExecutionRepository::new()),
        Arc::new(InMemoryAgentRepository::new()),
    ));
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let registry = NfsVolumeRegistry::new();
    let runner = Arc::new(RecordingRunner {
        registry: registry.clone(),
        volumes: volumes.clone(),
        storage_root,
        seen: Mutex::new(Vec::new()),
    });
    let use_case = RunContainerStepUseCase::new(runner.clone());
    use_case.set_run_attachments(
        Arc::new(WorkflowRunAttachments::new(
            executions.clone(),
            volume_service.clone(),
            files.clone(),
            fsal,
        )),
        registry.clone(),
    );
    Steps {
        tenant,
        registry,
        volumes,
        volume_service,
        files,
        executions,
        runner,
        use_case,
    }
}

impl Steps {
    /// A file `path` holding `data` in a persistent volume of `owner`;
    /// answers its attachment reference, named `name`.
    async fn upload(&self, owner: &str, path: &str, name: &str, data: &[u8]) -> Value {
        let volume = self
            .volume_service
            .create_volume(
                format!("chat-attachments-{}", uuid::Uuid::new_v4()),
                self.tenant.clone(),
                StorageClass::persistent(),
                10,
                VolumeOwnership::persistent(owner),
            )
            .await
            .unwrap();
        self.files
            .write_file(&volume, &self.tenant, owner, path, data, 1 << 20)
            .await
            .unwrap();
        json!({
            "volume_id": volume.0.to_string(),
            "path": path,
            "name": name,
            "mime_type": "application/octet-stream",
            "size": data.len(),
        })
    }

    /// A workflow execution of `PERSON` with `input`.
    async fn run(&self, input: Value) -> uuid::Uuid {
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
        let run = ExecutionId::new();
        let mut execution = WorkflowExecution::new(&workflow, run, input);
        execution.tenant_id = self.tenant.clone();
        execution.initiating_user_sub = Some(PERSON.to_string());
        self.executions
            .save_for_tenant(&self.tenant, &execution)
            .await
            .unwrap();
        run.0
    }

    fn input(&self, run: uuid::Uuid, read_only: bool) -> RunContainerStepInput {
        RunContainerStepInput {
            execution_id: ExecutionId::new(),
            state_name: StateName::new("TRANSCRIBE").unwrap(),
            name: "transcribe".to_string(),
            image: "ghcr.io/100monkeys-ai/aegis-model-whisper:test".to_string(),
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            command: vec!["transcribe".to_string()],
            env: HashMap::new(),
            workdir: None,
            volumes: vec![ContainerVolumeMount {
                name: "attachments".to_string(),
                mount_path: "/input".to_string(),
                read_only,
            }],
            resources: None,
            registry_credentials: None,
            max_attempts: 1,
            shell: false,
            read_only_root_filesystem: false,
            run_as_user: None,
            network_mode: Some("none".to_string()),
            workflow_execution_id: Some(run),
            tenant_id: self.tenant.clone(),
        }
    }

    fn runs(&self) -> usize {
        self.runner.seen.lock().unwrap().len()
    }

    /// The volumes owned by the workflow execution `run` and not deleted.
    async fn copies_of(&self, run: uuid::Uuid) -> usize {
        self.volume_service
            .list_volumes_by_ownership(&VolumeOwnership::workflow(run))
            .await
            .unwrap()
            .iter()
            .filter(|v| v.status != VolumeStatus::Deleted)
            .count()
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
async fn a_step_naming_attachments_mounts_the_runs_files_read_only_at_their_names() {
    let s = steps().await;
    let recording = s
        .upload(PERSON, "uploads/clip.wav", "recording.wav", RECORDING)
        .await;
    let notes = s.upload(PERSON, "notes.txt", "notes.txt", NOTES).await;
    let run = s.run(json!({ "attachments": [recording, notes] })).await;

    let output = s
        .use_case
        .run(s.input(run, true))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));
    assert_eq!(output.exit_code, 0);

    let seen = std::mem::take(&mut *s.runner.seen.lock().unwrap());
    assert_eq!(seen.len(), 1);
    let handed = &seen[0].config.volumes;
    assert_eq!(handed.len(), 1);
    assert_ne!(
        handed[0].name, "attachments",
        "the runner was handed the volume name, not the run's files"
    );
    assert_eq!(handed[0].mount_path, "/input");
    assert!(handed[0].read_only, "the attachments were mounted writable");
    assert_eq!(
        seen[0].files[0],
        HashMap::from([
            ("recording.wav".to_string(), RECORDING.to_vec()),
            ("notes.txt".to_string(), NOTES.to_vec()),
        ]),
        "the mounted volume does not hold exactly the run's files at their names"
    );
    assert_eq!(
        seen[0].registered[0],
        Some((PathBuf::from("/input"), false)),
        "the gateway did not hold the copy, read-only, at the step's path while it ran"
    );
    let copy = VolumeId(uuid::Uuid::parse_str(&handed[0].name).unwrap());
    assert!(
        s.registry.lookup(copy).is_none(),
        "the step's registration of the copy outlived the step"
    );
    assert_eq!(
        s.copies_of(run).await,
        0,
        "the step's copy outlived the step"
    );
}

#[tokio::test]
async fn the_steps_copy_is_a_volume_owned_by_the_workflow_execution_and_read_only_on_the_mount() {
    let s = steps().await;
    let recording = s
        .upload(PERSON, "uploads/clip.wav", "recording.wav", RECORDING)
        .await;
    let run = s.run(json!({ "attachments": [recording.clone()] })).await;

    // The entry asks for a writable mount; only read-only is given.
    s.use_case
        .run(s.input(run, false))
        .await
        .unwrap_or_else(|e| panic!("the step was not run: {e}"));

    let seen = std::mem::take(&mut *s.runner.seen.lock().unwrap());
    let source = recording["volume_id"].as_str().unwrap();
    assert_ne!(
        seen[0].config.volumes[0].name, source,
        "the step was handed the person's own volume, not a copy"
    );
    assert_eq!(
        seen[0].ownership[0],
        Some(VolumeOwnership::workflow(run)),
        "the copy is not a volume owned by the workflow execution"
    );
    assert!(
        seen[0].config.volumes[0].read_only,
        "the copy was mounted writable"
    );
    assert_eq!(
        seen[0].registered[0],
        Some((PathBuf::from("/input"), false)),
        "the gateway let the step write the copy"
    );
    let person_volume = s
        .volumes
        .find_by_id(VolumeId(uuid::Uuid::parse_str(source).unwrap()))
        .await
        .unwrap()
        .expect("the person's volume is gone");
    assert_eq!(person_volume.ownership, VolumeOwnership::persistent(PERSON));
}

#[tokio::test]
async fn an_attachment_in_a_volume_not_the_runs_persons_is_refused_before_any_container() {
    let s = steps().await;
    let mine = s.upload(PERSON, "notes.txt", "notes.txt", NOTES).await;
    let theirs = s
        .upload(
            "someone-else",
            "uploads/clip.wav",
            "recording.wav",
            RECORDING,
        )
        .await;
    let run = s.run(json!({ "attachments": [mine, theirs] })).await;

    let sentence = refusal(s.use_case.run(s.input(run, true)).await);
    assert_eq!(sentence, "this attachment is not yours");
    assert_eq!(s.runs(), 0, "a container was started");
    assert_eq!(s.copies_of(run).await, 0, "a copy was made");
}

#[tokio::test]
async fn a_step_naming_attachments_in_a_run_carrying_none_is_refused_before_any_container() {
    let s = steps().await;
    let run = s.run(json!({ "language": "en" })).await;

    let sentence = refusal(s.use_case.run(s.input(run, true)).await);
    assert_eq!(
        sentence,
        "this step reads the run's attachments and the run has none"
    );
    assert_eq!(s.runs(), 0, "a container was started");

    let empty = s.run(json!({ "attachments": [] })).await;
    let sentence = refusal(s.use_case.run(s.input(empty, true)).await);
    assert_eq!(
        sentence,
        "this step reads the run's attachments and the run has none"
    );
    assert_eq!(s.runs(), 0, "a container was started");
}

#[test]
fn a_container_step_may_name_attachments_beside_declared_volumes() {
    let yaml = r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: transcribe-like
  version: "1.0.0"
spec:
  initial_state: TRANSCRIBE
  storage:
    shared_volumes:
      - name: output
        storage_class: ephemeral
  states:
    TRANSCRIBE:
      kind: ContainerRun
      name: transcribe
      image: "docker.io/library/python:3.11-slim"
      command: ["transcribe"]
      volumes:
        - name: attachments
          mount_path: /input
          read_only: true
        - name: output
          mount_path: /output
      transitions: []
"#;
    WorkflowParser::parse_yaml(yaml)
        .unwrap_or_else(|e| panic!("a step naming the run's attachments was refused: {e}"));
}

// ===========================================================================
// Test doubles the dispatch reads
// ===========================================================================

fn agent() -> Agent {
    let manifest: AgentManifest = serde_yaml::from_str(
        r#"
apiVersion: 100monkeys.ai/v1
kind: Agent
metadata:
  name: workflow-attachments-test-agent
  version: "1.0.0"
spec:
  runtime:
    language: python
    version: "3.11"
    isolation: inherit
    model: smart
  tools: ["aegis.workflow.run"]
"#,
    )
    .unwrap();
    Agent {
        id: AgentId::new(),
        tenant_id: TenantId::default(),
        scope: aegis_orchestrator_core::domain::agent::AgentScope::default(),
        name: manifest.metadata.name.clone(),
        manifest,
        status: AgentStatus::Active,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn security_context() -> SecurityContext {
    SecurityContext {
        name: CONTEXT.to_string(),
        description: "workflow attachments test".to_string(),
        capabilities: vec![Capability {
            tool_pattern: "aegis.workflow.*".to_string(),
            path_allowlist: None,
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        }],
        deny_list: vec![],
        metadata: SecurityContextMetadata {
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            version: 1,
        },
    }
}

struct Executions(HashMap<ExecutionId, Execution>);

#[async_trait]
impl ExecutionService for Executions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&UserIdentity>,
    ) -> Result<ExecutionId> {
        Ok(execution_id)
    }
    async fn start_child_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: ExecutionId,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn get_execution_for_tenant(&self, _: &TenantId, id: ExecutionId) -> Result<Execution> {
        self.get_execution_unscoped(id).await
    }
    async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
        self.0
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("execution not found"))
    }
    async fn get_iterations_for_tenant(
        &self,
        _: &TenantId,
        _: ExecutionId,
    ) -> Result<Vec<Iteration>> {
        anyhow::bail!("not exercised")
    }
    async fn cancel_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn stream_execution(
        &self,
        _: ExecutionId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ExecutionEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn stream_agent_events(
        &self,
        _: AgentId,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
        anyhow::bail!("not exercised")
    }
    async fn list_executions_for_tenant(
        &self,
        _: &TenantId,
        _: Option<AgentId>,
        _: Option<aegis_orchestrator_core::domain::workflow::WorkflowId>,
        _: usize,
    ) -> Result<Vec<Execution>> {
        anyhow::bail!("not exercised")
    }
    async fn delete_execution_for_tenant(&self, _: &TenantId, _: ExecutionId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn record_llm_interaction(
        &self,
        _: ExecutionId,
        _: u8,
        _: aegis_orchestrator_core::domain::execution::LlmInteraction,
    ) -> Result<()> {
        Ok(())
    }
    async fn store_iteration_trajectory(
        &self,
        _: ExecutionId,
        _: u8,
        _: Vec<aegis_orchestrator_core::domain::execution::TrajectoryStep>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Resolves every agent to one agent with no `tool_validation`, so the
/// inner-loop judge does not run.
struct OneAgent(Agent);

#[async_trait]
impl AgentLifecycleService for OneAgent {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: aegis_orchestrator_core::domain::agent::AgentScope,
        _: Option<&UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        Ok(self.0.clone())
    }
    async fn update_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
        _: AgentManifest,
    ) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn delete_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<()> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(Some(self.0.id))
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        anyhow::bail!("not exercised")
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![self.0.clone()])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

struct NoOpPublisher;

#[async_trait]
impl aegis_orchestrator_core::domain::fsal::EventPublisher for NoOpPublisher {
    async fn publish_storage_event(
        &self,
        _event: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}
