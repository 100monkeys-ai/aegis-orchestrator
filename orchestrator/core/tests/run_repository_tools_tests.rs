// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A run's git tools in the git repository service
//!
//! AEGIS ADR-136 G7, G7b, G7c, G8, G8a, G8b and G5b through the service the
//! tool path calls: the run commits, reads and pushes the binding it holds,
//! and another run is refused; the push sends only the work branch and
//! answers a non-fast-forward in its sentence with nothing pushed; the
//! preparation is kept for the run's narrative row. Bindings live on a host
//! directory (libgit2), cloned from a bare repository on the local disk; the
//! volume path is exercised on the tool path, in the crate's
//! `tool_invocation_service/run_git_tools_tests.rs`.
//!
//! | Scenario | Test |
//! |---|---|
//! | The holding run commits; another is refused | `the_holding_run_commits_and_another_run_is_refused` |
//! | Status from the tree | `status_reads_clean_or_changed_and_head_from_the_tree` |
//! | Push only the work branch | `on_a_host_directory_the_push_sends_only_the_work_branch` |
//! | A non-fast-forward | `on_a_host_directory_a_non_fast_forward_answers_its_sentence_and_pushes_nothing` |
//! | The preparation kept once | `the_preparation_is_taken_once_and_released_with_the_run` |
//! | The branch URL | `the_branch_url_is_the_hosts_page_of_the_branch` |

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;

use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, EphemeralCliPaths, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{
    branch_url, GitRepoError, GitRepoService, RunRepositories,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    default_work_branch, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository, RunRepository,
};
use aegis_orchestrator_core::domain::repository::{RepositoryError, VolumeRepository};
use aegis_orchestrator_core::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner, InstanceId,
};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::InMemoryVolumeRepository;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

const OWNER: &str = "volume-owner";

// ===========================================================================
// The step runner: the step's own script, run by the host's shell
// ===========================================================================

/// Runs a step's entrypoint and command with the host's shell, its standard
/// input handed over as the container runner hands it, in an environment of
/// its own (no git configuration of the machine running the tests takes
/// part). Keeps every configuration it was given.
struct ShellStepRunner {
    home: tempfile::TempDir,
    seen: Mutex<Vec<ContainerStepConfig>>,
}

impl ShellStepRunner {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("home for the step"),
            seen: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ContainerStepRunner for ShellStepRunner {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        self.seen.lock().unwrap().push(config.clone());
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
// In-memory binding repository
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

// ===========================================================================
// A volume service the binding service never asks to create a volume
// ===========================================================================

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
        // A delete the hold failed to refuse reaches here and is answered,
        // so the test's own sentence names it.
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
        _execution_id: aegis_orchestrator_core::domain::execution::ExecutionId,
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

fn unused_storage_provider() -> Arc<dyn aegis_orchestrator_core::domain::storage::StorageProvider> {
    use aegis_orchestrator_core::domain::storage::{
        DirEntry, FileAttributes, FileHandle, OpenMode, StorageError, StorageProvider,
    };
    struct Unused;
    #[async_trait]
    impl StorageProvider for Unused {
        async fn create_directory(&self, _p: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn delete_directory(&self, _p: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn set_quota(&self, _p: &str, _b: u64) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_usage(&self, _p: &str) -> Result<u64, StorageError> {
            Ok(0)
        }
        async fn health_check(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn open_file(&self, _p: &str, _m: OpenMode) -> Result<FileHandle, StorageError> {
            unreachable!()
        }
        async fn read_at(
            &self,
            _h: &FileHandle,
            _o: u64,
            _l: usize,
        ) -> Result<Vec<u8>, StorageError> {
            unreachable!()
        }
        async fn write_at(
            &self,
            _h: &FileHandle,
            _o: u64,
            _d: &[u8],
        ) -> Result<usize, StorageError> {
            unreachable!()
        }
        async fn close_file(&self, _h: &FileHandle) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn stat(&self, _p: &str) -> Result<FileAttributes, StorageError> {
            unreachable!()
        }
        async fn readdir(&self, _p: &str) -> Result<Vec<DirEntry>, StorageError> {
            unreachable!()
        }
        async fn create_file(&self, _p: &str, _m: u32) -> Result<FileHandle, StorageError> {
            unreachable!()
        }
        async fn delete_file(&self, _p: &str) -> Result<(), StorageError> {
            unreachable!()
        }
        async fn rename(&self, _f: &str, _t: &str) -> Result<(), StorageError> {
            unreachable!()
        }
    }
    Arc::new(Unused)
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

/// A bare repository holding one commit on `main`, and a working copy of it
/// the fixture adds commits through. Returns the bare repository's path.
fn bare_repository(root: &Path) -> (PathBuf, PathBuf) {
    let bare = root.join("remote.git");
    let seed = root.join("seed");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main"]);
    git(&seed, &["init", "--initial-branch=main"]);
    std::fs::write(seed.join("README.md"), "first line\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "first"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "origin", "main"]);
    (bare, seed)
}

/// A bare repository holding `main` (one commit) and `feature/existing`
/// (one commit more), and a working copy of it the fixture commits through.
fn upstream(root: &Path) -> (PathBuf, PathBuf) {
    let (bare, seed) = bare_repository(root);
    git(&seed, &["checkout", "-b", "feature/existing"]);
    std::fs::write(seed.join("FEATURE.md"), "the feature\n").unwrap();
    git(&seed, &["add", "FEATURE.md"]);
    git(&seed, &["commit", "-m", "the feature"]);
    git(&seed, &["push", "origin", "feature/existing"]);
    git(&seed, &["checkout", "main"]);
    (bare, seed)
}

struct Fixture {
    service: Arc<GitRepoService>,
    bindings: Arc<Bindings>,
    volume_repo: Arc<InMemoryVolumeRepository>,
    bare: PathBuf,
    tenant: TenantId,
    dirs: tempfile::TempDir,
}

impl Fixture {
    fn main_sha(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }

    fn binding(&self, label: &str, volume: &Volume) -> GitRepoBinding {
        GitRepoBinding::new(
            self.tenant.clone(),
            None,
            format!("file://{}", self.bare.display()),
            GitRef::Branch("main".to_string()),
            None,
            volume.id,
            label.to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
            None,
            None,
        )
    }

    /// A Ready binding of a host directory owned by `owner`, its tree a
    /// clone of the bare repository at the directory's root.
    async fn host_binding(&self, label: &str, owner: &str) -> (GitRepoBindingId, PathBuf) {
        let dir = self
            .dirs
            .path()
            .join(format!("host-{}", uuid::Uuid::new_v4()));
        let volume = Volume::new(
            format!("git-{label}"),
            self.tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::HostPath { path: dir.clone() },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(owner),
        )
        .unwrap();
        self.volume_repo.save(&volume).await.unwrap();
        let out = Command::new("git")
            .args([
                "clone",
                "--quiet",
                self.bare.to_str().unwrap(),
                dir.to_str().unwrap(),
            ])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut binding = self.binding(label, &volume);
        binding.complete_clone(self.main_sha(), 0);
        self.bindings.save(&binding).await.unwrap();
        (binding.id, dir)
    }
}

async fn fixture() -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let (bare, _seed) = upstream(dirs.path());
    let workspace = dirs.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let scratch = dirs.path().join("scratch").join("aegis-git");
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();

    let event_bus = Arc::new(EventBus::new(64));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let tenant = TenantId::system();
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
        unused_storage_provider(),
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let engine = EphemeralCliEngine::new(
        Arc::new(ShellStepRunner::new()) as Arc<dyn ContainerStepRunner>,
        Arc::new(NfsVolumeRegistry::new()),
    )
    .with_paths(EphemeralCliPaths {
        workspace: workspace.display().to_string(),
        scratch: scratch.display().to_string(),
    });
    let clone_executor = Arc::new(GitCloneExecutor::new(
        secrets_manager.clone(),
        fsal,
        Some(Arc::new(engine)),
    ));
    let bindings = Arc::new(Bindings::default());
    let service = Arc::new(GitRepoService::new(
        bindings.clone() as Arc<dyn GitRepoBindingRepository>,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus,
    ));
    Fixture {
        service,
        bindings,
        volume_repo,
        bare,
        tenant,
        dirs,
    }
}

fn entry(id: GitRepoBindingId, branch: Option<&str>) -> RunRepository {
    RunRepository {
        binding_id: id,
        branch: branch.map(str::to_string),
    }
}

const AHEAD: &str = "has commits this run does not have; nothing was pushed";

/// A run holding the host binding `app`, its work branch checked out.
async fn held(fx: &Fixture) -> (GitRepoBindingId, PathBuf, uuid::Uuid, String) {
    let (id, tree) = fx.host_binding("app", OWNER).await;
    let run = uuid::Uuid::new_v4();
    fx.service
        .prepare_for_run(&fx.tenant, Some(OWNER), run, &[entry(id, None)])
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    (id, tree, run, default_work_branch(run))
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

#[tokio::test]
async fn the_holding_run_commits_and_another_run_is_refused() {
    let fx = fixture().await;
    let (id, tree, run, branch) = held(&fx).await;
    std::fs::write(tree.join("CHANGE.md"), "a change\n").unwrap();
    let another = fx
        .service
        .commit_for_run(
            uuid::Uuid::new_v4(),
            &id,
            &fx.tenant,
            OWNER,
            "not mine",
            "A",
            "a@example.invalid",
        )
        .await;
    assert!(
        matches!(another, Err(GitRepoError::HeldByRun { .. })),
        "another run's commit answered {another:?}"
    );
    let sha = fx
        .service
        .commit_for_run(
            run,
            &id,
            &fx.tenant,
            OWNER,
            "mine",
            "A",
            "a@example.invalid",
        )
        .await
        .unwrap_or_else(|e| panic!("the holding run's commit was refused: {e}"));
    assert_eq!(git(&tree, &["rev-parse", "HEAD"]), sha);
    assert_eq!(git(&tree, &["symbolic-ref", "--short", "HEAD"]), branch);
    let clean = fx
        .service
        .commit_for_run(
            run,
            &id,
            &fx.tenant,
            OWNER,
            "again",
            "A",
            "a@example.invalid",
        )
        .await;
    match clean {
        Err(e @ GitRepoError::NothingToCommit) => {
            assert_eq!(e.to_string(), "nothing to commit: working tree is clean")
        }
        other => panic!("a clean tree answered {other:?}"),
    }
}

#[tokio::test]
async fn status_reads_clean_or_changed_and_head_from_the_tree() {
    let fx = fixture().await;
    let (id, tree, run, _) = held(&fx).await;
    let status = fx
        .service
        .status_for_run(run, &id, &fx.tenant, OWNER)
        .await
        .unwrap_or_else(|e| panic!("status failed: {e}"));
    assert!(status.clean, "a clean tree read as changed");
    assert_eq!(status.head, fx.main_sha(), "HEAD is not the work branch's");
    std::fs::write(tree.join("UNTRACKED.md"), "new\n").unwrap();
    let changed = fx
        .service
        .status_for_run(run, &id, &fx.tenant, OWNER)
        .await
        .unwrap();
    assert!(!changed.clean, "a tree with a new file read as clean");
}

#[tokio::test]
async fn on_a_host_directory_the_push_sends_only_the_work_branch() {
    let fx = fixture().await;
    let (id, tree, run, branch) = held(&fx).await;
    git(&tree, &["branch", "not-for-the-remote"]);
    std::fs::write(tree.join("CHANGE.md"), "a change\n").unwrap();
    let sha = fx
        .service
        .commit_for_run(
            run,
            &id,
            &fx.tenant,
            OWNER,
            "mine",
            "A",
            "a@example.invalid",
        )
        .await
        .unwrap();
    let before = heads(&fx.bare);
    let pushed = fx
        .service
        .push_for_run(run, &id, &fx.tenant, OWNER, &branch)
        .await
        .unwrap_or_else(|e| panic!("the run's push failed: {e}"));
    assert_eq!(pushed.branch, branch);
    let after = heads(&fx.bare);
    assert_eq!(
        after.get(&format!("refs/heads/{branch}")),
        Some(&sha),
        "the work branch is not on the remote at the run's commit: {after:?}"
    );
    let mut others = after.clone();
    others.remove(&format!("refs/heads/{branch}"));
    assert_eq!(others, before, "a branch other than the work branch moved");
}

#[tokio::test]
async fn on_a_host_directory_a_non_fast_forward_answers_its_sentence_and_pushes_nothing() {
    let fx = fixture().await;
    let (id, tree, run, _) = held(&fx).await;
    // The run works on the remote's existing branch, which then gains a
    // commit the run does not have.
    let branch = "feature/existing".to_string();
    git(&tree, &["fetch", "--quiet", "origin", &branch]);
    git(&tree, &["checkout", "--quiet", "-B", &branch, "FETCH_HEAD"]);
    let other = fx.dirs.path().join("other");
    git(
        fx.dirs.path(),
        &["clone", "--quiet", fx.bare.to_str().unwrap(), "other"],
    );
    git(&other, &["checkout", "--quiet", &branch]);
    std::fs::write(other.join("THEIRS.md"), "theirs\n").unwrap();
    git(&other, &["add", "THEIRS.md"]);
    git(&other, &["commit", "-m", "theirs"]);
    git(&other, &["push", "--quiet", "origin", &branch]);
    let theirs = heads(&fx.bare)[&format!("refs/heads/{branch}")].clone();

    std::fs::write(tree.join("OURS.md"), "ours\n").unwrap();
    fx.service
        .commit_for_run(
            run,
            &id,
            &fx.tenant,
            OWNER,
            "ours",
            "A",
            "a@example.invalid",
        )
        .await
        .unwrap();
    let pushed = fx
        .service
        .push_for_run(run, &id, &fx.tenant, OWNER, &branch)
        .await;
    match pushed {
        Err(e @ GitRepoError::RemoteAhead { .. }) => assert_eq!(
            e.to_string(),
            format!("the remote branch '{branch}' {AHEAD}")
        ),
        other => panic!("the refused push answered {other:?}"),
    }
    assert_eq!(
        heads(&fx.bare)[&format!("refs/heads/{branch}")],
        theirs,
        "the remote's branch moved"
    );
}

#[tokio::test]
async fn the_preparation_is_taken_once_and_released_with_the_run() {
    let fx = fixture().await;
    let (_, _, run, branch) = held(&fx).await;
    let taken = fx.service.take_prepared(run);
    match taken.as_slice() {
        [p] if p.label == "app"
            && p.branch == branch
            && p.started_from == fx.main_sha()
            && p.created => {}
        other => panic!("the preparation kept for the run: {other:?}"),
    }
    assert!(
        fx.service.take_prepared(run).is_empty(),
        "the preparation was taken twice"
    );
    let (_, _, released, _) = held(&fx).await;
    fx.service.release_run(released);
    assert!(
        fx.service.take_prepared(released).is_empty(),
        "a released run's preparation was kept"
    );
}

#[test]
fn the_branch_url_is_the_hosts_page_of_the_branch() {
    let mut wrong = Vec::new();
    for (url, want) in [
        (
            "https://github.com/o/r.git",
            "https://github.com/o/r/tree/aegis/1234abcd",
        ),
        (
            "https://github.com/o/r",
            "https://github.com/o/r/tree/aegis/1234abcd",
        ),
        (
            "github.com:o/r.git",
            "https://github.com/o/r/tree/aegis/1234abcd",
        ),
        (
            "git@github.com:o/r.git",
            "https://github.com/o/r/tree/aegis/1234abcd",
        ),
        (
            "ssh://github.com:22/o/r.git",
            "https://github.com/o/r/tree/aegis/1234abcd",
        ),
    ] {
        let got = branch_url(url, "aegis/1234abcd");
        if got != want {
            wrong.push(format!("{url} gave {got}, not {want}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("; "));
}
