// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A run's repositories: the checks, the hold, the mount and the work branch
//!
//! AEGIS ADR-136 G3 to G5 with G3a, G4a and G5a, through the git repository
//! service the start paths call. Bindings live on a SeaweedFS volume (the
//! work branch checked out by the git step, whose script the host's shell
//! runs, `ShellStepRunner`) or on a host directory (libgit2), cloned from a
//! bare repository on the local disk that holds `main` and the branch
//! `feature/existing`.
//!
//! | Scenario | Test |
//! |---|---|
//! | Another person's binding | `another_persons_binding_is_refused_by_the_first_eight_digits_of_its_id` |
//! | A binding not Ready | `a_binding_that_is_not_ready_is_refused_with_its_status` |
//! | A binding another run holds | `a_binding_another_run_holds_is_refused_as_in_use` |
//! | A branch equal to the ref | `a_branch_equal_to_the_bindings_ref_is_refused` |
//! | A label that names no directory | `a_label_that_cannot_name_a_directory_is_refused` |
//! | A label named twice | `a_label_named_twice_is_refused` |
//! | Host directory, the default branch | `on_a_host_directory_the_default_branch_is_created_from_the_ref` |
//! | Host directory, the remote's branch | `on_a_host_directory_a_branch_the_remote_has_is_checked_out_from_its_copy` |
//! | Volume, the default branch | `on_a_volume_the_default_branch_is_created_from_the_ref_by_a_git_step` |
//! | Volume, the remote's branch | `on_a_volume_a_branch_the_remote_has_is_checked_out_from_its_copy` |
//! | Refresh and delete while held | `refresh_and_delete_from_outside_the_run_are_refused_while_it_holds_the_binding` |
//! | A failed checkout | `a_failed_checkout_releases_the_hold` |
//! | The mount's place | `the_mount_is_the_working_tree_at_workspace_label` |
//! | A hold a restart cleared | `a_mount_takes_a_hold_no_run_has` |
//! | The person's name and email stored (G5d) | `a_run_started_by_a_person_with_a_name_and_email_stores_them_in_each_entry` |
//! | No profile, or one unread (G5d) | `a_run_whose_person_has_no_profile_stores_no_author`, `a_profile_that_cannot_be_read_stores_no_author` |
//! | A given author replaced (G5d) | `an_author_an_entry_carried_is_replaced_by_the_persons_own` |

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
    GitRepoError, GitRepoService, PersonProfiles, RunRepositories, RunRepositoryError,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    default_work_branch, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository, RunAuthor, RunRepository,
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
    /// Where the step mounts the SeaweedFS volume; its tree is `repo`.
    workspace: PathBuf,
    bare: PathBuf,
    tenant: TenantId,
    dirs: tempfile::TempDir,
}

impl Fixture {
    fn main_sha(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }

    fn feature_sha(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/feature/existing"])
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

    /// A Ready binding of the SeaweedFS volume, cloned by the clone step.
    async fn volume_binding(&self, label: &str) -> GitRepoBindingId {
        let volume = Volume::new(
            format!("git-{label}"),
            self.tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                remote_path: "/aegis/seaweedfs/run-repo".to_string(),
            },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(OWNER),
        )
        .unwrap();
        self.volume_repo.save(&volume).await.unwrap();
        let mut binding = self.binding(label, &volume);
        binding.clone_strategy = CloneStrategy::EphemeralCli {
            reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
        };
        self.bindings.save(&binding).await.unwrap();
        self.service
            .clone_repo(&binding.id)
            .await
            .expect("the clone step clones the bare repository");
        binding.id
    }

    fn volume_tree(&self) -> PathBuf {
        self.workspace.join("repo")
    }
}

async fn fixture() -> Fixture {
    fixture_with(None).await
}

/// A person's profile as a stub identity provider answers it, keeping the
/// subjects it was asked for.
struct Profiles {
    answer: Result<Option<(String, String)>, String>,
    asked: Mutex<Vec<String>>,
}

impl Profiles {
    fn answering(answer: Result<Option<(&str, &str)>, &str>) -> Arc<Self> {
        Arc::new(Self {
            answer: answer
                .map(|found| found.map(|(n, e)| (n.to_string(), e.to_string())))
                .map_err(str::to_string),
            asked: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl PersonProfiles for Profiles {
    async fn name_and_email(&self, sub: &str) -> Result<Option<(String, String)>, String> {
        self.asked.lock().unwrap().push(sub.to_string());
        self.answer.clone()
    }
}

async fn fixture_with(profiles: Option<Arc<dyn PersonProfiles>>) -> Fixture {
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
    let service = GitRepoService::new(
        bindings.clone() as Arc<dyn GitRepoBindingRepository>,
        user_volume_service,
        clone_executor,
        secrets_manager,
        event_bus,
    );
    let service = Arc::new(match profiles {
        Some(profiles) => service.with_person_profiles(profiles),
        None => service,
    });
    Fixture {
        service,
        bindings,
        volume_repo,
        workspace,
        bare,
        tenant,
        dirs,
    }
}

fn entry(id: GitRepoBindingId, branch: Option<&str>) -> RunRepository {
    RunRepository {
        binding_id: id,
        branch: branch.map(str::to_string),
        author: None,
        label: None,
        git_ref: None,
        started_from: None,
    }
}

/// The refusal a prepare answered, or why it answered none.
fn refusal(result: Result<Vec<RunRepository>, RunRepositoryError>) -> String {
    match result {
        Err(RunRepositoryError::Refused(sentence)) => sentence,
        other => format!("not a refusal: {other:?}"),
    }
}

// ===========================================================================
// G3, G3a: the refusals
// ===========================================================================

#[tokio::test]
async fn another_persons_binding_is_refused_by_the_first_eight_digits_of_its_id() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", "alice").await;
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some("bob"),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await;
    assert_eq!(
        refusal(result),
        format!(
            "repository '{}' is not one of yours",
            &id.0.to_string()[..8]
        )
    );
    assert_eq!(
        fx.service.run_holding(&id),
        None,
        "a refused run holds the binding"
    );
}

#[tokio::test]
async fn a_binding_that_is_not_ready_is_refused_with_its_status() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let mut binding = fx.bindings.find_by_id(&id).await.unwrap().unwrap();
    binding.start_clone();
    fx.bindings.save(&binding).await.unwrap();
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await;
    assert_eq!(
        refusal(result),
        "repository 'app' is not ready (it is cloning); start the run when its clone has finished"
    );
}

#[tokio::test]
async fn a_binding_another_run_holds_is_refused_as_in_use() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let first = uuid::Uuid::new_v4();
    fx.service
        .prepare_for_run(&fx.tenant, Some(OWNER), first, &[entry(id, None)])
        .await
        .unwrap_or_else(|e| panic!("the first run was not prepared: {e}"));
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await;
    assert_eq!(refusal(result), "repository 'app' is in use by another run");
    assert_eq!(fx.service.run_holding(&id), Some(first));
}

#[tokio::test]
async fn a_branch_equal_to_the_bindings_ref_is_refused() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, Some("main"))],
        )
        .await;
    assert_eq!(
        refusal(result),
        "a run works on its own branch, not on 'main'"
    );
    assert_eq!(
        fx.service.run_holding(&id),
        None,
        "a refused run holds the binding"
    );
}

#[tokio::test]
async fn a_label_that_cannot_name_a_directory_is_refused() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("my app", OWNER).await;
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await;
    assert_eq!(
        refusal(result),
        "repository 'my app' cannot be mounted: its label must be letters, digits, '.', '_' or '-'"
    );
}

#[tokio::test]
async fn a_label_named_twice_is_refused() {
    let fx = fixture().await;
    let (first, _) = fx.host_binding("app", OWNER).await;
    let (second, _) = fx.host_binding("app", OWNER).await;
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(first, None), entry(second, None)],
        )
        .await;
    assert_eq!(refusal(result), "repository 'app' is named twice");
    assert_eq!(
        fx.service.run_holding(&first),
        None,
        "a refused run holds a binding"
    );
}

// ===========================================================================
// G5, G5a: the work branch
// ===========================================================================

#[tokio::test]
async fn on_a_host_directory_the_default_branch_is_created_from_the_ref() {
    let fx = fixture().await;
    let (id, tree) = fx.host_binding("app", OWNER).await;
    std::fs::write(tree.join("LEFTOVER.txt"), "from an earlier run\n").unwrap();
    let run = uuid::Uuid::new_v4();
    let prepared = fx
        .service
        .prepare_for_run(&fx.tenant, Some(OWNER), run, &[entry(id, None)])
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));

    let branch = default_work_branch(run);
    assert_eq!(branch, format!("aegis/{}", &run.to_string()[..8]));
    assert_eq!(
        prepared,
        vec![RunRepository {
            label: Some("app".to_string()),
            git_ref: Some("main".to_string()),
            started_from: Some(git(&tree, &["rev-parse", "HEAD"])),
            ..entry(id, Some(&branch))
        }],
        "the entry's branch is not filled in"
    );
    assert_eq!(git(&tree, &["symbolic-ref", "--short", "HEAD"]), branch);
    assert_eq!(
        git(&tree, &["rev-parse", "HEAD"]),
        fx.main_sha(),
        "not created from the ref"
    );
    assert!(
        !tree.join("LEFTOVER.txt").exists(),
        "an untracked file survived the checkout"
    );
    assert_eq!(fx.service.run_holding(&id), Some(run));
}

#[tokio::test]
async fn on_a_host_directory_a_branch_the_remote_has_is_checked_out_from_its_copy() {
    let fx = fixture().await;
    let (id, tree) = fx.host_binding("app", OWNER).await;
    fx.service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, Some("feature/existing"))],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    assert_eq!(
        git(&tree, &["symbolic-ref", "--short", "HEAD"]),
        "feature/existing"
    );
    assert_eq!(
        git(&tree, &["rev-parse", "HEAD"]),
        fx.feature_sha(),
        "the remote's copy of the branch was not checked out"
    );
}

#[tokio::test]
async fn on_a_volume_the_default_branch_is_created_from_the_ref_by_a_git_step() {
    let fx = fixture().await;
    let id = fx.volume_binding("app").await;
    std::fs::write(
        fx.volume_tree().join("LEFTOVER.txt"),
        "from an earlier run\n",
    )
    .unwrap();
    let run = uuid::Uuid::new_v4();
    let prepared = fx
        .service
        .prepare_for_run(&fx.tenant, Some(OWNER), run, &[entry(id, None)])
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    let branch = default_work_branch(run);
    assert_eq!(
        prepared,
        vec![RunRepository {
            label: Some("app".to_string()),
            git_ref: Some("main".to_string()),
            started_from: Some(git(&fx.volume_tree(), &["rev-parse", "HEAD"])),
            ..entry(id, Some(&branch))
        }]
    );
    assert_eq!(
        git(&fx.volume_tree(), &["symbolic-ref", "--short", "HEAD"]),
        branch
    );
    assert_eq!(
        git(&fx.volume_tree(), &["rev-parse", "HEAD"]),
        fx.main_sha()
    );
    assert!(
        !fx.volume_tree().join("LEFTOVER.txt").exists(),
        "an untracked file survived the step's checkout"
    );
}

#[tokio::test]
async fn on_a_volume_a_branch_the_remote_has_is_checked_out_from_its_copy() {
    let fx = fixture().await;
    let id = fx.volume_binding("app").await;
    fx.service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, Some("feature/existing"))],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    assert_eq!(
        git(&fx.volume_tree(), &["symbolic-ref", "--short", "HEAD"]),
        "feature/existing"
    );
    assert_eq!(
        git(&fx.volume_tree(), &["rev-parse", "HEAD"]),
        fx.feature_sha(),
        "the step did not check out the remote's copy of the branch"
    );
}

// ===========================================================================
// G4, G4a: the hold and the mount
// ===========================================================================

#[tokio::test]
async fn refresh_and_delete_from_outside_the_run_are_refused_while_it_holds_the_binding() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let run = uuid::Uuid::new_v4();
    fx.service
        .prepare_for_run(&fx.tenant, Some(OWNER), run, &[entry(id, None)])
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    const HELD: &str = "repository 'app' is in use by a run; try again when it has ended";

    let mut complaints = Vec::new();
    match fx.service.refresh_repo(&id, &fx.tenant, OWNER).await {
        Err(e @ GitRepoError::HeldByRun { .. }) if e.to_string() == HELD => {}
        other => complaints.push(format!("refresh answered {other:?}")),
    }
    match fx.service.delete_binding(&id, &fx.tenant, OWNER).await {
        Err(e @ GitRepoError::HeldByRun { .. }) if e.to_string() == HELD => {}
        other => complaints.push(format!("delete answered {other:?}")),
    }
    if fx.bindings.find_by_id(&id).await.unwrap().is_none() {
        complaints.push("the held binding was deleted".to_string());
    }
    fx.service.release_run(run);
    if let Err(e) = fx.service.refresh_repo(&id, &fx.tenant, OWNER).await {
        complaints.push(format!("refresh after the run ended answered {e}"));
    }
    assert!(complaints.is_empty(), "{}", complaints.join("\n"));
}

#[tokio::test]
async fn a_failed_checkout_releases_the_hold() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let mut binding = fx.bindings.find_by_id(&id).await.unwrap().unwrap();
    binding.repo_url = format!("file://{}/no-such-repository.git", fx.dirs.path().display()).into();
    fx.bindings.save(&binding).await.unwrap();
    let result = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await;
    assert!(
        matches!(result, Err(RunRepositoryError::Failed(_))),
        "a checkout from a missing repository did not fail: {result:?}"
    );
    assert_eq!(
        fx.service.run_holding(&id),
        None,
        "the failed run still holds the binding"
    );
}

#[tokio::test]
async fn the_mount_is_the_working_tree_at_workspace_label() {
    let fx = fixture().await;
    let (host, _) = fx.host_binding("app", OWNER).await;
    let volume = fx.volume_binding("lib").await;
    let run = uuid::Uuid::new_v4();
    let entries = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            run,
            &[entry(host, None), entry(volume, None)],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    let mounts = fx
        .service
        .mounts_for_run(&fx.tenant, Some(OWNER), run, &entries)
        .await
        .unwrap_or_else(|e| panic!("the run's trees were not mounted: {e}"));
    let seen: Vec<(String, PathBuf, AccessMode, String)> = mounts
        .iter()
        .map(|m| {
            (
                m.label.clone(),
                m.mount.mount_point.clone(),
                m.mount.access_mode,
                m.mount.remote_path.clone(),
            )
        })
        .collect();
    let host_volume = fx
        .bindings
        .find_by_id(&host)
        .await
        .unwrap()
        .unwrap()
        .volume_id;
    assert_eq!(
        seen,
        vec![
            (
                "app".to_string(),
                PathBuf::from("/workspace/app"),
                AccessMode::ReadWrite,
                format!("/aegis/volumes/{}/{}", fx.tenant, host_volume),
            ),
            (
                "lib".to_string(),
                PathBuf::from("/workspace/lib"),
                AccessMode::ReadWrite,
                "/aegis/seaweedfs/run-repo/repo".to_string(),
            ),
        ],
        "a mount is not the working tree at /workspace/<label>"
    );
}

#[tokio::test]
async fn a_mount_takes_a_hold_no_run_has() {
    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let run = uuid::Uuid::new_v4();
    let entries = vec![entry(id, Some("aegis/restart"))];
    fx.service
        .mounts_for_run(&fx.tenant, Some(OWNER), run, &entries)
        .await
        .unwrap_or_else(|e| panic!("the state did not mount: {e}"));
    assert_eq!(fx.service.run_holding(&id), Some(run));
    let other = fx
        .service
        .mounts_for_run(&fx.tenant, Some(OWNER), uuid::Uuid::new_v4(), &entries)
        .await;
    match other {
        Err(RunRepositoryError::Refused(s)) => {
            assert_eq!(s, "repository 'app' is in use by another run")
        }
        other => panic!("another run mounted a held binding: {other:?}"),
    }
}

// ===========================================================================
// G5d: the person a run's commits are authored as
// ===========================================================================

/// A run started by a person whose profile has a name and an email stores
/// them in each of its entries, read once by the person's subject.
#[tokio::test]
async fn a_run_started_by_a_person_with_a_name_and_email_stores_them_in_each_entry() {
    let profiles = Profiles::answering(Ok(Some(("Ada Lovelace", "ada@example.com"))));
    let fx = fixture_with(Some(profiles.clone() as Arc<dyn PersonProfiles>)).await;
    let (first, _) = fx.host_binding("app", OWNER).await;
    let (second, _) = fx.host_binding("lib", OWNER).await;
    let run = uuid::Uuid::new_v4();
    let prepared = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            run,
            &[entry(first, None), entry(second, None)],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));

    let author = RunAuthor::new("Ada Lovelace", "ada@example.com");
    let authors: Vec<_> = prepared.iter().map(|e| e.author.clone()).collect();
    assert_eq!(
        authors,
        vec![author.clone(), author],
        "the person's name and email are not stored in each entry"
    );
    assert_eq!(
        *profiles.asked.lock().unwrap(),
        vec![OWNER.to_string()],
        "the profile was not read once, by the run's person"
    );
}

/// A run whose person has no profile with an email, or no person at all,
/// stores no author, and its commits carry the platform's author as before.
#[tokio::test]
async fn a_run_whose_person_has_no_profile_stores_no_author() {
    let fx = fixture_with(Some(
        Profiles::answering(Ok(None)) as Arc<dyn PersonProfiles>
    ))
    .await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let prepared = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    assert_eq!(
        prepared[0].author, None,
        "an author was stored with no profile"
    );

    let fx = fixture().await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let prepared = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    assert_eq!(
        prepared[0].author, None,
        "an author was stored with no profile reader"
    );
}

/// A profile that cannot be read leaves the run without an author: the run
/// starts, and commits as before.
#[tokio::test]
async fn a_profile_that_cannot_be_read_stores_no_author() {
    let fx = fixture_with(Some(Profiles::answering(Err(
        "keycloak admin read failed: connection refused",
    )) as Arc<dyn PersonProfiles>))
    .await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let prepared = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[entry(id, None)],
        )
        .await
        .unwrap_or_else(|e| panic!("an unread profile refused the run: {e}"));
    assert_eq!(
        prepared[0].author, None,
        "an unread profile stored an author"
    );
}

/// The author is the platform's: one an entry carried (the HTTP execute
/// route keeps the value unchecked) is replaced by the person's own, or
/// removed when the person's profile gives none.
#[tokio::test]
async fn an_author_an_entry_carried_is_replaced_by_the_persons_own() {
    let forged = RunRepository {
        author: RunAuthor::new("Someone Else", "someone@example.com"),
        ..entry(GitRepoBindingId::new(), None)
    };

    let fx = fixture_with(Some(
        Profiles::answering(Ok(Some(("Ada Lovelace", "ada@example.com"))))
            as Arc<dyn PersonProfiles>,
    ))
    .await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let given = RunRepository {
        binding_id: id,
        ..forged.clone()
    };
    let prepared = fx
        .service
        .prepare_for_run(
            &fx.tenant,
            Some(OWNER),
            uuid::Uuid::new_v4(),
            &[given.clone()],
        )
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    assert_eq!(
        prepared[0].author,
        RunAuthor::new("Ada Lovelace", "ada@example.com"),
        "the entry's given author was not replaced by the person's"
    );

    let fx = fixture_with(Some(
        Profiles::answering(Ok(None)) as Arc<dyn PersonProfiles>
    ))
    .await;
    let (id, _) = fx.host_binding("app", OWNER).await;
    let given = RunRepository {
        binding_id: id,
        ..forged
    };
    let prepared = fx
        .service
        .prepare_for_run(&fx.tenant, Some(OWNER), uuid::Uuid::new_v4(), &[given])
        .await
        .unwrap_or_else(|e| panic!("the run was not prepared: {e}"));
    assert_eq!(
        prepared[0].author, None,
        "the entry's given author survived a person with no profile"
    );
}
