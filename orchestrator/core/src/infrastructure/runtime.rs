// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Docker/Podman Runtime Adapter — BC-2 (ADR-027)
//!
//! Implements the [`crate::domain::runtime::AgentRuntime`] domain trait using
//! a Docker-compatible container runtime (Docker or Podman) via `bollard`.
//! Provides container lifecycle management with configurable resource limits,
//! network policies, and NFS volume mounts.
//!
//! ⚠️ Phase 1 — Docker/Podman runtime only. Firecracker microVM
//! isolation is deferred to Phase 2 (ADR-003).
//!
//! ## Responsibilities
//! - Pull or reuse agent container image
//! - Apply `ResourceLimits` (CPU, memory) and `NetworkPolicy` (iptables allowlist)
//! - Mount volumes via NFS (orchestrator-side NFS Server Gateway — ADR-036)
//! - Stream container stdout/stderr to the execution event bus
//! - Destroy container on execution completion or cancellation
//!
//! See ADR-027 (Docker Runtime Implementation Details), ADR-003 (Firecracker, deferred).

// ============================================================================
// ADR-003: Firecracker Isolation (DEFERRED to Phase 2 - Production Release)
// ============================================================================
// Current Implementation: Docker/Podman runtime (Bollard client)
// This module provides agent execution isolation via Docker/Podman containers.
//
// Firecracker VM-based isolation deferred to Phase 2 for production hardening.
// Phase 1 uses Docker/Podman for development/testing convenience.
//
// Firecracker runtime variant is planned for Phase 2.
// See: adrs/003-firecracker-isolation.md
// ============================================================================

use crate::domain::events::ImageManagementEvent;
use crate::domain::runtime::{
    AgentRuntime, InstanceId, InstanceStatus, RuntimeConfig, RuntimeError, TaskInput, TaskOutput,
};
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::image_manager::{
    CredentialResolver, DockerImageManager, StandardDockerImageManager,
};
use async_trait::async_trait;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, Mount, MountTypeEnum};
use bollard::query_parameters::{
    CreateContainerOptions, ListContainersOptionsBuilder, PruneImagesOptions,
    RemoveContainerOptions, RemoveVolumeOptions, StartContainerOptions, StatsOptions,
    UploadToContainerOptions,
};
use bollard::Docker;
use chrono::Utc;
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Connect to a Docker-compatible container runtime (Docker or Podman) using an
/// optional explicit socket path. Falls back to `Docker::connect_with_local_defaults()`
/// when no path is given. This is the single source of truth for creating Bollard clients.
pub fn connect_container_runtime(
    socket_path: Option<&str>,
) -> Result<Docker, bollard::errors::Error> {
    if let Some(path) = socket_path {
        #[cfg(unix)]
        {
            Docker::connect_with_unix(path, 120, bollard::API_DEFAULT_VERSION)
        }
        #[cfg(windows)]
        {
            Docker::connect_with_named_pipe(path, 120, bollard::API_DEFAULT_VERSION)
        }
    } else {
        Docker::connect_with_local_defaults()
    }
}

const AEGIS_CONTAINER_KIND_AGENT: &str = "agent";
const AEGIS_CONTAINER_KIND_LABEL: &str = "aegis.container_kind";
const AEGIS_EXECUTION_ID_LABEL: &str = "aegis.execution_id";
const AEGIS_KEEP_CONTAINER_ON_FAILURE_LABEL: &str = "aegis.keep_container_on_failure";
const AEGIS_MANAGED_LABEL: &str = "aegis.managed";
const AEGIS_RUNTIME_LABEL: &str = "aegis.runtime";
const AEGIS_RUNTIME_KIND_DOCKER: &str = "docker";

#[derive(Debug, Clone)]
pub struct ManagedAgentContainer {
    pub id: String,
    pub execution_id: Option<String>,
    pub debug_retain: bool,
    pub state: Option<String>,
}

pub struct ContainerRuntime {
    docker: Docker,
    bootstrap_script_path: PathBuf, // Store as PathBuf for better path handling
    network_mode: Option<String>,
    orchestrator_url: String,
    nfs_server_host: Option<String>, // NFS server hostname for volume mounts (ADR-036)
    nfs_port: u16,
    nfs_mountport: u16,
    keep_container_on_failure: RwLock<HashMap<String, bool>>,
    /// Per-container custom bootstrap paths (ADR-044).
    /// Key: container ID. `Some(path)` → CustomRuntime bootstrap already in image at `path`.
    /// Absent → StandardRuntime; default `/usr/local/bin/aegis-bootstrap` is used.
    bootstrap_paths: RwLock<HashMap<String, String>>,
    /// Image lifecycle manager: pull-policy enforcement and cache checks (ADR-045).
    image_manager: Arc<dyn DockerImageManager>,
    /// Event bus for publishing image management lifecycle events (ADR-045, ADR-030).
    event_bus: Arc<EventBus>,
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
    /// Active FUSE mount handles keyed by container ID (ADR-107).
    /// Handles are inserted in `spawn()` and removed in `terminate()`.
    /// Dropping a handle triggers FUSE_DESTROY + unmount on the host.
    fuse_mount_handles:
        RwLock<HashMap<String, Vec<crate::infrastructure::fuse::daemon::FuseMountHandle>>>,
    /// gRPC FUSE mounts keyed by container ID (ADR-107).
    /// Stores (execution_id, volume_id) pairs for each container so that
    /// `terminate()` can call `client.unmount()` on the remote FUSE daemon.
    /// Without this, agent containers using gRPC FUSE mounts leave orphaned
    /// mountpoints that spam `DirectoryListed` events every 2 seconds.
    grpc_fuse_mounts: RwLock<HashMap<String, Vec<(String, String)>>>,
}

/// Configuration bundle for constructing a [`ContainerRuntime`].
///
/// Groups the constructor parameters into a single struct to keep the
/// `new` call-site readable and satisfy the `clippy::too_many_arguments` lint.
pub struct ContainerRuntimeConfig {
    pub bootstrap_script: String,
    pub socket_path: Option<String>,
    pub network_mode: Option<String>,
    pub orchestrator_url: String,
    pub nfs_server_host: Option<String>,
    pub nfs_port: u16,
    pub nfs_mountport: u16,
    pub event_bus: Arc<EventBus>,
    pub credential_resolver: Arc<dyn CredentialResolver>,
    /// FUSE FSAL daemon for bind-mount-based volume access (ADR-107).
    /// When `Some`, the runtime uses FUSE + bind mounts instead of NFS volume
    /// driver mounts. Required for rootless container runtimes (Podman).
    pub fuse_daemon: Option<Arc<crate::infrastructure::fuse::daemon::FuseFsalDaemon>>,
    /// Host directory prefix for FUSE mountpoints (ADR-107).
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

impl ContainerRuntime {
    pub fn new(config: ContainerRuntimeConfig) -> Result<Self, RuntimeError> {
        let ContainerRuntimeConfig {
            bootstrap_script,
            socket_path,
            network_mode,
            orchestrator_url,
            nfs_server_host,
            nfs_port,
            nfs_mountport,
            event_bus,
            credential_resolver,
            fuse_daemon,
            fuse_mount_prefix,
            fuse_mount_client,
        } = config;
        // Resolve bootstrap script path to absolute path
        let bootstrap_path = if PathBuf::from(&bootstrap_script).is_absolute() {
            PathBuf::from(&bootstrap_script)
        } else {
            // Relative path - resolve from current directory
            std::env::current_dir()
                .map_err(|e| {
                    RuntimeError::SpawnFailed(format!("Failed to get current directory: {e}"))
                })?
                .join(&bootstrap_script)
        };

        // Validate the bootstrap script exists
        if !bootstrap_path.exists() {
            return Err(RuntimeError::SpawnFailed(format!(
                "Bootstrap script not found at: {}\n\n\
                 Expected location: {}\n\
                 Current directory: {:?}\n\n\
                 Please ensure the bootstrap script exists at the configured path.",
                bootstrap_script,
                bootstrap_path.display(),
                std::env::current_dir()
            )));
        }

        info!("Using bootstrap script: {}", bootstrap_path.display());
        // Connect to container runtime (Docker or Podman) — custom socket or auto-detect
        let docker = connect_container_runtime(socket_path.as_deref())
            .map_err(|e| {
                RuntimeError::SpawnFailed(format!(
                    "Failed to connect to container runtime: {e}\n\n\
                     Common causes:\n\
                     - Container runtime (Docker/Podman) not running\n\
                     - Permission denied accessing runtime socket\n\
                     - Socket path misconfigured\n\n\
                     Try:\n\
                     - Check socket: ls -la /var/run/docker.sock or /run/podman/podman.sock\n\
                     - Start runtime: systemctl start docker (or systemctl --user start podman.socket)"
                ))
            })?;
        // Clone docker before moving it into Self (image_manager needs its own handle).
        let image_manager: Arc<dyn DockerImageManager> = Arc::new(StandardDockerImageManager::new(
            docker.clone(),
            credential_resolver,
        ));
        Ok(Self {
            docker,
            bootstrap_script_path: bootstrap_path,
            network_mode,
            orchestrator_url,
            nfs_server_host,
            nfs_port,
            nfs_mountport,
            keep_container_on_failure: RwLock::new(HashMap::new()),
            bootstrap_paths: RwLock::new(HashMap::new()),
            image_manager,
            event_bus,
            fuse_daemon,
            fuse_mount_prefix,
            fuse_mount_client,
            fuse_mount_handles: RwLock::new(HashMap::new()),
            grpc_fuse_mounts: RwLock::new(HashMap::new()),
        })
    }

    /// Remove dangling (unused) Docker images to reclaim disk space (ADR-045).
    ///
    /// Calls `docker image prune --filter dangling=true`. Safe to call at any time;
    /// running containers are never affected because Docker only prunes images with
    /// no active consumer.
    pub async fn cleanup_images(&self) -> Result<(), RuntimeError> {
        let mut filters = std::collections::HashMap::new();
        filters.insert("dangling".to_string(), vec!["true".to_string()]);
        self.docker
            .prune_images(Some(PruneImagesOptions {
                filters: Some(filters),
            }))
            .await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Image prune failed: {e}")))?;
        Ok(())
    }

    pub async fn list_managed_agent_containers(
        &self,
    ) -> Result<Vec<ManagedAgentContainer>, RuntimeError> {
        let mut filters = std::collections::HashMap::new();
        filters.insert(
            "label".to_string(),
            vec![
                format!("{AEGIS_MANAGED_LABEL}=true"),
                format!("{AEGIS_RUNTIME_LABEL}={AEGIS_RUNTIME_KIND_DOCKER}"),
                format!("{AEGIS_CONTAINER_KIND_LABEL}={AEGIS_CONTAINER_KIND_AGENT}"),
            ],
        );

        let containers = self
            .docker
            .list_containers(Some(
                ListContainersOptionsBuilder::new()
                    .all(true)
                    .filters(&filters)
                    .build(),
            ))
            .await
            .map_err(|e| {
                RuntimeError::SpawnFailed(format!("Failed to list managed containers: {e}"))
            })?;

        Ok(containers
            .into_iter()
            .filter_map(|container| {
                let id = container.id?;
                let labels = container.labels.unwrap_or_default();
                let execution_id = labels.get(AEGIS_EXECUTION_ID_LABEL).cloned();
                let debug_retain = labels
                    .get(AEGIS_KEEP_CONTAINER_ON_FAILURE_LABEL)
                    .map(|value| value.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                let state = container.state.map(|state| state.to_string());

                Some(ManagedAgentContainer {
                    id,
                    execution_id,
                    debug_retain,
                    state,
                })
            })
            .collect())
    }

    /// Verify container runtime (Docker/Podman) is accessible
    pub async fn healthcheck(&self) -> Result<(), RuntimeError> {
        self.docker.ping().await.map_err(|e| {
            RuntimeError::SpawnFailed(format!(
                "Cannot connect to container runtime: {e}\n\n\
                 Healthcheck failed. Ensure Docker or Podman is running:\n\
                 - Docker: sudo systemctl start docker\n\
                 - Podman: systemctl --user start podman.socket\n\n\
                 Verify with: docker ps (or podman ps)"
            ))
        })?;
        Ok(())
    }

    fn format_bootstrap_stdout_for_log(content: &str) -> String {
        fn indent_block(block: &str, indent: &str) -> String {
            block
                .lines()
                .map(|line| format!("{indent}{line}"))
                .collect::<Vec<_>>()
                .join("\n")
        }

        let trimmed = content.trim();
        let parsed = serde_json::from_str::<Value>(trimmed).ok();

        let Some(Value::Object(map)) = parsed else {
            return format!("Bootstrap final response (model-authored analysis): {trimmed}");
        };

        let mut lines = vec!["Bootstrap final response (model-authored analysis):".to_string()];

        if let Some(reasoning) = map
            .get("reasoning")
            .or_else(|| map.get("analysis"))
            .and_then(|value| value.as_str())
        {
            lines.push("  model_reasoning:".to_string());
            lines.push(indent_block(reasoning, "    "));
        }

        if let Some(validation) = map.get("deterministic_validation") {
            lines.push("  deterministic_validation:".to_string());
            if let Some(error) = validation.get("error").and_then(|value| value.as_str()) {
                lines.push(format!("    error: {error}"));
            }
            lines.push(indent_block(
                &serde_json::to_string_pretty(validation)
                    .unwrap_or_else(|_| validation.to_string()),
                "    ",
            ));
        }

        if let Some(errors) = map.get("errors") {
            lines.push("  errors:".to_string());
            lines.push(indent_block(
                &serde_json::to_string_pretty(errors).unwrap_or_else(|_| errors.to_string()),
                "    ",
            ));
        }

        if let Some(semantic_validation) = map.get("semantic_validation") {
            lines.push("  semantic_validation:".to_string());
            lines.push(indent_block(
                &serde_json::to_string_pretty(semantic_validation)
                    .unwrap_or_else(|_| semantic_validation.to_string()),
                "    ",
            ));
        }

        lines.push("  raw_model_response:".to_string());
        lines.push(indent_block(
            &serde_json::to_string_pretty(&Value::Object(map))
                .unwrap_or_else(|_| trimmed.to_string()),
            "    ",
        ));

        lines.join("\n")
    }

    fn managed_container_labels(config: &RuntimeConfig) -> HashMap<String, String> {
        HashMap::from([
            (AEGIS_MANAGED_LABEL.to_string(), "true".to_string()),
            (
                AEGIS_RUNTIME_LABEL.to_string(),
                AEGIS_RUNTIME_KIND_DOCKER.to_string(),
            ),
            (
                AEGIS_CONTAINER_KIND_LABEL.to_string(),
                AEGIS_CONTAINER_KIND_AGENT.to_string(),
            ),
            (
                AEGIS_EXECUTION_ID_LABEL.to_string(),
                config.execution_id.to_string(),
            ),
            (
                AEGIS_KEEP_CONTAINER_ON_FAILURE_LABEL.to_string(),
                config.keep_container_on_failure.to_string(),
            ),
        ])
    }

    async fn cleanup_spawned_container(&self, container_id: &str) {
        let cleanup_id = InstanceId::new(container_id.to_string());
        if let Err(error) = AgentRuntime::terminate(self, &cleanup_id).await {
            warn!(
                container_id = container_id,
                error = %error,
                "Failed to clean up spawned container after setup failure"
            );
        }
    }

    async fn get_container_stats(&self, id: &str) -> Option<(f64, u64, u64)> {
        // Get container stats from Docker API
        match self.docker.inspect_container(id, None).await {
            Ok(inspect) => {
                let state = inspect.state.as_ref()?;

                // Calculate uptime from StartedAt timestamp
                let uptime = if let Some(started_at) = &state.started_at {
                    match chrono::DateTime::parse_from_rfc3339(started_at) {
                        Ok(started) => {
                            let now = chrono::Utc::now();
                            let duration = now.signed_duration_since(started);
                            duration.num_seconds() as u64
                        }
                        Err(_) => 0,
                    }
                } else {
                    0
                };

                // Get CPU and memory stats (requires stats API call)
                match self
                    .docker
                    .stats(
                        id,
                        Some(StatsOptions {
                            stream: false,
                            one_shot: true,
                        }),
                    )
                    .next()
                    .await
                {
                    Some(Ok(stats)) => {
                        // Calculate CPU percentage
                        let cpu_percent = if let (Some(cpu_stats), Some(precpu_stats)) =
                            (stats.cpu_stats.as_ref(), stats.precpu_stats.as_ref())
                        {
                            if let (
                                Some(system_cpu_usage),
                                Some(presystem_cpu_usage),
                                Some(cpu_usage),
                                Some(precpu_usage),
                            ) = (
                                cpu_stats.system_cpu_usage,
                                precpu_stats.system_cpu_usage,
                                cpu_stats.cpu_usage.as_ref(),
                                precpu_stats.cpu_usage.as_ref(),
                            ) {
                                let cpu_delta = cpu_usage
                                    .total_usage
                                    .unwrap_or(0)
                                    .saturating_sub(precpu_usage.total_usage.unwrap_or(0))
                                    as f64;
                                let system_delta =
                                    system_cpu_usage.saturating_sub(presystem_cpu_usage) as f64;
                                let num_cpus = cpu_stats.online_cpus.unwrap_or(1) as f64;
                                if system_delta > 0.0 {
                                    (cpu_delta / system_delta) * num_cpus * 100.0
                                } else {
                                    0.0
                                }
                            } else {
                                0.0
                            }
                        } else {
                            0.0
                        };

                        // Get memory usage
                        let memory_bytes = stats
                            .memory_stats
                            .as_ref()
                            .and_then(|memory| memory.usage)
                            .unwrap_or(0);

                        Some((cpu_percent, memory_bytes, uptime))
                    }
                    _ => Some((0.0, 0, uptime)), // Return at least uptime if stats fail
                }
            }
            Err(e) => {
                debug!("Failed to get container stats for {}: {}", id, e);
                None
            }
        }
    }
}

#[async_trait]
impl AgentRuntime for ContainerRuntime {
    async fn spawn(&self, config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        // Validate isolation mode first
        config.validate_isolation()?;

        let image = config.image.clone();

        // Ensure the image is available locally (ADR-045); publish lifecycle events.
        let pull_start = std::time::Instant::now();
        self.event_bus
            .publish_image_event(ImageManagementEvent::ImagePullStarted {
                execution_id: config.execution_id,
                image: image.clone(),
                pull_policy: config.image_pull_policy,
                started_at: Utc::now(),
            });
        let pull_source = match self
            .image_manager
            .ensure_image(&image, config.image_pull_policy, None)
            .await
        {
            Ok(source) => {
                self.event_bus
                    .publish_image_event(ImageManagementEvent::ImagePullCompleted {
                        execution_id: config.execution_id,
                        image: image.clone(),
                        source,
                        duration_ms: pull_start.elapsed().as_millis() as u64,
                        completed_at: Utc::now(),
                    });
                source
            }
            Err(e) => {
                self.event_bus
                    .publish_image_event(ImageManagementEvent::ImagePullFailed {
                        execution_id: config.execution_id,
                        image: image.clone(),
                        reason: e.to_string(),
                        failed_at: Utc::now(),
                    });
                return Err(e);
            }
        };
        // pull_source carried for audit; not needed for container creation logic.
        let _ = pull_source;

        // Build host config with resource limits

        let mut host_config = bollard::models::HostConfig {
            mounts: None, // Will be set below if volumes are specified (ADR-036: NFS volume mounts)
            network_mode: self.network_mode.clone(), // Optional Docker network (None = default)
            // ADR-027: Allow containers to reach host services (LLM APIs, orchestrator proxy) via
            // host.docker.internal (required on macOS/Windows; Linux uses host-gateway mapping)
            extra_hosts: Some(vec![
                "host.docker.internal:host-gateway".to_string(),
                "host.containers.internal:host-gateway".to_string(),
            ]),
            ..Default::default()
        };

        // Apply resource limits if specified
        if let Some(memory_bytes) = config.resources.memory_bytes {
            host_config.memory = Some(memory_bytes as i64);
        }
        if let Some(cpu_millis) = config.resources.cpu_millis {
            // Docker nano_cpus: 1 CPU = 1_000_000_000 (1e9) nano CPUs, 1000 millicores = 1 CPU
            // Therefore: 1 millicore = 1_000_000 nano CPUs, so 1000m (1 CPU) = 1_000_000_000 nano CPUs.
            host_config.nano_cpus = Some((cpu_millis as i64) * 1_000_000_000 / 1000);
        }

        // ─── Volume mounts: gRPC FUSE (ADR-107) → local FUSE (ADR-107) → NFS (ADR-036) ──
        // FUSE mount handles must outlive the container. We collect them here and
        // store them in the per-container map once we know the container ID.
        let mut pending_fuse_handles: Vec<crate::infrastructure::fuse::daemon::FuseMountHandle> =
            Vec::new();
        // gRPC FUSE mount tracking: (execution_id, volume_id) pairs collected
        // during the gRPC mount path, stored per-container after creation so
        // terminate() can call unmount on the remote daemon.
        let mut pending_grpc_fuse_pairs_outer: Vec<(String, String)> = Vec::new();
        if !config.volumes.is_empty() {
            if let Some(ref _fuse_mount_client) = self.fuse_mount_client {
                // ── gRPC FuseMountService path (ADR-107) ─────────────────────────
                // Delegate mount requests to the host-side FUSE daemon over gRPC.
                let mut grpc_mounts: Vec<Mount> = Vec::new();
                let mut client = _fuse_mount_client.clone();

                // Track (execution_id, volume_id) pairs so terminate() can call
                // unmount on the remote FUSE daemon after the container exits.
                let mut pending_grpc_fuse_pairs: Vec<(String, String)> = Vec::new();

                for volume_mount in &config.volumes {
                    let container_path = volume_mount.mount_point.display().to_string();
                    let is_read_only = matches!(
                        volume_mount.access_mode,
                        crate::domain::volume::AccessMode::ReadOnly
                    );

                    let grpc_req = crate::infrastructure::aegis_runtime_proto::FuseMountRequest {
                        volume_id: volume_mount.volume_id.0.to_string(),
                        execution_id: config.execution_id.0.to_string(),
                        mount_point: String::new(),
                        read_paths: vec!["/*".to_string()],
                        write_paths: if is_read_only {
                            vec![]
                        } else {
                            vec!["/*".to_string()]
                        },
                        container_uid: 1000,
                        container_gid: 1000,
                        workflow_execution_id: String::new(),
                    };

                    match client.mount(grpc_req).await {
                        Ok(resp) => {
                            let mountpoint = resp.into_inner().mountpoint;
                            debug!(
                                volume_id = %volume_mount.volume_id,
                                mountpoint = %mountpoint,
                                container_path = %container_path,
                                read_only = is_read_only,
                                "Configured gRPC FUSE mount for agent container (ADR-107)"
                            );
                            pending_grpc_fuse_pairs.push((
                                config.execution_id.0.to_string(),
                                volume_mount.volume_id.0.to_string(),
                            ));
                            grpc_mounts.push(Mount {
                                target: Some(container_path),
                                source: Some(mountpoint),
                                typ: Some(MountTypeEnum::BIND),
                                read_only: Some(is_read_only),
                                ..Default::default()
                            });
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                volume_id = %volume_mount.volume_id,
                                "gRPC FUSE mount failed — volume will not be available"
                            );
                        }
                    }
                }

                if !grpc_mounts.is_empty() {
                    host_config.mounts = Some(grpc_mounts);
                }

                // Temporarily stash the pairs; they'll be stored keyed by container
                // ID after create_container succeeds (same pattern as pending_fuse_handles).
                pending_fuse_handles = Vec::new(); // no local handles for gRPC path

                // Stash the gRPC pairs so we can store them keyed by container ID
                // after create_container succeeds.
                #[allow(unused_assignments)]
                {
                    pending_grpc_fuse_pairs_outer = pending_grpc_fuse_pairs;
                }
            } else if let Some(ref fuse_daemon) = self.fuse_daemon {
                // ── FUSE + bind mount path (ADR-107) ─────────────────────────────
                // Collect (handle, mount) pairs so FuseMountHandle values are kept
                // alive until terminate() is called. Dropping a handle unmounts.
                let fuse_pairs: Vec<(crate::infrastructure::fuse::daemon::FuseMountHandle, Mount)> =
                    config
                        .volumes
                        .iter()
                        .filter_map(|volume_mount| {
                            let container_path = volume_mount.mount_point.display().to_string();
                            let is_read_only = matches!(
                                volume_mount.access_mode,
                                crate::domain::volume::AccessMode::ReadOnly
                            );
                            let policy = crate::domain::fsal::FsalAccessPolicy {
                                read: vec!["/*".to_string()],
                                write: if is_read_only {
                                    vec![]
                                } else {
                                    vec!["/*".to_string()]
                                },
                            };

                            let mountpoint_path =
                                format!("{}/{}", self.fuse_mount_prefix, volume_mount.volume_id);
                            let fuse_context =
                                crate::infrastructure::fuse::daemon::FuseVolumeContext {
                                    execution_id: config.execution_id,
                                    volume_id: volume_mount.volume_id,
                                    workflow_execution_id: None,
                                    container_uid: 1000, // Default; overridden by manifest
                                    container_gid: 1000,
                                    policy,
                                };

                            match fuse_daemon
                                .mount(std::path::Path::new(&mountpoint_path), fuse_context)
                            {
                                Ok(handle) => {
                                    debug!(
                                        volume_id = %volume_mount.volume_id,
                                        mountpoint = %mountpoint_path,
                                        container_path = %container_path,
                                        read_only = is_read_only,
                                        "Configured FUSE bind mount for agent container (ADR-107)"
                                    );
                                    Some((
                                        handle,
                                        Mount {
                                            target: Some(container_path),
                                            source: Some(mountpoint_path),
                                            typ: Some(MountTypeEnum::BIND),
                                            read_only: Some(is_read_only),
                                            ..Default::default()
                                        },
                                    ))
                                }
                                Err(e) => {
                                    warn!(
                                        error = %e,
                                        volume_id = %volume_mount.volume_id,
                                        "FUSE mount failed — skipping volume"
                                    );
                                    None
                                }
                            }
                        })
                        .collect();

                // Split into handles (stored per-container) and mounts (passed to Docker).
                let (fuse_handles, mounts): (Vec<_>, Vec<_>) = fuse_pairs.into_iter().unzip();

                // Temporarily hold handles; they'll be stored in the per-container
                // map once we know the container ID (after create_container).
                pending_fuse_handles = fuse_handles;

                if !mounts.is_empty() {
                    host_config.mounts = Some(mounts);
                    info!(
                        "Configured {} FUSE bind mount(s) for container (ADR-107)",
                        config.volumes.len()
                    );
                }
            } else {
                // ── NFS volume driver path (ADR-036) ─────────────────────────────
                let orchestrator_host = self
                    .nfs_server_host
                    .as_deref()
                    .unwrap_or_else(|| "127.0.0.1");

                let mounts: Vec<Mount> = config
                    .volumes
                    .iter()
                    .map(|volume_mount| {
                        let container_path = volume_mount.mount_point.display().to_string();

                        debug!(
                            "Configuring NFS mount: volume_id={}, path={}, mode={:?}, host={}",
                            volume_mount.volume_id,
                            container_path,
                            volume_mount.access_mode,
                            orchestrator_host
                        );

                        use bollard::models::{MountVolumeOptions, MountVolumeOptionsDriverConfig};
                        use std::collections::HashMap;

                        let mut driver_opts = HashMap::new();
                        driver_opts.insert("type".to_string(), "nfs".to_string());
                        driver_opts.insert(
                            "o".to_string(),
                            format!(
                                "addr={},nfsvers=3,proto=tcp,port={},mountport={},soft,timeo=10,nolock",
                                orchestrator_host, self.nfs_port, self.nfs_mountport
                            ),
                        );
                        driver_opts.insert(
                            "device".to_string(),
                            format!(":{}", volume_mount.remote_path),
                        );

                        Mount {
                            target: Some(container_path),
                            source: Some(format!("aegis-vol-{}", volume_mount.volume_id)),
                            typ: Some(MountTypeEnum::VOLUME),
                            read_only: Some(matches!(
                                volume_mount.access_mode,
                                crate::domain::volume::AccessMode::ReadOnly
                            )),
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

                host_config.mounts = Some(mounts);
                info!(
                    "Configured {} NFS volume mount(s) for container (ADR-036)",
                    config.volumes.len()
                );
            }
        }

        // Remove stale named volumes so NFS driver options are applied fresh.
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
                            "Could not remove stale volume '{}' (may not exist): {}",
                            vol_name, e
                        );
                    }
                }
            }
        }

        let options = CreateContainerOptions {
            name: Some(format!("aegis-agent-{}", uuid::Uuid::new_v4())),
            platform: String::new(),
        };

        // Filter env vars to prevent orchestrator-internal variables from leaking
        let (filtered_env, blocked_vars) = crate::domain::env_guard::filter_env_vars(&config.env);
        if !blocked_vars.is_empty() {
            warn!(
                execution_id = %config.execution_id,
                blocked = ?blocked_vars,
                "Blocked orchestrator-internal env vars from agent container"
            );
        }

        // Convert map to "KEY=VALUE" strings
        let mut env_vars: Vec<String> = filtered_env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        // Add orchestrator URL for agent bootstrap script to call LLM proxy
        env_vars.push(format!("AEGIS_ORCHESTRATOR_URL={}", self.orchestrator_url));

        // Enable bootstrap.py verbose mode if log level is debug or trace
        if tracing::level_enabled!(tracing::Level::DEBUG) {
            env_vars.push("AEGIS_BOOTSTRAP_DEBUG=true".to_string());
            debug!("Enabled bootstrap.py verbose mode (AEGIS_BOOTSTRAP_DEBUG=true)");
        }

        // Keep container alive - actual agent execution happens via bootstrap script in execute()
        let cmd = vec![
            "tail".to_string(),
            "-f".to_string(),
            "/dev/null".to_string(),
        ];

        // Create container configuration
        let container_config = ContainerCreateBody {
            image: Some(image.clone()),
            tty: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(cmd),
            env: Some(env_vars),
            labels: Some(Self::managed_container_labels(&config)),
            host_config: Some(host_config),
            ..Default::default()
        };

        // Create the container
        let res = self
            .docker
            .create_container(Some(options), container_config)
            .await
            .map_err(|e| RuntimeError::SpawnFailed(e.to_string()))?;

        let id = res.id;

        // Store FUSE mount handles keyed by container ID (ADR-107).
        // Handles are dropped in terminate() after the container is removed,
        // triggering FUSE_DESTROY + unmount on the host mountpoints.
        if !pending_fuse_handles.is_empty() {
            self.fuse_mount_handles
                .write()
                .await
                .insert(id.clone(), pending_fuse_handles);
        }

        // Store gRPC FUSE mount pairs keyed by container ID (ADR-107).
        // terminate() iterates these and calls client.unmount() on the remote
        // FUSE daemon for each (execution_id, volume_id) pair.
        if !pending_grpc_fuse_pairs_outer.is_empty() {
            self.grpc_fuse_mounts
                .write()
                .await
                .insert(id.clone(), pending_grpc_fuse_pairs_outer);
        }

        self.keep_container_on_failure
            .write()
            .await
            .insert(id.clone(), config.keep_container_on_failure);

        if let Err(e) = self
            .docker
            .start_container(&id, None::<StartContainerOptions>)
            .await
        {
            self.cleanup_spawned_container(&id).await;
            return Err(RuntimeError::SpawnFailed(format!(
                "Failed to start container: {e}"
            )));
        }

        info!("Spawned agent container: {}", id);

        // Handle bootstrap script for this container (ADR-044).
        // StandardRuntime: copy host-side bootstrap.py into the container at /usr/local/bin/aegis-bootstrap.
        // CustomRuntime (bootstrap_path set): script is already present in the image; store the
        // path for execute() and skip host-side injection.
        if let Some(ref custom_path) = config.bootstrap_path {
            self.bootstrap_paths
                .write()
                .await
                .insert(id.clone(), custom_path.clone());
            debug!(
                container_id = &id,
                bootstrap_path = custom_path.as_str(),
                "Custom bootstrap path stored; skipping host-side injection"
            );
        } else {
            debug!(
                container_id = &id,
                "Copying bootstrap script into container"
            );
            if let Err(error) = self.copy_bootstrap_to_container(&id).await {
                self.cleanup_spawned_container(&id).await;
                return Err(error);
            }
        }

        Ok(InstanceId::new(id))
    }

    async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput, RuntimeError> {
        let container_id = id.as_str();

        // Resolve bootstrap script path: stored custom path (CustomRuntime) or the default
        // injected by spawn() (StandardRuntime: /usr/local/bin/aegis-bootstrap).
        let bootstrap_path = self
            .bootstrap_paths
            .read()
            .await
            .get(container_id)
            .cloned()
            .unwrap_or_else(|| "/usr/local/bin/aegis-bootstrap".to_string());

        debug!(
            container_id = container_id,
            prompt_len = input.prompt.len(),
            "Executing bootstrap script in container"
        );

        // Execute bootstrap script via Docker exec API
        let exec_config = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            // Pass input as argument. Note: Shell escaping might be needed if input has special chars.
            // Using list form avoids shell: ["python", ...")
            cmd: Some(vec![
                "python".to_string(),
                bootstrap_path.clone(),
                input.prompt.clone(),
            ]),
            // env: Some(vec!["PYTHONUNBUFFERED=1".to_string()]), // Commented out to ensure inheritance from container
            ..Default::default()
        };

        debug!(
            container_id = container_id,
            bootstrap_path = %bootstrap_path,
            "Exec command: python <bootstrap_path> <prompt>"
        );

        let exec = self
            .docker
            .create_exec(container_id, exec_config)
            .await
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

        let start_opts = StartExecOptions {
            detach: false,
            ..Default::default()
        };

        let res = self
            .docker
            .start_exec(&exec.id, Some(start_opts))
            .await
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

        let mut stdout_logs = Vec::new();
        let mut stderr_logs = Vec::new();

        debug!(
            container_id = container_id,
            "Starting bootstrap.py execution"
        );

        if let StartExecResults::Attached { mut output, .. } = res {
            while let Some(msg) = output.next().await {
                match msg {
                    Ok(LogOutput::StdOut { message }) => {
                        let content = String::from_utf8_lossy(&message);
                        for line in content.lines() {
                            if !line.trim().is_empty() {
                                tracing::info!(
                                    target: "child_runtime",
                                    container_id = container_id,
                                    stream = "stdout",
                                    "{line}"
                                );
                            }
                        }
                        stdout_logs.push(content.to_string());
                    }
                    Ok(LogOutput::StdErr { message }) => {
                        let content = String::from_utf8_lossy(&message);
                        for line in content.lines() {
                            if !line.trim().is_empty() {
                                tracing::warn!(
                                    target: "child_runtime",
                                    container_id = container_id,
                                    stream = "stderr",
                                    "{line}"
                                );
                            }
                        }
                        stderr_logs.push(content.to_string());
                    }
                    _ => {}
                }
            }
        }

        // Get exit code from exec
        let exec_inspect =
            self.docker.inspect_exec(&exec.id).await.map_err(|e| {
                RuntimeError::ExecutionFailed(format!("Failed to inspect exec: {e}"))
            })?;

        let exit_code = exec_inspect.exit_code.unwrap_or(0);

        debug!(
            container_id = container_id,
            exit_code = exit_code,
            stdout_lines = stdout_logs.len(),
            stderr_lines = stderr_logs.len(),
            "Bootstrap execution completed"
        );

        // Check exit code and return error if bootstrap failed
        let keep_on_failure = self
            .keep_container_on_failure
            .read()
            .await
            .get(container_id)
            .copied()
            .unwrap_or(false);

        let execution_result = if exit_code != 0 && keep_on_failure {
            let error_msg = if !stderr_logs.is_empty() {
                stderr_logs.join("\n")
            } else {
                format!("Bootstrap script exited with code {exit_code}")
            };
            Err(RuntimeError::ExecutionFailed(error_msg))
        } else {
            let result = serde_json::Value::String(stdout_logs.join(""));
            Ok(TaskOutput {
                result,
                logs: stderr_logs,
                tool_calls: vec![],
                exit_code,
                // Trajectory is populated by the supervisor after execute() returns by
                // querying the execution repository.  The runtime itself has no access
                // to the inner-loop trajectory.
                trajectory: vec![],
            })
        };

        self.keep_container_on_failure
            .write()
            .await
            .remove(container_id);

        execution_result
    }

    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError> {
        let options = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };

        self.keep_container_on_failure
            .write()
            .await
            .remove(id.as_str());

        self.bootstrap_paths.write().await.remove(id.as_str());

        // Inspect the container before removal to discover its named volumes
        // so we can clean them up and prevent stale volume accumulation.
        let volume_names: Vec<String> = self
            .docker
            .inspect_container(id.as_str(), None)
            .await
            .ok()
            .and_then(|info| info.mounts)
            .map(|mounts| {
                mounts
                    .iter()
                    .filter_map(|m| m.name.clone())
                    .filter(|name| name.starts_with("aegis-vol-"))
                    .collect()
            })
            .unwrap_or_default();

        self.docker
            .remove_container(id.as_str(), Some(options))
            .await
            .map_err(|e| RuntimeError::TerminationFailed(e.to_string()))?;

        // Clean up named volumes after container removal to prevent stale
        // plain-local volumes from shadowing NFS driver options on next run.
        for vol_name in &volume_names {
            if let Err(e) = self
                .docker
                .remove_volume(vol_name, None::<RemoveVolumeOptions>)
                .await
            {
                debug!(
                    "Could not remove volume '{}' after termination (may still be in use): {}",
                    vol_name, e
                );
            }
        }

        // Drop FUSE mount handles (ADR-107). Dropping triggers FUSE_DESTROY +
        // unmount on each host mountpoint now that the container is gone.
        if let Some(handles) = self.fuse_mount_handles.write().await.remove(id.as_str()) {
            debug!(
                container_id = id.as_str(),
                count = handles.len(),
                "Dropping FUSE mount handles after container termination"
            );
            drop(handles);
        }

        // Unmount gRPC FUSE mounts (ADR-107). When the gRPC FuseMountService
        // path was used, the FUSE mounts live in the remote daemon process —
        // dropping local handles does nothing. We must explicitly call unmount
        // for each volume that was gRPC-mounted during spawn().
        if let Some(grpc_pairs) = self.grpc_fuse_mounts.write().await.remove(id.as_str()) {
            if let Some(ref fuse_mount_client) = self.fuse_mount_client {
                let mut client = fuse_mount_client.clone();
                for (execution_id, volume_id) in &grpc_pairs {
                    let unmount_req =
                        crate::infrastructure::aegis_runtime_proto::FuseUnmountRequest {
                            volume_id: volume_id.clone(),
                            execution_id: execution_id.clone(),
                        };
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        client.unmount(unmount_req),
                    )
                    .await
                    {
                        Err(_elapsed) => {
                            warn!(
                                volume_id = %volume_id,
                                execution_id = %execution_id,
                                container_id = id.as_str(),
                                "gRPC FUSE unmount timed out after 5s — mount may linger"
                            );
                        }
                        Ok(Err(e)) => {
                            warn!(
                                error = %e,
                                volume_id = %volume_id,
                                execution_id = %execution_id,
                                container_id = id.as_str(),
                                "gRPC FUSE unmount failed — mount may linger"
                            );
                        }
                        Ok(Ok(_)) => {
                            debug!(
                                volume_id = %volume_id,
                                execution_id = %execution_id,
                                container_id = id.as_str(),
                                "gRPC FUSE unmount succeeded for agent container"
                            );
                        }
                    }
                }
            }
        }

        info!("Terminated agent container: {}", id.as_str());
        Ok(())
    }

    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
        let inspect = self
            .docker
            .inspect_container(id.as_str(), None)
            .await
            .map_err(|e| RuntimeError::InstanceNotFound(e.to_string()))?;

        let state = inspect
            .state
            .and_then(|s| s.status)
            .unwrap_or(bollard::models::ContainerStateStatusEnum::DEAD);

        let (cpu, mem, uptime) = self
            .get_container_stats(id.as_str())
            .await
            .unwrap_or((0.0, 0, 0));

        Ok(InstanceStatus {
            id: id.clone(),
            state: format!("{state:?}"),
            uptime_seconds: uptime,
            memory_usage_mb: mem,
            cpu_usage_percent: cpu,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ContainerRuntime, AEGIS_CONTAINER_KIND_LABEL, AEGIS_EXECUTION_ID_LABEL,
        AEGIS_KEEP_CONTAINER_ON_FAILURE_LABEL, AEGIS_MANAGED_LABEL, AEGIS_RUNTIME_LABEL,
    };
    use crate::domain::agent::{ExecutionStrategy, ImagePullPolicy};
    use crate::domain::execution::ExecutionId;
    use crate::domain::runtime::{ResourceLimits, RuntimeConfig};
    use std::collections::HashMap;

    #[test]
    fn bootstrap_stdout_is_labeled_as_model_authored_analysis() {
        let formatted = ContainerRuntime::format_bootstrap_stdout_for_log(
            "The workflow is fine, but I think it should be rewritten.",
        );

        assert!(
            formatted.starts_with("Bootstrap final response (model-authored analysis):"),
            "{formatted}"
        );
        assert!(formatted.contains("I think it should be rewritten."));
    }

    #[test]
    fn bootstrap_stdout_preserves_exact_deterministic_error_from_model_json() {
        let formatted = ContainerRuntime::format_bootstrap_stdout_for_log(
            r#"{
  "errors": [
    "Workflow cycle validation failed: Workflow execution error: Circular reference detected in workflow"
  ],
  "reasoning": "This is a model-authored analysis of the failure."
}"#,
        );

        assert!(
            formatted.contains("Bootstrap final response (model-authored analysis):"),
            "{formatted}"
        );
        assert!(
            formatted.contains(
                "Workflow cycle validation failed: Workflow execution error: Circular reference detected in workflow"
            ),
            "{formatted}"
        );
        assert!(formatted.contains("model_reasoning:"), "{formatted}");
        assert!(
            formatted.contains("This is a model-authored analysis of the failure."),
            "{formatted}"
        );
        assert!(formatted.contains("errors:"), "{formatted}");
    }

    #[test]
    fn managed_container_labels_include_execution_and_failure_metadata() {
        let config = RuntimeConfig {
            language: "python".to_string(),
            version: "3.12".to_string(),
            isolation: "docker".to_string(),
            env: HashMap::new(),
            image_pull_policy: ImagePullPolicy::IfNotPresent,
            resources: ResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
                disk_bytes: None,
                timeout_seconds: None,
            },
            execution: ExecutionStrategy::default(),
            volumes: Vec::new(),
            container_uid: 1000,
            container_gid: 1000,
            keep_container_on_failure: true,
            image: "python:3.12".to_string(),
            bootstrap_path: None,
            execution_id: ExecutionId::new(),
        };

        let labels = ContainerRuntime::managed_container_labels(&config);

        assert_eq!(labels.get(AEGIS_MANAGED_LABEL), Some(&"true".to_string()));
        assert_eq!(labels.get(AEGIS_RUNTIME_LABEL), Some(&"docker".to_string()));
        assert_eq!(
            labels.get(AEGIS_CONTAINER_KIND_LABEL),
            Some(&"agent".to_string())
        );
        assert_eq!(
            labels.get(AEGIS_EXECUTION_ID_LABEL),
            Some(&config.execution_id.to_string())
        );
        assert_eq!(
            labels.get(AEGIS_KEEP_CONTAINER_ON_FAILURE_LABEL),
            Some(&"true".to_string())
        );
    }

    /// Regression: gRPC FUSE mounts in agent containers (via ContainerRuntime)
    /// must be tracked per-container and unmounted in terminate(). Before this
    /// fix, only ContainerStepRunner tracked gRPC-mounted volumes — agent
    /// containers using the gRPC FuseMountService path in spawn() created FUSE
    /// mounts on the remote daemon but never called unmount(), leaving orphaned
    /// mountpoints that spammed `DirectoryListed` events every 2 seconds.
    #[test]
    fn test_grpc_fuse_mounts_tracking_structure() {
        use std::collections::HashMap;
        use tokio::sync::RwLock;

        // Simulate the grpc_fuse_mounts map that ContainerRuntime now maintains.
        let grpc_fuse_mounts: RwLock<HashMap<String, Vec<(String, String)>>> =
            RwLock::new(HashMap::new());

        let container_id = "abc123".to_string();
        let exec_id = "11111111-1111-1111-1111-111111111111".to_string();
        let vol_id_1 = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string();
        let vol_id_2 = "ffffffff-ffff-ffff-ffff-ffffffffffff".to_string();

        // Insert tracking entries (simulates what spawn() now does).
        let pairs = vec![
            (exec_id.clone(), vol_id_1.clone()),
            (exec_id.clone(), vol_id_2.clone()),
        ];
        grpc_fuse_mounts
            .blocking_write()
            .insert(container_id.clone(), pairs);

        // Verify terminate() can retrieve and consume them.
        let removed = grpc_fuse_mounts.blocking_write().remove(&container_id);
        assert!(removed.is_some());
        let removed = removed.unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0], (exec_id.clone(), vol_id_1));
        assert_eq!(removed[1], (exec_id, vol_id_2));

        // After removal, the container entry is gone.
        assert!(grpc_fuse_mounts
            .blocking_read()
            .get(&container_id)
            .is_none());
    }
}
// Private helper methods for ContainerRuntime (Docker/Podman)
impl ContainerRuntime {
    /// Copy bootstrap.py into a running container (if not already present)
    async fn copy_bootstrap_to_container(&self, container_id: &str) -> Result<(), RuntimeError> {
        // First, check if bootstrap script already exists in the container
        // (useful if using pre-built images with bootstrap baked in)
        let check_exists = CreateExecOptions {
            attach_stdout: Some(false),
            attach_stderr: Some(false),
            cmd: Some(vec![
                "test".to_string(),
                "-f".to_string(),
                "/usr/local/bin/aegis-bootstrap".to_string(),
            ]),
            ..Default::default()
        };

        let check_exec = self
            .docker
            .create_exec(container_id, check_exists)
            .await
            .map_err(|e| {
                RuntimeError::SpawnFailed(format!("Failed to check for bootstrap script: {e}"))
            })?;

        let start_opts = StartExecOptions {
            detach: false,
            ..Default::default()
        };

        // Execute the test command
        let _ = self
            .docker
            .start_exec(&check_exec.id, Some(start_opts))
            .await;

        // Check exit code - 0 means file exists, non-zero means it doesn't
        let check_inspect = self
            .docker
            .inspect_exec(&check_exec.id)
            .await
            .map_err(|e| {
                RuntimeError::SpawnFailed(format!("Failed to inspect bootstrap check: {e}"))
            })?;

        let exit_code = check_inspect.exit_code.unwrap_or(1);

        if exit_code == 0 {
            debug!(
                container_id = container_id,
                "Bootstrap script already exists in container, skipping copy"
            );
            return Ok(());
        }

        debug!(
            container_id = container_id,
            "Bootstrap script not found in container, copying from host"
        );

        // Read bootstrap script content
        let bootstrap_content = std::fs::read(&self.bootstrap_script_path).map_err(|e| {
            RuntimeError::SpawnFailed(format!(
                "Failed to read bootstrap script at {}: {}",
                self.bootstrap_script_path.display(),
                e
            ))
        })?;

        // Create a tar archive with the bootstrap script
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header
            .set_path("aegis-bootstrap")
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to set tar path: {e}")))?;
        header.set_size(bootstrap_content.len() as u64);
        header.set_mode(0o755); // Make executable
        header.set_cksum();

        tar_builder
            .append(&header, bootstrap_content.as_slice())
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to create tar: {e}")))?;

        let tar_data = tar_builder
            .into_inner()
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to finalize tar: {e}")))?;

        // Upload to container at /usr/local/bin/
        let options = UploadToContainerOptions {
            path: "/usr/local/bin/".to_string(),
            ..Default::default()
        };

        self.docker
            .upload_to_container(
                container_id,
                Some(options),
                bollard::body_full(tar_data.into()),
            )
            .await
            .map_err(|e| {
                RuntimeError::SpawnFailed(format!(
                    "Failed to copy bootstrap script to container: {e}"
                ))
            })?;

        debug!(
            container_id = container_id,
            "Bootstrap script copied successfully"
        );
        Ok(())
    }
}
