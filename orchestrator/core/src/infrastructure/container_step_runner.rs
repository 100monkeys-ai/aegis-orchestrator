// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Docker Container Step Runner — BC-3 CI/CD (ADR-050)
//!
//! Implements the [`crate::domain::runtime::ContainerStepRunner`] domain trait
//! using the Docker Engine API via `bollard`. Executes deterministic CI/CD
//! container steps inside AEGIS workflow manifests without any LLM loop.
//!
//! ## Responsibilities
//! - Pull container image using shared [`crate::infrastructure::image_manager::DockerImageManager`]
//! - Apply optional resource limits (CPU millicores, memory, timeout)
//! - Mount NFS volumes via the Orchestrator Proxy Pattern (ADR-036)
//! - Stream stdout/stderr from the container with a 1 MiB cap per stream
//! - Return exit code, captured output, and duration to the application layer
//! - Publish [`ContainerRunEvent`] domain events for audit trail and Cortex learning
//! - Always remove the container on completion or error (no leaked resources)
//!
//! See ADR-050 (CI/CD Orchestration via Workflows), ADR-036 (NFS Server Gateway),
//! ADR-027 (Docker Runtime Implementation Details).

use crate::domain::events::{ContainerRunEvent, ContainerRunFailureReason};
use crate::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
};
use crate::domain::secrets::AccessContext;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::image_manager::DockerImageManager;
use crate::infrastructure::secrets_manager::SecretsManager;
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, HostConfig, Mount, MountTypeEnum, MountVolumeOptions,
    MountVolumeOptionsDriverConfig,
};
use bollard::query_parameters::{
    CreateContainerOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions,
    WaitContainerOptions,
};
use bollard::Docker;
use chrono::Utc;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::time::{timeout, Duration};
use tracing::{debug, info, warn};

// 1 MiB cap per stream (stdout/stderr) to prevent runaway output from flooding memory.
const STREAM_BYTES_CAP: usize = 1_048_576;

/// Infrastructure implementation of [`ContainerStepRunner`] backed by the
/// Docker Engine API (bollard). Shares image management and NFS configuration
/// with [`crate::infrastructure::runtime::ContainerRuntime`].
pub struct ContainerStepRunnerImpl {
    docker: Docker,
    image_manager: Arc<dyn DockerImageManager>,
    /// Explicit NFS server host used for volume mount options (addr=...).
    /// Same semantics as `ContainerRuntime::nfs_server_host` (ADR-036).
    nfs_server_host: Option<String>,
    nfs_port: u16,
    nfs_mountport: u16,
    event_bus: Arc<EventBus>,
    /// Used to resolve per-step registry credentials stored in OpenBao (ADR-050).
    /// When `ContainerStepConfig::registry_credentials` is `Some("secret:engine/path")`,
    /// the runner reads `username`, `password`, and optionally `serveraddress` from
    /// the vault secret and passes them as a `DockerCredentials` override to
    /// [`DockerImageManager::ensure_image`].
    secrets_manager: Arc<SecretsManager>,
}

impl ContainerStepRunnerImpl {
    pub fn new(
        docker: Docker,
        image_manager: Arc<dyn DockerImageManager>,
        nfs_server_host: Option<String>,
        nfs_port: u16,
        nfs_mountport: u16,
        event_bus: Arc<EventBus>,
        secrets_manager: Arc<SecretsManager>,
    ) -> Self {
        Self {
            docker,
            image_manager,
            nfs_server_host,
            nfs_port,
            nfs_mountport,
            event_bus,
            secrets_manager,
        }
    }

    fn failure_reason_for_error(error: &ContainerStepError) -> ContainerRunFailureReason {
        match error {
            ContainerStepError::ImagePullFailed { image, error } => {
                ContainerRunFailureReason::ImagePullFailed {
                    image: image.clone(),
                    error: error.clone(),
                }
            }
            ContainerStepError::TimeoutExpired { timeout_secs } => {
                ContainerRunFailureReason::TimeoutExpired {
                    timeout_secs: *timeout_secs,
                }
            }
            ContainerStepError::VolumeMountFailed { volume, error } => {
                ContainerRunFailureReason::VolumeMountFailed {
                    volume: volume.clone(),
                    error: error.clone(),
                }
            }
            ContainerStepError::ResourceExhausted { detail } => {
                ContainerRunFailureReason::ResourceExhausted {
                    detail: detail.clone(),
                }
            }
            ContainerStepError::DockerError(msg) => ContainerRunFailureReason::ResourceExhausted {
                detail: msg.clone(),
            },
        }
    }

    fn publish_failed_event(
        &self,
        config: &ContainerStepConfig,
        reason: ContainerRunFailureReason,
    ) {
        self.event_bus
            .publish_container_run_event(ContainerRunEvent::ContainerRunFailed {
                execution_id: config.execution_id,
                state_name: config.state_name.to_string(),
                step_name: config.name.clone(),
                reason,
                failed_at: Utc::now(),
            });
    }
}

#[async_trait]
impl ContainerStepRunner for ContainerStepRunnerImpl {
    async fn run_step(
        &self,
        config: ContainerStepConfig,
    ) -> Result<ContainerStepResult, ContainerStepError> {
        let start = Instant::now();

        info!(
            execution_id = %config.execution_id,
            state_name = %config.state_name,
            step_name = %config.name,
            image = %config.image,
            "ContainerStep starting"
        );

        // ─── 1. Publish ContainerRunStarted ───────────────────────────────────
        self.event_bus
            .publish_container_run_event(ContainerRunEvent::ContainerRunStarted {
                execution_id: config.execution_id,
                state_name: config.state_name.to_string(),
                step_name: config.name.clone(),
                image: config.image.clone(),
                command: config.command.clone(),
                started_at: Utc::now(),
            });

        // ─── 2. Resolve per-step registry credentials (ADR-050) ───────────────
        // Supports `None` (anonymous), `env:VAR_NAME`, and `secret:engine/path`.
        // Any other format is rejected immediately so misconfigurations fail fast.
        let credentials_override_result: Result<
            Option<bollard::auth::DockerCredentials>,
            ContainerStepError,
        > = match &config.registry_credentials {
            None => Ok(None),
            Some(s) if s.starts_with("env:") => {
                let var_name = s.strip_prefix("env:").unwrap();

                // Validate that the env var reference doesn't target orchestrator-internal vars
                if let Err(reason) = crate::domain::env_guard::validate_env_ref(var_name) {
                    let error = ContainerStepError::ImagePullFailed {
                        image: config.image.clone(),
                        error: reason,
                    };
                    self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                    return Err(error);
                }

                let resolved =
                    (|| -> Result<bollard::auth::DockerCredentials, ContainerStepError> {
                        let raw = std::env::var(var_name).map_err(|_| {
                            ContainerStepError::ImagePullFailed {
                                image: config.image.clone(),
                                error: format!(
                                "environment variable '{var_name}' (from registry_credentials) is not set"
                            ),
                            }
                        })?;

                        // Expected format: "username:password@serveraddress" or "username:password".
                        // Split on the last '@' so passwords that contain '@' still work.
                        let (user_pass, serveraddress) = if let Some(pos) = raw.rfind('@') {
                            (&raw[..pos], Some(raw[pos + 1..].to_string()))
                        } else {
                            (raw.as_str(), None)
                        };

                        let (username, password) = user_pass.split_once(':').ok_or_else(|| {
                            ContainerStepError::ImagePullFailed {
                                image: config.image.clone(),
                                error: format!(
                                    "env var '{var_name}' must be in format 'username:password' or \
                                     'username:password@serveraddress'"
                                ),
                            }
                        })?;

                        Ok(bollard::auth::DockerCredentials {
                            username: Some(username.to_string()),
                            password: Some(password.to_string()),
                            serveraddress,
                            ..Default::default()
                        })
                    })();

                resolved.map(Some)
            }
            Some(s) if s.starts_with("secret:") => {
                let secret_ref_path = s.strip_prefix("secret:").unwrap();
                let resolved = async {
                    let (engine, secret_path) =
                        secret_ref_path.split_once('/').ok_or_else(|| {
                            ContainerStepError::ImagePullFailed {
                                image: config.image.clone(),
                                error: format!(
                                    "secret registry_credentials '{s}' must be in format \
                                     'secret:engine/path'"
                                ),
                            }
                        })?;

                    let ctx = AccessContext::system("orchestrator");
                    let fields = self
                        .secrets_manager
                        .read_secret(engine, secret_path, &ctx)
                        .await
                        .map_err(|e| ContainerStepError::ImagePullFailed {
                            image: config.image.clone(),
                            error: format!(
                                "failed to read registry credentials from OpenBao at '{secret_ref_path}': {e}"
                            ),
                        })?;

                    let username = fields.get("username").ok_or_else(|| {
                        ContainerStepError::ImagePullFailed {
                            image: config.image.clone(),
                            error: format!(
                                "OpenBao secret at '{secret_ref_path}' is missing required field 'username'"
                            ),
                        }
                    })?;

                    let password = fields.get("password").ok_or_else(|| {
                        ContainerStepError::ImagePullFailed {
                            image: config.image.clone(),
                            error: format!(
                                "OpenBao secret at '{secret_ref_path}' is missing required field 'password'"
                            ),
                        }
                    })?;

                    let serveraddress = fields.get("serveraddress").map(|v| v.expose().to_string());
                    Ok::<bollard::auth::DockerCredentials, ContainerStepError>(
                        bollard::auth::DockerCredentials {
                            username: Some(username.expose().to_string()),
                            password: Some(password.expose().to_string()),
                            serveraddress,
                            ..Default::default()
                        },
                    )
                }
                .await;

                resolved.map(Some)
            }
            Some(s) => Err(ContainerStepError::ImagePullFailed {
                image: config.image.clone(),
                error: format!(
                    "unrecognised registry_credentials format '{s}'; \
                             expected 'env:VAR_NAME' or 'secret:engine/path'"
                ),
            }),
        };

        let credentials_override = match credentials_override_result {
            Ok(value) => value,
            Err(error) => {
                self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                return Err(error);
            }
        };

        // ─── 3. Pull image ─────────────────────────────────────────────────────
        if let Err(e) = self
            .image_manager
            .ensure_image(
                &config.image,
                config.image_pull_policy,
                credentials_override,
            )
            .await
        {
            let error = ContainerStepError::ImagePullFailed {
                image: config.image.clone(),
                error: e.to_string(),
            };
            self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
            return Err(error);
        }

        // ─── 4. Build NFS volume mounts (ADR-036) ─────────────────────────────────
        let host_config = {
            let mut hc = HostConfig {
                ..Default::default()
            };

            // Resource limits
            if let Some(ref res) = config.resources {
                if let Some(cpu) = res.cpu {
                    // cpu is in millicores; Docker expects nano CPUs (1 CPU = 1_000_000_000)
                    hc.nano_cpus = Some((cpu as i64) * 1_000_000_000 / 1000);
                }
                if let Some(ref mem) = res.memory {
                    // Parse human-readable memory string (e.g. "512m", "2g")
                    if let Some(bytes) = parse_memory_string(mem) {
                        hc.memory = Some(bytes);
                    } else {
                        warn!(
                            memory = %mem,
                            "Could not parse memory limit string; ignoring"
                        );
                    }
                }
            }

            // NFS mounts
            if !config.volumes.is_empty() {
                let nfs_host = self.nfs_server_host.as_deref().unwrap_or("127.0.0.1");

                let mounts: Vec<Mount> = config
                    .volumes
                    .iter()
                    .map(|vm| {
                        let mut driver_opts = HashMap::new();
                        driver_opts.insert("type".to_string(), "nfs".to_string());
                        driver_opts.insert(
                            "o".to_string(),
                            format!(
                                "addr={},nfsvers=3,proto=tcp,port={},mountport={},soft,timeo=10,nolock",
                                nfs_host, self.nfs_port, self.nfs_mountport
                            ),
                        );
                        // Device path: /{execution_id}/{volume_name}
                        // The NFS Server Gateway (AegisFSAL) owns this namespace.
                        driver_opts.insert(
                            "device".to_string(),
                            format!(":{}/{}", config.execution_id, vm.name),
                        );

                        debug!(
                            step_name = %config.name,
                            volume_name = %vm.name,
                            mount_path = %vm.mount_path,
                            read_only = vm.read_only,
                            nfs_host = nfs_host,
                            "Configuring NFS mount for container step"
                        );

                        Mount {
                            target: Some(vm.mount_path.clone()),
                            source: Some(format!(
                                "aegis-step-{}-{}",
                                config.execution_id, vm.name
                            )),
                            typ: Some(MountTypeEnum::VOLUME),
                            read_only: Some(vm.read_only),
                            volume_options: Some(MountVolumeOptions {
                                driver_config: Some(MountVolumeOptionsDriverConfig {
                                    name: Some("local".to_string()),
                                    options: Some(driver_opts),
                                }),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }
                    })
                    .collect();

                hc.mounts = Some(mounts);
                info!(
                    step_name = %config.name,
                    count = config.volumes.len(),
                    "Configured NFS volume mount(s) for container step (ADR-036)"
                );
            }

            hc
        };

        // ─── 4. Build env vars ────────────────────────────────────────────────
        // Intentionally does NOT inject AEGIS_ORCHESTRATOR_URL or any bootstrap
        // env vars — this is a deterministic CI/CD step, not an agent iteration.
        // Filter env vars to prevent orchestrator-internal variables from leaking.
        let (filtered_env, blocked_vars) = crate::domain::env_guard::filter_env_vars(&config.env);
        if !blocked_vars.is_empty() {
            warn!(
                execution_id = %config.execution_id,
                step_name = %config.name,
                blocked = ?blocked_vars,
                "Blocked orchestrator-internal env vars from container step"
            );
        }
        let env_strings: Vec<String> = filtered_env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        // ─── 5. Resolve command ───────────────────────────────────────────────
        // Shell wrapping (sh -c) is applied upstream in RunContainerStepUseCase
        // before constructing ContainerStepConfig; ContainerStepConfig always
        // carries the final resolved argv.
        let cmd: Vec<String> = config.command.clone();

        // ─── 6. Create container ──────────────────────────────────────────────
        let container_name = format!(
            "aegis-step-{}-{}",
            config.execution_id,
            uuid::Uuid::new_v4()
        );

        let container_config = ContainerCreateBody {
            image: Some(config.image.clone()),
            cmd: Some(cmd),
            env: Some(env_strings),
            working_dir: config.workdir.clone(),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            // No tty — we want raw stdout/stderr separately
            tty: Some(false),
            host_config: Some(host_config),
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: Some(container_name.clone()),
            platform: String::new(),
        };

        let container = self
            .docker
            .create_container(Some(options), container_config)
            .await
            .map_err(|e| {
                if e.to_string().to_lowercase().contains("mount") {
                    ContainerStepError::VolumeMountFailed {
                        volume: "unknown".to_string(),
                        error: format!("create_container: {e}"),
                    }
                } else {
                    ContainerStepError::DockerError(format!("create_container: {e}"))
                }
            });

        let container = match container {
            Ok(container) => container,
            Err(error) => {
                self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                return Err(error);
            }
        };

        let container_id = container.id.clone();

        // ─── 7. Start container ───────────────────────────────────────────────
        if let Err(e) = self
            .docker
            .start_container(&container_id, None::<StartContainerOptions>)
            .await
        {
            if let Err(cleanup_error) = self
                .docker
                .remove_container(
                    &container_id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                warn!(
                    container_id = %container_id,
                    error = %cleanup_error,
                    "Failed to remove container after start failure"
                );
            }
            let error = ContainerStepError::DockerError(format!("start_container: {e}"));
            self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
            return Err(error);
        }

        debug!(
            container_id = %container_id,
            step_name = %config.name,
            "Container step started"
        );

        // ─── 9. Stream logs + wait with optional timeout ──────────────────────
        let timeout_duration = config.resources.as_ref().and_then(|r| r.timeout);

        let capture_result =
            capture_logs_and_wait(&self.docker, &container_id, timeout_duration).await;

        // ─── 10. Always remove container ───────────────────────────────────────────
        if let Err(e) = self
            .docker
            .remove_container(
                &container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            warn!(
                container_id = %container_id,
                error = %e,
                "Failed to remove container step; resource may linger"
            );
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        // ─── 11. Map capture result to domain result / event ───────────────────
        match capture_result {
            Err(CaptureError::Timeout { timeout_secs }) => {
                let error = ContainerStepError::TimeoutExpired { timeout_secs };
                self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                Err(error)
            }
            Err(CaptureError::Docker(msg)) => {
                let error = ContainerStepError::DockerError(msg);
                self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                Err(error)
            }
            Ok(captured) => {
                let result = ContainerStepResult {
                    exit_code: captured.exit_code,
                    stdout: captured.stdout,
                    stderr: captured.stderr,
                    duration_ms,
                };

                if result.exit_code == 0 {
                    self.event_bus.publish_container_run_event(
                        ContainerRunEvent::ContainerRunCompleted {
                            execution_id: config.execution_id,
                            state_name: config.state_name.to_string(),
                            step_name: config.name.clone(),
                            exit_code: result.exit_code,
                            stdout_bytes: result.stdout.len() as u64,
                            stderr_bytes: result.stderr.len() as u64,
                            duration_ms,
                            completed_at: Utc::now(),
                        },
                    );
                } else {
                    self.publish_failed_event(
                        &config,
                        ContainerRunFailureReason::NonZeroExitCode {
                            code: result.exit_code,
                        },
                    );
                }

                info!(
                    container_id = %container_id,
                    step_name = %config.name,
                    exit_code = result.exit_code,
                    duration_ms = duration_ms,
                    "Container step completed"
                );

                Ok(result)
            }
        }
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

struct CapturedOutput {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

enum CaptureError {
    Timeout { timeout_secs: u64 },
    Docker(String),
}

/// Stream stdout/stderr from a started container and wait for it to exit.
/// Applies an optional `timeout_duration` around the entire wait window.
/// Truncates each stream at [`STREAM_BYTES_CAP`] bytes.
async fn capture_logs_and_wait(
    docker: &Docker,
    container_id: &str,
    timeout_duration: Option<Duration>,
) -> Result<CapturedOutput, CaptureError> {
    let fut = do_capture(docker, container_id);

    if let Some(dur) = timeout_duration {
        match timeout(dur, fut).await {
            Ok(result) => result,
            Err(_elapsed) => Err(CaptureError::Timeout {
                timeout_secs: dur.as_secs(),
            }),
        }
    } else {
        fut.await
    }
}

async fn do_capture(docker: &Docker, container_id: &str) -> Result<CapturedOutput, CaptureError> {
    // Stream logs with follow=true — this blocks until the container stops.
    let log_opts = LogsOptions {
        stdout: true,
        stderr: true,
        follow: true,
        ..Default::default()
    };

    let mut log_stream = docker.logs(container_id, Some(log_opts));

    let mut stdout_bytes: usize = 0;
    let mut stderr_bytes: usize = 0;
    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    while let Some(msg) = log_stream.next().await {
        match msg {
            Ok(LogOutput::StdOut { message }) => {
                if stdout_bytes < STREAM_BYTES_CAP {
                    let remaining = STREAM_BYTES_CAP - stdout_bytes;
                    let chunk = String::from_utf8_lossy(&message);
                    let truncated = if chunk.len() > remaining {
                        &chunk[..remaining]
                    } else {
                        &chunk
                    };
                    stdout_buf.push_str(truncated);
                    stdout_bytes += message.len();
                }
            }
            Ok(LogOutput::StdErr { message }) => {
                if stderr_bytes < STREAM_BYTES_CAP {
                    let remaining = STREAM_BYTES_CAP - stderr_bytes;
                    let chunk = String::from_utf8_lossy(&message);
                    let truncated = if chunk.len() > remaining {
                        &chunk[..remaining]
                    } else {
                        &chunk
                    };
                    stderr_buf.push_str(truncated);
                    stderr_bytes += message.len();
                }
            }
            Ok(_) => {}
            Err(e) => {
                return Err(CaptureError::Docker(format!("logs stream error: {e}")));
            }
        }
    }

    // Wait for container exit to get the exit code.
    let wait_opts = WaitContainerOptions {
        condition: "not-running".to_string(),
    };
    let mut wait_stream = docker.wait_container(container_id, Some(wait_opts));

    let exit_code = if let Some(result) = wait_stream.next().await {
        match result {
            Ok(body) => body.status_code as i32,
            Err(e) => {
                return Err(CaptureError::Docker(format!("wait_container error: {e}")));
            }
        }
    } else {
        0
    };

    Ok(CapturedOutput {
        exit_code,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// Parse Docker-style memory strings (e.g. `"512m"`, `"2g"`, `"1024k"`, `"1073741824"`).
/// Returns `None` if the string cannot be parsed.
fn parse_memory_string(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();
    if let Some(num) = s.strip_suffix("gi") {
        num.trim().parse::<i64>().ok().map(|n| n * 1_073_741_824)
    } else if let Some(num) = s.strip_suffix("mi") {
        num.trim().parse::<i64>().ok().map(|n| n * 1_048_576)
    } else if let Some(num) = s.strip_suffix("ki") {
        num.trim().parse::<i64>().ok().map(|n| n * 1_024)
    } else if let Some(num) = s.strip_suffix('g') {
        num.trim().parse::<i64>().ok().map(|n| n * 1_073_741_824)
    } else if let Some(num) = s.strip_suffix("gb") {
        num.trim().parse::<i64>().ok().map(|n| n * 1_073_741_824)
    } else if let Some(num) = s.strip_suffix('m') {
        num.trim().parse::<i64>().ok().map(|n| n * 1_048_576)
    } else if let Some(num) = s.strip_suffix("mb") {
        num.trim().parse::<i64>().ok().map(|n| n * 1_048_576)
    } else if let Some(num) = s.strip_suffix('k') {
        num.trim().parse::<i64>().ok().map(|n| n * 1_024)
    } else if let Some(num) = s.strip_suffix("kb") {
        num.trim().parse::<i64>().ok().map(|n| n * 1_024)
    } else {
        s.parse::<i64>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_memory_string;

    #[test]
    fn parse_memory_string_supports_iec_units() {
        assert_eq!(parse_memory_string("4Gi"), Some(4 * 1_073_741_824));
        assert_eq!(parse_memory_string("512Mi"), Some(512 * 1_048_576));
        assert_eq!(parse_memory_string("64Ki"), Some(64 * 1_024));
    }

    #[test]
    fn parse_memory_string_supports_decimal_units() {
        assert_eq!(parse_memory_string("2g"), Some(2 * 1_073_741_824));
        assert_eq!(parse_memory_string("256m"), Some(256 * 1_048_576));
        assert_eq!(parse_memory_string("1024k"), Some(1024 * 1_024));
    }
}
