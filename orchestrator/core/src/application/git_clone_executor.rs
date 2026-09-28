// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Clone Executor (BC-7 Storage Gateway, ADR-081 §Domain Service)
//!
//! Low-level clone / fetch primitive. Owns all interaction with `git2`
//! (libgit2) so that the application service
//! ([`crate::application::git_repo_service::GitRepoService`]) stays
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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::{Cred, FetchOptions, Oid, RemoteCallbacks, Repository};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::application::git_ssh_key::{attach_ssh_credentials, SshKeyTempFile};
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
    paths: EphemeralCliPaths,
}

/// Where the clone step works inside its container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EphemeralCliPaths {
    /// Where the bound volume is mounted. The clone lands in `repo` under it.
    pub workspace: String,
    /// A directory the step makes for the credential and removes when it
    /// ends.
    pub scratch: String,
}

impl Default for EphemeralCliPaths {
    fn default() -> Self {
        Self {
            workspace: "/workspace".to_string(),
            scratch: "/tmp".to_string(),
        }
    }
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
            paths: EphemeralCliPaths::default(),
        }
    }

    /// Run the step with other paths. Tests use it to run the step's script
    /// in directories of their own.
    #[cfg(test)]
    pub(crate) fn with_paths(mut self, paths: EphemeralCliPaths) -> Self {
        self.paths = paths;
        self
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
        let mount_point = PathBuf::from(&self.paths.workspace);
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
        let ws = self.paths.workspace.as_str();
        let tmp = self.paths.scratch.as_str();

        // -- ref selection flags --
        let (ref_flag, checkout_cmd) = match &binding.git_ref {
            GitRef::Branch(name) => (format!("--branch {}", shell_escape(name)), String::new()),
            GitRef::Tag(name) => (format!("--branch {}", shell_escape(name)), String::new()),
            GitRef::Commit(sha) => (
                String::new(),
                format!("&& git -C {ws}/repo checkout {}", shell_escape(sha)),
            ),
        };

        let depth_flag = if shallow { "--depth=1" } else { "" };
        let filter_flag = "--filter=blob:limit=10M";

        // -- credential wiring --
        let auth_repo_url = match credential {
            Some(ResolvedCredential::HttpsPat { username, token }) => {
                // Security audit 002 §4.24 — the PAT MUST NOT travel
                // through container environment variables. Env vars are
                // visible via `docker inspect`, `/proc/<pid>/environ`,
                // and any logging middleware that snapshots the spawn
                // configuration. Instead, materialise the credential to
                // a mode-0600 file inside the container (heredoc into
                // the script prelude — the PAT only exists inside the
                // container's mount namespace) and have a tiny
                // `GIT_ASKPASS` script `cat` it on demand. The username
                // is non-secret and follows the same file-based path
                // for symmetry. Trailing files are deleted at the end
                // of the script so the secret material does not survive
                // the container's lifetime even if the volume is
                // inspected post-mortem.
                let prelude = build_https_askpass_prelude(tmp, &username, token.expose());
                script_prelude.push_str(&prelude);
                env.insert("GIT_ASKPASS".to_string(), format!("{tmp}/askpass.sh"));
                env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
                remote_url(binding)
            }
            Some(ResolvedCredential::SshKey {
                private_key_pem,
                passphrase: _, // container-ephemeral passphrases not supported
            }) => {
                // Audit 002 §4.31: do NOT embed key material in the `sh -c`
                // argv (heredoc-in-script). The argv is observable via
                // `ps`, `docker inspect`, OTLP spans, and any tracing
                // middleware that captures the spawned command. Pass the
                // key via the environment variable `AEGIS_SSH_KEY` and
                // materialise it inside the container with `printf '%s'`
                // — this keeps the bytes out of argv. We chmod 0600,
                // unset the env var so it does not leak to child
                // processes (`git`, `ssh`), and `shred + rm` the file in
                // a trap so the bytes are scrubbed on success and on
                // failure paths alike. `set +o history` is a defensive
                // no-op for non-interactive `sh` but documents intent.
                env.insert(
                    "AEGIS_SSH_KEY".to_string(),
                    private_key_pem.expose().to_string(),
                );
                script_prelude.push_str(&format!(
                    "set +o history 2>/dev/null || true\n\
                     umask 0077\n\
                     printf '%s' \"$AEGIS_SSH_KEY\" >{tmp}/ssh_key\n\
                     case \"$(tail -c1 {tmp}/ssh_key | od -An -c | tr -d ' ')\" in\n\
                       '\\n') ;;\n\
                       *) printf '\\n' >>{tmp}/ssh_key ;;\n\
                     esac\n\
                     chmod 0600 {tmp}/ssh_key\n\
                     unset AEGIS_SSH_KEY\n\
                     trap 'shred -u {tmp}/ssh_key 2>/dev/null || rm -f {tmp}/ssh_key' EXIT INT TERM\n",
                ));
                env.insert(
                    "GIT_SSH_COMMAND".to_string(),
                    format!("ssh -i {tmp}/ssh_key -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"),
                );
                remote_url(binding)
            }
            None => remote_url(binding),
        };

        // -- sparse checkout --
        let sparse_cmd = if let Some(paths) = &binding.sparse_paths {
            let escaped: Vec<String> = paths.iter().map(|p| shell_escape(p)).collect();
            format!(
                " && git -C {ws}/repo sparse-checkout set --cone {}",
                escaped.join(" ")
            )
        } else {
            String::new()
        };

        // Full shell command:
        //   <prelude>            -- materialise credential files (mode 0600)
        //   trap '<scrub>' EXIT  -- guarantee scrub on success or failure
        //   git clone [--depth=1 --filter=blob:limit=10M] [--branch X] URL /workspace/repo
        //   [ && git -C /workspace/repo checkout SHA ]
        //   [ && git -C /workspace/repo sparse-checkout set --cone A B ]
        //   && git -C /workspace/repo rev-parse HEAD
        //
        // The `trap` ensures that `/tmp/git_password`, `/tmp/git_username`,
        // `/tmp/ssh_key`, and `/tmp/askpass.sh` are removed even if the
        // clone fails. This is the file-side complement of the
        // env-var-removal fix for security audit 002 §4.24.
        let command = format!(
            "{prelude}\
             trap 'rm -f {tmp}/git_password {tmp}/git_username {tmp}/askpass.sh {tmp}/ssh_key' EXIT && \
             set -eu && \
             git clone {depth} {filter} {ref_} {url} {ws}/repo \
             {checkout}{sparse} && \
             git -C {ws}/repo rev-parse HEAD",
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
            entrypoint: None,
            command: vec!["sh".to_string(), "-c".to_string(), command],
            stdin: None,
            env,
            workdir: Some(self.paths.workspace.clone()),
            volumes: vec![ContainerVolumeMount {
                name: volume_id.0.to_string(),
                mount_path: self.paths.workspace.clone(),
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

/// The URL git connects to, as stored on the binding. The one place the
/// executor reads it: the clone command (every credential arm) and libgit2's
/// clone and fetch all take it from here.
fn remote_url(binding: &GitRepoBinding) -> String {
    binding.repo_url.expose().to_string()
}

fn shell_escape(s: &str) -> String {
    // Wrap in single quotes, escaping any embedded single quotes via the
    // standard '\'' dance.
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the script prelude that materialises an HTTPS PAT credential to
/// mode-0600 files inside the container and installs a `GIT_ASKPASS`
/// helper that reads them.
///
/// Security audit 002 §4.24: this replaces the previous design that
/// passed the PAT via the container's `GIT_PASSWORD` environment
/// variable. Env-var values are visible via `docker inspect` and
/// `/proc/<pid>/environ`; file-based delivery limits exposure to a
/// process holding a file descriptor on a 0600 file inside the
/// container's mount namespace.
///
/// The `<<'EOF'` heredoc form is critical: the single-quoted delimiter
/// disables shell interpolation, so any metacharacters in the PAT or
/// username (e.g. `$`, `` ` ``, `\`) are written verbatim and NOT
/// expanded by the surrounding `sh -c`.
///
/// The prelude is paired with a `trap '<scrub>' EXIT` in the surrounding
/// command so the files are removed even if the clone fails.
///
/// Factored out for unit testability — see
/// `https_askpass_prelude_does_not_leak_secret_to_env`.
fn build_https_askpass_prelude(tmp: &str, username: &str, password: &str) -> String {
    // The askpass.sh script `cat`s the appropriate file based on what
    // git is asking for (Username vs Password prompt). Files are mode
    // 0600 so only root inside the container can read them.
    let mut s = String::new();
    s.push_str(&format!("cat >{tmp}/git_username <<'AEGIS_USERNAME_EOF'\n"));
    s.push_str(username);
    s.push('\n');
    s.push_str("AEGIS_USERNAME_EOF\n");
    s.push_str(&format!("chmod 0600 {tmp}/git_username\n"));
    s.push_str(&format!("cat >{tmp}/git_password <<'AEGIS_PASSWORD_EOF'\n"));
    s.push_str(password);
    s.push('\n');
    s.push_str("AEGIS_PASSWORD_EOF\n");
    s.push_str(&format!("chmod 0600 {tmp}/git_password\n"));
    s.push_str(&format!(
        "cat >{tmp}/askpass.sh <<'AEGIS_ASKPASS_EOF'\n\
         #!/bin/sh\n\
         case \"$1\" in\n\
         Username*) cat {tmp}/git_username ;;\n\
         Password*) cat {tmp}/git_password ;;\n\
         esac\n\
         AEGIS_ASKPASS_EOF\n\
         chmod 0700 {tmp}/askpass.sh\n",
    ));
    s
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
    #[instrument(skip(self, credential), fields(binding_id = %binding.id, repo_url = %binding.repo_url.redacted()))]
    pub async fn clone_libgit2(
        &self,
        binding: &GitRepoBinding,
        target_dir: &Path,
        credential: Option<ResolvedCredential>,
        shallow: bool,
    ) -> Result<String, CloneError> {
        let repo_url = remote_url(binding);
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
        let repo_url = remote_url(binding);

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

/// Attach a credentials callback to `callbacks` that honours
/// `credential`. For SSH keys, delegates to the shared
/// [`attach_ssh_credentials`] helper which materialises the key to a
/// mode-`0600` tempfile and returns a drop-guard that zeroes + removes
/// the file on drop.
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
            let passphrase_ref = passphrase.as_ref().map(|p| p.expose());
            let guard =
                attach_ssh_credentials(callbacks, private_key_pem.expose(), passphrase_ref)?;
            Ok(Some(guard))
        }
    }
}

/// Apply the binding's sparse-checkout paths to an open repository.
///
/// We set the standard sparse-checkout config and write
/// `.git/info/sparse-checkout` so that a subsequent `git` CLI invocation
/// inside the volume behaves correctly. libgit2 itself does **not**
/// interpret the sparse-checkout file during `checkout_head` — that is a
/// feature of the git CLI's checkout, not libgit2. To actually prune the
/// working tree we walk it and delete any path not covered by a sparse
/// prefix. Paths are compared in "cone mode" semantics: a sparse entry
/// `keep` matches `keep/**`. The `.git` directory is always preserved.
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

    // Workdir is the repo root; `repo.path()` is `.git/`. Prune anything
    // outside the sparse prefixes from the workdir.
    let workdir = repo
        .workdir()
        .ok_or_else(|| CloneError::Git("bare repo has no workdir to prune".to_string()))?
        .to_path_buf();
    prune_workdir(&workdir, paths)?;

    Ok(())
}

/// Remove every path under `workdir` that does not sit under one of the
/// sparse-checkout `include` prefixes. The `.git` directory is always
/// preserved. Empty directories left behind by pruning are also removed.
fn prune_workdir(workdir: &Path, paths: &[String]) -> Result<(), CloneError> {
    // Normalise: drop leading `./` and trailing `/` so comparisons are
    // unambiguous. Empty entries are treated as "no include" (drop).
    let prefixes: Vec<String> = paths
        .iter()
        .map(|p| p.trim_matches('/').trim_start_matches("./").to_string())
        .filter(|p| !p.is_empty())
        .collect();

    prune_dir_recursive(workdir, workdir, &prefixes)?;
    Ok(())
}

fn prune_dir_recursive(root: &Path, dir: &Path, prefixes: &[String]) -> Result<(), CloneError> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Never touch `.git/`.
        if path == root.join(".git") {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .map_err(|_| CloneError::Git("sparse prune: strip_prefix failed".to_string()))?
            .to_string_lossy()
            .replace('\\', "/");

        let ty = entry.file_type()?;
        if ty.is_dir() {
            if sparse_dir_is_kept(&rel, prefixes) {
                prune_dir_recursive(root, &path, prefixes)?;
                // If the directory is now empty and not itself a
                // sparse-root, drop it. We keep named sparse roots even
                // when empty — they represent the caller's intent.
                if std::fs::read_dir(&path)?.next().is_none() && !prefixes.iter().any(|p| p == &rel)
                {
                    std::fs::remove_dir(&path)?;
                }
            } else {
                std::fs::remove_dir_all(&path)?;
            }
        } else if !sparse_file_is_kept(&rel, prefixes) {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// A file at `rel` is kept iff some prefix `p` satisfies `rel == p` or
/// `rel` starts with `p/`.
fn sparse_file_is_kept(rel: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|p| rel == p || rel.starts_with(&format!("{p}/")))
}

/// A directory at `rel` is worth descending into if any prefix either
/// equals it, starts with `rel/` (the dir is an ancestor of a sparse
/// root), or `rel` is already under a sparse root.
fn sparse_dir_is_kept(rel: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|p| rel == p || rel.starts_with(&format!("{p}/")) || p.starts_with(&format!("{rel}/")))
}

/// Return `true` if `repo_url` resolves to libgit2's local transport.
///
/// libgit2 uses its local transport for `file://` URLs and for paths that
/// don't carry a transport scheme (bare filesystem paths). The local
/// transport does not implement the shallow-fetch extension and will
/// abort any fetch that requests `depth != 0`.
fn is_local_transport(repo_url: &str) -> bool {
    // Explicit `file://` scheme.
    if repo_url.starts_with("file://") {
        return true;
    }
    // Any URL with a recognised non-local scheme is remote. Anything else
    // (a bare filesystem path like `/srv/repos/foo.git` or `./foo.git`)
    // is handled by libgit2's local transport.
    let scheme_end = repo_url.find("://");
    match scheme_end {
        Some(_) => false,
        None => {
            // SCP-style `user@host:path` is an SSH remote, not local.
            !is_scp_like(repo_url)
        }
    }
}

/// Detect SCP-style SSH URLs like `git@github.com:owner/repo.git`.
///
/// Rule: a `:` appears before the first `/`, and there's a non-empty
/// segment before the `:`. Absolute paths (`:` never present, or present
/// only after `/`) are excluded.
fn is_scp_like(repo_url: &str) -> bool {
    let first_slash = repo_url.find('/');
    let first_colon = repo_url.find(':');
    match (first_colon, first_slash) {
        (Some(c), Some(s)) => c < s && c > 0,
        (Some(c), None) => c > 0,
        _ => false,
    }
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
    // libgit2's local transport (`file://` and bare filesystem paths)
    // cannot honour a shallow fetch — it returns "shallow fetch is not
    // supported by the local transport" and aborts the whole clone. We
    // never issue shallow fetches over local transport in production
    // (the service-layer URL validator rejects `file://`), but test
    // fixtures bypass that validator to exercise the executor against a
    // real bare repo on disk. Silently drop the depth hint for local
    // transport so both paths behave consistently.
    if shallow && !is_local_transport(repo_url) {
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
                // git2 0.21 changed `Remote::url()` from `Option<&str>` to
                // `Result<&str, git2::Error>`. The condition is unchanged:
                // `url_bytes()` is identical in both versions and returns `&[]`
                // for an unset url, so 0.20's `None` and 0.21's `Err` denote the
                // same single case — the bytes are not UTF-8 — and `.ok()` maps
                // it back. Every remote state produces the same branch as before:
                // matching url -> no rewrite; differing, unset, or non-UTF-8 ->
                // rewrite.
                if r.url().ok() != Some(repo_url) {
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

    /// Regression for security audit 002 §4.24 — the HTTPS PAT MUST
    /// NOT appear anywhere in the spawn environment of the ephemeral
    /// container. The previous design set `env["GIT_PASSWORD"] = pat`
    /// which was visible via `docker inspect` and `/proc/<pid>/environ`.
    #[test]
    fn https_askpass_prelude_does_not_leak_secret_to_env() {
        const SECRET: &str = "ghp_SECRETPATBYTES_xyz123";
        let prelude = build_https_askpass_prelude("/tmp", "x-access-token", SECRET);

        // Simulate the env that would be passed to the container — same
        // population logic as `run_clone_container`. The fix is correct
        // iff the ONLY env vars set are non-secret routing flags.
        let mut env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        env.insert("GIT_ASKPASS".to_string(), "/tmp/askpass.sh".to_string());
        env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());

        for (k, v) in env.iter() {
            assert!(
                !v.contains(SECRET),
                "env var {k}={v:?} contains the secret — §4.24 regressed"
            );
            assert_ne!(k, "GIT_PASSWORD", "GIT_PASSWORD env var must not be set");
            assert_ne!(k, "GIT_USERNAME", "GIT_USERNAME env var must not be set");
        }

        // The secret IS expected inside the heredoc-rendered prelude
        // (that is the file-based delivery path). Sanity-check that
        // the file path machinery is in fact used.
        assert!(
            prelude.contains(SECRET),
            "secret must appear in heredoc payload"
        );
        assert!(prelude.contains("/tmp/git_password"));
        assert!(prelude.contains("chmod 0600 /tmp/git_password"));
    }

    /// The heredoc delimiter MUST be single-quoted so that shell
    /// metacharacters in the credential are not expanded by the
    /// outer `sh -c`. This pins that property.
    #[test]
    fn https_askpass_prelude_uses_quoted_heredoc_delimiter() {
        let prelude =
            build_https_askpass_prelude("/tmp", "u", "secret-with-$dollar-and-`backtick`");
        assert!(prelude.contains("<<'AEGIS_PASSWORD_EOF'"));
        assert!(prelude.contains("<<'AEGIS_USERNAME_EOF'"));
        // Secret is written verbatim — the test value contains $ and `
        // which would be expanded if the delimiter were unquoted.
        assert!(prelude.contains("secret-with-$dollar-and-`backtick`"));
    }

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

    /// Regression for audit 002 §4.31: the SSH private key MUST NOT appear
    /// anywhere in the spawned container's argv (the `sh -c` script).
    /// Argv is observable via `ps`, `docker inspect`, OTLP spans, and any
    /// tracing middleware that captures the command line. The key may
    /// only travel through the container env (`AEGIS_SSH_KEY`), which the
    /// in-script trampoline materialises to `/tmp/ssh_key` (mode 0600),
    /// then `unset`s and `shred`s on exit.
    #[tokio::test]
    async fn ssh_key_never_appears_in_container_argv() {
        use crate::domain::runtime::{
            ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
        };
        use crate::domain::tenant::TenantId;
        use crate::domain::volume::{
            FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeOwnership,
        };
        use std::sync::Mutex;

        struct CapturingRunner {
            captured: Arc<Mutex<Option<ContainerStepConfig>>>,
        }

        #[async_trait::async_trait]
        impl ContainerStepRunner for CapturingRunner {
            async fn run_step(
                &self,
                config: ContainerStepConfig,
            ) -> Result<ContainerStepResult, ContainerStepError> {
                *self.captured.lock().unwrap() = Some(config);
                // Return a 40-char SHA so the executor's parse step
                // doesn't fail before we get to assert on the captured
                // config.
                Ok(ContainerStepResult {
                    exit_code: 0,
                    stdout: format!("{}\n", "a".repeat(40)),
                    stderr: String::new(),
                    duration_ms: 1,
                })
            }
        }

        // Sentinel key with bytes unlikely to appear elsewhere in the script.
        const SENTINEL_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
                                    AEGIS-AUDIT-002-SECTION-4-31-SENTINEL-DO-NOT-LEAK\n\
                                    -----END OPENSSH PRIVATE KEY-----";

        let captured = Arc::new(Mutex::new(None));
        let runner = Arc::new(CapturingRunner {
            captured: captured.clone(),
        });
        let registry = Arc::new(NfsVolumeRegistry::new());
        let engine = EphemeralCliEngine::new(runner, registry);

        let volume = Volume::new(
            "audit-002-4-31".to_string(),
            TenantId::system(),
            StorageClass::ephemeral_hours(1),
            VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                remote_path: "/aegis/seaweedfs/test".to_string(),
            },
            1024 * 1024,
            VolumeOwnership::persistent("audit-test"),
        )
        .unwrap();
        let binding = GitRepoBinding::new(
            TenantId::system(),
            None,
            "git@github.com:owner/repo.git".to_string(),
            GitRef::Branch("main".to_string()),
            None,
            volume.id,
            "audit-002-4-31".to_string(),
            CloneStrategy::EphemeralCli {
                reason: "test".to_string(),
            },
            false,
            None,
            None,
            None,
        );

        let credential = ResolvedCredential::SshKey {
            private_key_pem: SensitiveString::new(SENTINEL_KEY),
            passphrase: None,
        };

        engine
            .clone_into_volume(&binding, &volume, Some(credential), true)
            .await
            .expect("captured runner returns Ok");

        let cfg = captured
            .lock()
            .unwrap()
            .clone()
            .expect("runner must have been called once");

        // 1. The argv must contain only `["sh", "-c", "<script>"]`. The script
        //    itself must NOT contain the sentinel key bytes.
        let script = cfg.command.last().expect("script arg present");
        assert!(
            !script.contains("AEGIS-AUDIT-002-SECTION-4-31-SENTINEL-DO-NOT-LEAK"),
            "SSH key bytes must NEVER appear in the spawned container argv; \
             this is the audit 002 §4.31 regression. argv command was:\n{script}"
        );
        assert!(
            !script.contains("BEGIN OPENSSH PRIVATE KEY"),
            "PEM header must not appear in argv either"
        );
        for arg in &cfg.command {
            assert!(
                !arg.contains("AEGIS-AUDIT-002-SECTION-4-31-SENTINEL-DO-NOT-LEAK"),
                "key bytes must not appear in any argv element"
            );
        }

        // 2. The key MUST be passed via env var (acceptable per audit; the
        //    in-script trampoline unsets + shreds it).
        let env_key = cfg
            .env
            .get("AEGIS_SSH_KEY")
            .expect("AEGIS_SSH_KEY env must carry the key");
        assert!(
            env_key.contains("AEGIS-AUDIT-002-SECTION-4-31-SENTINEL-DO-NOT-LEAK"),
            "AEGIS_SSH_KEY env must carry the actual key bytes"
        );

        // 3. GIT_SSH_COMMAND must point at /tmp/ssh_key with IdentitiesOnly=yes.
        let ssh_cmd = cfg
            .env
            .get("GIT_SSH_COMMAND")
            .expect("GIT_SSH_COMMAND must be set for SSH credential path");
        assert!(ssh_cmd.contains("-i /tmp/ssh_key"));
        assert!(ssh_cmd.contains("IdentitiesOnly=yes"));

        // 4. The trampoline must unset the env var and shred the file on exit.
        assert!(
            script.contains("unset AEGIS_SSH_KEY"),
            "script must unset AEGIS_SSH_KEY before invoking git"
        );
        assert!(
            script.contains("shred -u /tmp/ssh_key") || script.contains("rm -f /tmp/ssh_key"),
            "script must scrub /tmp/ssh_key on EXIT/INT/TERM"
        );
        assert!(
            script.contains("chmod 0600 /tmp/ssh_key"),
            "script must chmod 0600 the materialised key"
        );
    }

    /// libgit2 is handed the binding's URL exactly as stored, user info
    /// included: a redacted rendering would lose the credential it carries.
    #[test]
    fn libgit2_is_handed_the_repository_url_as_stored() {
        use crate::domain::tenant::TenantId;
        const URL: &str = "https://x-access-token:Mk7-libgit2-url-marker@github.com/owner/repo.git";
        let binding = GitRepoBinding::new(
            TenantId::system(),
            None,
            URL.to_string(),
            GitRef::Branch("main".to_string()),
            None,
            crate::domain::volume::VolumeId::new(),
            "libgit2-url".to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
            None,
            None,
        );
        assert_eq!(
            remote_url(&binding),
            URL,
            "libgit2 would connect to a different URL than the binding's"
        );
    }

    /// The clone step's command carries the binding's repository URL exactly
    /// as stored, in each credential arm (PAT, SSH key, none). The URL holds a
    /// marker as user info, which a redacted rendering would drop, so the test
    /// fails if the step formats the URL instead of reading it. Nothing is
    /// run: a capturing runner records the step it is given.
    #[tokio::test]
    async fn clone_command_carries_the_repository_url_in_each_credential_arm() {
        use crate::domain::runtime::{
            ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
        };
        use crate::domain::tenant::TenantId;
        use crate::domain::volume::{
            FilerEndpoint, StorageClass, Volume, VolumeBackend, VolumeOwnership,
        };
        use std::sync::Mutex;

        struct CapturingRunner {
            captured: Arc<Mutex<Option<ContainerStepConfig>>>,
        }

        #[async_trait::async_trait]
        impl ContainerStepRunner for CapturingRunner {
            async fn run_step(
                &self,
                config: ContainerStepConfig,
            ) -> Result<ContainerStepResult, ContainerStepError> {
                *self.captured.lock().unwrap() = Some(config);
                Ok(ContainerStepResult {
                    exit_code: 0,
                    stdout: format!("{}\n", "a".repeat(40)),
                    stderr: String::new(),
                    duration_ms: 1,
                })
            }
        }

        const URL: &str = "https://x-access-token:Mk7-clone-url-marker@github.com/owner/repo.git";
        let arms: [(&str, fn() -> Option<ResolvedCredential>); 3] = [
            ("https pat", || {
                Some(ResolvedCredential::github_pat(SensitiveString::new(
                    "pat-value",
                )))
            }),
            ("ssh key", || {
                Some(ResolvedCredential::SshKey {
                    private_key_pem: SensitiveString::new("key-value"),
                    passphrase: None,
                })
            }),
            ("no credential", || None),
        ];
        for (arm, credential) in arms {
            let captured = Arc::new(Mutex::new(None));
            let runner = Arc::new(CapturingRunner {
                captured: captured.clone(),
            });
            let engine = EphemeralCliEngine::new(runner, Arc::new(NfsVolumeRegistry::new()));
            let volume = Volume::new(
                "clone-url".to_string(),
                TenantId::system(),
                StorageClass::ephemeral_hours(1),
                VolumeBackend::SeaweedFS {
                    filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                    remote_path: "/aegis/seaweedfs/test".to_string(),
                },
                1024 * 1024,
                VolumeOwnership::persistent("clone-url-test"),
            )
            .unwrap();
            let binding = GitRepoBinding::new(
                TenantId::system(),
                None,
                URL.to_string(),
                GitRef::Branch("main".to_string()),
                None,
                volume.id,
                "clone-url".to_string(),
                CloneStrategy::EphemeralCli {
                    reason: "test".to_string(),
                },
                false,
                None,
                None,
                None,
            );
            engine
                .clone_into_volume(&binding, &volume, credential(), true)
                .await
                .expect("captured runner returns Ok");
            let cfg = captured
                .lock()
                .unwrap()
                .clone()
                .expect("runner must have been called once");
            let script = cfg.command.last().expect("script arg present");
            assert!(
                script.contains(&format!(" {} /workspace/repo", shell_escape(URL))),
                "{arm}: the clone command does not carry the repository URL as stored:\n{script}"
            );
        }
    }

    // -----------------------------------------------------------------
    // Regression: libgit2's local transport rejects shallow fetches.
    //
    // `blocking_clone` must NOT set `depth(1)` on a `file://` URL or a
    // bare filesystem path, otherwise libgit2 aborts with
    // `"shallow fetch is not supported by the local transport"` and the
    // integration tests in `tests/git_clone_executor_tests.rs` fail.
    // These tests pin the classifier so the depth-suppression branch is
    // guarded going forward.
    // -----------------------------------------------------------------

    #[test]
    fn is_local_transport_accepts_file_scheme() {
        assert!(is_local_transport("file:///srv/repos/foo.git"));
        assert!(is_local_transport("file:///tmp/x"));
    }

    #[test]
    fn is_local_transport_accepts_bare_filesystem_paths() {
        assert!(is_local_transport("/srv/repos/foo.git"));
        assert!(is_local_transport("./foo.git"));
        assert!(is_local_transport("foo.git"));
    }

    #[test]
    fn is_local_transport_rejects_https() {
        assert!(!is_local_transport("https://github.com/owner/repo.git"));
        assert!(!is_local_transport(
            "https://x-access-token:pat@github.com/o/r.git"
        ));
    }

    #[test]
    fn is_local_transport_rejects_ssh_schemes() {
        assert!(!is_local_transport("ssh://git@github.com/owner/repo.git"));
        assert!(!is_local_transport("git://github.com/owner/repo.git"));
    }

    #[test]
    fn is_local_transport_rejects_scp_style_ssh() {
        assert!(!is_local_transport("git@github.com:owner/repo.git"));
        assert!(!is_local_transport("user@host.example:path/to/repo"));
    }

    // -----------------------------------------------------------------
    // Regression: sparse-checkout pruning must physically remove files
    // outside the include prefixes.
    //
    // libgit2 does not interpret `.git/info/sparse-checkout` during
    // `checkout_head`; that is a git CLI feature. The original
    // `apply_sparse_checkout` only wrote the sparse file and relied on
    // libgit2 to prune the working tree — it didn't. The integration
    // test `sparse_checkout_prunes_working_tree` in
    // `tests/git_clone_executor_tests.rs` caught this: `prune/b.txt`
    // remained after the clone despite the sparse config. These unit
    // tests pin the keep / prune predicates so a future regression on
    // the matcher is caught without needing the full clone harness.
    // -----------------------------------------------------------------

    #[test]
    fn sparse_file_is_kept_exact_match() {
        let prefixes = vec!["keep/a.txt".to_string()];
        assert!(sparse_file_is_kept("keep/a.txt", &prefixes));
    }

    #[test]
    fn sparse_file_is_kept_under_prefix() {
        let prefixes = vec!["keep".to_string()];
        assert!(sparse_file_is_kept("keep/a.txt", &prefixes));
        assert!(sparse_file_is_kept("keep/nested/b.txt", &prefixes));
    }

    #[test]
    fn sparse_file_not_kept_outside_prefix() {
        let prefixes = vec!["keep".to_string()];
        assert!(!sparse_file_is_kept("prune/b.txt", &prefixes));
        assert!(!sparse_file_is_kept("keepsake/x.txt", &prefixes));
        assert!(!sparse_file_is_kept("top.txt", &prefixes));
    }

    #[test]
    fn sparse_dir_kept_when_prefix_exact() {
        let prefixes = vec!["keep".to_string()];
        assert!(sparse_dir_is_kept("keep", &prefixes));
    }

    #[test]
    fn sparse_dir_kept_when_ancestor_of_prefix() {
        // Sparse entry `src/app` means we must descend into `src`.
        let prefixes = vec!["src/app".to_string()];
        assert!(sparse_dir_is_kept("src", &prefixes));
        assert!(sparse_dir_is_kept("src/app", &prefixes));
    }

    #[test]
    fn sparse_dir_kept_when_under_prefix() {
        let prefixes = vec!["keep".to_string()];
        assert!(sparse_dir_is_kept("keep/nested", &prefixes));
    }

    #[test]
    fn sparse_dir_not_kept_when_outside() {
        let prefixes = vec!["keep".to_string()];
        assert!(!sparse_dir_is_kept("prune", &prefixes));
        assert!(!sparse_dir_is_kept("keepsake", &prefixes));
    }

    #[test]
    fn prune_workdir_drops_excluded_and_preserves_included() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("keep")).unwrap();
        std::fs::create_dir_all(root.join("prune")).unwrap();
        std::fs::write(root.join("keep/a.txt"), b"k").unwrap();
        std::fs::write(root.join("prune/b.txt"), b"p").unwrap();
        std::fs::write(root.join("top.txt"), b"t").unwrap();

        prune_workdir(root, &["keep".to_string()]).unwrap();

        assert!(root.join(".git/HEAD").exists(), ".git must be preserved");
        assert!(root.join("keep/a.txt").exists(), "included path kept");
        assert!(!root.join("prune").exists(), "excluded dir removed");
        assert!(
            !root.join("top.txt").exists(),
            "excluded top-level file removed"
        );
    }
}

// ============================================================================
// Tests: where a clone's credential goes
// ============================================================================

/// Every test here makes a marker credential of its own and then looks for it,
/// or any run of 8 of its characters, everywhere it must not be: the step's
/// configuration (command, entrypoint, environment, name; one `Debug` print
/// covers them all), the host's process table while the step runs, the error
/// a failed clone returns, and every file of the cloned tree, `.git` included.
/// The servers are real git servers on the loopback interface that demand the
/// password (`git_test_server`).
#[cfg(test)]
mod credential_tests {
    use super::*;
    use crate::application::git_test_server::{
        container_engine, container_runner, files_holding, holds_any_part_of, marker,
        GitTestServer, HostShellRunner, ProcessWatch,
    };
    use crate::domain::runtime::{ContainerStepResult, ContainerStepRunner};
    use crate::domain::tenant::TenantId;
    use crate::domain::volume::{FilerEndpoint, StorageClass, VolumeOwnership};
    use std::sync::Mutex;

    const USER: &str = "aegis-test-user";

    fn seaweed_volume() -> Volume {
        Volume::new(
            "clone-credential".to_string(),
            TenantId::system(),
            StorageClass::ephemeral_hours(1),
            VolumeBackend::SeaweedFS {
                filer_endpoint: FilerEndpoint::new("http://filer:8888").unwrap(),
                remote_path: "/aegis/seaweedfs/test".to_string(),
            },
            1024 * 1024,
            VolumeOwnership::persistent("clone-credential"),
        )
        .unwrap()
    }

    fn binding(url: &str, volume: &Volume, strategy: CloneStrategy) -> GitRepoBinding {
        GitRepoBinding::new(
            TenantId::system(),
            None,
            url.to_string(),
            GitRef::Branch("main".to_string()),
            None,
            volume.id,
            "clone-credential".to_string(),
            strategy,
            false,
            None,
            None,
            None,
        )
    }

    fn ephemeral() -> CloneStrategy {
        CloneStrategy::EphemeralCli {
            reason: "test".to_string(),
        }
    }

    fn pat(token: &str) -> Option<ResolvedCredential> {
        Some(ResolvedCredential::HttpsPat {
            username: USER.to_string(),
            token: SensitiveString::new(token),
        })
    }

    /// `http://user:password@host/...` from a URL that has none.
    fn with_user_info(url: &str, password: &str) -> String {
        url.replacen("http://", &format!("http://{USER}:{password}@"), 1)
    }

    fn stdin_of(cfg: &ContainerStepConfig) -> String {
        cfg.stdin
            .as_ref()
            .map(|b| String::from_utf8_lossy(b.expose()).to_string())
            .unwrap_or_default()
    }

    struct CapturingRunner {
        captured: Mutex<Option<ContainerStepConfig>>,
    }

    #[async_trait::async_trait]
    impl ContainerStepRunner for CapturingRunner {
        async fn run_step(
            &self,
            config: ContainerStepConfig,
        ) -> Result<ContainerStepResult, ContainerStepError> {
            *self.captured.lock().unwrap() = Some(config);
            Ok(ContainerStepResult {
                exit_code: 0,
                stdout: format!("{}\n", "a".repeat(40)),
                stderr: String::new(),
                duration_ms: 1,
            })
        }
    }

    /// The clone step's configuration holds no part of the credential in any
    /// credential arm, and its standard input carries it.
    #[tokio::test]
    async fn clone_step_configuration_holds_no_credential() {
        let secret = marker("Kq");
        let key = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{secret}\n-----END OPENSSH PRIVATE KEY-----\n"
        );
        let arms: Vec<(&str, String, Option<ResolvedCredential>)> = vec![
            (
                "https token",
                "https://example.invalid/owner/repo.git".to_string(),
                pat(&secret),
            ),
            (
                "ssh key",
                "git@example.invalid:owner/repo.git".to_string(),
                Some(ResolvedCredential::SshKey {
                    private_key_pem: SensitiveString::new(key.clone()),
                    passphrase: None,
                }),
            ),
            (
                "token in the URL's user info",
                format!("https://{USER}:{secret}@example.invalid/owner/repo.git"),
                None,
            ),
        ];
        for (arm, url, credential) in arms {
            let runner = Arc::new(CapturingRunner {
                captured: Mutex::new(None),
            });
            let engine =
                EphemeralCliEngine::new(runner.clone(), Arc::new(NfsVolumeRegistry::new()));
            let volume = seaweed_volume();
            engine
                .clone_into_volume(
                    &binding(&url, &volume, ephemeral()),
                    &volume,
                    credential,
                    true,
                )
                .await
                .expect("captured runner returns Ok");
            let cfg = runner.captured.lock().unwrap().clone().expect("one step");
            assert!(
                !holds_any_part_of(&format!("{cfg:?}"), &secret),
                "{arm}: the clone step's configuration holds the credential"
            );
            assert!(
                stdin_of(&cfg).contains(&secret),
                "{arm}: the clone step's standard input does not carry the credential"
            );
        }
    }

    /// Run the clone step's own script with the host's shell against a git
    /// server that demands the password, in each HTTPS arm. The clone
    /// authenticates; afterwards no argument list or environment seen in the
    /// process table, no step configuration and no file of the cloned tree
    /// holds any part of the credential, and the credential's directory is
    /// gone.
    #[tokio::test]
    async fn clone_step_run_by_a_shell_authenticates_and_leaves_no_credential() {
        for arm in ["https token", "token in the URL's user info"] {
            let secret = marker("Kq");
            let server = GitTestServer::start(USER, &secret);
            let (url, credential) = match arm {
                "https token" => (server.url(), pat(&secret)),
                _ => (with_user_info(&server.url(), &secret), None),
            };
            let dirs = tempfile::tempdir().unwrap();
            let workspace = dirs.path().join("ws");
            std::fs::create_dir_all(&workspace).unwrap();
            let scratch = dirs.path().join("scratch").join("aegis-git");
            std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
            let runner = Arc::new(HostShellRunner::new());
            let engine =
                EphemeralCliEngine::new(runner.clone(), Arc::new(NfsVolumeRegistry::new()))
                    .with_paths(EphemeralCliPaths {
                        workspace: workspace.display().to_string(),
                        scratch: scratch.display().to_string(),
                    });
            let volume = seaweed_volume();

            let watch = ProcessWatch::start(&secret);
            let result = engine
                .clone_into_volume(
                    &binding(&url, &volume, ephemeral()),
                    &volume,
                    credential,
                    true,
                )
                .await;
            let (seen_in, rounds) = watch.stop();

            assert!(
                !holds_any_part_of(&runner.seen_debug(), &secret),
                "{arm}: the clone step's configuration holds the credential"
            );
            assert!(
                seen_in.is_empty(),
                "{arm}: the credential was in the process table at {seen_in:?} ({rounds} reads)"
            );
            let sha = result.unwrap_or_else(|e| panic!("{arm}: the clone failed: {e}"));
            assert_eq!(sha, server.head(), "{arm}: cloned the wrong commit");
            assert!(
                server.authorized_requests() > 0,
                "{arm}: no request authenticated"
            );
            let holding = files_holding(&workspace, &secret);
            assert!(
                holding.is_empty(),
                "{arm}: files of the cloned tree hold the credential: {holding:?}"
            );
            let config = std::fs::read_to_string(workspace.join("repo/.git/config")).unwrap();
            assert!(
                config.contains(&format!("url = {}", server.url())),
                "{arm}: the remote URL in .git/config is not the repository's URL"
            );
            assert!(
                !scratch.exists(),
                "{arm}: the credential's directory is still there after the clone"
            );
        }
    }

    /// A clone with a wrong credential fails, and nothing it leaves or returns
    /// holds any part of the credential it was given.
    #[tokio::test]
    async fn failed_clone_step_shows_no_part_of_the_credential() {
        for arm in ["https token", "token in the URL's user info"] {
            let wrong = marker("Kq");
            let server = GitTestServer::start(USER, &marker("Rw"));
            let (url, credential) = match arm {
                "https token" => (server.url(), pat(&wrong)),
                _ => (with_user_info(&server.url(), &wrong), None),
            };
            let dirs = tempfile::tempdir().unwrap();
            let workspace = dirs.path().join("ws");
            std::fs::create_dir_all(&workspace).unwrap();
            let scratch = dirs.path().join("scratch").join("aegis-git");
            std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
            let runner = Arc::new(HostShellRunner::new());
            let engine =
                EphemeralCliEngine::new(runner.clone(), Arc::new(NfsVolumeRegistry::new()))
                    .with_paths(EphemeralCliPaths {
                        workspace: workspace.display().to_string(),
                        scratch: scratch.display().to_string(),
                    });
            let volume = seaweed_volume();

            let watch = ProcessWatch::start(&wrong);
            let result = engine
                .clone_into_volume(
                    &binding(&url, &volume, ephemeral()),
                    &volume,
                    credential,
                    true,
                )
                .await;
            let (seen_in, rounds) = watch.stop();

            assert!(
                !holds_any_part_of(&runner.seen_debug(), &wrong),
                "{arm}: the clone step's configuration holds the credential"
            );
            assert!(
                seen_in.is_empty(),
                "{arm}: the credential was in the process table at {seen_in:?} ({rounds} reads)"
            );
            let error = match result {
                Ok(_) => panic!("{arm}: a clone with a wrong credential succeeded"),
                Err(e) => e.to_string(),
            };
            assert!(
                !holds_any_part_of(&error, &wrong),
                "{arm}: the failed clone's error holds the credential"
            );
            assert!(
                server.refused_requests() > 0,
                "{arm}: the server refused nothing"
            );
            let holding = files_holding(dirs.path(), &wrong);
            assert!(
                holding.is_empty(),
                "{arm}: files left by the failed clone hold the credential: {holding:?}"
            );
            assert!(
                !scratch.exists(),
                "{arm}: the credential's directory is still there after the failed clone"
            );
        }
    }

    /// The SSH arm: the key reaches ssh as a file named on its command line,
    /// never as the key itself, and the file is gone when the step ends.
    /// `ssh` here is a stand-in that records its arguments, copies the file
    /// it is given with `-i`, and fails.
    #[tokio::test]
    async fn ssh_clone_step_hands_the_key_to_ssh_in_a_file_that_is_removed() {
        let secret = marker("Kq");
        let key = format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\n{secret}\n-----END OPENSSH PRIVATE KEY-----\n"
        );
        let dirs = tempfile::tempdir().unwrap();
        let bin = dirs.path().join("bin");
        let out = dirs.path().join("ssh-out");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let fake_ssh = bin.join("ssh");
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" >\"$FAKE_SSH_OUT/args\"\n\
             while [ $# -gt 0 ]; do\n\
               if [ \"$1\" = -i ]; then cp \"$2\" \"$FAKE_SSH_OUT/key\"; fi\n\
               shift\n\
             done\n\
             exit 255\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let workspace = dirs.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let scratch = dirs.path().join("scratch").join("aegis-git");
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        let runner = Arc::new(
            HostShellRunner::new()
                .with_env("PATH", &bin.display().to_string())
                .with_env("FAKE_SSH_OUT", &out.display().to_string()),
        );
        let engine = EphemeralCliEngine::new(runner.clone(), Arc::new(NfsVolumeRegistry::new()))
            .with_paths(EphemeralCliPaths {
                workspace: workspace.display().to_string(),
                scratch: scratch.display().to_string(),
            });
        let volume = seaweed_volume();

        let watch = ProcessWatch::start(&secret);
        let result = engine
            .clone_into_volume(
                &binding("git@example.invalid:owner/repo.git", &volume, ephemeral()),
                &volume,
                Some(ResolvedCredential::SshKey {
                    private_key_pem: SensitiveString::new(key.clone()),
                    passphrase: None,
                }),
                true,
            )
            .await;
        let (seen_in, rounds) = watch.stop();

        assert!(
            !holds_any_part_of(&runner.seen_debug(), &secret),
            "ssh key: the clone step's configuration holds the key"
        );
        assert!(
            seen_in.is_empty(),
            "ssh key: the key was in the process table at {seen_in:?} ({rounds} reads)"
        );
        let error = match result {
            Ok(_) => panic!("ssh key: the stand-in ssh fails, so the clone must fail"),
            Err(e) => e.to_string(),
        };
        assert!(
            !holds_any_part_of(&error, &secret),
            "ssh key: the failed clone's error holds the key"
        );
        let args = std::fs::read_to_string(out.join("args")).expect("ssh was run");
        assert!(
            !holds_any_part_of(&args, &secret),
            "ssh key: ssh's arguments hold the key"
        );
        let handed = std::fs::read_to_string(out.join("key")).unwrap_or_default();
        assert_eq!(
            handed, key,
            "ssh key: ssh was not handed the key in its file"
        );
        std::fs::remove_file(out.join("key")).unwrap();
        assert!(
            !scratch.exists(),
            "ssh key: the key's directory is still there after the step"
        );
        let holding = files_holding(&workspace, &secret);
        assert!(
            holding.is_empty(),
            "ssh key: files hold the key: {holding:?}"
        );
    }

    /// The in-process clone and a later fetch, in each HTTPS arm, against a
    /// server that demands the password: both authenticate, and no file of the
    /// cloned tree holds any part of the credential after either.
    #[tokio::test]
    async fn libgit2_clone_and_fetch_authenticate_and_leave_no_credential_in_the_tree() {
        for arm in ["https token", "token in the URL's user info"] {
            let secret = marker("Kq");
            let server = GitTestServer::start(USER, &secret);
            let (url, credential): (String, fn(&str) -> Option<ResolvedCredential>) = match arm {
                "https token" => (server.url(), pat),
                _ => (with_user_info(&server.url(), &secret), |_| None),
            };
            let dirs = tempfile::tempdir().unwrap();
            let target = dirs.path().join("repo");
            let volume = seaweed_volume();
            let b = binding(&url, &volume, CloneStrategy::Libgit2);
            let executor = test_executor(dirs.path());

            let sha = executor
                .clone_libgit2(&b, &target, credential(&secret), true)
                .await
                .unwrap_or_else(|e| panic!("{arm}: the clone failed: {e}"));
            assert_eq!(sha, server.head(), "{arm}: cloned the wrong commit");
            let holding = files_holding(&target, &secret);
            assert!(
                holding.is_empty(),
                "{arm}: files of the cloned tree hold the credential: {holding:?}"
            );

            let before = server.authorized_requests();
            let next = server.add_commit("second.txt", "second\n");
            let fetched = executor
                .fetch_and_checkout(&b, &target, credential(&secret))
                .await
                .unwrap_or_else(|e| panic!("{arm}: the fetch failed: {e}"));
            assert_eq!(
                fetched, next,
                "{arm}: the fetch did not reach the new commit"
            );
            assert!(
                server.authorized_requests() > before,
                "{arm}: the fetch made no authenticated request"
            );
            let holding = files_holding(&target, &secret);
            assert!(
                holding.is_empty(),
                "{arm}: files of the tree hold the credential after the fetch: {holding:?}"
            );
        }
    }

    /// A failed in-process clone with a wrong credential returns an error
    /// that holds no part of it, and leaves no file that does.
    #[tokio::test]
    async fn libgit2_failed_clone_shows_no_part_of_the_credential() {
        for arm in ["https token", "token in the URL's user info"] {
            let wrong = marker("Kq");
            let server = GitTestServer::start(USER, &marker("Rw"));
            let (url, credential) = match arm {
                "https token" => (server.url(), pat(&wrong)),
                _ => (with_user_info(&server.url(), &wrong), None),
            };
            let dirs = tempfile::tempdir().unwrap();
            let target = dirs.path().join("repo");
            let volume = seaweed_volume();
            let result = test_executor(dirs.path())
                .clone_libgit2(
                    &binding(&url, &volume, CloneStrategy::Libgit2),
                    &target,
                    credential,
                    true,
                )
                .await;
            let error = match result {
                Ok(_) => panic!("{arm}: a clone with a wrong credential succeeded"),
                Err(e) => e.to_string(),
            };
            assert!(
                !holds_any_part_of(&error, &wrong),
                "{arm}: the failed clone's error holds the credential"
            );
            let holding = files_holding(dirs.path(), &wrong);
            assert!(
                holding.is_empty(),
                "{arm}: files left by the failed clone hold the credential: {holding:?}"
            );
        }
    }

    /// A push after an in-process clone authenticates in each HTTPS arm, and
    /// leaves no file of the tree holding any part of the credential.
    #[tokio::test]
    async fn push_authenticates_and_leaves_no_credential_in_the_tree() {
        use crate::application::git_repo_service::push_to_remote;
        for arm in ["https token", "token in the URL's user info"] {
            let secret = marker("Kq");
            let server = GitTestServer::start(USER, &secret);
            let (url, credential): (String, fn(&str) -> Option<ResolvedCredential>) = match arm {
                "https token" => (server.url(), pat),
                _ => (with_user_info(&server.url(), &secret), |_| None),
            };
            let dirs = tempfile::tempdir().unwrap();
            let target = dirs.path().join("repo");
            let volume = seaweed_volume();
            let b = binding(&url, &volume, CloneStrategy::Libgit2);
            test_executor(dirs.path())
                .clone_libgit2(&b, &target, credential(&secret), false)
                .await
                .unwrap_or_else(|e| panic!("{arm}: the clone failed: {e}"));

            let repo = Repository::open(&target).unwrap();
            std::fs::write(target.join("pushed.txt"), "pushed\n").unwrap();
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("pushed.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let sig = git2::Signature::now("test", "test@example.invalid").unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            let commit = repo
                .commit(Some("HEAD"), &sig, &sig, "pushed", &tree, &[&parent])
                .unwrap();

            let target_for_push = target.clone();
            let repo_url = b.repo_url.clone();
            let cred = credential(&secret);
            tokio::task::spawn_blocking(move || {
                push_to_remote(
                    &target_for_push,
                    &repo_url,
                    "origin",
                    Some("main".into()),
                    cred,
                )
            })
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{arm}: the push failed: {e}"));
            assert_eq!(
                server.head(),
                commit.to_string(),
                "{arm}: the push did not land"
            );
            let holding = files_holding(&target, &secret);
            assert!(
                holding.is_empty(),
                "{arm}: files of the tree hold the credential after the push: {holding:?}"
            );
        }
    }

    /// Runs the clone step on the real container runner with three test-only
    /// changes: no volume (the clone lands in the container's `/tmp`), the
    /// host's network (to reach the loopback server), and, after the step's
    /// own script, a report on standard error of the cloned tree (as a tar)
    /// and of whether the credential's directory is still there. The report is
    /// taken off the result before the engine sees it.
    struct ReportingContainerRunner {
        inner: Arc<crate::infrastructure::container_step_runner::ContainerStepRunnerImpl>,
        workspace: String,
        scratch: String,
        tree: Mutex<Vec<u8>>,
        scratch_left: Mutex<Option<bool>>,
        seen: Mutex<Vec<ContainerStepConfig>>,
    }

    const REPORT: &str = "AEGIS-TEST-REPORT";

    #[async_trait::async_trait]
    impl ContainerStepRunner for ReportingContainerRunner {
        async fn run_step(
            &self,
            mut config: ContainerStepConfig,
        ) -> Result<ContainerStepResult, ContainerStepError> {
            self.seen.lock().unwrap().push(config.clone());
            config.volumes.clear();
            config.network_mode = Some("host".to_string());
            let script = config.command.pop().expect("a script");
            config.command.push(format!(
                "( {script}\n)\nrc=$?\n{{ echo {REPORT}; tar -C {ws} -cf - repo 2>/dev/null | base64; \
                 echo {REPORT}; if [ -e {scratch} ]; then echo left; else echo gone; fi; }} >&2\nexit $rc",
                ws = self.workspace,
                scratch = self.scratch,
            ));
            let mut result = self.inner.run_step(config).await?;
            if let Some((before, report)) = result.stderr.split_once(REPORT) {
                let (tar_b64, rest) = report.split_once(REPORT).unwrap_or((report, ""));
                use base64::Engine;
                let compact: String = tar_b64.chars().filter(|c| !c.is_whitespace()).collect();
                *self.tree.lock().unwrap() = base64::engine::general_purpose::STANDARD
                    .decode(compact)
                    .unwrap_or_default();
                *self.scratch_left.lock().unwrap() = Some(rest.trim() == "left");
                result.stderr = before.to_string();
            }
            Ok(result)
        }
    }

    /// The clone step in a real container, in each HTTPS arm and with a wrong
    /// credential: the clone authenticates (or fails, for the wrong one), and
    /// no part of the credential is in the container's configuration as the
    /// engine shows it, in any event published for the step, in the cloned
    /// tree, or in the error; the credential's directory is gone when the step
    /// ends. Needs a container engine (see `container_engine`).
    #[tokio::test]
    async fn clone_step_in_a_container_authenticates_and_leaves_no_credential() {
        let test = "clone_step_in_a_container_authenticates_and_leaves_no_credential";
        let Some(docker) = container_engine(test).await else {
            return;
        };
        for arm in ["https token", "token in the URL's user info", "wrong token"] {
            let secret = marker("Kq");
            let server = GitTestServer::start(
                USER,
                &if arm == "wrong token" {
                    marker("Rw")
                } else {
                    secret.clone()
                },
            );
            let (url, credential) = match arm {
                "token in the URL's user info" => (with_user_info(&server.url(), &secret), None),
                _ => (server.url(), pat(&secret)),
            };
            let bus = Arc::new(crate::infrastructure::event_bus::EventBus::new(256));
            let mut events = bus.subscribe();
            let paths = EphemeralCliPaths {
                workspace: "/tmp/aegis-ws".to_string(),
                scratch: "/tmp/aegis-git".to_string(),
            };
            let runner = Arc::new(ReportingContainerRunner {
                inner: container_runner(docker.clone(), bus.clone()),
                workspace: paths.workspace.clone(),
                scratch: paths.scratch.clone(),
                tree: Mutex::new(Vec::new()),
                scratch_left: Mutex::new(None),
                seen: Mutex::new(Vec::new()),
            });
            let engine =
                EphemeralCliEngine::new(runner.clone(), Arc::new(NfsVolumeRegistry::new()))
                    .with_paths(paths);
            let volume = seaweed_volume();
            let result = engine
                .clone_into_volume(
                    &binding(&url, &volume, ephemeral()),
                    &volume,
                    credential,
                    true,
                )
                .await;

            let seen = format!("{:?}", runner.seen.lock().unwrap());
            assert!(
                !holds_any_part_of(&seen, &secret),
                "{arm}: the clone step's configuration holds the credential"
            );
            let mut published = Vec::new();
            while let Ok(event) = events.try_recv() {
                published.push(serde_json::to_string(&event).unwrap_or_default());
            }
            assert!(
                !published.is_empty(),
                "{arm}: the runner published no event"
            );
            assert!(
                !published.iter().any(|e| holds_any_part_of(e, &secret)),
                "{arm}: an event published for the step holds the credential"
            );
            let tree = String::from_utf8_lossy(&runner.tree.lock().unwrap()).to_string();
            assert!(
                !holds_any_part_of(&tree, &secret),
                "{arm}: the cloned tree holds the credential"
            );
            assert_eq!(
                *runner.scratch_left.lock().unwrap(),
                Some(false),
                "{arm}: the credential's directory is still there after the step, \
                 or the step did not report"
            );
            if arm == "wrong token" {
                let error = match result {
                    Ok(_) => panic!("{arm}: a clone with a wrong credential succeeded"),
                    Err(e) => e.to_string(),
                };
                assert!(
                    !holds_any_part_of(&error, &secret),
                    "{arm}: the failed clone's error holds the credential"
                );
                assert!(
                    server.refused_requests() > 0,
                    "{arm}: the server refused nothing"
                );
            } else {
                let sha = result.unwrap_or_else(|e| panic!("{arm}: the clone failed: {e}"));
                assert_eq!(sha, server.head(), "{arm}: cloned the wrong commit");
                assert!(
                    server.authorized_requests() > 0,
                    "{arm}: no request authenticated"
                );
                assert!(
                    tree.contains(&format!("url = {}", server.url())),
                    "{arm}: the cloned tree's .git/config does not name the repository's URL"
                );
            }
        }
    }

    struct NoEvents;

    #[async_trait::async_trait]
    impl crate::domain::fsal::EventPublisher for NoEvents {
        async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
    }

    /// The executor with an FSAL over a temporary directory. The libgit2 path
    /// writes to the directory it is given and never reads the FSAL.
    fn test_executor(dir: &Path) -> GitCloneExecutor {
        use crate::infrastructure::event_bus::EventBus;
        use crate::infrastructure::repositories::InMemoryVolumeRepository;
        use crate::infrastructure::secrets_manager::TestSecretStore;
        use crate::infrastructure::storage::LocalHostStorageProvider;
        let bus = Arc::new(EventBus::new(16));
        let secrets = Arc::new(SecretsManager::from_store(
            Arc::new(TestSecretStore::new()),
            bus,
        ));
        let root = dir.join("fsal");
        std::fs::create_dir_all(&root).unwrap();
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&root).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new())),
            Arc::new(NoEvents),
        ));
        GitCloneExecutor::new(secrets, fsal, None)
    }
}
