// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # BollardContainerVerifier (BC-12, ADR-035 §4.1)
//!
//! Infrastructure adapter that verifies whether a container is currently
//! running by querying the local container runtime (Docker or Podman) via the
//! bollard client.
//!
//! ## Fail-fast attestation
//!
//! Container identity verification sits on the synchronous attestation
//! request path. If the configured runtime socket is unreachable or
//! misconfigured, bollard's default 120s socket timeout would cause every
//! attestation request to hang for two minutes before returning a 401, which
//! piles up handlers and breaks Claude Code's MCP usage. The verifier
//! therefore enforces an explicit short timeout ([`VERIFIER_TIMEOUT_SECS`])
//! independent of the runtime adapter's longer-lived lifecycle timeout.

use crate::application::ports::ContainerVerificationPort;

/// Wall-clock budget for a single `inspect_container` call against the
/// container runtime. Tight by design — attestation is on the request path
/// and a misconfigured socket must surface as an error in seconds rather
/// than minutes (bollard's default is 120s, which is unacceptable here).
pub const VERIFIER_TIMEOUT_SECS: u64 = 5;

/// Verifies container liveness using the bollard client.
pub struct BollardContainerVerifier {
    docker: bollard::Docker,
}

impl BollardContainerVerifier {
    /// Connect to the container runtime using the configured socket path
    /// (Docker or Podman). When `socket_path` is `None`, bollard's local
    /// defaults are used (`DOCKER_HOST` env var or `/var/run/docker.sock`).
    ///
    /// The bollard client is configured with [`VERIFIER_TIMEOUT_SECS`] so
    /// that a misconfigured or unreachable socket fails fast instead of
    /// blocking on bollard's 120s default.
    pub fn new(socket_path: Option<&str>) -> anyhow::Result<Self> {
        let docker = if let Some(path) = socket_path {
            #[cfg(unix)]
            {
                bollard::Docker::connect_with_unix(
                    path,
                    VERIFIER_TIMEOUT_SECS,
                    bollard::API_DEFAULT_VERSION,
                )?
            }
            #[cfg(windows)]
            {
                bollard::Docker::connect_with_named_pipe(
                    path,
                    VERIFIER_TIMEOUT_SECS,
                    bollard::API_DEFAULT_VERSION,
                )?
            }
        } else {
            // Bollard's local-defaults connector uses its own 120s timeout;
            // re-wrap onto the unix socket it would have selected so that
            // attestation still fails fast on the default path.
            #[cfg(unix)]
            {
                bollard::Docker::connect_with_unix(
                    "/var/run/docker.sock",
                    VERIFIER_TIMEOUT_SECS,
                    bollard::API_DEFAULT_VERSION,
                )?
            }
            #[cfg(not(unix))]
            {
                bollard::Docker::connect_with_local_defaults()?
            }
        };
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Regression test for the 120s attestation hang.
    ///
    /// Before the fix, [`BollardContainerVerifier::new`] called
    /// `bollard::Docker::connect_with_local_defaults()`, which on a Podman
    /// deployment with a misconfigured `docker.sock` path wedged on bollard's
    /// 120s default timeout, breaking Claude Code's MCP usage.
    ///
    /// This test points the verifier at a deliberately-unreachable socket
    /// path and asserts that `verify_container_running` returns an error in
    /// well under 10 seconds (the [`VERIFIER_TIMEOUT_SECS`] budget is 5s).
    #[tokio::test]
    async fn verifier_fails_fast_when_socket_unreachable() {
        let bogus = "/tmp/aegis-attestation-nonexistent-socket.sock";
        let verifier = BollardContainerVerifier::new(Some(bogus))
            .expect("verifier construction must not require a live socket");

        let result = timeout(
            Duration::from_secs(10),
            verifier.verify_container_running("any-container"),
        )
        .await;

        let inner = result.expect(
            "verify_container_running must fail within 10s; bollard's 120s default timeout \
             would indicate the verifier is not honoring VERIFIER_TIMEOUT_SECS",
        );
        assert!(
            inner.is_err(),
            "verifier pointed at a nonexistent socket must return an error"
        );
    }

    /// Regression test that the configured socket path is honored.
    ///
    /// Two verifiers are constructed against two distinct unreachable paths;
    /// the resulting error messages must reference (or be consistent with)
    /// the path that was actually configured. This catches a regression
    /// where the verifier silently falls back to `connect_with_local_defaults`
    /// and ignores the caller-supplied path.
    #[tokio::test]
    async fn verifier_uses_configured_socket_path() {
        let path_a = "/tmp/aegis-attestation-path-a.sock";
        let path_b = "/tmp/aegis-attestation-path-b.sock";

        let verifier_a = BollardContainerVerifier::new(Some(path_a)).unwrap();
        let verifier_b = BollardContainerVerifier::new(Some(path_b)).unwrap();

        let err_a = timeout(
            Duration::from_secs(10),
            verifier_a.verify_container_running("c"),
        )
        .await
        .expect("must fail fast")
        .expect_err("nonexistent socket must error")
        .to_string();

        let err_b = timeout(
            Duration::from_secs(10),
            verifier_b.verify_container_running("c"),
        )
        .await
        .expect("must fail fast")
        .expect_err("nonexistent socket must error")
        .to_string();

        // The two errors must mention their respective paths (or otherwise
        // differ): if the verifier was ignoring `socket_path` and falling
        // back to local defaults, both errors would be identical.
        assert!(
            err_a.contains(path_a) || err_b.contains(path_b) || err_a != err_b,
            "verifier must honor the configured socket path; got identical errors:\n  a: {err_a}\n  b: {err_b}"
        );
    }
}
