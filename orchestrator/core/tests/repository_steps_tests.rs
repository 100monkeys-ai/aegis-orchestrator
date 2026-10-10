// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The workflow interpreter's repository steps
//!
//! AEGIS ADR-141 F5 and F8: a workflow run's own `diff`, `commit` and `land`
//! on the repository it holds, and the landing itself: the work branch
//! pushed, then its HEAD pushed to the binding's branch with no force, after
//! the run's person approved at the gate. The repository is a binding of a
//! host directory (libgit2), cloned from a bare repository on the local
//! disk.
//!
//! | Scenario | Test |
//! |---|---|
//! | The ref fast-forwarded | `a_landing_fast_forwards_the_bindings_branch_to_the_work_branch` |
//! | The ref moved on the remote | `a_landing_whose_ref_moved_answers_its_sentence_and_leaves_the_ref` |
//! | A tag binding | `a_landing_on_a_tag_is_refused_before_any_push` |
//! | No repository | `every_step_of_a_run_holding_no_repository_answers_its_sentence` |
//! | Diff, commit, a clean tree | `diff_and_commit_act_on_the_runs_repository_and_a_clean_tree_answers_its_sentence` |
//! | Land after approval, and the row | `a_landing_waits_for_the_persons_approval_then_lands_and_publishes_its_row` |
//! | A declined landing | `a_declined_landing_pushes_nothing` |
//! | Only the workflow's own step | `the_landing_is_answered_only_for_a_workflow_execution_of_its_person` |
//! | The catalogue's mark | `aegis_git_land_is_gated_with_no_summary_and_listed_to_no_agent` |
//! | An unknown action | `an_unknown_action_is_refused` |

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::agent::AgentLifecycleService;
use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::application::git_clone_executor::GitCloneExecutor;
use aegis_orchestrator_core::application::git_repo_service::{
    GitRepoError, GitRepoService, RunRepositories,
};
use aegis_orchestrator_core::application::git_repo_service::{
    RepositoryActionAnswer, RepositoryActionError, LAND_IS_THE_WORKFLOWS, RUN_HOLDS_NO_REPOSITORY,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::tool_approval_service::{
    ApprovedCallRunner, ToolApprovalService,
};
use aegis_orchestrator_core::application::tool_invocation_service::ToolInvocationService;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::agent::{Agent, AgentId, AgentManifest};
use aegis_orchestrator_core::domain::events::{repository_landed_line, ExecutionEvent};
use aegis_orchestrator_core::domain::execution::{
    Execution, ExecutionId, ExecutionInput, Iteration,
};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    default_work_branch, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository, RunRepository,
};
use aegis_orchestrator_core::domain::repository::{
    AgentVersion, RepositoryError, VolumeRepository, WorkflowExecutionRepository,
};
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::seal_session::SealSessionError;
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::tool_approval::{
    ToolApprovalDecision, ToolApprovalRequest, ToolApprovalStatus,
};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
};
use aegis_orchestrator_core::domain::workflow::WorkflowExecution;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus, EventReceiver};
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
const REF_AHEAD: &str =
    "the remote branch 'main' has commits this run does not have; nothing was landed";
const CLEAN: &str = "nothing to commit: working tree is clean";

// ===========================================================================
// git on the host
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

/// Whether `ancestor` is an ancestor of `of` in `dir`.
fn is_ancestor(dir: &Path, ancestor: &str, of: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, of])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs")
        .success()
}

/// The bare repository's branches and their commits.
fn heads(bare: &Path) -> HashMap<String, String> {
    git(
        bare,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
        ],
    )
    .lines()
    .filter_map(|l| l.split_once(' '))
    .map(|(r, s)| (r.to_string(), s.to_string()))
    .collect()
}

// ===========================================================================
// Test doubles
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

/// Runs an approved landing as the dispatch path's stored-call run reaches
/// `aegis.git.land`: in the request's execution, as the request's user.
struct Lands(Arc<ToolInvocationService>);

#[async_trait]
impl ApprovedCallRunner for Lands {
    async fn run_approved_call(&self, request: &ToolApprovalRequest) -> Result<Value, String> {
        assert_eq!(request.tool_name, "aegis.git.land");
        self.0
            .land_for_interpreter(&request.tenant_id, request.execution_id, &request.user_sub)
            .await
            .map_err(|e| e.to_string())
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
    events: EventReceiver,
    bare: PathBuf,
    seed: PathBuf,
    tenant: TenantId,
    dirs: tempfile::TempDir,
}

/// A workflow run holding one repository, its work branch checked out.
struct Run {
    id: uuid::Uuid,
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
    let storage_root = dirs.path().join("fsal");
    let fsal = Arc::new(AegisFSAL::new(
        Arc::new(
            aegis_orchestrator_core::infrastructure::storage::LocalHostStorageProvider::new(
                &storage_root,
            )
            .unwrap(),
        ),
        volume_repo.clone() as Arc<dyn VolumeRepository>,
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
    let executions = Arc::new(InMemoryWorkflowExecutionRepository::new());
    let approvals = Arc::new(ToolApprovalService::new(
        Arc::new(InMemoryToolApprovalRepository::new()),
        event_bus.clone(),
    ));
    let service = Arc::new(
        ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            Arc::new(InMemorySecurityContextRepository::new()),
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
        bare,
        seed,
        tenant,
        dirs,
    }
}

impl Fixture {
    /// A workflow run of `PERSON` holding a binding of `git_ref` on a host
    /// directory, its repositories prepared as a start prepares them.
    async fn run(&self, git_ref: GitRef) -> Run {
        let tree = self
            .dirs
            .path()
            .join(format!("host-{}", uuid::Uuid::new_v4()));
        let volume = Volume::new(
            "git-app".to_string(),
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
            git_ref,
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
        self.bindings.save(&binding).await.unwrap();

        let run = ExecutionId::new();
        let prepared = self
            .git_repos
            .prepare_for_run(
                &self.tenant,
                Some(PERSON),
                run.0,
                &[RunRepository {
                    binding_id: binding.id,
                    branch: None,
                    author: None,
                    label: None,
                    git_ref: None,
                    started_from: None,
                }],
            )
            .await
            .unwrap_or_else(|e| panic!("the run's repository was not prepared: {e}"));
        self.save_run(run, json!({ "repositories": prepared }))
            .await;
        Run {
            id: run.0,
            started_from: git(&tree, &["rev-parse", "HEAD"]),
            tree,
            branch: default_work_branch(run.0),
        }
    }

    /// A workflow execution of `PERSON` with `input`.
    async fn save_run(&self, run: ExecutionId, input: Value) {
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
        let mut execution = WorkflowExecution::new(&workflow, run, input);
        execution.tenant_id = self.tenant.clone();
        execution.initiating_user_sub = Some(PERSON.to_string());
        self.executions
            .save_for_tenant(&self.tenant, &execution)
            .await
            .unwrap();
    }

    /// A commit of a new file on the run's work branch.
    fn change(&self, run: &Run) -> String {
        std::fs::write(run.tree.join("CHANGE.md"), "a change\n").unwrap();
        git(&run.tree, &["add", "CHANGE.md"]);
        git(&run.tree, &["commit", "-m", "the change"]);
        git(&run.tree, &["rev-parse", "HEAD"])
    }

    /// A commit on the remote's `main` the run does not have.
    fn remote_moves_main(&self) -> String {
        std::fs::write(self.seed.join("OTHER.md"), "another change\n").unwrap();
        git(&self.seed, &["add", "OTHER.md"]);
        git(&self.seed, &["commit", "-m", "elsewhere"]);
        git(&self.seed, &["push", "origin", "main"]);
        git(&self.seed, &["rev-parse", "HEAD"])
    }

    async fn step(
        &self,
        run: uuid::Uuid,
        action: &str,
        message: Option<&str>,
    ) -> RepositoryActionAnswer {
        self.service
            .run_repository_action(&self.tenant, run, action, message)
            .await
            .unwrap_or_else(|e| panic!("the step `{action}` failed: {e}"))
    }

    /// The one pending landing of `PERSON`, waited for.
    async fn pending_landing(&self) -> ToolApprovalRequest {
        for _ in 0..100 {
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

    fn published(&mut self) -> Vec<ExecutionEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            if let DomainEvent::Execution(e) = event {
                seen.push(e);
            }
        }
        seen
    }
}

// ===========================================================================
// The landing in the git repository service
// ===========================================================================

#[tokio::test]
async fn a_landing_fast_forwards_the_bindings_branch_to_the_work_branch() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    let binding = fx
        .bindings
        .bindings
        .read()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .id;
    let sha = fx.change(&run);

    let landed = fx
        .git_repos
        .land_for_run(run.id, &binding, &fx.tenant, PERSON, &run.branch)
        .await
        .unwrap_or_else(|e| panic!("the landing was refused: {e}"));

    assert_eq!(landed.commit_sha, sha);
    assert_eq!(landed.git_ref, "main");
    assert_eq!(landed.branch, run.branch);
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
}

#[tokio::test]
async fn a_landing_whose_ref_moved_answers_its_sentence_and_leaves_the_ref() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    let binding = fx
        .bindings
        .bindings
        .read()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .id;
    fx.change(&run);
    let moved = fx.remote_moves_main();

    let refused = fx
        .git_repos
        .land_for_run(run.id, &binding, &fx.tenant, PERSON, &run.branch)
        .await;
    match refused {
        Err(e @ GitRepoError::RefAhead { .. }) => assert_eq!(e.to_string(), REF_AHEAD),
        other => panic!("a moved ref answered {other:?}"),
    }
    assert_eq!(
        heads(&fx.bare).get("refs/heads/main"),
        Some(&moved),
        "the moved ref was changed"
    );
}

#[tokio::test]
async fn a_landing_on_a_tag_is_refused_before_any_push() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Tag("v1".to_string())).await;
    let binding = fx
        .bindings
        .bindings
        .read()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .id;
    fx.change(&run);
    let before = heads(&fx.bare);

    let refused = fx
        .git_repos
        .land_for_run(run.id, &binding, &fx.tenant, PERSON, &run.branch)
        .await;
    match refused {
        Err(e @ GitRepoError::NotABranch { .. }) => {
            assert_eq!(
                e.to_string(),
                "a run lands only on a branch; 'v1' is not one"
            )
        }
        other => panic!("a tag binding's landing answered {other:?}"),
    }
    assert_eq!(heads(&fx.bare), before, "something was pushed");

    let answered = fx.step(run.id, "land", None).await;
    assert_eq!(
        answered.sentence.as_deref(),
        Some("a run lands only on a branch; 'v1' is not one")
    );
    assert!(
        fx.approvals
            .list_for_user(&fx.tenant, PERSON, None)
            .await
            .unwrap()
            .is_empty(),
        "the person was asked about a landing refused before any push"
    );
}

// ===========================================================================
// The interpreter's steps
// ===========================================================================

#[tokio::test]
async fn every_step_of_a_run_holding_no_repository_answers_its_sentence() {
    let fx = fixture().await;
    let run = ExecutionId::new();
    fx.save_run(run, json!({ "task": "x" })).await;
    for (action, message) in [("diff", None), ("commit", Some("m")), ("land", None)] {
        let answered = fx.step(run.0, action, message).await;
        assert_eq!(
            answered.sentence.as_deref(),
            Some(RUN_HOLDS_NO_REPOSITORY),
            "`{action}` on a run holding no repository answered {answered:?}"
        );
        assert_eq!(answered.commit_sha, None);
    }
}

#[tokio::test]
async fn diff_and_commit_act_on_the_runs_repository_and_a_clean_tree_answers_its_sentence() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    std::fs::write(run.tree.join("README.md"), "first line\nsecond line\n").unwrap();

    let diffed = fx.step(run.id, "diff", None).await;
    assert_eq!(diffed.sentence, None);
    assert_eq!(diffed.branch, run.branch);
    assert_eq!(diffed.git_ref, "main");
    assert!(
        diffed
            .diff
            .as_deref()
            .unwrap_or_default()
            .contains("+second line"),
        "the diff does not show the change: {diffed:?}"
    );

    let committed = fx
        .step(run.id, "commit", Some("the-forge: a second line"))
        .await;
    let sha = committed
        .commit_sha
        .clone()
        .unwrap_or_else(|| panic!("the commit answered no sha: {committed:?}"));
    assert_eq!(git(&run.tree, &["rev-parse", "HEAD"]), sha);
    assert_eq!(
        git(&run.tree, &["log", "-1", "--format=%s"]),
        "the-forge: a second line"
    );

    let clean = fx.step(run.id, "commit", Some("again")).await;
    assert_eq!(clean.sentence.as_deref(), Some(CLEAN));
    assert_eq!(clean.commit_sha, None);
}

#[tokio::test]
async fn a_landing_waits_for_the_persons_approval_then_lands_and_publishes_its_row() {
    let mut fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    let sha = fx.change(&run);
    let before = heads(&fx.bare);

    let service = fx.service.clone();
    let tenant = fx.tenant.clone();
    let id = run.id;
    let landing = tokio::spawn(async move {
        service
            .run_repository_action(&tenant, id, "land", None)
            .await
    });
    let request = fx.pending_landing().await;
    assert_eq!(request.tool_name, "aegis.git.land");
    assert_eq!(request.execution_id, ExecutionId(run.id));
    assert_eq!(
        heads(&fx.bare),
        before,
        "something was pushed before the person answered"
    );

    fx.approvals
        .decide(
            request.id,
            &fx.tenant,
            PERSON,
            ToolApprovalDecision::Once,
            &Lands(fx.service.clone()),
        )
        .await
        .unwrap_or_else(|e| panic!("the approval was not taken: {e}"));
    let answered = tokio::time::timeout(std::time::Duration::from_secs(30), landing)
        .await
        .expect("the landing did not answer after the approval")
        .unwrap()
        .unwrap_or_else(|e| panic!("the landing failed: {e}"));

    assert_eq!(answered.sentence, None, "{answered:?}");
    assert_eq!(answered.commit_sha.as_deref(), Some(sha.as_str()));
    assert_eq!(answered.git_ref, "main");
    assert_eq!(heads(&fx.bare).get("refs/heads/main"), Some(&sha));

    let landed: Vec<ExecutionEvent> = fx
        .published()
        .into_iter()
        .filter(|e| matches!(e, ExecutionEvent::RepositoryLanded { .. }))
        .collect();
    assert_eq!(landed.len(), 1, "one landing row: {landed:?}");
    let ExecutionEvent::RepositoryLanded {
        execution_id,
        label,
        branch,
        git_ref,
        commit_sha,
        branch_url,
        ..
    } = &landed[0]
    else {
        unreachable!()
    };
    assert_eq!(*execution_id, ExecutionId(run.id));
    assert_eq!(label, "app");
    assert_eq!(branch, &run.branch);
    assert_eq!(git_ref, "main");
    assert_eq!(commit_sha, &sha);
    assert!(
        branch_url.as_str().ends_with("/tree/main"),
        "the row's URL is not the ref's page: {}",
        branch_url.as_str()
    );
    assert_eq!(
        repository_landed_line(commit_sha, label, git_ref),
        format!("Landed {sha} of repository app on main")
    );
    let row = serde_json::to_value(&landed[0]).unwrap();
    assert_eq!(row["RepositoryLanded"]["ref"], json!("main"), "{row}");
}

#[tokio::test]
async fn a_declined_landing_pushes_nothing() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    fx.change(&run);
    let before = heads(&fx.bare);

    let service = fx.service.clone();
    let tenant = fx.tenant.clone();
    let id = run.id;
    let landing = tokio::spawn(async move {
        service
            .run_repository_action(&tenant, id, "land", None)
            .await
    });
    let request = fx.pending_landing().await;
    fx.approvals
        .decide(
            request.id,
            &fx.tenant,
            PERSON,
            ToolApprovalDecision::Deny,
            &Lands(fx.service.clone()),
        )
        .await
        .unwrap();
    let answered = tokio::time::timeout(std::time::Duration::from_secs(30), landing)
        .await
        .expect("the landing did not answer after the decline")
        .unwrap()
        .unwrap();

    assert_eq!(
        answered.sentence.as_deref(),
        Some("approval request denied; nothing was landed")
    );
    assert_eq!(answered.commit_sha, None);
    assert_eq!(
        heads(&fx.bare),
        before,
        "a declined landing pushed something"
    );
}

#[tokio::test]
async fn the_landing_is_answered_only_for_a_workflow_execution_of_its_person() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    fx.change(&run);
    let before = heads(&fx.bare);

    for (execution, person) in [
        (ExecutionId::new(), PERSON),
        (ExecutionId(run.id), "someone-else"),
    ] {
        match fx
            .service
            .land_for_interpreter(&fx.tenant, execution, person)
            .await
        {
            Err(SealSessionError::InvalidArguments(sentence)) => {
                assert_eq!(sentence, LAND_IS_THE_WORKFLOWS)
            }
            other => panic!("a landing outside its workflow answered {other:?}"),
        }
    }
    assert_eq!(heads(&fx.bare), before, "something was pushed");
}

#[tokio::test]
async fn aegis_git_land_is_gated_with_no_summary_and_listed_to_no_agent() {
    let router = ToolRouter::new(ToolRouter::builtin_dispatchers());
    assert!(
        router.requires_approval("aegis.git.land"),
        "aegis.git.land does not wait at the approval gate"
    );
    let contract = router.approval_contract("aegis.git.land");
    assert!(
        contract.approval_summary.is_none(),
        "aegis.git.land declares a summary: {contract:?}"
    );
    let listed = router.list_tools().await.unwrap();
    assert!(
        !listed.iter().any(|t| t.name == "aegis.git.land"),
        "aegis.git.land is listed"
    );
}

#[tokio::test]
async fn an_unknown_action_is_refused() {
    let fx = fixture().await;
    let run = fx.run(GitRef::Branch("main".to_string())).await;
    match fx
        .service
        .run_repository_action(&fx.tenant, run.id, "push", None)
        .await
    {
        Err(RepositoryActionError::UnknownAction(action)) => assert_eq!(action, "push"),
        other => panic!("an unknown action answered {other:?}"),
    }
}
