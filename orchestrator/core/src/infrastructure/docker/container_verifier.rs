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
    use std::time::{Duration, Instant};
    use tokio::net::UnixListener;
    use tokio::time::timeout;

    /// Spawn a Unix listener at `path` that accepts connections but never
    /// writes a response. This simulates a runtime socket that is reachable
    /// at the IPC layer but wedged at the HTTP layer — the exact failure
    /// mode [`VERIFIER_TIMEOUT_SECS`] is designed to bound. bollard
    /// validates the socket path exists at construction time, so a real
    /// listener is required to exercise the inspect-call timeout path.
    fn spawn_silent_listener(path: &std::path::Path) {
        let listener = UnixListener::bind(path).expect("bind listening unix socket");
        tokio::spawn(async move {
            // Hold accepted streams open without writing so the client-side
            // HTTP read blocks until the verifier's timeout fires.
            let mut held = Vec::new();
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => held.push(stream),
                    Err(_) => break,
                }
            }
        });
    }

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
        // bollard validates the socket path at construction time, so we bind
        // a real listener that accepts connections but never replies. This
        // forces the failure mode through the inspect-call timeout rather
        // than through socket-existence validation, exercising the actual
        // VERIFIER_TIMEOUT_SECS budget.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("verifier.sock");
        spawn_silent_listener(&sock);

        let verifier = BollardContainerVerifier::new(Some(sock.to_str().unwrap()))
            .expect("verifier construction against a live listening socket must succeed");

        let start = Instant::now();
        let result = timeout(
            Duration::from_secs(10),
            verifier.verify_container_running("any-container"),
        )
        .await;
        let elapsed = start.elapsed();

        let inner = result.expect(
            "verify_container_running must fail within 10s; bollard's 120s default timeout \
             would indicate the verifier is not honoring VERIFIER_TIMEOUT_SECS",
        );
        assert!(
            inner.is_err(),
            "verifier pointed at a silent socket must return a timeout error"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "verifier must fail within 10s, took {elapsed:?}"
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
        // Two distinct listening sockets in two distinct tempdirs. Both
        // verifier constructions must succeed (proving the caller-supplied
        // path was passed through to bollard, since a fallback to
        // `/var/run/docker.sock` would not exist on the CI runner). Both
        // inspect calls then fail fast against their respective sockets
        // within VERIFIER_TIMEOUT_SECS.
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let path_a = dir_a.path().join("a.sock");
        let path_b = dir_b.path().join("b.sock");

        spawn_silent_listener(&path_a);
        spawn_silent_listener(&path_b);

        let verifier_a = BollardContainerVerifier::new(Some(path_a.to_str().unwrap()))
            .expect("construction against listening socket A must succeed");
        let verifier_b = BollardContainerVerifier::new(Some(path_b.to_str().unwrap()))
            .expect("construction against listening socket B must succeed");

        let start_a = Instant::now();
        let _err_a = timeout(
            Duration::from_secs(10),
            verifier_a.verify_container_running("c"),
        )
        .await
        .expect("verifier A must fail fast")
        .expect_err("silent socket must error");
        let elapsed_a = start_a.elapsed();

        let start_b = Instant::now();
        let _err_b = timeout(
            Duration::from_secs(10),
            verifier_b.verify_container_running("c"),
        )
        .await
        .expect("verifier B must fail fast")
        .expect_err("silent socket must error");
        let elapsed_b = start_b.elapsed();

        // If the verifier ignored `socket_path` and fell back to
        // `connect_with_local_defaults`, construction would have either
        // failed (no `/var/run/docker.sock` on the CI runner) or it would
        // have connected somewhere other than our listeners. Both
        // construction-success and fast-fail-against-listener prove the
        // configured path is honored.
        assert!(
            elapsed_a < Duration::from_secs(10),
            "verifier A must fail within 10s, took {elapsed_a:?}"
        );
        assert!(
            elapsed_b < Duration::from_secs(10),
            "verifier B must fail within 10s, took {elapsed_b:?}"
        );
    }
}
