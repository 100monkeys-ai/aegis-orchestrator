// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A run names its repository by URL, and the platform keeps the binding
//!
//! AEGIS ADR-141 F2a, through the git repository service the start paths
//! call and through the workflow start. The repository is a bare repository
//! on the local disk, reached at `https://git.example.test/org/repo.git` by
//! the clone step's own git (its configuration rewrites that host to the
//! local directory), on a SeaweedFS-shaped volume whose git steps the host's
//! shell runs.
//!
//! | Scenario | Test |
//! |---|---|
//! | The person's binding of that URL and ref is reused, first | `a_url_with_a_binding_of_that_url_and_ref_reuses_it_before_any_credential_is_read` |
//! | None: made from the one GitHub credential, cloned, then prepared | `a_url_with_no_binding_is_bound_with_the_persons_one_github_credential_and_cloned` |
//! | No active GitHub credential | `a_url_is_refused_when_the_person_has_no_active_github_credential` |
//! | Two active GitHub credentials | `a_url_is_refused_when_the_person_has_two_active_github_credentials` |
//! | A URL the credential cannot reach | `a_url_the_clone_cannot_reach_is_refused_with_the_clones_sentence_and_leaves_no_binding` |
//! | The workflow start resolves before it prepares | `a_workflow_start_resolves_a_url_entry_before_its_repositories_are_prepared` |
//! | A refused URL starts nothing | `a_workflow_start_whose_url_is_refused_prepares_nothing_and_starts_nothing` |

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use serde_json::{json, Value};

use aegis_orchestrator_core::application::git_clone_executor::{
    EphemeralCliEngine, EphemeralCliPaths, GitCloneExecutor,
};
use aegis_orchestrator_core::application::git_repo_service::{
    GitRepoService, PreparedRepository, RunMount, RunRepositories, RunRepositoryError,
};
use aegis_orchestrator_core::application::nfs_gateway::NfsVolumeRegistry;
use aegis_orchestrator_core::application::ports::{StartWorkflowParams, WorkflowEnginePort};
use aegis_orchestrator_core::application::start_workflow_execution::{
    StandardStartWorkflowExecutionUseCase, StartWorkflowExecutionRequest,
    StartWorkflowExecutionUseCase,
};
use aegis_orchestrator_core::application::temporal_mapper::TemporalWorkflowDefinition;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeService;
use aegis_orchestrator_core::application::volume_manager::VolumeService;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::execution::{ExecutionError, ServerChoice};
use aegis_orchestrator_core::domain::fsal::{AegisFSAL, EventPublisher};
use aegis_orchestrator_core::domain::git_repo::{
    parse_requested_repositories, CloneStrategy, GitRef, GitRepoBinding, GitRepoBindingId,
    GitRepoBindingRepository, GitRepoStatus, RequestedRepository, RunRepository,
};
use aegis_orchestrator_core::domain::iam::ZaruTier;
use aegis_orchestrator_core::domain::repository::{
    RepositoryError, VolumeRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner, InstanceId,
};
use aegis_orchestrator_core::domain::secrets::{SecretPath, SecretStore, SensitiveString};
use aegis_orchestrator_core::domain::shared_kernel::{TenantId, VolumeId};
use aegis_orchestrator_core::domain::volume::{
    AccessMode, FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeMount, VolumeOwnership,
    VolumeStatus,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::repositories::{
    InMemoryVolumeRepository, InMemoryWorkflowExecutionRepository, InMemoryWorkflowRepository,
};
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use aegis_orchestrator_core::infrastructure::workflow_parser::WorkflowParser;

const PERSON: &str = "person-sub";
const URL: &str = "https://git.example.test/org/repo.git";
const MISSING_URL: &str = "https://git.example.test/org/missing.git";
const TOKEN: &str = "Mk21-stored-token";

// ===========================================================================
// The step runner: the step's own script, run by the host's shell, with a
// git configuration that rewrites the test host to the local disk
// ===========================================================================

struct ShellStepRunner {
    home: tempfile::TempDir,
    seen: Mutex<Vec<ContainerStepConfig>>,
}

impl ShellStepRunner {
    /// A runner whose git reaches `https://git.example.test/org/<name>` at
    /// `<root>/<name>`.
    fn rewriting_to(root: &Path) -> Self {
        let home = tempfile::tempdir().expect("home for the step");
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[url \"file://{}/\"]\n\tinsteadOf = https://git.example.test/org/\n",
                root.display()
            ),
        )
        .expect("the step's git configuration");
        Self {
            home,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn steps(&self) -> usize {
        self.seen.lock().unwrap().len()
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
                .env("GIT_ALLOW_PROTOCOL", "file:https")
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
// In-memory repositories
// ===========================================================================

#[derive(Default)]
struct Bindings {
    bindings: RwLock<HashMap<GitRepoBindingId, GitRepoBinding>>,
}

impl Bindings {
    fn all(&self) -> Vec<GitRepoBinding> {
        self.bindings.read().unwrap().values().cloned().collect()
    }
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

#[derive(Default)]
struct Credentials(RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

#[async_trait]
impl CredentialBindingRepository for Credentials {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.0.write().unwrap().insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.0.read().unwrap().get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self
            .0
            .read()
            .unwrap()
            .values()
            .filter(|b| &b.tenant_id == tenant_id && b.owner_user_id == owner_user_id)
            .cloned()
            .collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _: &TenantId,
        _: &str,
        _: &CredentialProvider,
        _: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.0.write().unwrap().remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        _: &str,
        _: &CredentialBindingId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(
        &self,
        _: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<u64> {
        Ok(0)
    }
}

/// Creates SeaweedFS-shaped volumes, as production's storage does.
struct SeaweedVolumes {
    repo: Arc<InMemoryVolumeRepository>,
}

#[async_trait]
impl VolumeService for SeaweedVolumes {
    async fn create_volume(
        &self,
        name: String,
        tenant_id: TenantId,
        storage_class: StorageClass,
        size_limit_mb: u64,
        ownership: VolumeOwnership,
    ) -> anyhow::Result<VolumeId> {
        let id = VolumeId::new();
        self.repo
            .save(&Volume {
                id,
                name,
                tenant_id,
                storage_class,
                backend: VolumeBackend::SeaweedFS {
                    filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                    remote_path: format!("/aegis/seaweedfs/{}", id.0),
                },
                size_limit_bytes: size_limit_mb * 1024 * 1024,
                status: VolumeStatus::Available,
                ownership,
                created_at: chrono::Utc::now(),
                attached_at: None,
                detached_at: None,
                expires_at: None,
                host_node_id: None,
            })
            .await?;
        Ok(id)
    }
    async fn get_volume(&self, id: VolumeId) -> anyhow::Result<Volume> {
        self.repo
            .find_by_id(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("not found"))
    }
    async fn list_volumes_by_tenant(&self, tenant_id: TenantId) -> anyhow::Result<Vec<Volume>> {
        Ok(self.repo.find_by_tenant(tenant_id).await?)
    }
    async fn list_volumes_by_ownership(
        &self,
        ownership: &VolumeOwnership,
    ) -> anyhow::Result<Vec<Volume>> {
        Ok(self.repo.find_by_ownership(ownership).await?)
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
    async fn delete_volume(&self, volume_id: VolumeId) -> anyhow::Result<()> {
        self.repo.delete(volume_id).await?;
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
// Fixture
// ===========================================================================

/// Run `git` in `dir` with no configuration of the machine's.
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

struct Fixture {
    service: Arc<GitRepoService>,
    bindings: Arc<Bindings>,
    credentials: Arc<Credentials>,
    store: Arc<TestSecretStore>,
    volume_repo: Arc<InMemoryVolumeRepository>,
    runner: Arc<ShellStepRunner>,
    bare: PathBuf,
    tenant: TenantId,
    _dirs: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let dirs = tempfile::tempdir().unwrap();
    let remotes = dirs.path().join("remotes");
    let bare = remotes.join("repo.git");
    let seed = dirs.path().join("seed");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    git(&bare, &["init", "--bare", "--initial-branch=main"]);
    git(&seed, &["init", "--initial-branch=main"]);
    std::fs::write(seed.join("README.md"), "first line\n").unwrap();
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "first"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "origin", "main"]);

    let workspace = dirs.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let scratch = dirs.path().join("scratch").join("aegis-git");
    std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();

    let event_bus = Arc::new(EventBus::new(64));
    let volume_repo = Arc::new(InMemoryVolumeRepository::new());
    let user_volume_service = Arc::new(UserVolumeService::new(
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        Arc::new(SeaweedVolumes {
            repo: volume_repo.clone(),
        }) as Arc<dyn VolumeService>,
        event_bus.clone(),
        aegis_orchestrator_core::domain::volume::StorageTierLimits::default(),
    ));
    let store = Arc::new(TestSecretStore::new());
    let secrets_manager = Arc::new(SecretsManager::from_store(store.clone(), event_bus.clone()));
    let fsal = Arc::new(AegisFSAL::new(
        unused_storage_provider(),
        volume_repo.clone() as Arc<dyn VolumeRepository>,
        Arc::new(parking_lot::RwLock::new(HashMap::new())),
        Arc::new(NoEvents),
    ));
    let runner = Arc::new(ShellStepRunner::rewriting_to(&remotes));
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
    let credentials = Arc::new(Credentials::default());
    let service = Arc::new(
        GitRepoService::new(
            bindings.clone() as Arc<dyn GitRepoBindingRepository>,
            user_volume_service,
            clone_executor,
            secrets_manager,
            event_bus,
        )
        .with_credential_repo(credentials.clone() as Arc<dyn CredentialBindingRepository>),
    );
    Fixture {
        service,
        bindings,
        credentials,
        store,
        volume_repo,
        runner,
        bare,
        tenant: TenantId::consumer(),
        _dirs: dirs,
    }
}

impl Fixture {
    /// A credential binding of the person's, its token stored.
    async fn credential(&self, provider: &str, status: CredentialStatus) -> CredentialBindingId {
        let id = CredentialBindingId::new();
        let now = chrono::Utc::now();
        let secret_path = SecretPath::for_tenant(
            self.tenant.clone(),
            "kv",
            format!("users/{PERSON}/credentials/{}", id.0),
        );
        let mut secret = HashMap::new();
        secret.insert("value".to_string(), SensitiveString::new(TOKEN));
        self.store
            .write(&secret_path.effective_mount(), &secret_path.path, secret)
            .await
            .expect("store the secret");
        self.credentials
            .save(&UserCredentialBinding {
                id,
                owner_user_id: PERSON.to_string(),
                tenant_id: self.tenant.clone(),
                credential_type: CredentialType::Secret,
                provider: CredentialProvider::new(provider),
                secret_path,
                scope: CredentialScope::Personal,
                status,
                metadata: CredentialMetadata {
                    label: provider.to_string(),
                    tags: None,
                    service_url: None,
                    external_account_id: None,
                    oauth_scopes: None,
                    mailbox: None,
                    reach: None,
                    calendar: None,
                },
                grants: Vec::new(),
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("save the credential binding");
        id
    }

    /// A Ready binding of the person's of `url` on `git_ref`, never cloned.
    async fn existing_binding(&self, url: &str, git_ref: &str, label: &str) -> GitRepoBindingId {
        let volume = Volume::new(
            format!("git-{label}"),
            self.tenant.clone(),
            StorageClass::persistent(),
            VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                remote_path: format!("/aegis/seaweedfs/{label}"),
            },
            64 * 1024 * 1024,
            VolumeOwnership::persistent(PERSON),
        )
        .unwrap();
        self.volume_repo.save(&volume).await.unwrap();
        let mut binding = GitRepoBinding::new(
            self.tenant.clone(),
            None,
            url.to_string(),
            GitRef::Branch(git_ref.to_string()),
            None,
            volume.id,
            label.to_string(),
            CloneStrategy::EphemeralCli {
                reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
            },
            false,
            None,
            None,
            None,
        );
        binding.complete_clone("0".repeat(40), 0);
        let _ = binding.take_events();
        self.bindings.save(&binding).await.unwrap();
        binding.id
    }

    /// Resolve `value`'s entries for a run whose GitHub choice is `github`
    /// (Zaru ADR-0071 D1 (d)).
    async fn resolve(
        &self,
        value: Value,
        github: ServerChoice,
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        let requested = parse_requested_repositories(&value)
            .unwrap_or_else(|e| panic!("{value} was not admitted: {e}"));
        self.service
            .resolve_requested(
                &self.tenant,
                Some(PERSON),
                ZaruTier::Pro,
                &github,
                requested,
            )
            .await
    }
}

fn refusal<T: std::fmt::Debug>(result: Result<T, RunRepositoryError>) -> String {
    match result {
        Err(RunRepositoryError::Refused(sentence)) => sentence,
        other => panic!("the URL was not refused: {other:?}"),
    }
}

// ===========================================================================
// The resolution: reuse, create, refuse
// ===========================================================================

#[tokio::test]
async fn a_url_with_a_binding_of_that_url_and_ref_reuses_it_before_any_credential_is_read() {
    let fx = fixture().await;
    // The run names a GitHub binding (Zaru ADR-0071 D1 (d)); the binding of
    // that URL, made with no credential, is reused with no git step.
    let github = fx.credential("github", CredentialStatus::Active).await;
    let on_release = fx
        .existing_binding(
            "https://git.example.test/org/repo",
            "release",
            "repo-release",
        )
        .await;
    let on_main = fx
        .existing_binding("https://git.example.test/org/repo", "main", "repo")
        .await;

    let entries = fx
        .resolve(
            json!([{ "url": URL, "branch": "feature/x" }]),
            ServerChoice::Bindings(vec![github]),
        )
        .await
        .unwrap_or_else(|e| panic!("the URL with a binding of it on main was refused: {e:?}"));

    assert_eq!(
        entries,
        vec![RunRepository {
            binding_id: on_main,
            branch: Some("feature/x".to_string()),
            author: None,
            label: None,
            git_ref: None,
            started_from: None,
            tree_reset: None,
            purge: None,
            test_globs: None,
            tests: None,
        }],
        "the URL did not resolve to the binding of that URL and ref (main), \
         the one on release being {on_release}"
    );
    assert_eq!(
        fx.bindings.all().len(),
        2,
        "a binding was made beside the reused one"
    );
    assert_eq!(fx.runner.steps(), 0, "a git step ran for a reused binding");
}

#[tokio::test]
async fn a_url_with_no_binding_is_bound_with_the_persons_one_github_credential_and_cloned() {
    let fx = fixture().await;
    let github = fx.credential("github", CredentialStatus::Active).await;
    // Neither of these is the one: another provider's, and an inactive GitHub one.
    fx.credential("gitlab", CredentialStatus::Active).await;
    fx.credential("github", CredentialStatus::Revoked).await;

    let entries = fx
        .resolve(
            json!([{ "url": URL }]),
            ServerChoice::Bindings(vec![github]),
        )
        .await
        .unwrap_or_else(|e| panic!("the URL with one active GitHub credential was refused: {e:?}"));

    let made = fx.bindings.all();
    assert_eq!(made.len(), 1, "not exactly one binding was made: {made:?}");
    let binding = &made[0];
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].binding_id, binding.id,
        "the entry names another binding"
    );
    assert_eq!(
        entries[0].branch, None,
        "the entry gained a branch it was not given"
    );
    assert_eq!(
        binding.label, "repo",
        "the label is not the repository's name"
    );
    assert_eq!(binding.credential_binding_id, Some(github));
    assert_eq!(binding.repo_url.expose(), URL);
    assert_eq!(binding.git_ref, GitRef::Branch("main".to_string()));
    assert_eq!(
        binding.status,
        GitRepoStatus::Ready,
        "the clone was not awaited to Ready"
    );
    assert_eq!(
        binding.last_commit_sha.as_deref(),
        Some(git(&fx.bare, &["rev-parse", "refs/heads/main"]).as_str()),
        "the binding was not cloned from the repository's main"
    );

    // Then the run proceeds as with a binding_id.
    let run = uuid::Uuid::new_v4();
    let prepared = fx
        .service
        .prepare_for_run(&fx.tenant, Some(PERSON), run, &entries)
        .await
        .unwrap_or_else(|e| panic!("the made binding was not prepared for the run: {e:?}"));
    assert_eq!(prepared[0].label.as_deref(), Some("repo"));
    assert_eq!(prepared[0].git_ref.as_deref(), Some("main"));
}

#[tokio::test]
async fn a_url_is_refused_when_the_person_has_no_active_github_credential() {
    let fx = fixture().await;
    fx.credential("gitlab", CredentialStatus::Active).await;
    let revoked = fx.credential("github", CredentialStatus::Revoked).await;

    let sentence = refusal(
        fx.resolve(
            json!([{ "url": URL }]),
            ServerChoice::Bindings(vec![revoked]),
        )
        .await,
    );

    assert_eq!(sentence, "no active GitHub credential of yours");
    assert!(fx.bindings.all().is_empty(), "a binding was made");
    assert_eq!(fx.runner.steps(), 0, "a git step ran");
}

#[tokio::test]
async fn a_url_is_refused_when_the_person_has_two_active_github_credentials() {
    let fx = fixture().await;
    let first = fx.credential("github", CredentialStatus::Active).await;
    let second = fx.credential("github", CredentialStatus::Active).await;

    let sentence = refusal(
        fx.resolve(
            json!([{ "url": URL }]),
            ServerChoice::Bindings(vec![first, second]),
        )
        .await,
    );

    assert_eq!(
        sentence,
        "you have 2 active GitHub credentials, so which one reaches this repository is not known; link the repository with the one to use, then name its binding_id"
    );
    assert!(fx.bindings.all().is_empty(), "a binding was made");
    assert_eq!(fx.runner.steps(), 0, "a git step ran");
}

#[tokio::test]
async fn a_url_the_clone_cannot_reach_is_refused_with_the_clones_sentence_and_leaves_no_binding() {
    let fx = fixture().await;
    let github = fx.credential("github", CredentialStatus::Active).await;

    let sentence = refusal(
        fx.resolve(
            json!([{ "url": MISSING_URL }]),
            ServerChoice::Bindings(vec![github]),
        )
        .await,
    );

    assert!(
        sentence.starts_with("git: ") && sentence.contains("missing.git"),
        "the refusal is not the clone's own sentence: {sentence}"
    );
    assert!(
        !sentence.contains(TOKEN),
        "the refusal holds the credential: {sentence}"
    );
    assert!(
        fx.bindings.all().is_empty(),
        "the binding the failed clone was made for was left: {:?}",
        fx.bindings.all()
    );
    assert_eq!(
        fx.runner.steps(),
        1,
        "a step ran beside the one clone (no run's container may start)"
    );
}

// ===========================================================================
// The workflow start: resolved, then prepared
// ===========================================================================

const MADE: &str = "8a1d2c3b-4e5f-4a6b-9c7d-0e1f2a3b4c5d";

fn manifest(name: &str) -> String {
    format!(
        r#"apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: {name}
  version: "1.0.0"
spec:
  initial_state: START
  repositories: 1
  states:
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
    repositories: Mutex<Vec<Option<Value>>>,
}

#[async_trait]
impl WorkflowEnginePort for RecordingEngine {
    async fn register_workflow(&self, _def: &TemporalWorkflowDefinition) -> anyhow::Result<()> {
        Ok(())
    }

    async fn start_workflow(&self, params: StartWorkflowParams<'_>) -> anyhow::Result<String> {
        self.repositories
            .lock()
            .unwrap()
            .push(params.repositories.clone());
        Ok("recorded-run-id".to_string())
    }
}

/// Resolves every URL to [`MADE`], or refuses it, and records the order of
/// what it was asked.
struct Resolving {
    refuse: Option<&'static str>,
    calls: Mutex<Vec<String>>,
    /// The GitHub choice each resolution was given (Zaru ADR-0071 D1 (d)).
    github: Mutex<Vec<ServerChoice>>,
}

#[async_trait]
impl RunRepositories for Resolving {
    async fn resolve_requested(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _tier: ZaruTier,
        github: &ServerChoice,
        requested: Vec<RequestedRepository>,
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        self.calls.lock().unwrap().push("resolve".to_string());
        self.github.lock().unwrap().push(github.clone());
        if let Some(sentence) = self.refuse {
            return Err(RunRepositoryError::Refused(sentence.to_string()));
        }
        Ok(requested
            .into_iter()
            .map(|entry| match entry {
                RequestedRepository::Binding(entry) => entry,
                RequestedRepository::Url(by_url) => RunRepository {
                    binding_id: GitRepoBindingId(uuid::Uuid::parse_str(MADE).unwrap()),
                    branch: by_url.branch,
                    author: None,
                    label: None,
                    git_ref: None,
                    started_from: None,
                    tree_reset: None,
                    purge: None,
                    test_globs: None,
                    tests: None,
                },
            })
            .collect())
    }

    async fn prepare_for_run(
        &self,
        _tenant_id: &TenantId,
        _person: Option<&str>,
        _run: uuid::Uuid,
        entries: &[RunRepository],
    ) -> Result<Vec<RunRepository>, RunRepositoryError> {
        self.calls.lock().unwrap().push(format!(
            "prepare {}",
            serde_json::to_value(entries).unwrap()
        ));
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

async fn start_with(
    refuse: Option<&'static str>,
) -> (
    StandardStartWorkflowExecutionUseCase,
    Arc<RecordingEngine>,
    Arc<Resolving>,
) {
    let workflow = WorkflowParser::parse_yaml(&manifest("the-forge")).expect("the manifest parses");
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
    let repositories = Arc::new(Resolving {
        refuse,
        calls: Mutex::new(Vec::new()),
        github: Mutex::new(Vec::new()),
    });
    use_case.set_repositories(repositories.clone());
    (use_case, engine, repositories)
}

fn request(input: Value) -> StartWorkflowExecutionRequest {
    StartWorkflowExecutionRequest {
        workflow_id: "the-forge".to_string(),
        input,
        blackboard: None,
        version: None,
        tenant_id: Some(TenantId::consumer()),
        security_context_name: None,
        intent: None,
    }
}

#[tokio::test]
async fn a_workflow_start_resolves_a_url_entry_before_its_repositories_are_prepared() {
    let (use_case, engine, repositories) = start_with(None).await;

    use_case
        .start_execution(request(json!({
            "task": "x",
            "repositories": [{ "url": URL, "ref": "main", "branch": "feature/x" }]
        })))
        .await
        .unwrap_or_else(|e| panic!("a start naming its repository by URL was refused: {e:#}"));

    assert_eq!(
        *repositories.calls.lock().unwrap(),
        vec![
            "resolve".to_string(),
            format!("prepare [{{\"binding_id\":\"{MADE}\",\"branch\":\"feature/x\"}}]"),
        ],
        "the URL was not resolved to its binding before the run was prepared"
    );
    assert_eq!(
        *engine.repositories.lock().unwrap(),
        vec![Some(json!([{ "binding_id": MADE, "branch": "feature/x" }]))],
        "the run did not start with the binding the URL resolved to"
    );
}

#[tokio::test]
async fn a_workflow_start_whose_url_is_refused_prepares_nothing_and_starts_nothing() {
    let (use_case, engine, repositories) =
        start_with(Some("no active GitHub credential of yours")).await;

    let refused = use_case
        .start_execution(request(json!({
            "task": "x",
            "repositories": [{ "url": URL }]
        })))
        .await
        .expect_err("a start whose URL was refused started");

    match refused.downcast_ref::<ExecutionError>() {
        Some(ExecutionError::Refused(sentence)) => {
            assert_eq!(sentence, "no active GitHub credential of yours")
        }
        _ => panic!("the start failed, not refused: {refused:#}"),
    }
    assert_eq!(
        *repositories.calls.lock().unwrap(),
        vec!["resolve".to_string()],
        "a repository was prepared after the URL was refused"
    );
    assert!(
        engine.repositories.lock().unwrap().is_empty(),
        "Temporal was started"
    );
}

// Zaru ADR-0071 D1 (d): the URL form only through the run's named GitHub
// binding, with this file's fixture.
#[path = "deny_by_default_orchestrator_2/repository.rs"]
mod deny_by_default_orchestrator_2;
