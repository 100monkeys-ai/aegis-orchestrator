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
//! point; swapping the resolver for Phase 2 requires no structural changes to `DockerRuntime`.
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
use tracing::info;

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
/// Extracted from inline `DockerRuntime::spawn()` logic to enable independent
/// testing and Phase 2 credential injection. `DockerRuntime` calls
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
    /// Used by [`crate::infrastructure::container_step_runner::DockerContainerStepRunner`]
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
        let image_exists = self.docker.inspect_image(image).await.is_ok();

        let should_pull = match policy {
            ImagePullPolicy::Always => true,
            ImagePullPolicy::IfNotPresent => {
                if !image_exists {
                    info!("Image {} not found locally, will attempt to pull", image);
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

        info!("Pulling image: {}", image);
        let options = Some(CreateImageOptions {
            from_image: Some(image.to_string()),
            ..Default::default()
        });

        let mut stream = self.docker.create_image(options, None, auth);
        while let Some(result) = stream.next().await {
            if let Err(e) = result {
                let error_msg = format!(
                    "Failed to pull image {image}: {e}\n\n\
                     Common causes:\n\
                     - Docker daemon not responding (connection issue)\n\
                     - No internet connectivity to Docker Hub\n\
                     - Image name is incorrect or doesn't exist\n\
                     - Corporate proxy/firewall blocking Docker Hub\n\
                     - Registry authentication required\n\n\
                     Try manually: docker pull {image}"
                );
                return Err(RuntimeError::SpawnFailed(error_msg));
            }
        }
        info!("Successfully pulled image: {}", image);

        Ok(PullSource::Downloaded)
    }
}
