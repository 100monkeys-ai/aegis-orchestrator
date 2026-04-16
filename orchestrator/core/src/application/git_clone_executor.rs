// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Clone Executor (BC-7 Storage Gateway, ADR-081 §Domain Service)
//!
//! Low-level clone / fetch primitive. Owns all interaction with `git2`
//! (libgit2) so that the application service ([`GitRepoService`]) stays
//! transport-agnostic.
//!
//! ## A2 Scope (Phase 1)
//!
//! - Libgit2 clone of public HTTPS repos
//! - Libgit2 clone of private HTTPS repos using a PAT via
//!   `Cred::userpass_plaintext` (GitHub fine-grained tokens default to
//!   `"x-access-token"` as the username)
//! - Shallow clone by default (`depth = 1`)
//! - Target directory is provided by the caller — resolution from
//!   [`AegisFSAL`] / [`Volume`] is handled by the service layer
//!
//! ## A3 Extensions
//!
//! The executor is intentionally shaped so A3 can plug in without
//! refactoring:
//!
//! - [`GitCloneExecutor::fetch_and_checkout`] — current stub returns
//!   `CloneError::NotYetImplemented`. A3 will implement ref pinning
//!   (branch fast-forward / tag / exact SHA checkout).
//! - [`GitCloneExecutor::select_strategy`] — current impl always returns
//!   [`CloneStrategy::Libgit2`]. A3 will plug in the heuristic (LFS /
//!   submodule / custom git-config detection) and route through the
//!   [`EphemeralCliEngine`] marker.
//! - [`EphemeralCliEngine`] — stub marker type. A3 (see ADR-053) will
//!   replace with the real container-spawning fallback.
//! - SSH key credential handling per ADR-081 §Security is **NOT** in A2.
//!   The credential-resolution branch below carries a clear `TODO(A3)`
//!   marker at the insertion point for the temp-file + `zeroize` +
//!   `scopeguard` flow.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use git2::{Cred, FetchOptions, RemoteCallbacks, Repository};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::domain::fsal::AegisFSAL;
use crate::domain::git_repo::{CloneStrategy, GitRepoBinding};
use crate::domain::secrets::SensitiveString;
use crate::infrastructure::secrets_manager::SecretsManager;

// ============================================================================
// EphemeralCliEngine — A3 marker type
// ============================================================================

/// Placeholder for the Phase-3 ephemeral CLI container runtime (ADR-053).
///
/// Wave A2 only needs a type slot so [`GitCloneExecutor::new`] can accept
/// `Option<Arc<EphemeralCliEngine>>` today. A3 replaces this stub with the
/// real container-spawning implementation when LFS / submodules / custom
/// git-config need to fall back to `alpine/git` in a sandboxed container.
#[derive(Debug, Default)]
pub struct EphemeralCliEngine;

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

    /// Functionality not yet implemented in this wave — deferred to A3.
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
    /// SSH private key material. **NOT implemented in A2.** A3 will
    /// materialize the key to a mode-`0600` temp file, pass the path to
    /// libgit2, then `zeroize` + `remove_file` in a `scopeguard` drop.
    SshKey {
        #[allow(dead_code)]
        private_key_pem: SensitiveString,
        #[allow(dead_code)]
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
/// - `secret_manager` — resolves [`CredentialBindingId`] → token at the
///   call site. Never leaks.
/// - `fsal` — path & policy boundary owned by the application layer; the
///   executor itself does not authorize, it only writes to the resolved
///   path.
/// - `cli_engine` — **A3 only.** Stub today.
///
/// [`CredentialBindingId`]: crate::domain::credential::CredentialBindingId
pub struct GitCloneExecutor {
    #[allow(dead_code)]
    secret_manager: Arc<SecretsManager>,
    #[allow(dead_code)]
    fsal: Arc<AegisFSAL>,
    #[allow(dead_code)]
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

    /// Select the [`CloneStrategy`] for a given [`GitRepoBinding`].
    ///
    /// A2 always returns [`CloneStrategy::Libgit2`]. A3 plugs in the
    /// heuristic (LFS, submodule, custom git-config) and routes through
    /// [`EphemeralCliEngine`].
    pub fn select_strategy(&self, _binding: &GitRepoBinding) -> CloneStrategy {
        CloneStrategy::Libgit2
    }

    /// Clone the `binding` into `target_dir` and return the HEAD commit
    /// SHA on success.
    ///
    /// - `credential` is `None` for public repos. When `Some`, it is
    ///   passed into libgit2's credentials callback for the duration of
    ///   the clone and dropped immediately afterward.
    /// - `shallow == true` sets `depth = 1` on the fetch. Default is
    ///   `true` — full-history clones are opt-in via
    ///   [`CreateGitRepoCommand::shallow`].
    ///
    /// [`CreateGitRepoCommand::shallow`]: crate::application::git_repo_service::CreateGitRepoCommand::shallow
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

        info!(
            target = %target_dir.display(),
            shallow,
            "cloning git repository"
        );

        // libgit2 is fully blocking — run on a dedicated blocking thread
        // so we don't stall the tokio reactor.
        let sha = tokio::task::spawn_blocking(move || -> Result<String, CloneError> {
            blocking_clone(&repo_url, &target_dir, credential, shallow)
        })
        .await
        .map_err(|e| CloneError::Io(format!("clone task panicked: {e}")))??;

        debug!(commit_sha = %sha, "clone completed");
        Ok(sha)
    }

    /// **A3 STUB.** Fetch from the remote and check out the binding's
    /// [`crate::domain::git_repo::GitRef`].
    ///
    /// In A2 this returns [`CloneError::NotYetImplemented`] — wave A3
    /// fills in the ref-pinning semantics (branch fast-forward, tag /
    /// commit SHA checkout).
    pub async fn fetch_and_checkout(
        &self,
        _binding: &GitRepoBinding,
        _target_dir: &Path,
        _credential: Option<ResolvedCredential>,
    ) -> Result<String, CloneError> {
        Err(CloneError::NotYetImplemented(
            "fetch_and_checkout is deferred to ADR-081 Wave A3",
        ))
    }
}

// ============================================================================
// Blocking git2 helpers
// ============================================================================

/// Run the libgit2 clone on the calling (blocking) thread.
///
/// The credential closure is called by libgit2 whenever authentication is
/// required. We map our in-scope [`ResolvedCredential`] to the correct
/// [`Cred`] variant.
fn blocking_clone(
    repo_url: &str,
    target_dir: &Path,
    credential: Option<ResolvedCredential>,
    shallow: bool,
) -> Result<String, CloneError> {
    // Ensure parent exists.
    if let Some(parent) = target_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Build the callbacks closure. The credential is moved into the
    // FetchOptions and dropped alongside it at the end of this function.
    let mut callbacks = RemoteCallbacks::new();

    if let Some(cred) = credential {
        match cred {
            ResolvedCredential::HttpsPat { username, token } => {
                // SAFETY: the closure captures by move; SensitiveString
                // is dropped with this RemoteCallbacks when the fetch
                // completes. libgit2 may call the callback several times
                // during a single fetch (retry on auth failure), so we
                // clone out of the closure's captured references.
                callbacks.credentials(move |_url: &str, _user_from_url: Option<&str>, _allowed| {
                    Cred::userpass_plaintext(&username, token.expose())
                });
            }
            ResolvedCredential::SshKey { .. } => {
                // TODO(A3): materialize the SSH key to a mode-0600 temp
                // file using `scopeguard` + `zeroize` per ADR-081
                // §Security, then call
                // `Cred::ssh_key(username, None, &keypath, passphrase)`.
                // Wave A2 deliberately does not ship SSH support.
                return Err(CloneError::NotYetImplemented(
                    "SSH credential support is deferred to ADR-081 Wave A3",
                ));
            }
        }
    }

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

    // Resolve HEAD → commit SHA.
    let head = repo.head()?;
    let commit_sha = head
        .target()
        .ok_or_else(|| CloneError::Git("HEAD has no direct target".to_string()))?
        .to_string();
    Ok(commit_sha)
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
}
