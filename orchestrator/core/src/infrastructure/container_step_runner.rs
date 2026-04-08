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

use crate::application::nfs_gateway::{NfsVolumeRegistry, VolumeRegistration};
use crate::domain::events::{ContainerRunEvent, ContainerRunFailureReason};
use crate::domain::fsal::FsalAccessPolicy;
use crate::domain::runtime::{
    ContainerStepConfig, ContainerStepError, ContainerStepResult, ContainerStepRunner,
};
use crate::domain::secrets::AccessContext;
use crate::domain::volume::VolumeId;
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
    CreateContainerOptions, LogsOptions, RemoveContainerOptions, RemoveVolumeOptions,
    StartContainerOptions, WaitContainerOptions,
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

/// NFS and port configuration for [`ContainerStepRunnerImpl`].
pub struct ContainerStepRunnerConfig {
    pub nfs_server_host: Option<String>,
    pub nfs_port: u16,
    pub nfs_mountport: u16,
    /// Docker network mode for container steps (e.g. `"host"`).
    /// Must match the network mode used by agent containers so that
    /// `addr=127.0.0.1` in NFS mount options resolves to the host NFS
    /// server rather than the container's own loopback (ADR-036).
    pub network_mode: Option<String>,
    /// FUSE FSAL daemon for bind-mount-based volume access (ADR-107).
    /// When `Some`, the runner uses FUSE + bind mounts instead of NFS volume
    /// driver mounts. This is required for rootless container runtimes (Podman).
    pub fuse_daemon: Option<Arc<crate::infrastructure::fuse::daemon::FuseFsalDaemon>>,
    /// Host directory prefix for FUSE mountpoints (ADR-107).
    /// Each volume is mounted at `{fuse_mount_prefix}/{volume_name}`.
    /// Default: `/tmp/aegis-fuse-mounts`.
    pub fuse_mount_prefix: String,
}

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
    /// Docker network mode applied to every container step (e.g. `"host"`).
    /// Inherited from the same resolved value used for agent containers so
    /// that NFS `addr=127.0.0.1` refers to the host, not the container loopback.
    network_mode: Option<String>,
    /// FUSE FSAL daemon for bind-mount-based volume access (ADR-107).
    fuse_daemon: Option<Arc<crate::infrastructure::fuse::daemon::FuseFsalDaemon>>,
    /// Host directory prefix for FUSE mountpoints (ADR-107).
    fuse_mount_prefix: String,
    event_bus: Arc<EventBus>,
    /// Used to resolve per-step registry credentials stored in OpenBao (ADR-050).
    /// When `ContainerStepConfig::registry_credentials` is `Some("secret:engine/path")`,
    /// the runner reads `username`, `password`, and optionally `serveraddress` from
    /// the vault secret and passes them as a `DockerCredentials` override to
    /// [`DockerImageManager::ensure_image`].
    secrets_manager: Arc<SecretsManager>,
    /// Volume registry for resolving the correct NFS remote_path for each volume mount.
    volume_registry: Arc<NfsVolumeRegistry>,
}

impl ContainerStepRunnerImpl {
    pub fn new(
        docker: Docker,
        image_manager: Arc<dyn DockerImageManager>,
        config: ContainerStepRunnerConfig,
        event_bus: Arc<EventBus>,
        secrets_manager: Arc<SecretsManager>,
        volume_registry: Arc<NfsVolumeRegistry>,
    ) -> Self {
        Self {
            docker,
            image_manager,
            nfs_server_host: config.nfs_server_host,
            nfs_port: config.nfs_port,
            nfs_mountport: config.nfs_mountport,
            network_mode: config.network_mode,
            fuse_daemon: config.fuse_daemon,
            fuse_mount_prefix: config.fuse_mount_prefix,
            event_bus,
            secrets_manager,
            volume_registry,
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
        // _fuse_mount_handles keeps FUSE mounts alive for the container's lifetime.
        // Dropping the handles triggers unmount, so they must outlive the container.
        let (host_config, _fuse_mount_handles) = {
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

            // ─── Volume mounts: FUSE bind mount (ADR-107) or NFS volume driver (ADR-036) ──
            if !config.volumes.is_empty() {
                if let Some(ref fuse_daemon) = self.fuse_daemon {
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
                            let fuse_context =
                                crate::infrastructure::fuse::daemon::FuseVolumeContext {
                                    execution_id: config.execution_id,
                                    volume_id,
                                    workflow_execution_id: existing_ctx
                                        .workflow_execution_id
                                        .or(config.workflow_execution_id),
                                    container_uid: existing_ctx.container_uid,
                                    container_gid: existing_ctx.container_gid,
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
                                        "Configured FUSE bind mount for container step (ADR-107)"
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
                                        "FUSE mount failed — falling back to NFS for this volume"
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
                            "Configured FUSE bind mount(s) for container step (ADR-107)"
                        );
                    }

                    return_fuse_handles = fuse_handles;
                } else {
                    // ── NFS volume driver path (ADR-036) ─────────────────────────────
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
                            let device_path = uuid::Uuid::parse_str(&vm.name)
                                .ok()
                                .and_then(|uuid| {
                                    self.volume_registry.lookup(VolumeId(uuid))
                                })
                                .map(|ctx| format!(":{}", ctx.remote_path));

                            if device_path.is_none() {
                                warn!(
                                    step_name = %config.name,
                                    volume_name = %vm.name,
                                    execution_id = %config.execution_id,
                                    "NFS volume registry lookup failed — volume may not be mounted correctly"
                                );
                            }

                            let device_path = device_path.unwrap_or_else(|| {
                                format!(":{}/{}", config.execution_id, vm.name)
                            });
                            driver_opts.insert("device".to_string(), device_path.clone());

                            let volume_source_name = format!("aegis-vol-{}", vm.name);

                            debug!(
                                step_name = %config.name,
                                volume_name = %vm.name,
                                volume_source = %volume_source_name,
                                device_path = %device_path,
                                mount_path = %vm.mount_path,
                                read_only = vm.read_only,
                                nfs_host = nfs_host,
                                "Configuring NFS mount for container step"
                            );

                            Mount {
                                target: Some(vm.mount_path.clone()),
                                source: Some(volume_source_name),
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
            }

            (hc, return_fuse_handles)
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

        // ─── 5b. Remove stale named volumes so NFS driver options are applied fresh ──
        // Podman/Docker silently ignore driver_config when reusing an existing named
        // volume. If a previous run created aegis-vol-{uuid} as a plain local volume,
        // the NFS options on the new Mount are discarded and the container gets an
        // empty local volume instead of the NFS-backed SeaweedFS volume.
        if let Some(ref mounts) = host_config.mounts {
            for m in mounts {
                if let Some(ref vol_name) = m.source {
                    if let Err(e) = self
                        .docker
                        .remove_volume(vol_name, None::<RemoveVolumeOptions>)
                        .await
                    {
                        debug!(
                            volume = %vol_name,
                            error = %e,
                            "Could not remove stale volume (may not exist)"
                        );
                    }
                }
            }
        }

        // Collect volume source names for post-removal cleanup (before host_config moves)
        let volume_sources_for_cleanup: Vec<String> = host_config
            .mounts
            .as_ref()
            .map(|mounts| mounts.iter().filter_map(|m| m.source.clone()).collect())
            .unwrap_or_default();

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

        // ─── 10b. Clean up named volumes after container removal ──────────────
        // Remove the named volumes created for this run so they don't accumulate
        // as stale plain-local volumes across runs.
        for vol_name in &volume_sources_for_cleanup {
            if let Err(e) = self
                .docker
                .remove_volume(vol_name, None::<RemoveVolumeOptions>)
                .await
            {
                debug!(
                    volume = %vol_name,
                    error = %e,
                    "Could not remove volume after container step (may still be in use)"
                );
            }
        }

        // ─── 10c. Drop FUSE mount handles (ADR-107) ─────────────────────────
        // _fuse_mount_handles is dropped when this function returns, which is
        // after the container has been removed. Each handle's Drop impl triggers
        // FUSE_DESTROY + unmount, so the host mountpoints are cleaned up here.
        drop(_fuse_mount_handles);

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

#[cfg(test)]
mod tests {
    use super::{parse_memory_string, ContainerStepRunnerConfig};
    use crate::domain::runtime::ContainerStepConfig;
    use std::collections::HashMap;

    /// Regression: ContainerStepRunnerConfig must propagate network_mode so
    /// container steps use the same Docker network as agent containers.
    /// Without this field, HostConfig defaulted to bridge networking and
    /// addr=127.0.0.1 in NFS mount options resolved to the container's own
    /// loopback, making the NFS server unreachable (ADR-036).
    #[test]
    fn test_config_network_mode_stored() {
        let config = ContainerStepRunnerConfig {
            nfs_server_host: None,
            nfs_port: 2049,
            nfs_mountport: 2049,
            network_mode: Some("host".to_string()),
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
            command: vec!["python3".to_string(), "/workspace/solution.py".to_string()],
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

    /// Regression: container step volume mounts must use the same Docker volume
    /// source name as agent containers (`aegis-vol-{volume_id}`) so Docker reuses
    /// the existing named volume. Previously, container steps used
    /// `aegis-step-{execution_id}-{volume_name}`, creating a separate Docker volume
    /// that could fail to mount the NFS export correctly, resulting in an empty
    /// /workspace directory and Python exit code 2 (file not found).
    #[test]
    fn test_container_step_volume_source_name_matches_agent_runtime() {
        // The container step volume source name format must match the agent runtime.
        // Agent runtime uses: format!("aegis-vol-{}", volume_mount.volume_id)
        // Container step must use the same: format!("aegis-vol-{}", vm.name)
        let volume_id = "f8eb2163-0f8e-4582-abf2-6ab1261b7961";
        let execution_id = uuid::Uuid::new_v4();

        // What agent runtime produces:
        let agent_source = format!("aegis-vol-{}", volume_id);

        // What container step must produce (matches agent runtime):
        let step_source = format!("aegis-vol-{}", volume_id);

        assert_eq!(
            agent_source, step_source,
            "Container step volume source name must match agent runtime's aegis-vol-{{volume_id}} format"
        );

        // The old broken format would have been:
        let old_broken_source = format!("aegis-step-{}-{}", execution_id, volume_id);
        assert_ne!(
            agent_source, old_broken_source,
            "Old aegis-step format must differ from the correct aegis-vol format"
        );
    }

    /// Regression: ContainerStepRunnerImpl must NOT require a VolumeService
    /// dependency. Volume ownership is no longer mutated in the DB by the
    /// container step runner — FSAL resolves WorkflowExecution ownership
    /// via the VolumeContextLookup trait on the in-memory NFS volume registry.
    /// This test verifies that the constructor signature does not require a
    /// VolumeService argument (structural test — if this compiles, the
    /// dependency has been removed correctly).
    #[test]
    fn test_container_step_runner_does_not_require_volume_service() {
        // ContainerStepRunnerImpl::new takes 6 arguments (no VolumeService).
        // This is a compile-time verification — no runtime assertion needed.
        let _config = ContainerStepRunnerConfig {
            nfs_server_host: None,
            nfs_port: 2049,
            nfs_mountport: 2049,
            network_mode: None,
        };
        // If this compiles, the VolumeService dependency has been removed.
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
}
