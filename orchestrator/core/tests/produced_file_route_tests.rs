// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The execution file route's reach (AEGIS ADR-005 I8, choices P2 and P3).
//!
//! `FileOperationsService::read_file_for_execution` is what
//! `GET /v1/executions/:id/files/*path` and `aegis.execution.file` read
//! through. These tests drive it over a real FSAL on a local directory: a
//! file the execution's record lists among its produced files is read from
//! the volume and path the supervisor read it at; any other path is read from
//! the volume mounted at `/workspace`, never the first volume found; and
//! another tenant's execution or volume is answered as not found.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use aegis_orchestrator_core::application::file_operations_service::{
    FileOperationsError, FileOperationsService,
};
use aegis_orchestrator_core::domain::agent::{Agent, AgentManifest};
use aegis_orchestrator_core::domain::events::StorageEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, ProducedFile,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::repository::{
    AgentRepository, ExecutionRepository, VolumeRepository,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::volume::{
    StorageClass, Volume, VolumeBackend, VolumeId, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryAgentRepository, InMemoryExecutionRepository, InMemoryVolumeRepository,
};
use aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider;

struct NoOpPublisher;

#[async_trait]
impl EventPublisher for NoOpPublisher {
    async fn publish_storage_event(&self, _event: StorageEvent) {}
}

fn tenant(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).unwrap()
}

/// The service over a real FSAL whose storage is a scratch directory, with
/// the stores a test fills.
struct World {
    _root: tempfile::TempDir,
    root: PathBuf,
    volumes: Arc<InMemoryVolumeRepository>,
    executions: Arc<InMemoryExecutionRepository>,
    agents: Arc<InMemoryAgentRepository>,
    service: FileOperationsService,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let volumes = Arc::new(InMemoryVolumeRepository::new());
        let executions = Arc::new(InMemoryExecutionRepository::new());
        let agents = Arc::new(InMemoryAgentRepository::new());
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&root).unwrap()),
            volumes.clone(),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoOpPublisher),
        ));
        let service = FileOperationsService::new(fsal, executions.clone(), agents.clone());
        Self {
            _root: dir,
            root,
            volumes,
            executions,
            agents,
            service,
        }
    }

    /// A host volume named `name` of `tenant`, owned as `ownership`, whose
    /// files live under `<root>/hosts/<id>`.
    async fn volume(&self, name: &str, tenant: &TenantId, ownership: VolumeOwnership) -> VolumeId {
        let id = VolumeId::new();
        let mut volume = Volume::new(
            name.to_string(),
            tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: PathBuf::from(format!("/hosts/{id}")),
            },
            1024 * 1024,
            ownership,
        )
        .unwrap();
        volume.id = id;
        volume.mark_available().unwrap();
        self.volumes.save(&volume).await.unwrap();
        id
    }

    /// Write `bytes` at `path` (rooted at `/`) inside volume `id`.
    fn put(&self, id: VolumeId, path: &str, bytes: &[u8]) {
        let file = self
            .root
            .join(format!("hosts/{id}"))
            .join(path.trim_start_matches('/'));
        std::fs::create_dir_all(file.parent().unwrap_or(Path::new("/"))).unwrap();
        std::fs::write(file, bytes).unwrap();
    }

    /// An agent of `tenant` whose manifest mounts `volumes` (name, mount).
    async fn agent(&self, tenant: &TenantId, volumes: &[(&str, &str)]) -> Agent {
        let mut yaml = String::from(
            "apiVersion: aegis.ai/v1\nkind: Agent\nmetadata:\n  name: route-agent\n  \
             version: \"1.0.0\"\nspec:\n  runtime:\n    language: python\n    \
             version: \"3.11\"\n    model: smart\n  volumes:\n",
        );
        if volumes.is_empty() {
            yaml = yaml.replace("  volumes:\n", "  volumes: []\n");
        }
        for (name, mount) in volumes {
            yaml.push_str(&format!(
                "    - name: {name}\n      storage_class: ephemeral\n      \
                 mount_path: {mount}\n      access_mode: read-write\n      size_limit: 1Gi\n"
            ));
        }
        let manifest: AgentManifest = serde_yaml::from_str(&yaml).unwrap();
        let agent = Agent::new(manifest);
        self.agents.save_for_tenant(tenant, &agent).await.unwrap();
        agent
    }

    /// A completed execution of `agent` in `tenant` with `input`, whose last
    /// iteration produced `produced`.
    async fn execution(
        &self,
        tenant: &TenantId,
        agent: &Agent,
        input: ExecutionInput,
        produced: Vec<ProducedFile>,
    ) -> ExecutionId {
        let mut exec = Execution::new(agent.id, input, 3, "ctx".to_string());
        exec.tenant_id = tenant.clone();
        exec.start();
        exec.start_iteration("act".to_string()).unwrap();
        exec.complete_iteration("done".to_string());
        if !produced.is_empty() {
            exec.store_produced_files(1, produced).unwrap();
        }
        exec.complete();
        self.executions
            .save_for_tenant(tenant, &exec)
            .await
            .unwrap();
        exec.id
    }
}

fn input(workspace: Option<VolumeId>, workflow_execution_id: Option<uuid::Uuid>) -> ExecutionInput {
    ExecutionInput {
        intent: Some("make a pdf".to_string()),
        input: serde_json::json!({}),
        workspace_volume_id: workspace,
        workspace_volume_mount_path: Some(PathBuf::from("/workspace")),
        workspace_remote_path: None,
        workflow_execution_id,
        attachments: Vec::new(),
    }
}

fn produced(path: &str, size: u64, volume: VolumeId, in_volume: &str) -> ProducedFile {
    ProducedFile {
        path: path.to_string(),
        size_bytes: size,
        content_type: "application/pdf".to_string(),
        volume_id: Some(volume),
        path_in_volume: Some(in_volume.to_string()),
    }
}

fn describe(
    result: &Result<
        aegis_orchestrator_core::application::file_operations_service::FileContent,
        FileOperationsError,
    >,
) -> String {
    match result {
        Ok(content) => format!(
            "Ok({:?}, size_bytes {}, {})",
            String::from_utf8_lossy(&content.data),
            content.size_bytes,
            content.content_type
        ),
        Err(error) => format!("Err({error:?})"),
    }
}

/// (a) A step of a workflow wrote `/workspace/report.pdf` on the workflow's
/// workspace volume, owned by the workflow execution: the route reads its
/// bytes there, with the stat's size for `Content-Length`.
#[tokio::test]
async fn a_produced_file_on_a_workflows_workspace_volume_is_read_whole() {
    let w = World::new();
    let own = tenant("route-owner");
    let workflow_execution_id = uuid::Uuid::new_v4();
    let workspace = w
        .volume(
            "workspace",
            &own,
            VolumeOwnership::workflow(workflow_execution_id),
        )
        .await;
    let bytes = b"%PDF-1.7 the report";
    w.put(workspace, "/report.pdf", bytes);
    let agent = w.agent(&own, &[]).await;
    let execution = w
        .execution(
            &own,
            &agent,
            input(Some(workspace), Some(workflow_execution_id)),
            vec![produced(
                "/workspace/report.pdf",
                bytes.len() as u64,
                workspace,
                "/report.pdf",
            )],
        )
        .await;

    let result = w
        .service
        .read_file_for_execution(execution, &own, "report.pdf")
        .await;
    println!("(a) workflow workspace volume: {}", describe(&result));
    let content =
        result.expect("I8 (a): the produced file on the workflow's workspace was not read");
    assert_eq!(content.data, bytes, "I8 (a): not the file's bytes");
    assert_eq!(
        content.size_bytes,
        bytes.len() as u64,
        "I8 (a): not the stat's size"
    );
    assert_eq!(content.content_type, "application/pdf");
}

/// (b) An execution mounts two volumes; the produced file is on the one at
/// `/workspace/data`, created second: the route reads it there.
#[tokio::test]
async fn a_produced_file_on_a_manifest_volume_is_read_from_that_volume() {
    let w = World::new();
    let own = tenant("route-owner");
    let execution_id = uuid::Uuid::new_v4();
    let ownership = || VolumeOwnership::execution(ExecutionId(execution_id));
    let first = w.volume("workspace", &own, ownership()).await;
    let data = w.volume("data", &own, ownership()).await;
    w.put(first, "/data/out.pdf", b"the wrong file");
    w.put(data, "/out.pdf", b"%PDF-1.7 the data volume's file");
    let agent = w
        .agent(
            &own,
            &[("workspace", "/workspace"), ("data", "/workspace/data")],
        )
        .await;
    let mut exec = Execution::new(agent.id, input(None, None), 3, "ctx".to_string());
    exec.id = ExecutionId(execution_id);
    exec.tenant_id = own.clone();
    exec.start();
    exec.start_iteration("act".to_string()).unwrap();
    exec.complete_iteration("done".to_string());
    exec.store_produced_files(
        1,
        vec![produced("/workspace/data/out.pdf", 31, data, "/out.pdf")],
    )
    .unwrap();
    exec.complete();
    w.executions.save_for_tenant(&own, &exec).await.unwrap();

    let result = w
        .service
        .read_file_for_execution(ExecutionId(execution_id), &own, "data/out.pdf")
        .await;
    println!(
        "(b) manifest volume at /workspace/data: {}",
        describe(&result)
    );
    let content = result.expect("I8 (b): the produced file on the data volume was not read");
    assert_eq!(
        content.data, b"%PDF-1.7 the data volume's file",
        "I8 (b): read from a volume other than the one the file was recorded on"
    );
}

/// (c) The record names a volume of another tenant: answered exactly as a
/// produced file whose volume does not exist, never its bytes.
#[tokio::test]
async fn a_produced_file_on_another_tenants_volume_answers_not_found() {
    let w = World::new();
    let own = tenant("route-owner");
    let other = tenant("route-someone-else");
    let execution_id = uuid::Uuid::new_v4();
    let theirs = w
        .volume(
            "workspace",
            &other,
            VolumeOwnership::execution(ExecutionId(execution_id)),
        )
        .await;
    w.put(theirs, "/x.pdf", b"%PDF-1.7 theirs");
    let agent = w.agent(&own, &[]).await;
    let mut exec = Execution::new(agent.id, input(None, None), 3, "ctx".to_string());
    exec.id = ExecutionId(execution_id);
    exec.tenant_id = own.clone();
    exec.start();
    exec.start_iteration("act".to_string()).unwrap();
    exec.complete_iteration("done".to_string());
    exec.store_produced_files(1, vec![produced("/workspace/x.pdf", 15, theirs, "/x.pdf")])
        .unwrap();
    exec.complete();
    w.executions.save_for_tenant(&own, &exec).await.unwrap();

    let foreign = w
        .service
        .read_file_for_execution(ExecutionId(execution_id), &own, "x.pdf")
        .await;
    println!("(c) another tenant's volume: {}", describe(&foreign));

    // The same record naming a volume that does not exist.
    let mut missing_exec = exec.clone();
    missing_exec.id = ExecutionId(uuid::Uuid::new_v4());
    missing_exec.iterations.last_mut().unwrap().produced_files =
        vec![produced("/workspace/x.pdf", 15, VolumeId::new(), "/x.pdf")];
    w.executions
        .save_for_tenant(&own, &missing_exec)
        .await
        .unwrap();
    let missing = w
        .service
        .read_file_for_execution(missing_exec.id, &own, "x.pdf")
        .await;
    println!("(c) a volume that does not exist: {}", describe(&missing));

    let foreign_text = match &foreign {
        Err(FileOperationsError::NotFound(message)) => {
            message.replace(&execution_id.to_string(), "<id>")
        }
        other => panic!(
            "I8 (c): another tenant's volume was not answered as not found: {}",
            describe(other)
        ),
    };
    let missing_text = match &missing {
        Err(FileOperationsError::NotFound(message)) => {
            message.replace(&missing_exec.id.0.to_string(), "<id>")
        }
        other => panic!(
            "I8 (c): a missing volume was not answered as not found: {}",
            describe(other)
        ),
    };
    assert_eq!(foreign_text, missing_text, "I8 (c): the two answers differ");
}

/// (d) Another tenant's execution is answered as one that does not exist,
/// with the same text.
#[tokio::test]
async fn another_tenants_execution_answers_as_a_missing_one() {
    let w = World::new();
    let own = tenant("route-owner");
    let other = tenant("route-someone-else");
    let execution_id = uuid::Uuid::new_v4();
    let theirs = w
        .volume(
            "workspace",
            &other,
            VolumeOwnership::execution(ExecutionId(execution_id)),
        )
        .await;
    w.put(theirs, "/output.md", b"theirs");
    let agent = w.agent(&other, &[("workspace", "/workspace")]).await;
    let mut exec = Execution::new(agent.id, input(None, None), 3, "ctx".to_string());
    exec.id = ExecutionId(execution_id);
    exec.tenant_id = other.clone();
    exec.start();
    exec.complete();
    w.executions.save_for_tenant(&other, &exec).await.unwrap();

    let foreign = w
        .service
        .read_file_for_execution(ExecutionId(execution_id), &own, "output.md")
        .await;
    let missing_id = uuid::Uuid::new_v4();
    let missing = w
        .service
        .read_file_for_execution(ExecutionId(missing_id), &own, "output.md")
        .await;
    println!(
        "(d) another tenant's execution: {}; no such execution: {}",
        describe(&foreign),
        describe(&missing)
    );
    match (&foreign, &missing) {
        (Err(FileOperationsError::NotFound(a)), Err(FileOperationsError::NotFound(b))) => {
            assert_eq!(
                a.replace(&execution_id.to_string(), "<id>"),
                b.replace(&missing_id.to_string(), "<id>"),
                "I8 (d): the two answers differ"
            )
        }
        _ => panic!(
            "I8 (d): not both answered as not found: {} / {}",
            describe(&foreign),
            describe(&missing)
        ),
    }
}

/// (e) A path the record does not list is read from the volume mounted at
/// `/workspace`: for a standalone execution, its volume the agent's manifest
/// mounts there, even beside another volume of the execution (sixteen fresh
/// stores, whose listing order is unordered, so a first-found read cannot
/// pass them all); for a workflow step, the workflow's workspace volume.
#[tokio::test]
async fn a_plain_path_reads_the_volume_mounted_at_workspace_not_the_first_found() {
    let mut complaints: Vec<String> = Vec::new();
    let mut answers: Vec<String> = Vec::new();
    for _ in 0..16 {
        let w = World::new();
        let own = tenant("route-owner");
        let execution_id = uuid::Uuid::new_v4();
        let ownership = || VolumeOwnership::execution(ExecutionId(execution_id));
        let scratch = w.volume("scratch", &own, ownership()).await;
        let workspace = w.volume("workspace", &own, ownership()).await;
        w.put(scratch, "/notes.md", b"the scratch volume");
        w.put(workspace, "/notes.md", b"the workspace volume");
        let agent = w
            .agent(
                &own,
                &[
                    ("scratch", "/workspace/scratch"),
                    ("workspace", "/workspace"),
                ],
            )
            .await;
        let mut exec = Execution::new(agent.id, input(None, None), 3, "ctx".to_string());
        exec.id = ExecutionId(execution_id);
        exec.tenant_id = own.clone();
        exec.start();
        exec.complete();
        w.executions.save_for_tenant(&own, &exec).await.unwrap();

        let result = w
            .service
            .read_file_for_execution(ExecutionId(execution_id), &own, "notes.md")
            .await;
        let answer = describe(&result);
        if !matches!(&result, Ok(content) if content.data == b"the workspace volume") {
            complaints.push(format!(
                "I8 (e): a standalone execution's plain path was not read from the volume \
                 mounted at /workspace: {answer}"
            ));
        }
        answers.push(answer);
    }
    answers.dedup();
    println!("(e) a plain path, sixteen stores: {answers:?}");

    // A workflow step: the workflow's workspace, not the step's own volume.
    let w = World::new();
    let own = tenant("route-owner");
    let workflow_execution_id = uuid::Uuid::new_v4();
    let workflow_workspace = w
        .volume(
            "workspace",
            &own,
            VolumeOwnership::workflow(workflow_execution_id),
        )
        .await;
    w.put(workflow_workspace, "/notes.md", b"the workflow's workspace");
    let execution_id = uuid::Uuid::new_v4();
    let own_volume = w
        .volume(
            "scratch",
            &own,
            VolumeOwnership::execution(ExecutionId(execution_id)),
        )
        .await;
    w.put(own_volume, "/notes.md", b"the step's scratch volume");
    let agent = w.agent(&own, &[("scratch", "/workspace/scratch")]).await;
    let mut exec = Execution::new(
        agent.id,
        input(Some(workflow_workspace), Some(workflow_execution_id)),
        3,
        "ctx".to_string(),
    );
    exec.id = ExecutionId(execution_id);
    exec.tenant_id = own.clone();
    exec.start();
    exec.complete();
    w.executions.save_for_tenant(&own, &exec).await.unwrap();
    let result = w
        .service
        .read_file_for_execution(ExecutionId(execution_id), &own, "notes.md")
        .await;
    println!("(e) a workflow step's plain path: {}", describe(&result));
    if !matches!(&result, Ok(content) if content.data == b"the workflow's workspace") {
        complaints.push(format!(
            "I8 (e): a workflow step's plain path was not read from the workflow's workspace: {}",
            describe(&result)
        ));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}
