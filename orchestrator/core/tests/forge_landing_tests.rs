// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A workflow's landing, end to end in core
//!
//! AEGIS ADR-141 F8: a workflow run's `land` step asks the run's person at
//! the approval gate; on approval the gate runs the stored `aegis.git.land`
//! call through the dispatch path, which lands the work branch on the
//! binding's branch as a fast-forward. The repository is a binding of a
//! SeaweedFS-shaped volume, whose git steps run their own script with the
//! host's shell (`ShellStepRunner`), or of a host directory (libgit2), each
//! cloned from a bare repository on the local disk, so every push really
//! happens. The narrative gives a landing its line.
//!
//! | Scenario | Test |
//! |---|---|
//! | An approved landing's stored call reaches its builtin | `an_approved_landings_stored_call_is_dispatched_to_its_builtin_and_lands` |
//! | The ref fast-forwarded from a volume | `a_landing_from_a_seaweedfs_volume_fast_forwards_the_bindings_branch` |
//! | The ref moved on the remote | `a_landing_from_a_seaweedfs_volume_whose_ref_moved_answers_its_sentence_and_leaves_the_ref` |
//! | The work branch moved on the remote | `a_landing_from_a_seaweedfs_volume_whose_work_branch_moved_lands_nothing` |
//! | A tag binding | `a_landing_from_a_seaweedfs_volume_on_a_tag_is_refused_before_any_push` |
//! | A declined landing | `a_declined_landing_from_a_seaweedfs_volume_pushes_nothing` |
//! | The narrative line | `a_landing_has_its_narrative_line` |
//! | F3's entry fields, read and written back | `an_entrys_label_ref_and_started_from_are_read_and_written_back` |
//! | F3's entry fields, filled at preparation | `preparation_fills_each_entrys_label_ref_and_started_from` |

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::json;

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::correlated_activity_stream::normalize_domain_event;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, EphemeralCliPaths, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{
    GitRepoError, GitRepoService, RepositoryActionAnswer, RunRepositories,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_approval_service::ToolApprovalService;
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
    default_work_branch, parse_run_repositories, CloneStrategy, GitRef, GitRepoBinding,
    GitRepoBindingId, GitRepoBindingRepository, GitRepoStatus,
};
use aegis_orchestrator_core::domain::repository::{
    AgentVersion, RepositoryError, VolumeRepository, WorkflowExecutionRepository,
};
use aegis_orchestrator_core::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner, InstanceId,
};
use aegis_orchestrator_core::domain::secrets::RedactedUrl;
use aegis_orchestrator_core::domain::security_context::{
    Capability, SecurityContext, SecurityContextMetadata, SecurityContextRepository,
};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalDecision, ToolApprovalRequest, ToolApprovalStatus,
};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
};
use aegis_orchestrator_core::domain::workflow::WorkflowExecution;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::postgres_tool_approval::InMemoryToolApprovalRepository;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryVolumeRepository, InMemoryWorkflowExecutionRepository,
};
use aegis_orchestrator_core::infrastructure::seal::middleware::SealMiddleware;
use aegis_orchestrator_core::infrastructure::seal::session_repository::InMemorySealSessionRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use aegis_orchestrator_core::infrastructure::security_context::InMemorySecurityContextRepository;
use aegis_orchestrator_core::infrastructure::tool_router::ToolRouter;
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

const PERSON: &str = "the-person";
/// The context a workflow's landing is stored at the gate under, and the
/// stored call is run under on approval.
const LANDING_CONTEXT: &str = "aegis-system-operator";
const REF_AHEAD: &str =
    "the remote branch 'main' has commits this run does not have; nothing was landed";
const NOT_A_BRANCH: &str = "a run lands only on a branch; 'v1' is not one";
const DECLINED: &str = "approval request denied; nothing was landed";

// ===========================================================================
// The step runner: the step's own script, run by the host's shell
// ===========================================================================

/// Runs a git step's entrypoint and command with the host's shell, its
/// standard input handed over as the container runner hands it, in an
/// environment of its own (no git configuration of the machine running the
/// tests takes part).
struct ShellStepRunner {
    home: tempfile::TempDir,
}

#[async_trait]
impl ContainerStepRunner for ShellStepRunner {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        let mut argv: Vec<String> = config.entrypoint.clone().unwrap_or_default();
        argv.extend(config.command.iter().cloned());
        let home = self.home.path().to_path_buf();
        let env = config.env.clone();
        let stdin = config.stdin.as_ref().map(|b| b.expose().to_vec());
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..])
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .envs(env.iter())
                .stdin(if stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let started = std::time::Instant::now();
            let mut child = cmd
                .spawn()
                .map_err(|e| ContainerStepError::DockerError(format!("spawn: {e}")))?;
            if let Some(bytes) = stdin {
                let mut input = child.stdin.take().unwrap();
                input
                    .write_all(&bytes)
                    .map_err(|e| ContainerStepError::DockerError(format!("stdin: {e}")))?;
            }
            let out = child
                .wait_with_output()
                .map_err(|e| ContainerStepError::DockerError(format!("wait: {e}")))?;
            Ok(ContainerStepResult {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
            })
        })
        .await
        .map_err(|e| ContainerStepError::DockerError(format!("join: {e}")))?
    }
}

// ===========================================================================
// git on the host, for the fixtures
// ===========================================================================

/// Run `git` in `dir` with no configuration of the machine's, and return
/// what it printed.
fn git(dir: &Path, args: &[&str]) -> String {
    let home = dir.join(".git-test-home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("HOME", &home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn is_ancestor(dir: &Path, ancestor: &str, of: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, of])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs")
        .success()
}

/// Every branch and tag of the bare repository, by name.
fn heads(bare: &Path) -> HashMap<String, String> {
    git(bare, &["for-each-ref", "--format=%(refname) %(objectname)"])
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, sha)| (name.to_string(), sha.to_string()))
        .collect()
}

// ===========================================================================
// In-memory ports
// ===========================================================================

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
        unreachable!("these tests store their volume themselves")
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

/// No agent execution exists: the steps under test are the interpreter's.
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

/// No agent exists: the steps under test are the interpreter's.
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

/// The landing's security context, as the deployment defines it: the
/// operator's, admitting every `aegis.git.*` tool.
fn landing_context() -> SecurityContext {
    SecurityContext {
        name: LANDING_CONTEXT.to_string(),
        description: "the landing's context".to_string(),
        capabilities: vec![Capability {
            tool_pattern: "aegis.git.*".to_string(),
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

// ===========================================================================
// The fixture
// ===========================================================================

struct Fixture {
    git_repos: Arc<GitRepoService>,
    service: Arc<ToolInvocationService>,
    approvals: Arc<ToolApprovalService>,
    executions: Arc<InMemoryWorkflowExecutionRepository>,
    bindings: Arc<Bindings>,
    volume_repo: Arc<InMemoryVolumeRepository>,
    events: aegis_orchestrator_core::infrastructure::event_bus::EventReceiver,
    /// Where a git step mounts the one SeaweedFS volume; its tree is `repo`.
    workspace: PathBuf,
    bare: PathBuf,
    seed: PathBuf,
    tenant: TenantId,
    dirs: tempfile::TempDir,
}

/// A workflow run of `PERSON` holding one repository, its work branch
/// checked out.
struct Run {
    id: uuid::Uuid,
    /// The run's `repositories` as its preparation answered them.
    entries: serde_json::Value,
    binding: GitRepoBindingId,
    tree: PathBuf,
    branch: String,
    started_from: String,
}

async fn fixture() -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let bare = dirs.path().join("remote.git");
    let seed = dirs.path().join("seed");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main"]);
    git(&seed, &["init", "--initial-branch=main"]);
    std::fs::write(seed.join("README.md"), "first line\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "first"]);
    git(&seed, &["tag", "v1"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "origin", "main", "v1"]);
    let workspace = dirs.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let scratch = dirs.path().join("scratch").join("aegis-git");
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();

    let event_bus = Arc::new(EventBus::new(256));
    let events = event_bus.subscribe();
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let tenant = TenantId::consumer();
    let user_volume_service = Arc::new(UserVolumeService::new(
        volume_repo.clone() as Arc<dyn VolumeRepository>,
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
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let engine = EphemeralCliEngine::new(
        Arc::new(ShellStepRunner {
            home: tempfile::tempdir().expect("home for the step"),
        }) as Arc<dyn ContainerStepRunner>,
        Arc::new(NfsVolumeRegistry::new()),
    )
    .with_paths(EphemeralCliPaths {
        workspace: workspace.display().to_string(),
        scratch: scratch.display().to_string(),
    });
    let clone_executor = Arc::new(GitCloneExecutor::new(
        secrets_manager.clone(),
        fsal.clone(),
        Some(Arc::new(engine)),
    ));
    let bindings = Arc::new(Bindings::default());
    let git_repos = Arc::new(GitRepoService::new(
        bindings.clone() as Arc<dyn GitRepoBindingRepository>,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus.clone(),
    ));
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let approvals = Arc::new(ToolApprovalService::new(
        Arc::new(InMemoryToolApprovalRepository::new()),
        event_bus.clone(),
    ));
    let contexts = Arc::new(InMemorySecurityContextRepository::new());
    contexts.save(landing_context()).await.unwrap();
    let service = Arc::new(
        ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            contexts,
            Arc::new(SealMiddleware::new()),
            Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers())),
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(NoAgents),
            Arc::new(NoExecutions),
            Arc::new(
                aegis_orchestrator_core::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured(),
            ),
            event_bus,
            None,
        )
        .with_git_repo_service(git_repos.clone())
        .with_workflow_execution_repo(executions.clone())
        .with_tool_approvals(approvals.clone()),
    );
    Fixture {
        git_repos,
        service,
        approvals,
        executions,
        bindings,
        volume_repo,
        events,
        workspace,
        bare,
        seed,
        tenant,
        dirs,
    }
}

impl Fixture {
    /// A workflow run of `PERSON` holding a binding of `git_ref` on a
    /// SeaweedFS volume, cloned by the clone step and its repositories
    /// prepared as a start prepares them (the work branch by its step).
    async fn volume_run(&self, git_ref: GitRef) -> Run {
        let volume = Volume::new(
            "git-app".to_string(),
            self.tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                remote_path: "/aegis/seaweedfs/git-app".to_string(),
            },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(PERSON),
        )
        .unwrap();
        self.volume_repo.save(&volume).await.unwrap();
        let binding = GitRepoBinding::new(
            self.tenant.clone(),
            None,
            format!("file://{}", self.bare.display()),
            git_ref,
            None,
            volume.id,
            "app".to_string(),
            CloneStrategy::EphemeralCli {
                reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
            },
            false,
            None,
            None,
            None,
        );
        let id = binding.id;
        self.bindings.save(&binding).await.unwrap();
        self.git_repos
            .clone_repo(&id)
            .await
            .unwrap_or_else(|e| panic!("the clone step did not clone: {e}"));
        let cloned = self.bindings.find_by_id(&id).await.unwrap().unwrap();
        assert_eq!(cloned.status, GitRepoStatus::Ready);
        self.prepared(id, self.workspace.join("repo")).await
    }

    /// A workflow run of `PERSON` holding a binding of `main` on a host
    /// directory (libgit2), its repositories prepared as a start prepares
    /// them.
    async fn host_run(&self) -> Run {
        let tree = self
            .dirs
            .path()
            .join(format!("host-{}", uuid::Uuid::new_v4()));
        let volume = Volume::new(
            "git-host".to_string(),
            self.tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath { path: tree.clone() },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(PERSON),
        )
        .unwrap();
        self.volume_repo.save(&volume).await.unwrap();
        git(
            self.dirs.path(),
            &[
                "clone",
                "--quiet",
                self.bare.to_str().unwrap(),
                tree.to_str().unwrap(),
            ],
        );
        let mut binding = GitRepoBinding::new(
            self.tenant.clone(),
            None,
            format!("file://{}", self.bare.display()),
            GitRef::Branch("main".to_string()),
            None,
            volume.id,
            "app".to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
            None,
            None,
        );
        binding.complete_clone(git(&self.bare, &["rev-parse", "refs/heads/main"]), 0);
        let id = binding.id;
        self.bindings.save(&binding).await.unwrap();
        self.prepared(id, tree).await
    }

    async fn prepared(&self, binding: GitRepoBindingId, tree: PathBuf) -> Run {
        let run = ExecutionId::new();
        let prepared = self
            .git_repos
            .prepare_for_run(
                &self.tenant,
                Some(PERSON),
                run.0,
                &parse_run_repositories(&json!([{ "binding_id": binding.0.to_string() }])).unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("the run's repository was not prepared: {e}"));
        let workflow = WorkflowParser::parse_yaml(
            r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: forge-like
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
        let entries = serde_json::to_value(&prepared).unwrap();
        let mut execution =
            WorkflowExecution::new(&workflow, run, json!({ "repositories": entries }));
        execution.tenant_id = self.tenant.clone();
        execution.initiating_user_sub = Some(PERSON.to_string());
        self.executions
            .save_for_tenant(&self.tenant, &execution)
            .await
            .unwrap();
        Run {
            id: run.0,
            entries,
            binding,
            started_from: git(&tree, &["rev-parse", "HEAD"]),
            branch: default_work_branch(run.0),
            tree,
        }
    }

    /// A commit of a new file on the run's work branch.
    fn change(&self, run: &Run) -> String {
        assert_eq!(
            git(&run.tree, &["rev-parse", "--abbrev-ref", "HEAD"]),
            run.branch,
            "the tree is not on the run's work branch"
        );
        std::fs::write(run.tree.join("CHANGE.md"), "a change\n").unwrap();
        git(&run.tree, &["add", "CHANGE.md"]);
        git(&run.tree, &["commit", "-m", "the change"]);
        git(&run.tree, &["rev-parse", "HEAD"])
    }

    /// A commit on the remote's `refs/heads/<branch>` the run does not have.
    fn remote_moves(&self, branch: &str) -> String {
        std::fs::write(self.seed.join("OTHER.md"), format!("moved {branch}\n")).unwrap();
        git(&self.seed, &["add", "OTHER.md"]);
        git(&self.seed, &["commit", "-m", "elsewhere"]);
        git(
            &self.seed,
            &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
        git(&self.seed, &["rev-parse", "HEAD"])
    }

    /// The landing step of `run`, started; answered once its person decides.
    fn land(
        &self,
        run: &Run,
    ) -> tokio::task::JoinHandle<
        Result<
            RepositoryActionAnswer,
            aegis_orchestrator_core::application::git_repo_service::RepositoryActionError,
        >,
    > {
        let service = self.service.clone();
        let tenant = self.tenant.clone();
        let id = run.id;
        tokio::spawn(async move {
            service
                .run_repository_action(&tenant, id, "land", None)
                .await
        })
    }

    /// The one pending landing of `PERSON`, waited for.
    async fn pending_landing(&self) -> ToolApprovalRequest {
        for _ in 0..200 {
            let pending = self
                .approvals
                .list_for_user(&self.tenant, PERSON, Some(ToolApprovalStatus::Pending))
                .await
                .unwrap();
            if let Some(request) = pending.into_iter().next() {
                return request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("no landing waited at the gate");
    }

    /// The person's answer to the pending landing, run as the daemon runs
    /// it: the stored call through the tool invocation service's dispatch.
    async fn decide(&self, decision: ToolApprovalDecision) -> ToolApprovalRequest {
        let request = self.pending_landing().await;
        assert_eq!(request.tool_name, "aegis.git.land");
        self.approvals
            .decide(
                request.id,
                &self.tenant,
                PERSON,
                decision,
                self.service.as_ref(),
            )
            .await
            .unwrap_or_else(|e| panic!("the decision was not taken: {e}"))
    }

    fn landed_rows(&mut self) -> Vec<ExecutionEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let DomainEvent::Execution(e @ ExecutionEvent::RepositoryLanded { .. }) = event {
                seen.push(e);
            }
        }
        seen
    }
}

async fn answered(
    landing: tokio::task::JoinHandle<
        Result<
            RepositoryActionAnswer,
            aegis_orchestrator_core::application::git_repo_service::RepositoryActionError,
        >,
    >,
) -> RepositoryActionAnswer {
    tokio::time::timeout(std::time::Duration::from_secs(30), landing)
        .await
        .expect("the landing did not answer after the decision")
        .unwrap()
        .unwrap_or_else(|e| panic!("the landing failed: {e}"))
}

// ===========================================================================
// S2: the approved call reaches its builtin
// ===========================================================================

/// AEGIS ADR-141 F8: the gate runs an approved landing's stored call
/// through the dispatch path, which serves `aegis.git.land` itself; the
/// ref is fast-forwarded and the row published.
#[tokio::test]
async fn an_approved_landings_stored_call_is_dispatched_to_its_builtin_and_lands() {
    let mut fx = fixture().await;
    let run = fx.host_run().await;
    let sha = fx.change(&run);

    let landing = fx.land(&run);
    let decided = fx.decide(ToolApprovalDecision::Once).await;
    assert_eq!(
        decided.error, None,
        "the approved call was not run by its builtin"
    );
    let answer = answered(landing).await;

    assert_eq!(answer.sentence, None, "{answer:?}");
    assert_eq!(answer.commit_sha.as_deref(), Some(sha.as_str()));
    assert_eq!(heads(&fx.bare).get("refs/heads/main"), Some(&sha));
    assert!(is_ancestor(&fx.bare, &run.started_from, "refs/heads/main"));
    assert_eq!(fx.landed_rows().len(), 1, "one landing row");
}

// ===========================================================================
// S3: a landing from a SeaweedFS-shaped volume
// ===========================================================================

#[tokio::test]
async fn a_landing_from_a_seaweedfs_volume_fast_forwards_the_bindings_branch() {
    let mut fx = fixture().await;
    let run = fx.volume_run(GitRef::Branch("main".to_string())).await;
    let sha = fx.change(&run);

    let landing = fx.land(&run);
    fx.decide(ToolApprovalDecision::Once).await;
    let answer = answered(landing).await;

    assert_eq!(answer.sentence, None, "{answer:?}");
    assert_eq!(answer.commit_sha.as_deref(), Some(sha.as_str()));
    assert_eq!(answer.git_ref, "main");
    let after = heads(&fx.bare);
    assert_eq!(
        after.get("refs/heads/main"),
        Some(&sha),
        "main is not the run's commit: {after:?}"
    );
    assert_eq!(
        after.get(&format!("refs/heads/{}", run.branch)),
        Some(&sha),
        "the work branch was not pushed: {after:?}"
    );
    assert!(
        is_ancestor(&fx.bare, &run.started_from, "refs/heads/main"),
        "main is not a fast-forward of the commit the run started from"
    );
    let rows = fx.landed_rows();
    assert_eq!(rows.len(), 1, "one landing row: {rows:?}");
    let ExecutionEvent::RepositoryLanded {
        commit_sha,
        git_ref,
        ..
    } = &rows[0]
    else {
        unreachable!()
    };
    assert_eq!(
        (commit_sha.as_str(), git_ref.as_str()),
        (sha.as_str(), "main")
    );
}

#[tokio::test]
async fn a_landing_from_a_seaweedfs_volume_whose_ref_moved_answers_its_sentence_and_leaves_the_ref()
{
    let fx = fixture().await;
    let run = fx.volume_run(GitRef::Branch("main".to_string())).await;
    let sha = fx.change(&run);
    let moved = fx.remote_moves("main");

    let refused = fx
        .git_repos
        .land_for_run(run.id, &run.binding, &fx.tenant, PERSON, &run.branch)
        .await;
    match refused {
        Err(e @ GitRepoError::RefAhead { .. }) => assert_eq!(e.to_string(), REF_AHEAD),
        other => panic!("a moved ref answered {other:?}"),
    }
    let after = heads(&fx.bare);
    assert_eq!(
        after.get("refs/heads/main"),
        Some(&moved),
        "the moved ref was changed"
    );
    assert_eq!(
        after.get(&format!("refs/heads/{}", run.branch)),
        Some(&sha),
        "the work branch was not pushed before the ref was refused"
    );
}

#[tokio::test]
async fn a_landing_from_a_seaweedfs_volume_whose_work_branch_moved_lands_nothing() {
    let fx = fixture().await;
    let run = fx.volume_run(GitRef::Branch("main".to_string())).await;
    fx.change(&run);
    let main = heads(&fx.bare)["refs/heads/main"].clone();
    let moved = fx.remote_moves(&run.branch);

    let refused = fx
        .git_repos
        .land_for_run(run.id, &run.binding, &fx.tenant, PERSON, &run.branch)
        .await;
    match refused {
        Err(e @ GitRepoError::RemoteAhead { .. }) => assert_eq!(
            e.to_string(),
            format!(
                "the remote branch '{}' has commits this run does not have; nothing was pushed",
                run.branch
            )
        ),
        other => panic!("a moved work branch answered {other:?}"),
    }
    let after = heads(&fx.bare);
    assert_eq!(
        after.get("refs/heads/main"),
        Some(&main),
        "main was changed"
    );
    assert_eq!(
        after.get(&format!("refs/heads/{}", run.branch)),
        Some(&moved),
        "the moved work branch was changed"
    );
}

#[tokio::test]
async fn a_landing_from_a_seaweedfs_volume_on_a_tag_is_refused_before_any_push() {
    let fx = fixture().await;
    let run = fx.volume_run(GitRef::Tag("v1".to_string())).await;
    fx.change(&run);
    let before = heads(&fx.bare);

    let refused = fx
        .git_repos
        .land_for_run(run.id, &run.binding, &fx.tenant, PERSON, &run.branch)
        .await;
    match refused {
        Err(e @ GitRepoError::NotABranch { .. }) => assert_eq!(e.to_string(), NOT_A_BRANCH),
        other => panic!("a tag binding's landing answered {other:?}"),
    }
    let answer = fx
        .service
        .run_repository_action(&fx.tenant, run.id, "land", None)
        .await
        .unwrap_or_else(|e| panic!("the landing step failed: {e}"));
    assert_eq!(answer.sentence.as_deref(), Some(NOT_A_BRANCH));
    assert_eq!(heads(&fx.bare), before, "something was pushed");
    assert!(
        fx.approvals
            .list_for_user(&fx.tenant, PERSON, None)
            .await
            .unwrap()
            .is_empty(),
        "the person was asked about a landing refused before any push"
    );
}

#[tokio::test]
async fn a_declined_landing_from_a_seaweedfs_volume_pushes_nothing() {
    let mut fx = fixture().await;
    let run = fx.volume_run(GitRef::Branch("main".to_string())).await;
    fx.change(&run);
    let before = heads(&fx.bare);

    let landing = fx.land(&run);
    fx.decide(ToolApprovalDecision::Deny).await;
    let answer = answered(landing).await;

    assert_eq!(answer.sentence.as_deref(), Some(DECLINED), "{answer:?}");
    assert_eq!(answer.commit_sha, None);
    assert_eq!(heads(&fx.bare), before, "something was pushed");
    assert!(fx.landed_rows().is_empty(), "a declined landing has a row");
}

// ===========================================================================
// S4: the narrative line
// ===========================================================================

#[test]
fn a_landing_has_its_narrative_line() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let row = normalize_domain_event(
        &DomainEvent::Execution(ExecutionEvent::RepositoryLanded {
            execution_id: ExecutionId::new(),
            label: "app".to_string(),
            branch: "aegis/1234abcd".to_string(),
            git_ref: "main".to_string(),
            commit_sha: sha.to_string(),
            branch_url: RedactedUrl::new("https://github.com/o/r/tree/main"),
            landed_at: chrono::Utc::now(),
        }),
        None,
    );
    assert_eq!(
        row.message,
        format!("Landed {sha} of repository app on main")
    );
}

// ===========================================================================
// F3: the repository's entry carries its label, ref and starting commit
// ===========================================================================

/// AEGIS ADR-141 F3: an entry as the platform stores it, with `label`,
/// `ref` and `started_from`, is read and writes back as it was read.
#[test]
fn an_entrys_label_ref_and_started_from_are_read_and_written_back() {
    let value = json!([{
        "binding_id": "4f6b1c1e-2d3a-4b5c-8d7e-9f0a1b2c3d4e",
        "branch": "aegis/1234abcd",
        "label": "app",
        "ref": "main",
        "started_from": "0123456789abcdef0123456789abcdef01234567"
    }]);
    let entries = parse_run_repositories(&value).unwrap_or_else(|e| {
        panic!("an entry with its label, ref and started_from was refused: {e}")
    });
    assert_eq!(
        serde_json::to_value(&entries).unwrap(),
        value,
        "the entry does not write back as it was read"
    );
}

/// AEGIS ADR-141 F3: preparing a run's repositories fills each entry's
/// `label`, `ref` (the binding's) and `started_from` (the commit the work
/// branch started from), on a volume and on a host directory.
#[tokio::test]
async fn preparation_fills_each_entrys_label_ref_and_started_from() {
    let fx = fixture().await;
    for run in [
        fx.volume_run(GitRef::Branch("main".to_string())).await,
        fx.host_run().await,
    ] {
        let entry = &run.entries[0];
        assert_eq!(
            (&entry["label"], &entry["ref"], &entry["started_from"]),
            (&json!("app"), &json!("main"), &json!(run.started_from)),
            "the prepared entry is {entry}"
        );
    }
}
