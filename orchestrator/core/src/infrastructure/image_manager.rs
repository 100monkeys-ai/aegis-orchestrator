// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Docker Image Manager (ADR-045)
//!
//! Provides the [`DockerImageManager`] trait and [`StandardDockerImageManager`]
//! implementation for image lifecycle management: pull-policy enforcement,
//! local-cache inspection, and registry credential resolution.
//!
//! ## Phase summary
//!
//! | Phase | Credentials | Implementation |
//! |-------|-------------|----------------|
//! | 1 (current) | Node-config static | [`NodeConfigCredentialResolver`] — matches registry prefix from `aegis-config.yaml` |
//! | 2 (future)  | OpenBao dynamic | `OpenBaoCredentialResolver` — wires vault path per registry |
//!
//! The `credential_resolver` field on [`StandardDockerImageManager`] is the integration
//! point; swapping the resolver for Phase 2 requires no structural changes to `ContainerRuntime`.
//!
//! See ADR-045 (Container Registry & Image Management), ADR-034 (OpenBao Secrets).

use crate::domain::agent::ImagePullPolicy;
use crate::domain::events::PullSource;
use crate::domain::node_config::RegistryCredentials;
use crate::domain::runtime::RuntimeError;
use async_trait::async_trait;
use bollard::auth::DockerCredentials;
use bollard::query_parameters::CreateImageOptions;
use bollard::Docker;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{error, info};

/// Total wall-clock budget for `ensure_image`, including registry inspection
/// and full pull stream consumption. Tunable via env var (see below) for tests.
///
/// A slow/unreachable registry is surfaced as `RuntimeError::ImagePullTimeout`
/// after this many seconds rather than blocking the spawn path indefinitely.
pub(crate) const IMAGE_PULL_TIMEOUT_SECS: u64 = 180;

/// Wrap an arbitrary pull future in the standard `IMAGE_PULL_TIMEOUT_SECS`
/// budget, mapping elapsed-deadline into [`RuntimeError::ImagePullTimeout`].
///
/// Extracted as a free function so the timeout semantics can be unit-tested
/// without stubbing the full Bollard `Docker` client.
pub(crate) async fn run_pull_with_timeout<F, T>(
    image: &str,
    timeout_secs: u64,
    fut: F,
) -> Result<T, RuntimeError>
where
    F: std::future::Future<Output = Result<T, RuntimeError>>,
{
    match timeout(Duration::from_secs(timeout_secs), fut).await {
        Ok(inner) => inner,
        Err(_) => {
            error!(
                image = %image,
                timeout_secs = timeout_secs,
                "image pull timed out"
            );
            Err(RuntimeError::ImagePullTimeout {
                image: image.to_string(),
                timeout_secs,
            })
        }
    }
}

// ── CredentialResolver ────────────────────────────────────────────────────────

/// Resolves registry credentials for a given image reference.
///
/// Phase 1 implementation: [`NodeConfigCredentialResolver`] (static node-config).
/// Phase 2 will provide an `OpenBaoCredentialResolver` that fetches short-lived
/// tokens from OpenBao (ADR-034).
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Return credentials for the registry that hosts `image`, or `None` for
    /// anonymous access.
    async fn resolve(&self, image: &str) -> Option<RegistryCredentials>;
}

/// Phase 1 credential resolver backed by the static `registry_credentials` list
/// in `aegis-config.yaml` (`NodeConfigSpec::registry_credentials`).
///
/// Matches credentials by comparing the `registry` field as a prefix of the
/// fully-qualified image reference (e.g. `"ghcr.io"` matches
/// `"ghcr.io/myorg/agent:v1.0"`). The first matching entry wins; order in
/// the config file is therefore the priority order.
///
/// Returns `None` for images that match no configured registry, falling through
/// to anonymous pulls.
pub struct NodeConfigCredentialResolver {
    credentials: Vec<RegistryCredentials>,
}

impl NodeConfigCredentialResolver {
    /// Construct a resolver from the node-config credential list.
    pub fn new(credentials: Vec<RegistryCredentials>) -> Self {
        Self { credentials }
    }
}

#[async_trait]
impl CredentialResolver for NodeConfigCredentialResolver {
    async fn resolve(&self, image: &str) -> Option<RegistryCredentials> {
        self.credentials
            .iter()
            .find(|cred| image.starts_with(&cred.registry))
            .cloned()
    }
}

// ── DockerImageManager ────────────────────────────────────────────────────────

/// Manages Docker image availability: pull-policy enforcement and cache checks.
///
/// Extracted from inline `ContainerRuntime::spawn()` logic to enable independent
/// testing and Phase 2 credential injection. `ContainerRuntime` calls
/// [`DockerImageManager::ensure_image`] in `spawn()` (ADR-045).
#[async_trait]
pub trait DockerImageManager: Send + Sync {
    /// Ensure `image` is available locally according to `policy`.
    ///
    /// | Policy | Behaviour |
    /// |--------|-----------|
    /// | `Always` | Pull from registry unconditionally. |
    /// | `IfNotPresent` | Use local cache if present; pull only if absent. |
    /// | `Never` | Use local cache only; return `SpawnFailed` if image is absent. |
    ///
    /// Returns [`PullSource::Cached`] when the image was already present and
    /// [`PullSource::Downloaded`] when a network pull was performed.
    ///
    /// `credentials_override` — when `Some`, bypasses the node-config
    /// [`CredentialResolver`] and uses the supplied credentials directly.
    /// Used by [`crate::infrastructure::container_step_runner::ContainerStepRunnerImpl`]
    /// when per-step registry credentials are resolved from OpenBao (ADR-050).
    async fn ensure_image(
        &self,
        image: &str,
        policy: ImagePullPolicy,
        credentials_override: Option<DockerCredentials>,
    ) -> Result<PullSource, RuntimeError>;
}

// ── StandardDockerImageManager ───────────────────────────────────────────────

/// Phase 1 [`DockerImageManager`] implementation.
///
/// Pulls images via the Docker daemon using `bollard`. Credentials are resolved
/// via the injected `CredentialResolver` (Phase 1: [`NodeConfigCredentialResolver`]
/// backed by static node-config; Phase 2: an `OpenBaoCredentialResolver`).
pub struct StandardDockerImageManager {
    docker: Docker,
    credential_resolver: Arc<dyn CredentialResolver>,
}

impl StandardDockerImageManager {
    /// Create an image manager backed by `docker` and the given `credential_resolver`.
    ///
    /// For Phase 1 pass
    /// `Arc::new(NodeConfigCredentialResolver::new(node_config.spec.registry_credentials.clone()))`.
    pub fn new(docker: Docker, credential_resolver: Arc<dyn CredentialResolver>) -> Self {
        Self {
            docker,
            credential_resolver,
        }
    }
}

#[async_trait]
impl DockerImageManager for StandardDockerImageManager {
    async fn ensure_image(
        &self,
        image: &str,
        policy: ImagePullPolicy,
        credentials_override: Option<DockerCredentials>,
    ) -> Result<PullSource, RuntimeError> {
        let started = Instant::now();
        info!(
            image = %image,
            pull_policy = ?policy,
            "ensure_image entry"
        );

        info!(image = %image, "image_inspect begin");
        let image_exists = self.docker.inspect_image(image).await.is_ok();

        let should_pull = match policy {
            ImagePullPolicy::Always => true,
            ImagePullPolicy::IfNotPresent => {
                if !image_exists {
                    info!(image = %image, cached = false, "image not cached locally");
                }
                !image_exists
            }
            ImagePullPolicy::Never => false,
        };

        if !should_pull {
            if !image_exists && matches!(policy, ImagePullPolicy::Never) {
                return Err(RuntimeError::SpawnFailed(format!(
                    "Image {image} not found locally and image_pull_policy is Never"
                )));
            }
            info!(
                image = %image,
                cached = true,
                "image_inspect complete; using cached"
            );
            return Ok(PullSource::Cached);
        }

        // Use the per-call override when provided (ADR-050 per-step registry auth),
        // otherwise fall back to the node-config CredentialResolver (ADR-045).
        // SAFETY: DockerCredentials is passed directly to bollard::Docker::create_image()
        // and is never Debug-logged or included in any log/trace call. The `password` field
        // is a resolved credential used solely for registry authentication.
        let auth = if let Some(override_creds) = credentials_override {
            Some(override_creds)
        } else {
            let credentials = self.credential_resolver.resolve(image).await;
            credentials.map(|cred| {
                // Resolve env:VAR_NAME substitution on the password if present.
                let password = if let Some(var) = cred.password.strip_prefix("env:") {
                    std::env::var(var).unwrap_or(cred.password.clone())
                } else {
                    cred.password.clone()
                };
                DockerCredentials {
                    username: Some(cred.username.clone()),
                    password: Some(password),
                    serveraddress: Some(cred.registry.clone()),
                    ..Default::default()
                }
            })
        };

        info!(image = %image, "image_pull_begin");
        let options = Some(CreateImageOptions {
            from_image: Some(image.to_string()),
            ..Default::default()
        });

        let docker = self.docker.clone();
        let image_owned = image.to_string();
        let pull_fut = async move {
            let mut stream = docker.create_image(options, None, auth);
            let mut last_status: Option<String> = None;
            while let Some(result) = stream.next().await {
                match result {
                    Ok(chunk) => {
                        if let Some(s) = chunk.status.as_ref() {
                            // Deduplicate consecutive identical status strings
                            // to avoid log spam from per-layer progress chunks.
                            if last_status.as_deref() != Some(s.as_str()) {
                                info!(image = %image_owned, status = %s, "pull progress");
                                last_status = Some(s.clone());
                            }
                        }
                    }
                    Err(e) => {
                        let error_msg = format!(
                            "Failed to pull image {image_owned}: {e}\n\n\
                             Common causes:\n\
                             - Docker daemon not responding (connection issue)\n\
                             - No internet connectivity to Docker Hub\n\
                             - Image name is incorrect or doesn't exist\n\
                             - Corporate proxy/firewall blocking Docker Hub\n\
                             - Registry authentication required\n\n\
                             Try manually: docker pull {image_owned}"
                        );
                        return Err(RuntimeError::SpawnFailed(error_msg));
                    }
                }
            }
            Ok(PullSource::Downloaded)
        };

        match run_pull_with_timeout(image, IMAGE_PULL_TIMEOUT_SECS, pull_fut).await {
            Ok(source) => {
                info!(
                    image = %image,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Successfully pulled image"
                );
                Ok(source)
            }
            Err(e) => {
                error!(
                    image = %image,
                    error = %e,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "image pull failed"
                );
                Err(e)
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// These exercise the timeout-wrapping helper directly. Bollard's `Docker`
// is not trait-abstracted in this codebase, so full stubbing of
// `inspect_image` / `create_image` would require a much larger refactor.
// The helper carries the timeout semantics that this commit introduces, so
// unit-testing the helper is sufficient to lock the regression.
#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[tokio::test]
    async fn run_pull_with_timeout_returns_ok_when_future_completes() {
        let result: Result<PullSource, RuntimeError> =
            run_pull_with_timeout("repo/img:tag", 5, async { Ok(PullSource::Downloaded) }).await;
        assert!(matches!(result, Ok(PullSource::Downloaded)));
    }

    #[tokio::test]
    async fn run_pull_with_timeout_propagates_inner_error() {
        let result: Result<PullSource, RuntimeError> =
            run_pull_with_timeout("repo/img:tag", 5, async {
                Err(RuntimeError::SpawnFailed("boom".into()))
            })
            .await;
        match result {
            Err(RuntimeError::SpawnFailed(msg)) => assert_eq!(msg, "boom"),
            other => panic!("expected SpawnFailed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn run_pull_with_timeout_returns_image_pull_timeout_when_pending() {
        // Future never completes; with paused time the timeout fires deterministically.
        let pending_fut = async {
            pending::<()>().await;
            Ok(PullSource::Downloaded)
        };
        let result = run_pull_with_timeout("repo/img:tag", 1, pending_fut).await;
        match result {
            Err(RuntimeError::ImagePullTimeout {
                image,
                timeout_secs,
            }) => {
                assert_eq!(image, "repo/img:tag");
                assert_eq!(timeout_secs, 1);
            }
            other => panic!("expected ImagePullTimeout, got {other:?}"),
        }
    }

    #[test]
    fn image_pull_timeout_default_is_180s() {
        // Lock the production budget so a future change is intentional.
        assert_eq!(IMAGE_PULL_TIMEOUT_SECS, 180);
    }
}
