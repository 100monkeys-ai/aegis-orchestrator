// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git steps on a volume that is not a host directory
//!
//! A repository binding whose volume lives on SeaweedFS (or OpenDAL, or a
//! SEAL node) is cloned, refreshed, committed, pushed and diffed by a git
//! step: a container running the real `git`, the volume mounted at
//! `/workspace`, the credential on its standard input only. These tests run
//! the step's own script with the host's shell in directories of their own
//! (`ShellStepRunner`), against a bare repository on the local disk, so every
//! git operation the step performs really happens.
//!
//! | Scenario | Test |
//! |---|---|
//! | Commit on a SeaweedFS volume | `commit_on_a_seaweedfs_volume_runs_as_a_git_step` |
//! | A clean tree on a SeaweedFS volume | `commit_on_a_clean_seaweedfs_volume_answers_nothing_to_commit` |
//! | Push to a local bare repository | `push_from_a_seaweedfs_volume_reaches_a_local_bare_repository` |
//! | Push to a remote other than origin | `push_from_a_seaweedfs_volume_to_another_remote_is_refused` |
//! | Diff, unstaged and staged | `diff_on_a_seaweedfs_volume_shows_unstaged_and_staged_changes` |
//! | Diff at the output cap | `diff_on_a_seaweedfs_volume_at_the_output_cap_is_refused` |
//! | Refresh | `refresh_on_a_seaweedfs_volume_fetches_and_checks_out` |
//! | The step's network | `every_git_step_runs_on_the_configured_step_network` |
//! | The start-up line | `an_absent_step_network_logs_the_start_up_line` |
//! | The node configuration key | `the_step_network_is_read_from_spec_storage_git` |

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;

use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, EphemeralCliPaths, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{GitRepoError, GitRepoService};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId, GitRepoBindingRepository,
    GitRepoStatus,
};
use aegis_orchestrator_core::domain::repository::{RepositoryError, VolumeRepository};
use aegis_orchestrator_core::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner, InstanceId,
};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
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
        unreachable!()
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

// ===========================================================================
// The fixture
// ===========================================================================

struct Fixture {
    service: GitRepoService,
    bindings: Arc<Bindings>,
    runner: Arc<ShellStepRunner>,
    /// Where the step mounts the volume; the clone lands in `repo` under it.
    workspace: PathBuf,
    bare: PathBuf,
    seed: PathBuf,
    binding_id: GitRepoBindingId,
    tenant: TenantId,
    _dirs: tempfile::TempDir,
}

impl Fixture {
    fn tree(&self) -> PathBuf {
        self.workspace.join("repo")
    }

    async fn binding(&self) -> GitRepoBinding {
        self.bindings
            .find_by_id(&self.binding_id)
            .await
            .unwrap()
            .expect("binding")
    }
}

fn seaweed_volume(tenant: &TenantId) -> Volume {
    Volume::new(
        "git-steps".to_string(),
        tenant.clone(),
        StorageClass::persistent(),
        VolumeBackend::SeaweedFS {
            filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
            remote_path: "/aegis/seaweedfs/git-steps".to_string(),
        },
        64 * 1024 * 1024,
        VolumeOwnership::persistent(OWNER),
    )
    .unwrap()
}

/// A binding of a SeaweedFS volume, cloned from a local bare repository by
/// the clone step and Ready.
async fn cloned_fixture() -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let (bare, seed) = bare_repository(dirs.path());
    let workspace = dirs.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let scratch = dirs.path().join("scratch").join("aegis-git");
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();

    let event_bus = Arc::new(EventBus::new(64));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let tenant = TenantId::system();
    let volume = seaweed_volume(&tenant);
    volume_repo.save(&volume).await.unwrap();

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
    let runner = Arc::new(ShellStepRunner::new());
    let engine = EphemeralCliEngine::new(
        runner.clone() as Arc<dyn ContainerStepRunner>,
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
    let service = GitRepoService::new(
        bindings.clone() as Arc<dyn GitRepoBindingRepository>,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus,
    );

    let binding = GitRepoBinding::new(
        tenant.clone(),
        None,
        format!("file://{}", bare.display()),
        GitRef::Branch("main".to_string()),
        None,
        volume.id,
        "git-steps".to_string(),
        CloneStrategy::EphemeralCli {
            reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
        },
        false,
        None,
        None,
        None,
    );
    let binding_id = binding.id;
    bindings.save(&binding).await.unwrap();
    service
        .clone_repo(&binding_id)
        .await
        .expect("the clone step clones the bare repository");

    let fx = Fixture {
        service,
        bindings,
        runner,
        workspace,
        bare,
        seed,
        binding_id,
        tenant,
        _dirs: dirs,
    };
    assert_eq!(fx.binding().await.status, GitRepoStatus::Ready);
    fx
}

// ===========================================================================
// Commit, push, diff, refresh on a SeaweedFS volume (G11)
// ===========================================================================

#[tokio::test]
async fn commit_on_a_seaweedfs_volume_runs_as_a_git_step() {
    let fx = cloned_fixture().await;
    std::fs::write(fx.tree().join("README.md"), "first line\nsecond line\n").unwrap();
    std::fs::write(fx.tree().join("NEW.md"), "new file\n").unwrap();

    let result = fx
        .service
        .commit(
            &fx.binding_id,
            &fx.tenant,
            OWNER,
            "the second line",
            "Jane Person",
            "jane@example.invalid",
        )
        .await;

    let sha = result.unwrap_or_else(|e| panic!("commit on the volume failed: {e}"));
    assert_eq!(sha.len(), 40, "commit answers a full sha, got {sha:?}");
    assert_eq!(git(&fx.tree(), &["rev-parse", "HEAD"]), sha);
    assert_eq!(
        git(&fx.tree(), &["log", "-1", "--format=%s|%an|%ae"]),
        "the second line|Jane Person|jane@example.invalid"
    );
    assert_eq!(
        git(&fx.tree(), &["status", "--porcelain"]),
        "",
        "the commit staged every change"
    );
}

#[tokio::test]
async fn commit_on_a_clean_seaweedfs_volume_answers_nothing_to_commit() {
    let fx = cloned_fixture().await;
    let result = fx
        .service
        .commit(
            &fx.binding_id,
            &fx.tenant,
            OWNER,
            "nothing changed",
            "Jane Person",
            "jane@example.invalid",
        )
        .await;
    assert!(
        matches!(result, Err(GitRepoError::NothingToCommit)),
        "expected NothingToCommit on a clean tree, got {result:?}"
    );
}

#[tokio::test]
async fn push_from_a_seaweedfs_volume_reaches_a_local_bare_repository() {
    let fx = cloned_fixture().await;
    std::fs::write(fx.tree().join("README.md"), "first line\npushed line\n").unwrap();
    let sha = fx
        .service
        .commit(
            &fx.binding_id,
            &fx.tenant,
            OWNER,
            "to push",
            "Jane Person",
            "jane@example.invalid",
        )
        .await
        .unwrap_or_else(|e| panic!("commit on the volume failed: {e}"));
    // The refresh-free tree is on its branch; push it by name.
    let result = fx
        .service
        .push(&fx.binding_id, &fx.tenant, OWNER, None, Some("main"))
        .await;
    result.unwrap_or_else(|e| panic!("push from the volume failed: {e}"));
    assert_eq!(
        git(&fx.bare, &["rev-parse", "refs/heads/main"]),
        sha,
        "the bare repository's main is not the pushed commit"
    );
}

#[tokio::test]
async fn push_from_a_seaweedfs_volume_to_another_remote_is_refused() {
    let fx = cloned_fixture().await;
    let before = git(&fx.bare, &["rev-parse", "refs/heads/main"]);
    let result = fx
        .service
        .push(&fx.binding_id, &fx.tenant, OWNER, Some("upstream"), None)
        .await;
    match result {
        Err(GitRepoError::GitFailed(m)) => assert_eq!(
            m, "a push from a volume goes to the repository's own remote, origin",
            "a push to another remote is refused with the wrong sentence"
        ),
        other => panic!("a push to a remote other than origin was not refused: {other:?}"),
    }
    assert_eq!(git(&fx.bare, &["rev-parse", "refs/heads/main"]), before);
}

#[tokio::test]
async fn diff_on_a_seaweedfs_volume_shows_unstaged_and_staged_changes() {
    let fx = cloned_fixture().await;
    std::fs::write(fx.tree().join("README.md"), "first line\nunstaged line\n").unwrap();

    let unstaged = fx
        .service
        .diff(&fx.binding_id, &fx.tenant, OWNER, false)
        .await
        .unwrap_or_else(|e| panic!("diff on the volume failed: {e}"));
    assert!(
        unstaged.contains("+unstaged line"),
        "the unstaged diff does not show the change: {unstaged:?}"
    );
    let staged = fx
        .service
        .diff(&fx.binding_id, &fx.tenant, OWNER, true)
        .await
        .unwrap_or_else(|e| panic!("staged diff on the volume failed: {e}"));
    assert!(
        !staged.contains("+unstaged line"),
        "the staged diff shows an unstaged change: {staged:?}"
    );

    git(&fx.tree(), &["add", "README.md"]);
    let staged = fx
        .service
        .diff(&fx.binding_id, &fx.tenant, OWNER, true)
        .await
        .unwrap_or_else(|e| panic!("staged diff on the volume failed: {e}"));
    assert!(
        staged.contains("+unstaged line"),
        "the staged diff does not show the staged change: {staged:?}"
    );
}

#[tokio::test]
async fn diff_on_a_seaweedfs_volume_at_the_output_cap_is_refused() {
    let fx = cloned_fixture().await;
    // A change whose diff is past the step runner's 1 MiB output cap.
    let big: String = (0..40_000)
        .map(|i| format!("a line of the large change, number {i:08}\n"))
        .collect();
    std::fs::write(fx.tree().join("README.md"), big).unwrap();

    let result = fx
        .service
        .diff(&fx.binding_id, &fx.tenant, OWNER, false)
        .await;
    match result {
        Err(GitRepoError::GitFailed(m)) => assert_eq!(
            m, "the diff is larger than 1 MiB; commit or narrow the change and ask again",
            "a diff at the cap is refused with the wrong sentence"
        ),
        other => panic!(
            "a diff at the cap was not refused: {:?}",
            other.map(|d| d.len())
        ),
    }
}

#[tokio::test]
async fn refresh_on_a_seaweedfs_volume_fetches_and_checks_out() {
    let fx = cloned_fixture().await;
    let first = fx.binding().await.last_commit_sha.expect("cloned sha");
    std::fs::write(fx.seed.join("LATER.md"), "a later commit\n").unwrap();
    git(&fx.seed, &["add", "LATER.md"]);
    git(&fx.seed, &["commit", "-m", "later"]);
    git(&fx.seed, &["push", "origin", "main"]);
    let later = git(&fx.seed, &["rev-parse", "HEAD"]);
    assert_ne!(first, later);

    let result = fx
        .service
        .refresh_repo(&fx.binding_id, &fx.tenant, OWNER)
        .await;
    result.unwrap_or_else(|e| panic!("refresh on the volume failed: {e}"));

    let binding = fx.binding().await;
    assert_eq!(binding.status, GitRepoStatus::Ready);
    assert_eq!(binding.last_commit_sha.as_deref(), Some(later.as_str()));
    assert_eq!(git(&fx.tree(), &["rev-parse", "HEAD"]), later);
    assert!(
        fx.tree().join("LATER.md").exists(),
        "the refreshed tree does not hold the later commit's file"
    );
    // Every step that ran had a configuration; none of them is the old
    // refusal's path.
    assert!(fx.runner.seen.lock().unwrap().len() >= 2);
}

// ===========================================================================
// The step's network and the start-up line (G12)
// ===========================================================================

/// Answers every step as a success whose last line serves as a sha, a ref
/// and a diff, and keeps each configuration it was given.
#[derive(Default)]
struct CapturingRunner {
    seen: Mutex<Vec<ContainerStepConfig>>,
}

#[async_trait]
impl ContainerStepRunner for CapturingRunner {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        self.seen.lock().unwrap().push(config);
        Ok(ContainerStepResult {
            exit_code: 0,
            stdout: format!("{}\n", "a".repeat(40)),
            stderr: String::new(),
            duration_ms: 1,
        })
    }
}

/// Run clone, fetch, commit, push and diff through an executor whose engine
/// was given `network`, and answer each step's `network_mode`.
async fn step_networks(network: Option<String>) -> Vec<(String, Option<String>)> {
    let event_bus = Arc::new(EventBus::new(16));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let tenant = TenantId::system();
    let volume = seaweed_volume(&tenant);
    let runner = Arc::new(CapturingRunner::default());
    let engine = EphemeralCliEngine::new(
        runner.clone() as Arc<dyn ContainerStepRunner>,
        Arc::new(NfsVolumeRegistry::new()),
    )
    .with_step_network(network);
    let executor = GitCloneExecutor::new(
        Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            event_bus,
        )),
        Arc::new(AegisFSAL::new(
            unused_storage_provider(),
            volume_repo as Arc<dyn VolumeRepository>,
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoEvents),
        )),
        Some(Arc::new(engine)),
    );
    let binding = GitRepoBinding::new(
        tenant,
        None,
        "https://example.invalid/owner/repo.git".to_string(),
        GitRef::Branch("main".to_string()),
        None,
        volume.id,
        "git-steps".to_string(),
        CloneStrategy::EphemeralCli {
            reason: "test".to_string(),
        },
        false,
        None,
        None,
        None,
    );
    executor
        .clone_ephemeral(&binding, &volume, None, true)
        .await
        .expect("clone step");
    executor
        .fetch_ephemeral(&binding, &volume, None)
        .await
        .expect("fetch step");
    executor
        .commit_ephemeral(&volume, "message", "Jane Person", "jane@example.invalid")
        .await
        .expect("commit step");
    executor
        .push_ephemeral(&binding, &volume, Some("main"), None)
        .await
        .expect("push step");
    executor
        .diff_ephemeral(&volume, false)
        .await
        .expect("diff step");
    let seen = runner.seen.lock().unwrap();
    seen.iter()
        .map(|c| (c.state_name.as_str().to_string(), c.network_mode.clone()))
        .collect()
}

#[tokio::test]
async fn every_git_step_runs_on_the_configured_step_network() {
    let configured = step_networks(Some("aegis-git-steps".to_string())).await;
    assert_eq!(configured.len(), 5, "clone, fetch, commit, push, diff");
    for (step, network) in &configured {
        assert_eq!(
            network.as_deref(),
            Some("aegis-git-steps"),
            "the {step} step does not run on the configured step network"
        );
    }
    for absent in [None, Some(String::new())] {
        for (step, network) in step_networks(absent.clone()).await {
            assert_eq!(
                network, None,
                "with the key {absent:?}, the {step} step names a network instead of the runner's default"
            );
        }
    }
}

/// Records the message of every event logged while it is the dispatcher.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<String>>>);

struct MessageOf(String);

impl tracing::field::Visit for MessageOf {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Recorder {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = MessageOf(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .unwrap()
            .push(format!("{} {}", event.metadata().level(), message.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn logged_while_building(network: Option<String>) -> Vec<String> {
    let recorder = Recorder::default();
    tracing::subscriber::with_default(recorder.clone(), || {
        let _ = EphemeralCliEngine::new(
            Arc::new(CapturingRunner::default()) as Arc<dyn ContainerStepRunner>,
            Arc::new(NfsVolumeRegistry::new()),
        )
        .with_step_network(network);
    });
    let lines = recorder.0.lock().unwrap().clone();
    lines
}

#[test]
fn an_absent_step_network_logs_the_start_up_line() {
    let line = "WARN git steps run on the agents' network and cannot reach a git host";
    for absent in [None, Some(String::new())] {
        let logged = logged_while_building(absent.clone());
        assert!(
            logged.iter().any(|l| l == line),
            "with the key {absent:?}, the engine did not log {line:?}; it logged {logged:?}"
        );
    }
    let logged = logged_while_building(Some("aegis-git-steps".to_string()));
    assert!(
        !logged.iter().any(|l| l.contains("cannot reach a git host")),
        "with the key set, the engine still logged the start-up line: {logged:?}"
    );
}

#[test]
fn the_step_network_is_read_from_spec_storage_git() {
    use aegis_orchestrator_core::domain::node_config::StorageConfig;
    let with_key: StorageConfig = serde_yaml::from_str(
        "backend: seaweedfs\ngit:\n  step_network: \"env:AEGIS_GIT_STEP_NETWORK\"\n",
    )
    .expect("storage configuration parses");
    assert_eq!(
        with_key.git.and_then(|g| g.step_network).as_deref(),
        Some("env:AEGIS_GIT_STEP_NETWORK"),
        "spec.storage.git.step_network is not read"
    );
    let without: StorageConfig =
        serde_yaml::from_str("backend: seaweedfs\n").expect("storage configuration parses");
    assert!(
        without.git.and_then(|g| g.step_network).is_none(),
        "an absent spec.storage.git.step_network reads as set"
    );
}
