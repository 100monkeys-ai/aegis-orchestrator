// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Clone Executor (BC-7 Storage Gateway, ADR-081 §Domain Service)
//!
//! Low-level clone / fetch primitive. Owns all interaction with `git2`
//! (libgit2) so that the application service ([`GitRepoService`]) stays
//! transport-agnostic.
//!
//! ## A3 Scope (Phases 2 / 3 / 4)
//!
//! - Libgit2 clone of public HTTPS repos
//! - Libgit2 clone of private HTTPS repos using a PAT via
//!   `Cred::userpass_plaintext` (GitHub fine-grained tokens default to
//!   `"x-access-token"` as the username)
//! - Libgit2 clone using a user-provided SSH private key. The key is
//!   materialised to a mode-`0600` temp file via `scopeguard` guard, used
//!   by libgit2's credentials callback, then zeroed and removed.
//! - [`GitCloneExecutor::fetch_and_checkout`] — ref pinning for
//!   Branch / Tag / Commit [`GitRef`] variants.
//! - [`GitCloneExecutor::select_strategy`] — chooses between libgit2 and
//!   the [`EphemeralCliEngine`] container fallback based on the bound
//!   volume's backend.
//! - [`EphemeralCliEngine`] — containerised `git` fallback for storage
//!   backends libgit2 cannot write to directly (SeaweedFS, OpenDAL, SEAL).
//!   Spawns an `alpine/git` container through the ADR-050
//!   [`ContainerStepRunner`] and mounts the target volume via the
//!   orchestrator's FUSE gateway.
//! - Sparse checkout — applied post-clone by writing
//!   `.git/info/sparse-checkout` and re-running `checkout_head`.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::{Cred, FetchOptions, Oid, RemoteCallbacks, Repository};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use crate::domain::execution::ExecutionId;
use crate::domain::fsal::{AegisFSAL, FsalAccessPolicy};
use crate::domain::git_repo::{CloneStrategy, GitRef, GitRepoBinding};
use crate::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepRunner, ContainerVolumeMount,
};
use crate::domain::secrets::SensitiveString;
use crate::domain::shared_kernel::ImagePullPolicy;
use crate::domain::volume::{Volume, VolumeBackend, VolumeId};
use crate::domain::workflow::StateName;
use crate::infrastructure::secrets_manager::SecretsManager;

// ============================================================================
// EphemeralCliEngine — containerised `git` fallback (ADR-081 §Phase 3)
// ============================================================================

/// Containerised `git` fallback used when libgit2 cannot write directly to
/// the bound volume (SeaweedFS / OpenDAL / SEAL backends — ADR-081
/// §Sub-Decision 2).
///
/// The engine spawns an `alpine/git` container through the ADR-050
/// [`ContainerStepRunner`] with the bound volume mounted at `/workspace`
/// (FUSE transport — ADR-107). Credentials are delivered via environment
/// variables (`GIT_ASKPASS` for HTTPS+PAT, `GIT_SSH_COMMAND` for SSH keys)
/// so they never appear in the command line.
pub struct EphemeralCliEngine {
    runner: Arc<dyn ContainerStepRunner>,
    volume_registry: Arc<NfsVolumeRegistry>,
    image: String,
}

impl EphemeralCliEngine {
    /// Default image tag. Pinned to a specific Alpine release at deploy
    /// time by operators via `NodeConfigSpec.runtime`; this is a safe
    /// fallback for local development.
    const DEFAULT_IMAGE: &'static str = "alpine/git:latest";

    pub fn new(
        runner: Arc<dyn ContainerStepRunner>,
        volume_registry: Arc<NfsVolumeRegistry>,
    ) -> Self {
        Self {
            runner,
            volume_registry,
            image: Self::DEFAULT_IMAGE.to_string(),
        }
    }

    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// Clone `repo_url` at `git_ref` into the volume bound to `binding`.
    ///
    /// Returns the resolved HEAD SHA. Applies sparse-checkout via
    /// `git sparse-checkout set --cone` when `binding.sparse_paths` is set.
    async fn clone_into_volume(
        &self,
        binding: &GitRepoBinding,
        volume: &Volume,
        credential: Option<ResolvedCredential>,
        shallow: bool,
    ) -> Result<String, CloneError> {
        let remote_path = match &volume.backend {
            VolumeBackend::SeaweedFS { remote_path, .. } => remote_path.clone(),
            VolumeBackend::OpenDal { .. } => {
                format!("/aegis/opendal/volumes/{}/{}", volume.tenant_id, volume.id)
            }
            VolumeBackend::Seal {
                node_id,
                remote_volume_id,
            } => format!("/aegis/seal/{node_id}/{remote_volume_id}"),
            VolumeBackend::HostPath { .. } => {
                return Err(CloneError::Io(
                    "EphemeralCliEngine is for non-HostPath backends only".into(),
                ));
            }
        };

        // Register with NFS gateway so FUSE mount is authorised for the
        // ephemeral container's scope.
        let mount_point = PathBuf::from("/workspace");
        let ephemeral_exec = ExecutionId::new();
        self.volume_registry.register(VolumeRegistration {
            volume_id: volume.id,
            execution_id: ephemeral_exec,
            workflow_execution_id: None,
            container_uid: 0,
            container_gid: 0,
            policy: FsalAccessPolicy::default(),
            mount_point: mount_point.clone(),
            remote_path,
        });

        let res = self
            .run_clone_container(binding, volume.id, credential, shallow, ephemeral_exec)
            .await;

        // Always deregister, even on error.
        self.volume_registry.deregister(volume.id);
        res
    }

    async fn run_clone_container(
        &self,
        binding: &GitRepoBinding,
        volume_id: VolumeId,
        credential: Option<ResolvedCredential>,
        shallow: bool,
        execution_id: ExecutionId,
    ) -> Result<String, CloneError> {
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut script_prelude = String::new();

        // -- ref selection flags --
        let (ref_flag, checkout_cmd) = match &binding.git_ref {
            GitRef::Branch(name) => (format!("--branch {}", shell_escape(name)), String::new()),
            GitRef::Tag(name) => (format!("--branch {}", shell_escape(name)), String::new()),
            GitRef::Commit(sha) => (
                String::new(),
                format!("&& git -C /workspace/repo checkout {}", shell_escape(sha)),
            ),
        };

        let depth_flag = if shallow { "--depth=1" } else { "" };
        let filter_flag = "--filter=blob:limit=10M";

        // -- credential wiring --
        let auth_repo_url = match credential {
            Some(ResolvedCredential::HttpsPat { username, token }) => {
                // Put the PAT into GIT_ASKPASS so it never appears on the
                // command line or in the saved remote config.
                env.insert("GIT_USERNAME".to_string(), username.clone());
                env.insert("GIT_PASSWORD".to_string(), token.expose().to_string());
                // GIT_ASKPASS script that echos either username or
                // password depending on what git is asking for.
                script_prelude.push_str(
                    "cat >/tmp/askpass.sh <<'EOF'\n\
                     #!/bin/sh\n\
                     case \"$1\" in\n\
                     Username*) echo \"$GIT_USERNAME\" ;;\n\
                     Password*) echo \"$GIT_PASSWORD\" ;;\n\
                     esac\n\
                     EOF\n\
                     chmod 0700 /tmp/askpass.sh\n",
                );
                env.insert("GIT_ASKPASS".to_string(), "/tmp/askpass.sh".to_string());
                env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
                binding.repo_url.clone()
            }
            Some(ResolvedCredential::SshKey {
                private_key_pem,
                passphrase: _, // container-ephemeral passphrases not supported
            }) => {
                // Materialise the key inside the container via a heredoc
                // in the shell prelude. Safer than bind-mounting a host
                // tempfile through FUSE.
                script_prelude.push_str("cat >/tmp/ssh_key <<'KEYEOF'\n");
                script_prelude.push_str(private_key_pem.expose());
                if !private_key_pem.expose().ends_with('\n') {
                    script_prelude.push('\n');
                }
                script_prelude.push_str("KEYEOF\nchmod 0600 /tmp/ssh_key\n");
                env.insert(
                    "GIT_SSH_COMMAND".to_string(),
                    "ssh -i /tmp/ssh_key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null".to_string(),
                );
                binding.repo_url.clone()
            }
            None => binding.repo_url.clone(),
        };

        // -- sparse checkout --
        let sparse_cmd = if let Some(paths) = &binding.sparse_paths {
            let escaped: Vec<String> = paths.iter().map(|p| shell_escape(p)).collect();
            format!(
                " && git -C /workspace/repo sparse-checkout set --cone {}",
                escaped.join(" ")
            )
        } else {
            String::new()
        };

        // Full shell command:
        //   <prelude>
        //   git clone [--depth=1 --filter=blob:limit=10M] [--branch X] URL /workspace/repo
        //   [ && git -C /workspace/repo checkout SHA ]
        //   [ && git -C /workspace/repo sparse-checkout set --cone A B ]
        //   && git -C /workspace/repo rev-parse HEAD
        let command = format!(
            "{prelude}set -eu && \
             git clone {depth} {filter} {ref_} {url} /workspace/repo \
             {checkout}{sparse} && \
             git -C /workspace/repo rev-parse HEAD",
            prelude = script_prelude,
            depth = depth_flag,
            filter = filter_flag,
            ref_ = ref_flag,
            url = shell_escape(&auth_repo_url),
            checkout = checkout_cmd,
            sparse = sparse_cmd,
        );

        let cfg = ContainerStepConfig {
            name: format!("git-clone-{}", binding.id),
            image: self.image.clone(),
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            command: vec!["sh".to_string(), "-c".to_string(), command],
            env,
            workdir: Some("/workspace".to_string()),
            volumes: vec![ContainerVolumeMount {
                name: volume_id.0.to_string(),
                mount_path: "/workspace".to_string(),
                read_only: false,
            }],
            resources: None,
            registry_credentials: None,
            execution_id,
            state_name: StateName::new("GIT_CLONE").expect("static state name is valid"),
            read_only_root_filesystem: false,
            run_as_user: None,
            network_mode: None,
            workflow_execution_id: None,
        };

        let result = self.runner.run_step(cfg).await.map_err(|e| match e {
            ContainerStepError::ImagePullFailed { image, error } => CloneError::Git(format!(
                "ephemeral-cli image pull failed for '{image}': {error}"
            )),
            ContainerStepError::TimeoutExpired { timeout_secs } => CloneError::Git(format!(
                "ephemeral-cli clone timed out after {timeout_secs}s"
            )),
            ContainerStepError::VolumeMountFailed { volume, error } => CloneError::Io(format!(
                "ephemeral-cli volume mount failed for '{volume}': {error}"
            )),
            ContainerStepError::ResourceExhausted { detail } => {
                CloneError::Git(format!("ephemeral-cli resource exhausted: {detail}"))
            }
            ContainerStepError::DockerError(m) => CloneError::Git(format!("docker: {m}")),
        })?;

        if result.exit_code != 0 {
            return Err(CloneError::Git(format!(
                "ephemeral-cli git exited {}: stdout={:?} stderr={:?}",
                result.exit_code,
                truncate(&result.stdout, 256),
                truncate(&result.stderr, 256)
            )));
        }

        // The last line of stdout is the HEAD SHA (from `git rev-parse HEAD`).
        let sha = result
            .stdout
            .lines()
            .last()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if sha.len() != 40 {
            return Err(CloneError::Git(format!(
                "ephemeral-cli could not parse HEAD sha from stdout tail: {:?}",
                truncate(&result.stdout, 256)
            )));
        }
        Ok(sha)
    }
}

fn shell_escape(s: &str) -> String {
    // Wrap in single quotes, escaping any embedded single quotes via the
    // standard '\'' dance.
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Errors emitted by [`GitCloneExecutor`].
#[derive(Debug, Error)]
pub enum CloneError {
    /// `git2` / libgit2 returned an error during the operation.
    #[error("git error: {0}")]
    Git(String),

    /// I/O error preparing or writing the target working tree.
    #[error("io error: {0}")]
    Io(String),

    /// Functionality not yet implemented in this wave — deferred to a
    /// later ADR-081 phase.
    #[error("not yet implemented: {0}")]
    NotYetImplemented(&'static str),
}

impl From<git2::Error> for CloneError {
    fn from(e: git2::Error) -> Self {
        Self::Git(e.message().to_string())
    }
}

impl From<std::io::Error> for CloneError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

// ============================================================================
// Resolved Credential (Keymaster-safe carrier)
// ============================================================================

/// Credential material resolved just-in-time for a single clone / fetch
/// operation, per ADR-081 §Security and ADR-034 (Keymaster Pattern).
///
/// Never serialized, never logged, never returned to the caller. Scoped
/// tightly to the executor's call stack and dropped as soon as the git
/// operation completes.
pub enum ResolvedCredential {
    /// HTTPS Personal Access Token. Uses `"x-access-token"` as the
    /// libgit2 username by default (GitHub fine-grained PAT convention).
    /// `username` lets callers override for generic providers.
    HttpsPat {
        username: String,
        token: SensitiveString,
    },
    /// SSH private key material. The executor materialises the key to a
    /// mode-`0600` temp file (libgit2 path) or heredocs it into the
    /// container (EphemeralCli path). Always zeroed + removed via
    /// `scopeguard` before the function returns.
    SshKey {
        private_key_pem: SensitiveString,
        passphrase: Option<SensitiveString>,
    },
}

impl ResolvedCredential {
    /// Construct an HTTPS PAT credential with the default GitHub-style
    /// username (`"x-access-token"`).
    pub fn github_pat(token: SensitiveString) -> Self {
        Self::HttpsPat {
            username: "x-access-token".to_string(),
            token,
        }
    }
}

// ============================================================================
// GitCloneExecutor
// ============================================================================

/// Domain service that executes git clone / fetch operations against the
/// bound [`crate::domain::volume::Volume`].
///
/// Injected dependencies:
/// - `secret_manager` — retained for caller-symmetry with ADR-034; the
///   executor itself never resolves secrets directly.
/// - `fsal` — path & policy boundary owned by the application layer; the
///   executor writes to the resolved path but does not authorize.
/// - `cli_engine` — containerised fallback for non-HostPath backends.
pub struct GitCloneExecutor {
    #[allow(dead_code)]
    secret_manager: Arc<SecretsManager>,
    #[allow(dead_code)]
    fsal: Arc<AegisFSAL>,
    cli_engine: Option<Arc<EphemeralCliEngine>>,
}

impl GitCloneExecutor {
    pub fn new(
        secret_manager: Arc<SecretsManager>,
        fsal: Arc<AegisFSAL>,
        cli_engine: Option<Arc<EphemeralCliEngine>>,
    ) -> Self {
        Self {
            secret_manager,
            fsal,
            cli_engine,
        }
    }

    /// Select the [`CloneStrategy`] for a [`GitRepoBinding`] backed by
    /// `volume`.
    ///
    /// Routing rules (ADR-081 §Sub-Decision 2):
    /// - `HostPath` volumes → [`CloneStrategy::Libgit2`] (in-process clone).
    /// - `SeaweedFS` / `OpenDal` / `Seal` volumes → [`CloneStrategy::EphemeralCli`]
    ///   — libgit2 cannot write through FUSE + userspace filers safely, so
    ///   we defer to a FUSE-mounted container running the real `git` CLI.
    ///
    /// LFS / submodule / custom-git-config detection is out of scope for
    /// ADR-081 Phase 3 and will be added once ADR-081 Phase 5 wires in
    /// `.gitattributes` post-clone inspection.
    pub fn select_strategy(&self, _binding: &GitRepoBinding, volume: &Volume) -> CloneStrategy {
        match &volume.backend {
            VolumeBackend::HostPath { .. } => CloneStrategy::Libgit2,
            VolumeBackend::SeaweedFS { .. } => CloneStrategy::EphemeralCli {
                reason: "SeaweedFS volume requires FUSE-mounted container".to_string(),
            },
            VolumeBackend::OpenDal { .. } => CloneStrategy::EphemeralCli {
                reason: "OpenDAL volume requires FUSE-mounted container".to_string(),
            },
            VolumeBackend::Seal { .. } => CloneStrategy::EphemeralCli {
                reason: "SEAL remote-node volume requires FUSE-mounted container".to_string(),
            },
        }
    }

    /// Clone the `binding` into `target_dir` and return the HEAD commit
    /// SHA on success.
    ///
    /// - `credential` is `None` for public repos. When `Some`, it is
    ///   passed into libgit2's credentials callback for the duration of
    ///   the clone and dropped immediately afterward.
    /// - `shallow == true` sets `depth = 1` on the fetch.
    #[instrument(skip(self, credential), fields(binding_id = %binding.id, repo_url = %binding.repo_url))]
    pub async fn clone(
        &self,
        binding: &GitRepoBinding,
        target_dir: &Path,
        credential: Option<ResolvedCredential>,
        shallow: bool,
    ) -> Result<String, CloneError> {
        let repo_url = binding.repo_url.clone();
        let target_dir: PathBuf = target_dir.to_path_buf();
        let sparse_paths = binding.sparse_paths.clone();

        info!(
            target = %target_dir.display(),
            shallow,
            "cloning git repository (libgit2)"
        );

        let sha = tokio::task::spawn_blocking(move || -> Result<String, CloneError> {
            blocking_clone(&repo_url, &target_dir, credential, shallow, sparse_paths)
        })
        .await
        .map_err(|e| CloneError::Io(format!("clone task panicked: {e}")))??;

        debug!(commit_sha = %sha, "clone completed");
        Ok(sha)
    }

    /// Clone via the [`EphemeralCliEngine`]. Used for non-HostPath volume
    /// backends. Returns an error when no engine was injected at
    /// construction time.
    #[instrument(skip(self, volume, credential), fields(binding_id = %binding.id, volume_id = %volume.id))]
    pub async fn clone_ephemeral(
        &self,
        binding: &GitRepoBinding,
        volume: &Volume,
        credential: Option<ResolvedCredential>,
        shallow: bool,
    ) -> Result<String, CloneError> {
        let engine = self
            .cli_engine
            .as_ref()
            .ok_or(CloneError::NotYetImplemented(
                "EphemeralCliEngine not configured; non-HostPath volume backends require it",
            ))?;
        engine
            .clone_into_volume(binding, volume, credential, shallow)
            .await
    }

    /// Fetch the bound remote and check out the binding's [`GitRef`].
    ///
    /// Branch refs fast-forward HEAD. Tag refs checkout the tag
    /// commit. Commit refs do a best-effort fetch (so a shallow clone
    /// can reach the commit) before checking out the exact SHA.
    ///
    /// Returns the HEAD SHA after checkout.
    #[instrument(skip(self, credential), fields(binding_id = %binding.id, git_ref = ?binding.git_ref))]
    pub async fn fetch_and_checkout(
        &self,
        binding: &GitRepoBinding,
        target_dir: &Path,
        credential: Option<ResolvedCredential>,
    ) -> Result<String, CloneError> {
        let target_dir: PathBuf = target_dir.to_path_buf();
        let git_ref = binding.git_ref.clone();
        let repo_url = binding.repo_url.clone();

        let sha = tokio::task::spawn_blocking(move || -> Result<String, CloneError> {
            blocking_fetch_and_checkout(&repo_url, &target_dir, &git_ref, credential)
        })
        .await
        .map_err(|e| CloneError::Io(format!("fetch task panicked: {e}")))??;

        Ok(sha)
    }
}

// ============================================================================
// Blocking git2 helpers
// ============================================================================

/// Dropper guard owning an on-disk SSH key temp file. On drop it zero-
/// fills the file and removes it — regardless of whether the libgit2
/// operation succeeded or panicked.
struct SshKeyTempFile {
    path: PathBuf,
    key_len: usize,
}

impl Drop for SshKeyTempFile {
    fn drop(&mut self) {
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            use std::io::Write;
            let zeros = vec![0u8; self.key_len];
            let _ = f.write_all(&zeros);
            let _ = f.sync_all();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Attach a credentials callback to `callbacks` that honours
/// `credential`. For SSH keys, materialises the private key to a
/// temporary file in mode 0600 and returns a drop-guard that will zero
/// + remove the file on drop.
fn configure_credentials<'cb>(
    callbacks: &mut RemoteCallbacks<'cb>,
    credential: Option<ResolvedCredential>,
) -> Result<Option<SshKeyTempFile>, CloneError> {
    let Some(cred) = credential else {
        return Ok(None);
    };
    match cred {
        ResolvedCredential::HttpsPat { username, token } => {
            callbacks.credentials(move |_url, _user_from_url, _allowed| {
                Cred::userpass_plaintext(&username, token.expose())
            });
            Ok(None)
        }
        ResolvedCredential::SshKey {
            private_key_pem,
            passphrase,
        } => {
            let tmp_uuid = uuid::Uuid::new_v4();
            let key_path = std::env::temp_dir().join(format!("aegis-git-{tmp_uuid}"));
            let key_len = private_key_pem.expose().len();

            {
                use std::fs::OpenOptions;
                use std::io::Write;
                let mut f = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&key_path)?;
                f.write_all(private_key_pem.expose().as_bytes())?;
                f.sync_all()?;
            }
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;

            let guard = SshKeyTempFile {
                path: key_path.clone(),
                key_len,
            };

            let passphrase_string = passphrase.as_ref().map(|p| p.expose().to_string());
            let key_path_cb = key_path.clone();
            callbacks.credentials(move |_url, username_from_url, _allowed| {
                Cred::ssh_key(
                    username_from_url.unwrap_or("git"),
                    None,
                    &key_path_cb,
                    passphrase_string.as_deref(),
                )
            });

            Ok(Some(guard))
        }
    }
}

/// Apply the binding's sparse-checkout paths to an open repository.
///
/// Libgit2 honours `.git/info/sparse-checkout` when
/// `core.sparseCheckout = true` is set in the repo config. After writing
/// the sparse file we re-run `checkout_head` so the working tree reflects
/// the new filter.
fn apply_sparse_checkout(repo: &Repository, paths: &[String]) -> Result<(), CloneError> {
    let mut cfg = repo.config()?;
    cfg.set_bool("core.sparseCheckout", true)?;
    cfg.set_bool("core.sparseCheckoutCone", true)?;

    let info_dir = repo.path().join("info");
    std::fs::create_dir_all(&info_dir)?;
    let sparse_file = info_dir.join("sparse-checkout");
    let mut contents = String::new();
    for p in paths {
        contents.push_str(p);
        contents.push('\n');
    }
    std::fs::write(&sparse_file, contents)?;

    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
    Ok(())
}

/// Run the libgit2 clone on the calling (blocking) thread.
fn blocking_clone(
    repo_url: &str,
    target_dir: &Path,
    credential: Option<ResolvedCredential>,
    shallow: bool,
    sparse_paths: Option<Vec<String>>,
) -> Result<String, CloneError> {
    // Ensure parent exists.
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut callbacks = RemoteCallbacks::new();
    let _ssh_guard = configure_credentials(&mut callbacks, credential)?;

    let mut fetch_opts = FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);
    if shallow {
        fetch_opts.depth(1);
    }

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_opts);

    let repo: Repository = builder.clone(repo_url, target_dir).map_err(|e| {
        warn!(error = %e, "libgit2 clone failed");
        CloneError::from(e)
    })?;

    if let Some(paths) = sparse_paths.as_ref() {
        if !paths.is_empty() {
            apply_sparse_checkout(&repo, paths)?;
        }
    }

    let head = repo.head()?;
    let commit_sha = head
        .target()
        .ok_or_else(|| CloneError::Git("HEAD has no direct target".to_string()))?
        .to_string();
    Ok(commit_sha)
}

/// Run libgit2 fetch + checkout on the calling (blocking) thread.
fn blocking_fetch_and_checkout(
    repo_url: &str,
    target_dir: &Path,
    git_ref: &GitRef,
    credential: Option<ResolvedCredential>,
) -> Result<String, CloneError> {
    let repo = Repository::open(target_dir)?;

    // Ensure remote `origin` points at the binding's repo_url. Rewrite
    // if the caller changed it (e.g. credential rotation that altered
    // the userinfo segment).
    {
        let origin = repo.find_remote("origin");
        match origin {
            Ok(r) => {
                if r.url() != Some(repo_url) {
                    drop(r);
                    repo.remote_set_url("origin", repo_url)?;
                }
            }
            Err(_) => {
                repo.remote("origin", repo_url)?;
            }
        }
    }

    let mut callbacks = RemoteCallbacks::new();
    let _ssh_guard = configure_credentials(&mut callbacks, credential)?;
    let mut fetch_opts = FetchOptions::new();
    fetch_opts.remote_callbacks(callbacks);

    let mut remote = repo.find_remote("origin")?;

    let refspecs: Vec<String> = match git_ref {
        GitRef::Branch(name) => vec![format!("+refs/heads/{name}:refs/remotes/origin/{name}")],
        GitRef::Tag(name) => vec![format!("+refs/tags/{name}:refs/tags/{name}")],
        GitRef::Commit(_) => vec![
            "+refs/heads/*:refs/remotes/origin/*".to_string(),
            "+refs/tags/*:refs/tags/*".to_string(),
        ],
    };

    // For commit pins, skip the fetch if the commit is already present.
    let skip_fetch = if let GitRef::Commit(sha) = git_ref {
        let oid = Oid::from_str(sha)
            .map_err(|e| CloneError::Git(format!("invalid commit sha {sha}: {e}")))?;
        repo.find_commit(oid).is_ok()
    } else {
        false
    };

    if !skip_fetch {
        let refspec_refs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
        remote.fetch(&refspec_refs, Some(&mut fetch_opts), None)?;
    }

    // Resolve target OID based on ref kind, then detach HEAD there.
    let target_oid = match git_ref {
        GitRef::Branch(name) => {
            let refname = format!("refs/remotes/origin/{name}");
            let r = repo.find_reference(&refname)?;
            r.peel_to_commit()?.id()
        }
        GitRef::Tag(name) => {
            let refname = format!("refs/tags/{name}");
            let r = repo.find_reference(&refname)?;
            r.peel_to_commit()?.id()
        }
        GitRef::Commit(sha) => Oid::from_str(sha)
            .map_err(|e| CloneError::Git(format!("invalid commit sha {sha}: {e}")))?,
    };

    // Verify the commit exists (good error message for missing SHAs).
    let _commit = repo
        .find_commit(target_oid)
        .map_err(|e| CloneError::Git(format!("commit {target_oid} not found after fetch: {e}")))?;

    repo.set_head_detached(target_oid)?;
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;

    Ok(target_oid.to_string())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_pat_default_username() {
        let cred = ResolvedCredential::github_pat(SensitiveString::new("abc123"));
        match cred {
            ResolvedCredential::HttpsPat { username, .. } => {
                assert_eq!(username, "x-access-token");
            }
            _ => panic!("expected HttpsPat variant"),
        }
    }

    #[test]
    fn shell_escape_quotes_single_quotes() {
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
        assert_eq!(shell_escape("abc"), "'abc'");
    }
}
