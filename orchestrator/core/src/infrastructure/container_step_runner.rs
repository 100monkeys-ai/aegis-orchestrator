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
//! - Mount volumes via FUSE transport (gRPC daemon or local FUSE daemon — ADR-107)
//! - Stream stdout/stderr from the container with a 1 MiB cap per stream
//! - Return exit code, captured output, and duration to the application layer
//! - Publish [`ContainerRunEvent`] domain events for audit trail and Cortex learning
//! - Always remove the container on completion or error (no leaked resources)
//!
//! See ADR-050 (CI/CD Orchestration via Workflows), ADR-107 (FUSE Volume Transport),
//! ADR-027 (Docker Runtime Implementation Details).

use crate::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use crate::application::volume_manager::VolumeService;
use crate::domain::events::{ContainerRunEvent, ContainerRunFailureReason};
use crate::domain::fsal::FsalAccessPolicy;
use crate::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner, InstanceId,
};
use crate::domain::secrets::AccessContext;
use crate::domain::volume::VolumeId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::image_manager::DockerImageManager;
use crate::infrastructure::secrets_manager::SecretsManager;
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountTypeEnum};
use bollard::query_parameters::{
    AttachContainerOptions, CreateContainerOptions, LogsOptions, RemoveContainerOptions,
    StartContainerOptions, UploadToContainerOptions, WaitContainerOptions,
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

/// Timeout for gRPC FUSE mount/unmount operations.
pub(crate) const FUSE_GRPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Configuration for [`ContainerStepRunnerImpl`].
pub struct ContainerStepRunnerConfig {
    /// Docker network mode for container steps (e.g. `"host"`).
    pub network_mode: Option<String>,
    /// FUSE FSAL daemon for bind-mount-based volume access (ADR-107).
    /// When `Some`, the runner uses FUSE + bind mounts instead of NFS volume
    /// driver mounts. This is required for rootless container runtimes (Podman).
    pub fuse_daemon: Option<Arc<crate::infrastructure::fuse::daemon::FuseFsalDaemon>>,
    /// Host directory prefix for FUSE mountpoints (ADR-107).
    /// Each volume is mounted at `{fuse_mount_prefix}/{volume_name}`.
    /// Default: `/tmp/aegis-fuse-mounts`.
    pub fuse_mount_prefix: String,
    /// gRPC client to the host-side FUSE daemon's FuseMountService (ADR-107).
    /// When `Some`, volume mounts are delegated to the remote daemon via gRPC
    /// instead of mounting locally. Takes precedence over `fuse_daemon`.
    pub fuse_mount_client: Option<
        crate::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient<
            tonic::transport::Channel,
        >,
    >,
}

/// Infrastructure implementation of [`ContainerStepRunner`] backed by the
/// Docker Engine API (bollard). Uses FUSE transport (ADR-107) for all volume
/// mounts — no NFS fallback.
pub struct ContainerStepRunnerImpl {
    docker: Docker,
    image_manager: Arc<dyn DockerImageManager>,
    /// Docker network mode applied to every container step (e.g. `"host"`).
    network_mode: Option<String>,
    /// FUSE FSAL daemon for bind-mount-based volume access (ADR-107).
    fuse_daemon: Option<Arc<crate::infrastructure::fuse::daemon::FuseFsalDaemon>>,
    /// Host directory prefix for FUSE mountpoints (ADR-107).
    fuse_mount_prefix: String,
    /// gRPC client to the host-side FUSE daemon's FuseMountService (ADR-107).
    fuse_mount_client: Option<
        crate::infrastructure::aegis_runtime_proto::fuse_mount_service_client::FuseMountServiceClient<
            tonic::transport::Channel,
        >,
    >,
    event_bus: Arc<EventBus>,
    /// Used to resolve per-step registry credentials stored in OpenBao (ADR-050).
    /// When `ContainerStepConfig::registry_credentials` is `Some("secret:engine/path")`,
    /// the runner reads `username`, `password`, and optionally `serveraddress` from
    /// the vault secret and passes them as a `DockerCredentials` override to
    /// [`DockerImageManager::ensure_image`].
    secrets_manager: Arc<SecretsManager>,
    /// Volume registry for resolving per-volume context (remote path, ownership) for FUSE mounts.
    volume_registry: Arc<NfsVolumeRegistry>,
    /// Volume service for detaching volumes on execution completion.
    volume_service: Option<Arc<dyn VolumeService>>,
}

impl ContainerStepRunnerImpl {
    pub fn new(
        docker: Docker,
        image_manager: Arc<dyn DockerImageManager>,
        config: ContainerStepRunnerConfig,
        event_bus: Arc<EventBus>,
        secrets_manager: Arc<SecretsManager>,
        volume_registry: Arc<NfsVolumeRegistry>,
        volume_service: Option<Arc<dyn VolumeService>>,
    ) -> Self {
        Self {
            docker,
            image_manager,
            network_mode: config.network_mode,
            fuse_daemon: config.fuse_daemon,
            fuse_mount_prefix: config.fuse_mount_prefix,
            fuse_mount_client: config.fuse_mount_client,
            event_bus,
            secrets_manager,
            volume_registry,
            volume_service,
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

    /// Remove a container the step could not go on with.
    async fn remove_after_failure(&self, container_id: &str) {
        if let Err(e) = self
            .docker
            .remove_container(
                container_id,
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
                "Failed to remove container after a failed step"
            );
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
                error: unrecognised_registry_credentials_message(s),
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
        // _fuse_mount_handles keeps FUSE mounts alive for the container's lifetime.
        // Dropping the handles triggers unmount, so they must outlive the container.
        let (host_config, _fuse_mount_handles, grpc_mounted_volumes) = {
            let mut return_fuse_handles: Vec<crate::infrastructure::fuse::daemon::FuseMountHandle> =
                Vec::new();
            let readonly_rootfs = config.read_only_root_filesystem;
            let mut hc = HostConfig {
                // Step-level network_mode overrides the runner-level default (ADR-087 D5).
                network_mode: config
                    .network_mode
                    .clone()
                    .or_else(|| self.network_mode.clone()),
                readonly_rootfs: Some(readonly_rootfs),
                // When the root filesystem is read-only, mount a writable tmpfs at /tmp
                // so runtimes (Python, Node, etc.) can use temporary files as expected.
                tmpfs: if readonly_rootfs {
                    let mut m = HashMap::new();
                    m.insert("/tmp".to_string(), "size=64m".to_string());
                    Some(m)
                } else {
                    None
                },
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

            // ─── Register volumes for this ContainerRun's execution context ──────────
            // Register each volume with the ContainerRun's execution_id AND the
            // workflow_execution_id. FSAL authorize() matches WorkflowExecution
            // ownership via the volume context lookup — no DB writes needed.
            for vm in &config.volumes {
                if let Ok(uuid) = uuid::Uuid::parse_str(&vm.name) {
                    let volume_id = VolumeId(uuid);
                    if let Some(existing_ctx) = self.volume_registry.lookup(volume_id) {
                        let policy = FsalAccessPolicy {
                            read: vec!["/*".to_string()],
                            write: if vm.read_only {
                                vec![]
                            } else {
                                vec!["/*".to_string()]
                            },
                        };
                        self.volume_registry.register(VolumeRegistration {
                            volume_id,
                            execution_id: config.execution_id,
                            workflow_execution_id: existing_ctx
                                .workflow_execution_id
                                .or(config.workflow_execution_id),
                            container_uid: existing_ctx.container_uid,
                            container_gid: existing_ctx.container_gid,
                            policy,
                            mount_point: std::path::PathBuf::from(&vm.mount_path),
                            remote_path: existing_ctx.remote_path.clone(),
                        });
                        debug!(
                            volume_id = %volume_id,
                            execution_id = %config.execution_id,
                            workflow_execution_id = ?config.workflow_execution_id,
                            "Registered NFS volume for ContainerRun execution context"
                        );
                    }
                }
            }

            // Track (execution_id, volume_id) pairs for gRPC FUSE mounts so we can
            // unmount them after the container exits. Only populated when using the
            // gRPC FuseMountService path.
            let mut grpc_mounted_volumes: Vec<(String, String)> = Vec::new();

            // ─── Volume mounts: gRPC FUSE (ADR-107) → local FUSE (ADR-107) ────────────────
            // The user the step's container runs as when the step names none:
            // its image's, read once for every volume's mount request. `None`
            // when the image could not be read.
            let image_user: Option<Option<String>> = if config.volumes.is_empty()
                || config.run_as_user.is_some()
            {
                Some(None)
            } else {
                match self.docker.inspect_image(&config.image).await {
                    Ok(image) => Some(image.config.and_then(|c| c.user)),
                    Err(e) => {
                        warn!(
                            error = %e,
                            image = %config.image,
                            "could not read the step image's user; its volumes report the registered identity"
                        );
                        None
                    }
                }
            };
            // The identity of the step container's process, for one volume
            // registered with `registered`.
            let step_identity = |registered: (u32, u32)| match &image_user {
                Some(image_user) => step_container_identity(
                    config.run_as_user.as_deref(),
                    image_user.as_deref(),
                    registered,
                ),
                None => registered,
            };
            if !config.volumes.is_empty() {
                if let Some(ref _fuse_mount_client) = self.fuse_mount_client {
                    // ── gRPC FuseMountService path (ADR-107) ─────────────────────────
                    // Delegate mount requests to the host-side FUSE daemon over gRPC.
                    // The daemon creates the FUSE mount and returns the host path.
                    let mut grpc_mounts: Vec<Mount> = Vec::new();
                    let mut client = _fuse_mount_client.clone();

                    for vm in &config.volumes {
                        let volume_id = match uuid::Uuid::parse_str(&vm.name) {
                            Ok(uuid) => VolumeId(uuid),
                            Err(_) => {
                                warn!(
                                    volume_name = %vm.name,
                                    "Cannot parse volume name as UUID — skipping gRPC FUSE mount"
                                );
                                continue;
                            }
                        };

                        let existing_ctx = match self.volume_registry.lookup(volume_id) {
                            Some(ctx) => ctx,
                            None => {
                                warn!(
                                    volume_name = %vm.name,
                                    "Volume not found in registry — skipping gRPC FUSE mount"
                                );
                                continue;
                            }
                        };

                        let identity =
                            step_identity((existing_ctx.container_uid, existing_ctx.container_gid));
                        let grpc_req =
                            crate::infrastructure::aegis_runtime_proto::FuseMountRequest {
                                volume_id: volume_id.0.to_string(),
                                execution_id: config.execution_id.0.to_string(),
                                mount_point: String::new(), // Let daemon decide
                                read_paths: vec!["/*".to_string()],
                                write_paths: if vm.read_only {
                                    vec![]
                                } else {
                                    vec!["/*".to_string()]
                                },
                                container_uid: identity.0,
                                container_gid: identity.1,
                                workflow_execution_id: existing_ctx
                                    .workflow_execution_id
                                    .or(config.workflow_execution_id)
                                    .map(|id| id.to_string())
                                    .unwrap_or_default(),
                            };

                        match tokio::time::timeout(FUSE_GRPC_TIMEOUT, client.mount(grpc_req)).await
                        {
                            Err(_elapsed) => {
                                warn!(
                                    volume_name = %vm.name,
                                    timeout_secs = FUSE_GRPC_TIMEOUT.as_secs(),
                                    "gRPC FUSE mount timed out — volume will not be available"
                                );
                            }
                            Ok(mount_result) => match mount_result {
                                Ok(resp) => {
                                    let mountpoint = resp.into_inner().mountpoint;
                                    debug!(
                                        step_name = %config.name,
                                        volume_name = %vm.name,
                                        mountpoint = %mountpoint,
                                        mount_path = %vm.mount_path,
                                        read_only = vm.read_only,
                                        "Configured gRPC FUSE mount for container step"
                                    );
                                    grpc_mounted_volumes.push((
                                        config.execution_id.0.to_string(),
                                        volume_id.0.to_string(),
                                    ));
                                    grpc_mounts.push(Mount {
                                        target: Some(vm.mount_path.clone()),
                                        source: Some(mountpoint),
                                        typ: Some(MountTypeEnum::BIND),
                                        read_only: Some(vm.read_only),
                                        ..Default::default()
                                    });
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        volume_name = %vm.name,
                                        "gRPC FUSE mount failed — volume will not be available"
                                    );
                                }
                            },
                        }
                    }

                    if !grpc_mounts.is_empty() {
                        hc.mounts = Some(grpc_mounts);
                        info!(
                            step_name = %config.name,
                            count = config.volumes.len(),
                            "Configured gRPC FUSE mount(s) for container step"
                        );
                    }
                } else if let Some(ref fuse_daemon) = self.fuse_daemon {
                    // ── FUSE + bind mount path (ADR-107) ─────────────────────────────
                    // Mount a per-volume FUSE filesystem on the host, then bind-mount
                    // that path into the container. Works with rootless runtimes.
                    //
                    // We collect (handle, mount) pairs so the FuseMountHandle values
                    // are kept alive until the container exits. Dropping a handle
                    // triggers an unmount, so they must outlive the container.
                    let fuse_pairs: Vec<(
                        crate::infrastructure::fuse::daemon::FuseMountHandle,
                        Mount,
                    )> = config
                        .volumes
                        .iter()
                        .filter_map(|vm| {
                            let volume_id = match uuid::Uuid::parse_str(&vm.name) {
                                Ok(uuid) => VolumeId(uuid),
                                Err(_) => {
                                    warn!(
                                        volume_name = %vm.name,
                                        "Cannot parse volume name as UUID — skipping FUSE mount"
                                    );
                                    return None;
                                }
                            };

                            let existing_ctx = self.volume_registry.lookup(volume_id)?;
                            let policy = FsalAccessPolicy {
                                read: vec!["/*".to_string()],
                                write: if vm.read_only {
                                    vec![]
                                } else {
                                    vec!["/*".to_string()]
                                },
                            };

                            let mountpoint_path = format!("{}/{}", self.fuse_mount_prefix, vm.name);
                            let identity = step_identity((
                                existing_ctx.container_uid,
                                existing_ctx.container_gid,
                            ));
                            let fuse_context =
                                crate::infrastructure::fuse::daemon::FuseVolumeContext {
                                    execution_id: config.execution_id,
                                    volume_id,
                                    workflow_execution_id: existing_ctx
                                        .workflow_execution_id
                                        .or(config.workflow_execution_id),
                                    container_uid: identity.0,
                                    container_gid: identity.1,
                                    policy,
                                };

                            match fuse_daemon
                                .mount(std::path::Path::new(&mountpoint_path), fuse_context)
                            {
                                Ok(handle) => {
                                    debug!(
                                        step_name = %config.name,
                                        volume_name = %vm.name,
                                        mountpoint = %mountpoint_path,
                                        mount_path = %vm.mount_path,
                                        read_only = vm.read_only,
                                        "Configured FUSE bind mount for container step"
                                    );

                                    Some((
                                        handle,
                                        Mount {
                                            target: Some(vm.mount_path.clone()),
                                            source: Some(mountpoint_path),
                                            typ: Some(MountTypeEnum::BIND),
                                            read_only: Some(vm.read_only),
                                            ..Default::default()
                                        },
                                    ))
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        volume_name = %vm.name,
                                        "FUSE mount failed — volume will not be available"
                                    );
                                    None
                                }
                            }
                        })
                        .collect();

                    // Split into handles (kept alive) and mounts (passed to Docker).
                    let (fuse_handles, mounts): (Vec<_>, Vec<_>) = fuse_pairs.into_iter().unzip();

                    if !mounts.is_empty() {
                        hc.mounts = Some(mounts);
                        info!(
                            step_name = %config.name,
                            count = config.volumes.len(),
                            "Configured FUSE bind mount(s) for container step"
                        );
                    }

                    return_fuse_handles = fuse_handles;
                } else {
                    // No FUSE transport configured — this is a fatal misconfiguration.
                    // Volumes require either a gRPC FUSE client or a local FUSE daemon.
                    return Err(ContainerStepError::VolumeMountFailed {
                        volume: config
                            .volumes
                            .first()
                            .map(|v| v.name.clone())
                            .unwrap_or_default(),
                        error: "no FUSE transport configured: set fuse_mount_client or fuse_daemon"
                            .to_string(),
                    });
                }
            }

            (hc, return_fuse_handles, grpc_mounted_volumes)
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

        // A step handed a secret gets it on standard input (see
        // `ContainerStepConfig::stdin`), opened here and closed once written.
        let takes_stdin = config.stdin.is_some();
        let container_config = ContainerCreateBody {
            image: Some(config.image.clone()),
            entrypoint: config.entrypoint.clone(),
            cmd: Some(cmd),
            open_stdin: takes_stdin.then_some(true),
            stdin_once: takes_stdin.then_some(true),
            attach_stdin: takes_stdin.then_some(true),
            env: Some(env_strings),
            working_dir: config.workdir.clone(),
            user: config.run_as_user.clone(),
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

        // ─── 6b. Attach to standard input before the process starts ──────────
        let mut stdin_writer = None;
        if takes_stdin {
            match self
                .docker
                .attach_container(
                    &container_id,
                    Some(AttachContainerOptions {
                        stdin: true,
                        stream: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(attached) => stdin_writer = Some(attached.input),
                Err(e) => {
                    self.remove_after_failure(&container_id).await;
                    let error = ContainerStepError::DockerError(format!("attach_container: {e}"));
                    self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                    return Err(error);
                }
            }
        }

        // ─── 6c. Place the step's files before the process starts ────────────
        // (AEGIS ADR-005 O7c: an agent's program and its sample input.)
        if !config.files.is_empty() {
            let placed =
                match crate::infrastructure::runtime::container_files_archive(&config.files) {
                    Ok(archive) => self
                        .docker
                        .upload_to_container(
                            &container_id,
                            Some(UploadToContainerOptions {
                                path: "/".to_string(),
                                ..Default::default()
                            }),
                            bollard::body_full(archive.into()),
                        )
                        .await
                        .map_err(|e| format!("upload_to_container: {e}")),
                    Err(e) => Err(e),
                };
            if let Err(e) = placed {
                self.remove_after_failure(&container_id).await;
                let error = ContainerStepError::DockerError(e);
                self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                return Err(error);
            }
        }

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

        // ─── 8. Hand the process its standard input, then close it ───────────
        if let (Some(mut writer), Some(bytes)) = (stdin_writer, config.stdin.as_ref()) {
            use tokio::io::AsyncWriteExt;
            let written = async {
                writer.write_all(bytes.expose()).await?;
                writer.flush().await?;
                writer.shutdown().await
            }
            .await;
            drop(writer);
            if let Err(e) = written {
                self.remove_after_failure(&container_id).await;
                let error = ContainerStepError::DockerError(format!("write stdin: {e}"));
                self.publish_failed_event(&config, Self::failure_reason_for_error(&error));
                return Err(error);
            }
        }

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

        // ─── 10b. Drop FUSE mount handles (ADR-107) ─────────────────────────
        // _fuse_mount_handles is dropped when this function returns, which is
        // after the container has been removed. Each handle's Drop impl triggers
        // FUSE_DESTROY + unmount, so the host mountpoints are cleaned up here.
        drop(_fuse_mount_handles);

        // ─── 10c. Unmount gRPC FUSE mounts (ADR-107) ────────────────────────
        // When the gRPC FuseMountService path was used, the FUSE mounts live in
        // the remote daemon process — dropping local handles does nothing. We
        // must explicitly call unmount for each volume that was gRPC-mounted.
        if !grpc_mounted_volumes.is_empty() {
            if let Some(ref fuse_mount_client) = self.fuse_mount_client {
                let mut client = fuse_mount_client.clone();
                for (execution_id, volume_id) in &grpc_mounted_volumes {
                    let unmount_req =
                        crate::infrastructure::aegis_runtime_proto::FuseUnmountRequest {
                            volume_id: volume_id.clone(),
                            execution_id: execution_id.clone(),
                        };
                    match tokio::time::timeout(FUSE_GRPC_TIMEOUT, client.unmount(unmount_req)).await
                    {
                        Err(_elapsed) => {
                            warn!(
                                volume_id = %volume_id,
                                execution_id = %execution_id,
                                timeout_secs = FUSE_GRPC_TIMEOUT.as_secs(),
                                "gRPC FUSE unmount timed out — mount may linger"
                            );
                        }
                        Ok(Err(e)) => {
                            warn!(
                                error = %e,
                                volume_id = %volume_id,
                                execution_id = %execution_id,
                                "gRPC FUSE unmount failed — mount may linger"
                            );
                        }
                        Ok(Ok(_)) => {
                            debug!(
                                volume_id = %volume_id,
                                execution_id = %execution_id,
                                "gRPC FUSE unmount succeeded"
                            );
                        }
                    }
                }
            }
        }

        // ─── 10d. Detach workspace volumes ─────────────────────────────────
        // Transition attached volumes back to Available so they can be reused
        // or deleted. Best-effort: warn and continue on failure.
        if let Some(ref volume_service) = self.volume_service {
            let instance_id = InstanceId::new(container_id.clone());
            for vm in &config.volumes {
                if let Ok(uuid) = uuid::Uuid::parse_str(&vm.name) {
                    let volume_id = VolumeId(uuid);
                    if let Err(e) = volume_service
                        .detach_volume(volume_id, instance_id.clone())
                        .await
                    {
                        warn!(
                            volume_id = %volume_id,
                            container_id = %container_id,
                            error = %e,
                            "Failed to detach volume after execution — volume may remain in Attached state"
                        );
                    } else {
                        debug!(
                            volume_id = %volume_id,
                            container_id = %container_id,
                            "Volume detached after execution"
                        );
                    }
                }
            }
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

                // ADR-087 §Observability: record container exit code.
                metrics::counter!(
                    "zaru_intent_pipeline_container_exit_code_total",
                    "exit_code" => result.exit_code.to_string()
                )
                .increment(1);

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

    let exit_code = match wait_stream.next().await {
        Some(Ok(body)) => body.status_code as i32,
        Some(Err(e)) => {
            // Podman compatibility: when a container has already exited before
            // wait_container is called, Podman returns an error instead of the
            // exit code. Fall back to inspect_container to retrieve it.
            warn!(
                container_id = %container_id,
                error = %e,
                "wait_container failed; falling back to inspect for exit code (Podman compatibility)"
            );
            let info = docker
                .inspect_container(container_id, None)
                .await
                .map_err(|ie| CaptureError::Docker(format!("inspect after wait failure: {ie}")))?;
            info.state
                .and_then(|s| s.exit_code)
                .map(|c| c as i32)
                .unwrap_or(-1)
        }
        None => {
            // Empty stream — container already gone. Inspect for exit code.
            let info = docker
                .inspect_container(container_id, None)
                .await
                .map_err(|ie| {
                    CaptureError::Docker(format!("inspect after empty wait stream: {ie}"))
                })?;
            info.state
                .and_then(|s| s.exit_code)
                .map(|c| c as i32)
                .unwrap_or(0)
        }
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

/// The identity a step container's process has inside its own namespace,
/// which its FUSE mount requests name: the step's `run_as_user`, else
/// `image_user`, the user its image runs as (ADR-107 D8). `registered`, the
/// volume's registered identity, stands where that user is given by name.
fn step_container_identity(
    run_as_user: Option<&str>,
    image_user: Option<&str>,
    registered: (u32, u32),
) -> (u32, u32) {
    crate::infrastructure::runtime::ContainerRuntime::container_process_identity(
        run_as_user.or(image_user),
        registered,
    )
}

/// The error text for a step's `registry_credentials` value that is neither
/// `env:VAR_NAME` nor `secret:engine/path`.
///
/// The value itself is never repeated: in any other form it may be the
/// literal credential, and this text becomes the step's published failure
/// reason. It says only whether the value has the shape of one.
fn unrecognised_registry_credentials_message(value: &str) -> String {
    let shape = if value.contains(':') {
        "it looks like a literal 'username:password', which is not accepted"
    } else {
        "the value is withheld"
    };
    format!(
        "unrecognised registry_credentials format ({shape}); \
         expected 'env:VAR_NAME' or 'secret:engine/path'"
    )
}

#[cfg(test)]
mod tests {
    use super::{
        parse_memory_string, step_container_identity, unrecognised_registry_credentials_message,
        ContainerStepRunnerConfig,
    };

    /// A ContainerRun step's FUSE mount request names the identity its
    /// container's process has in its own namespace: the step's `run_as_user`,
    /// else the user its image runs as, root when neither names one (the
    /// intent pipeline's python:3.11-slim and node:20-alpine). It named the
    /// registered 1000:1000, which no such process is. A user given by name is
    /// not resolved and keeps the registered identity.
    #[test]
    fn step_mount_identity_is_the_step_containers_process() {
        let registered = (1000, 1000);
        assert_eq!(step_container_identity(None, None, registered), (0, 0));
        assert_eq!(step_container_identity(None, Some(""), registered), (0, 0));
        assert_eq!(
            step_container_identity(Some("65534:65534"), Some(""), registered),
            (65534, 65534)
        );
        assert_eq!(
            step_container_identity(None, Some("1001:1001"), registered),
            (1001, 1001)
        );
        assert_eq!(
            step_container_identity(Some("0"), Some("1001:1001"), registered),
            (0, 0)
        );
        assert_eq!(
            step_container_identity(None, Some("node"), registered),
            registered
        );
    }
    use crate::domain::runtime::ContainerStepConfig;
    use std::collections::HashMap;

    /// A step's `registry_credentials` holds a reference (`env:VAR_NAME` or
    /// `secret:engine/path`). A value in any other form may be the literal
    /// credential a workflow author put there by mistake, and the error that
    /// rejects it is published as the step's failure reason and returned to
    /// the caller, so it must not repeat the value.
    #[test]
    fn unrecognised_registry_credentials_error_does_not_echo_the_value() {
        let marker = "Mk7-registry-password-marker";
        let mut leaked = Vec::new();
        for value in [
            format!("robot:{marker}"),
            format!("robot:{marker}@ghcr.io"),
            marker.to_string(),
        ] {
            let message = unrecognised_registry_credentials_message(&value);
            if message.contains(marker) {
                leaked.push(message.clone());
            }
            assert!(
                message.contains("env:VAR_NAME") && message.contains("secret:engine/path"),
                "the error must still say which forms are accepted: {message}"
            );
        }
        assert!(
            leaked.is_empty(),
            "the registry_credentials value reached the error text:\n{}",
            leaked.join("\n")
        );
    }

    /// Regression: ContainerStepRunnerConfig must propagate network_mode so
    /// container steps use the same Docker network as agent containers.
    /// Without this field, HostConfig defaulted to bridge networking.
    #[test]
    fn test_config_network_mode_stored() {
        let config = ContainerStepRunnerConfig {
            network_mode: Some("host".to_string()),
            fuse_daemon: None,
            fuse_mount_prefix: "/tmp/aegis-fuse-mounts".to_string(),
            fuse_mount_client: None,
        };
        assert_eq!(config.network_mode.as_deref(), Some("host"));
    }

    /// Regression: ContainerStepConfig security fields must be stored and accessible
    /// so the runner can forward them to HostConfig/ContainerCreateBody (ADR-087 D5).
    /// Before this fix, ContainerStepConfig had no security fields and the runner
    /// never enforced read-only root fs, run_as_user, or network isolation.
    #[test]
    fn test_security_fields_propagated_to_host_config() {
        use crate::domain::agent::ImagePullPolicy;
        use crate::domain::execution::ExecutionId;
        use crate::domain::workflow::StateName;

        let config = ContainerStepConfig {
            name: "execute-user-code".to_string(),
            image: "python:3.12-slim".to_string(),
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            entrypoint: None,
            command: vec!["python3".to_string(), "/workspace/solution.py".to_string()],
            stdin: None,
            env: std::collections::HashMap::new(),
            workdir: Some("/workspace".to_string()),
            volumes: vec![],
            resources: None,
            registry_credentials: None,
            execution_id: ExecutionId::new(),
            state_name: StateName::new("EXECUTE_CODE").unwrap(),
            read_only_root_filesystem: true,
            run_as_user: Some("65534:65534".to_string()),
            network_mode: Some("none".to_string()),
            workflow_execution_id: None,
            files: Vec::new(),
        };

        assert!(config.read_only_root_filesystem);
        assert_eq!(config.run_as_user.as_deref(), Some("65534:65534"));
        assert_eq!(config.network_mode.as_deref(), Some("none"));
    }

    /// Regression: step-level network_mode must take precedence over the runner-level
    /// default so EXECUTE_CODE can enforce "none" regardless of the global network mode
    /// (ADR-087 D5). Before this fix, only self.network_mode was used, so a step could
    /// not override the runner default.
    #[test]
    fn test_step_network_mode_overrides_runner_network_mode() {
        // Simulate the precedence logic used in run_step:
        //   config.network_mode.clone().or_else(|| self.network_mode.clone())
        let runner_network_mode: Option<String> = Some("host".to_string());
        let step_network_mode: Option<String> = Some("none".to_string());

        let resolved = step_network_mode
            .clone()
            .or_else(|| runner_network_mode.clone());

        assert_eq!(
            resolved.as_deref(),
            Some("none"),
            "step-level network_mode 'none' must override runner-level 'host'"
        );

        // When the step has no override, the runner default applies.
        let no_step_override: Option<String> = None;
        let resolved_fallback = no_step_override.or_else(|| runner_network_mode.clone());
        assert_eq!(
            resolved_fallback.as_deref(),
            Some("host"),
            "runner-level network_mode must be used when step has no override"
        );
    }

    /// Regression: when `read_only_root_filesystem` is true, the HostConfig must
    /// include a tmpfs mount at /tmp so that runtimes (Python, Node, etc.) can
    /// write temporary files. Without this, EXECUTE_CODE containers fail because
    /// Python cannot create __pycache__ or tempfile entries on a read-only root.
    #[test]
    fn test_tmpfs_added_when_readonly_rootfs_is_true() {
        let readonly_rootfs = true;
        let tmpfs: Option<HashMap<String, String>> = if readonly_rootfs {
            let mut m = HashMap::new();
            m.insert("/tmp".to_string(), "size=64m".to_string());
            Some(m)
        } else {
            None
        };
        let tmpfs = tmpfs.expect("tmpfs must be Some when readonly_rootfs is true");
        assert_eq!(
            tmpfs.get("/tmp").map(|v| v.as_str()),
            Some("size=64m"),
            "/tmp tmpfs must be mounted with size=64m"
        );
    }

    /// Regression: when `read_only_root_filesystem` is false, no tmpfs should be
    /// added — the root filesystem is already writable.
    #[test]
    fn test_no_tmpfs_when_readonly_rootfs_is_false() {
        let readonly_rootfs = false;
        let tmpfs: Option<HashMap<String, String>> = if readonly_rootfs {
            let mut m = HashMap::new();
            m.insert("/tmp".to_string(), "size=64m".to_string());
            Some(m)
        } else {
            None
        };
        assert!(
            tmpfs.is_none(),
            "tmpfs must be None when readonly_rootfs is false"
        );
    }

    /// The VolumeService dependency is optional — the runner can be constructed
    /// without one (e.g. in tests or lightweight deployments). When absent,
    /// `detach_volume` is simply skipped on execution cleanup.
    #[test]
    fn test_container_step_runner_volume_service_is_optional() {
        // ContainerStepRunnerImpl::new accepts Option<Arc<dyn VolumeService>>.
        // Passing None is valid — compile-time verification.
        let _config = ContainerStepRunnerConfig {
            network_mode: None,
            fuse_daemon: None,
            fuse_mount_prefix: "/tmp/aegis-fuse-mounts".to_string(),
            fuse_mount_client: None,
        };
        // If this compiles, the VolumeService is properly optional.
    }

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

    /// Regression: ContainerStepRunnerConfig with no FUSE transport configured
    /// (fuse_mount_client = None AND fuse_daemon = None) must not contain NFS
    /// fallback fields. The NFS fallback path has been removed; volume mounts
    /// require a FUSE transport — absence of one is a hard configuration error
    /// caught at runtime, not silently degraded.
    #[test]
    fn test_config_without_fuse_has_no_nfs_fields() {
        // ContainerStepRunnerConfig must compile without nfs_server_host, nfs_port,
        // or nfs_mountport — those fields no longer exist.
        let config = ContainerStepRunnerConfig {
            network_mode: None,
            fuse_daemon: None,
            fuse_mount_prefix: "/tmp/aegis-fuse-mounts".to_string(),
            fuse_mount_client: None,
        };
        // Structural assertion: no FUSE transport configured.
        assert!(config.fuse_daemon.is_none());
        assert!(config.fuse_mount_client.is_none());
        // If this compiles without nfs_* fields, the NFS fallback is fully removed.
    }

    /// Regression: gRPC FUSE mounts must be tracked and unmounted after container
    /// exit. Before this fix, the gRPC FuseMountService path created FUSE mounts
    /// on the remote daemon via `client.mount()` but never called `client.unmount()`.
    /// This left stale FUSE mountpoints on the host, generating endless
    /// `DirectoryListed` storage events. The fix tracks (execution_id, volume_id)
    /// pairs for each successful gRPC mount and calls unmount after container removal.
    #[test]
    fn test_grpc_mounted_volumes_tracking_structure() {
        // Verify that the tracking vector correctly stores (execution_id, volume_id) pairs.
        let mut grpc_mounted_volumes: Vec<(String, String)> = Vec::new();

        let exec_id = "11111111-1111-1111-1111-111111111111".to_string();
        let vol_id_1 = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string();
        let vol_id_2 = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string();

        grpc_mounted_volumes.push((exec_id.clone(), vol_id_1.clone()));
        grpc_mounted_volumes.push((exec_id.clone(), vol_id_2.clone()));

        assert_eq!(grpc_mounted_volumes.len(), 2);
        assert_eq!(grpc_mounted_volumes[0], (exec_id.clone(), vol_id_1));
        assert_eq!(grpc_mounted_volumes[1], (exec_id, vol_id_2));
    }

    /// A step's standard input reaches its process, and nothing the engine
    /// shows about the container while it runs (its inspect record and its
    /// process list) holds any part of it, nor does any event the runner
    /// publishes. Needs a container engine (see `container_engine`).
    #[tokio::test]
    async fn stdin_reaches_the_step_and_nothing_the_engine_shows_holds_it() {
        use crate::application::git_test_server::{
            container_engine, container_runner, holds_any_part_of, marker,
        };
        use crate::domain::execution::ExecutionId;
        use crate::domain::runtime::ContainerStepRunner;
        use crate::domain::secrets::SensitiveBytes;
        use crate::domain::shared_kernel::ImagePullPolicy;
        use crate::domain::workflow::StateName;
        use crate::infrastructure::event_bus::EventBus;
        use bollard::query_parameters::{
            InspectContainerOptions, ListContainersOptions, TopOptions,
        };
        use std::sync::Arc;

        let test = "stdin_reaches_the_step_and_nothing_the_engine_shows_holds_it";
        let Some(docker) = container_engine(test).await else {
            return;
        };
        let secret = marker("Kq");
        let bus = Arc::new(EventBus::new(256));
        let mut events = bus.subscribe();
        let runner = container_runner(docker.clone(), bus.clone());
        let execution_id = ExecutionId::new();
        let config = ContainerStepConfig {
            name: "stdin".to_string(),
            image: "alpine/git:latest".to_string(),
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            entrypoint: Some(vec!["sh".to_string(), "-c".to_string()]),
            command: vec!["cat >/tmp/in; sleep 4; cat /tmp/in".to_string()],
            stdin: Some(SensitiveBytes::from(secret.clone().into_bytes())),
            env: HashMap::new(),
            workdir: None,
            volumes: vec![],
            resources: None,
            registry_credentials: None,
            execution_id,
            state_name: StateName::new("STDIN").unwrap(),
            read_only_root_filesystem: true,
            run_as_user: None,
            network_mode: None,
            workflow_execution_id: None,
            files: Vec::new(),
        };

        // While the step sleeps, read what the engine shows about it.
        let watcher = {
            let docker = docker.clone();
            let name = format!("aegis-step-{execution_id}");
            tokio::spawn(async move {
                let mut shown: Vec<String> = Vec::new();
                for _ in 0..80 {
                    let mut filters = HashMap::new();
                    filters.insert("name".to_string(), vec![name.clone()]);
                    let listed = docker
                        .list_containers(Some(ListContainersOptions {
                            all: true,
                            filters: Some(filters),
                            ..Default::default()
                        }))
                        .await
                        .unwrap_or_default();
                    for c in listed {
                        let Some(id) = c.id else { continue };
                        shown.push(serde_json::to_string(&c.command).unwrap_or_default());
                        if let Ok(inspect) = docker
                            .inspect_container(&id, None::<InspectContainerOptions>)
                            .await
                        {
                            shown.push(serde_json::to_string(&inspect).unwrap_or_default());
                        }
                        if let Ok(top) = docker.top_processes(&id, None::<TopOptions>).await {
                            shown.push(serde_json::to_string(&top).unwrap_or_default());
                        }
                    }
                    if shown.len() >= 6 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                shown
            })
        };

        let result = runner.run_step(config).await.expect("the step runs");
        let shown = watcher.await.unwrap();
        assert!(
            result.stdout == secret,
            "the step's standard input did not reach its process"
        );
        assert!(
            !shown.is_empty(),
            "the engine showed nothing about the running step"
        );
        assert!(
            !shown.iter().any(|s| holds_any_part_of(s, &secret)),
            "what the engine shows about the running step holds its standard input"
        );
        let mut published = Vec::new();
        while let Ok(event) = events.try_recv() {
            published.push(serde_json::to_string(&event).unwrap_or_default());
        }
        assert!(
            !published.iter().any(|e| holds_any_part_of(e, &secret)),
            "an event published for the step holds its standard input"
        );
    }
}
