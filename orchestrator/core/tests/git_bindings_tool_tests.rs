// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # `aegis.git.list`: a person's git repository bindings, as a tool
//!
//! The tool answers what `GET /v1/storage/git` answers: the caller's git
//! repository bindings, redacted, read through the same `GitRepoService`
//! (`list_bindings`). A person starting a workflow on a repository reads
//! the binding's id here and passes it as `aegis.workflow.run`'s
//! `repositories`, which that tool's schema now lists.
//!
//! | Scenario | Test |
//! |---|---|
//! | The caller's bindings, and no one else's | `aegis_git_list_answers_the_callers_bindings_and_not_another_persons` |
//! | Listed, with no arguments | `aegis_git_list_is_advertised_and_takes_no_arguments` |
//! | The run tool lists `repositories` | `aegis_workflow_run_advertises_repositories` |

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::git_clone_executor::GitCloneExecutor;
use aegis_orchestrator_core::application::git_repo_service::GitRepoService;
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest};
use aegis_orchestrator_core::domain::events::ExecutionEvent;
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository,
};
use aegis_orchestrator_core::domain::mcp::ToolInputContract;
use aegis_orchestrator_core::domain::repository::{
    AgentVersion, RepositoryError, VolumeRepository,
};
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::seal_session::{
    EnvelopeVerifier, SealSession, SealSessionError,
};
use aegis_orchestrator_core::domain::seal_session_repository::SealSessionRepository;
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::security_context::capability::Capability;
use aegis_orchestrator_core::domain::security_context::repository::SecurityContextRepository;
use aegis_orchestrator_core::domain::security_context::{SecurityContext, SecurityContextMetadata};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;

const PERSON: &str = "bindings-person";
const OTHER: &str = "bindings-other-person";
const TOOL: &str = "aegis.git.list";

// ===========================================================================
// Test doubles
// ===========================================================================

/// Bindings kept by tenant, as the Postgres repository keeps them.
#[derive(Default)]
struct Bindings {
    bindings: RwLock<HashMap<GitRepoBindingId, GitRepoBinding>>,
}

#[async_trait]
impl GitRepoBindingRepository for Bindings {
    async fn save(&self, binding: &GitRepoBinding) -> Result<(), RepositoryError> {
        self.bindings
            .write()
            .unwrap()
            .insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &GitRepoBindingId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self.bindings.read().unwrap().get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<Vec<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .filter(|b| &b.tenant_id == tenant_id)
            .cloned()
            .collect())
    }
    async fn find_by_volume_id(
        &self,
        volume_id: &VolumeId,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(self
            .bindings
            .read()
            .unwrap()
            .values()
            .find(|b| &b.volume_id == volume_id)
            .cloned())
    }
    async fn find_by_webhook_lookup_hash(
        &self,
        _hash: &str,
    ) -> Result<Option<GitRepoBinding>, RepositoryError> {
        Ok(None)
    }
    async fn count_by_owner(
        &self,
        _tenant_id: &TenantId,
        _owner: &str,
    ) -> Result<u32, RepositoryError> {
        Ok(0)
    }
    async fn delete(&self, id: &GitRepoBindingId) -> Result<(), RepositoryError> {
        self.bindings.write().unwrap().remove(id);
        Ok(())
    }
}

struct UnusedVolumeService;

#[async_trait]
impl VolumeService for UnusedVolumeService {
    async fn create_volume(
        &self,
        _name: String,
        _tenant_id: TenantId,
        _storage_class: StorageClass,
        _size_limit_mb: u64,
        _ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        unreachable!("these tests store their volumes themselves")
    }
    async fn get_volume(&self, _id: VolumeId) -> anyhow::Result<Volume> {
        unreachable!()
    }
    async fn list_volumes_by_tenant(&self, _tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        unreachable!()
    }
    async fn list_volumes_by_ownership(
        &self,
        _ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        unreachable!()
    }
    async fn attach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
        _mount_point: PathBuf,
        _access_mode: AccessMode,
    ) -> anyhow::Result<VolumeMount> {
        unreachable!()
    }
    async fn detach_volume(
        &self,
        _volume_id: VolumeId,
        _instance_id: InstanceId,
    ) -> anyhow::Result<()> {
        unreachable!()
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

struct NoEvents;

#[async_trait]
impl EventPublisher for NoEvents {
    async fn publish_storage_event(
        &self,
        _e: aegis_orchestrator_core::domain::events::StorageEvent,
    ) {
    }
}

/// No agent execution exists: the call is a person's, outside any run.
struct NoExecutions;

#[async_trait]
impl ExecutionService for NoExecutions {
    async fn start_execution(
        &self,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<ExecutionId> {
        anyhow::bail!("not exercised")
    }
    async fn start_execution_with_id(
        &self,
        execution_id: ExecutionId,
        _: AgentId,
        _: ExecutionInput,
        _: String,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
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
    async fn get_execution_unscoped(&self, _id: ExecutionId) -> Result<Execution> {
        anyhow::bail!("execution not found")
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

/// No agent exists: the call is a person's.
struct NoAgents;

#[async_trait]
impl AgentLifecycleService for NoAgents {
    async fn deploy_agent_for_tenant(
        &self,
        _: &TenantId,
        _: AgentManifest,
        _: bool,
        _: aegis_orchestrator_core::domain::agent::AgentScope,
        _: Option<&aegis_orchestrator_core::domain::iam::UserIdentity>,
    ) -> Result<AgentId> {
        anyhow::bail!("not exercised")
    }
    async fn get_agent_for_tenant(&self, _: &TenantId, _: AgentId) -> Result<Agent> {
        anyhow::bail!("no agent")
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
        Ok(vec![])
    }
    async fn lookup_agent_for_tenant(&self, _: &TenantId, _: &str) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_visible_for_tenant(
        &self,
        _: &TenantId,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn lookup_agent_for_tenant_with_version(
        &self,
        _: &TenantId,
        _: &str,
        _: &str,
    ) -> Result<Option<AgentId>> {
        Ok(None)
    }
    async fn list_agents_visible_for_tenant(&self, _: &TenantId) -> Result<Vec<Agent>> {
        Ok(vec![])
    }
    async fn list_versions_for_tenant(
        &self,
        _: &TenantId,
        _: AgentId,
    ) -> Result<Vec<AgentVersion>> {
        Ok(vec![])
    }
}

/// A call as `POST /v1/seal/invoke` receives it: the session's token, the
/// tool, its arguments.
struct Envelope {
    token: SensitiveString,
    tool: String,
    arguments: Value,
}

impl EnvelopeVerifier for Envelope {
    fn security_token(&self) -> &SensitiveString {
        &self.token
    }
    fn verify_signature(&self, _: &[u8]) -> Result<(), SealSessionError> {
        Ok(())
    }
    fn extract_tool_name(&self) -> Option<String> {
        Some(self.tool.clone())
    }
    fn extract_arguments(&self) -> Option<Value> {
        Some(self.arguments.clone())
    }
    fn replay_nonce(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

// ===========================================================================
// The fixture
// ===========================================================================

fn context() -> SecurityContext {
    SecurityContext {
        name: "zaru-pro".to_string(),
        description: "consumer".to_string(),
        capabilities: vec![Capability {
            tool_pattern: TOOL.to_string(),
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

struct Fixture {
    service: ToolInvocationService,
    bindings: Arc<Bindings>,
    volumes: Arc<InMemoryVolumeRepository>,
    token: String,
    _dirs: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let event_bus = Arc::new(EventBus::new(64));
    let volumes = Arc::new(InMemoryVolumeRepository::new());
    let user_volume_service = Arc::new(UserVolumeService::new(
        volumes.clone() as Arc<dyn VolumeRepository>,
        Arc::new(UnusedVolumeService) as Arc<dyn VolumeService>,
        event_bus.clone(),
        aegis_orchestrator_core::domain::volume::StorageTierLimits::default(),
    ));
    let secrets_manager = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        event_bus.clone(),
    ));
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(
            aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                dirs.path().join("fsal"),
            )
            .unwrap(),
        ),
        volumes.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let clone_executor = Arc::new(GitCloneExecutor::new(
        secrets_manager.clone(),
        fsal.clone(),
        None,
    ));
    let bindings = Arc::new(Bindings::default());
    let git_repos = Arc::new(GitRepoService::new(
        bindings.clone() as Arc<dyn GitRepoBindingRepository>,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus.clone(),
    ));

    let sessions = Arc::new(InMemorySealSessionRepository::new());
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(context()).await.unwrap();
    let token = format!("token-{}", uuid::Uuid::new_v4());
    let session = SealSession::new(
        AgentId::new(),
        ExecutionId::new(),
        vec![],
        token.clone(),
        context(),
        TenantId::for_consumer_user(PERSON).unwrap(),
    )
    .with_principal_metadata(
        Some(PERSON.to_string()),
        Some(PERSON.to_string()),
        None,
        None,
    );
    sessions.save(session).await.unwrap();

    let service = ToolInvocationService::new(
        sessions,
        contexts,
        Arc::new(SealMiddleware::new()),
        Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
        fsal,
        NfsVolumeRegistry::new(),
        Arc::new(NoAgents),
        Arc::new(NoExecutions),
        Arc::new(
            aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(
            ),
        ),
        event_bus,
        None,
    )
    .with_git_repo_service(git_repos);
    Fixture {
        service,
        bindings,
        volumes,
        token,
        _dirs: dirs,
    }
}

impl Fixture {
    /// A binding of `owner`'s in `tenant`, on a volume `owner` owns.
    async fn binding(&self, tenant: &TenantId, owner: &str, label: &str) -> GitRepoBinding {
        let volume = Volume::new(
            format!("git-{label}"),
            tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath {
                path: PathBuf::from(format!("/nonexistent/{label}")),
            },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(owner),
        )
        .unwrap();
        self.volumes.save(&volume).await.unwrap();
        let binding = GitRepoBinding::new(
            tenant.clone(),
            None,
            format!(
                "https://Mk9-bindings-user:Mk9-bindings-token@git.example.invalid/o/{label}.git"
            ),
            GitRef::Branch("main".to_string()),
            None,
            volume.id,
            label.to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
            None,
            None,
        );
        self.bindings.save(&binding).await.unwrap();
        binding
    }

    async fn call(&self, tool: &str, arguments: Value) -> Result<Value, SealSessionError> {
        self.service
            .invoke_tool(&Envelope {
                token: self.token.clone().into(),
                tool: tool.to_string(),
                arguments,
            })
            .await
    }
}

/// The answer's list of bindings, wherever the invoke result carries it.
fn listed(answer: &Value) -> Vec<Value> {
    if let Some(items) = answer.as_array() {
        return items.clone();
    }
    for key in ["result", "content", "data"] {
        if let Some(items) = answer.get(key).and_then(Value::as_array) {
            return items.clone();
        }
    }
    panic!("the answer holds no list of bindings: {answer}");
}

// ===========================================================================
// The tests
// ===========================================================================

/// The tool answers the caller's own bindings, each with its id,
/// repository URL, ref, state and label, as the route lists them; never a
/// binding of another tenant's, nor one on a volume of another person's in
/// the caller's tenant; and no credential written into a stored URL.
#[tokio::test]
async fn aegis_git_list_answers_the_callers_bindings_and_not_another_persons() {
    let fixture = fixture().await;
    let mine = TenantId::for_consumer_user(PERSON).unwrap();
    let theirs = TenantId::for_consumer_user(OTHER).unwrap();
    let own = fixture.binding(&mine, PERSON, "own-repo").await;
    fixture.binding(&theirs, OTHER, "other-tenant-repo").await;
    fixture.binding(&mine, OTHER, "other-person-repo").await;

    let answer = fixture
        .call(TOOL, json!({}))
        .await
        .unwrap_or_else(|e| panic!("{TOOL} with no arguments was refused: {e}"));
    let items = listed(&answer);
    let labels: Vec<&str> = items.iter().filter_map(|b| b["label"].as_str()).collect();
    assert_eq!(
        labels,
        vec!["own-repo"],
        "{TOOL} answered other than the caller's one binding: {answer}"
    );
    let item = &items[0];
    assert_eq!(item["id"], json!(own.id.0), "{item}");
    assert_eq!(item["git_ref"], json!({"Branch": "main"}), "{item}");
    assert!(item.get("status").is_some(), "no state: {item}");
    assert!(
        item["repo_url"]
            .as_str()
            .is_some_and(|u| u.contains("git.example.invalid/o/own-repo.git")),
        "no repository URL: {item}"
    );
    assert!(
        !answer.to_string().contains("Mk9-bindings"),
        "the answer carries a credential from the stored URL: {answer}"
    );
}

/// The catalogue lists `aegis.git.list` with a schema of no arguments, and
/// the input contract requires none.
#[tokio::test]
async fn aegis_git_list_is_advertised_and_takes_no_arguments() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let tool = tools
        .iter()
        .find(|t| t.name == TOOL)
        .unwrap_or_else(|| panic!("the catalogue does not list {TOOL}"));
    assert_eq!(
        tool.input_schema,
        json!({"type": "object", "properties": {}, "required": []}),
        "{TOOL}'s schema is not one of no arguments"
    );
    assert!(
        tool.description.contains("git repository bindings"),
        "{TOOL}'s description: {}",
        tool.description
    );
    assert!(
        ToolInputContract::required_fields(TOOL).is_empty(),
        "{TOOL} requires {:?}",
        ToolInputContract::required_fields(TOOL)
    );
}

/// `aegis.workflow.run`'s advertised schema lists `repositories`: an array
/// of `{binding_id, branch?}`, as the orchestrator parses it.
#[tokio::test]
async fn aegis_workflow_run_advertises_repositories() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    let tools = router.list_tools().await.unwrap();
    let run = tools
        .iter()
        .find(|t| t.name == "aegis.workflow.run")
        .expect("the catalogue lists aegis.workflow.run");
    let repositories = &run.input_schema["properties"]["repositories"];
    assert_eq!(
        repositories["type"], "array",
        "aegis.workflow.run's schema does not list repositories: {}",
        run.input_schema
    );
    let item = &repositories["items"];
    assert_eq!(item["type"], "object", "{repositories}");
    assert_eq!(item["required"], json!(["binding_id"]), "{repositories}");
    assert_eq!(
        item["properties"]["binding_id"]["type"], "string",
        "{repositories}"
    );
    assert_eq!(
        item["properties"]["branch"]["type"], "string",
        "{repositories}"
    );
    assert_eq!(item["additionalProperties"], false, "{repositories}");
    assert_eq!(
        run.input_schema["required"],
        json!(["name"]),
        "repositories is optional"
    );
}
