// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # BollardContainerVerifier (BC-12, ADR-035 §4.1)
//!
//! Infrastructure adapter that verifies whether a Docker container is currently
//! running by querying the local Docker daemon via the bollard client.

use crate::application::ports::ContainerVerificationPort;

/// Verifies Docker container liveness using the bollard client.
pub struct BollardContainerVerifier {
    docker: bollard::Docker,
}

impl BollardContainerVerifier {
    /// Connect to the Docker daemon using local defaults (Unix socket or
    /// `DOCKER_HOST` env var).
    pub fn new() -> anyhow::Result<Self> {
        let docker = bollard::Docker::connect_with_local_defaults()?;
        Ok(Self { docker })
    }
}

#[async_trait::async_trait]
impl ContainerVerificationPort for BollardContainerVerifier {
    async fn verify_container_running(&self, container_id: &str) -> anyhow::Result<()> {
        let inspect = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Docker inspect failed for container '{}': {}",
                    container_id,
                    e
                )
            })?;

        let running = inspect.state.and_then(|s| s.running).unwrap_or(false);

        if running {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Container '{}' is not in a running state",
                container_id
            ))
        }
    }
}
