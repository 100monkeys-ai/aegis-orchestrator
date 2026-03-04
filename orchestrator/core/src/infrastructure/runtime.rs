// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Docker Runtime Adapter — BC-2 (ADR-027)
//!
//! Implements the [`crate::domain::runtime::AgentRuntime`] domain trait using
//! the Docker Engine API via `bollard`. Provides container lifecycle management
//! with configurable resource limits, network policies, and NFS volume mounts.
//!
//! ⚠️ Phase 1 — Docker is the only supported runtime. Firecracker microVM
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
// Current Implementation: Docker runtime only (Bollard client)
// This module provides agent execution isolation via Docker containers.
//
// Firecracker VM-based isolation deferred to Phase 2 for production hardening.
// Phase 1 uses Docker for development/testing convenience.
//
// TODO: Implement Firecracker runtime variant when Phase 2 begins.
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
use bollard::container::{
    Config, CreateContainerOptions, LogOutput, RemoveContainerOptions, StartContainerOptions,
    UploadToContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{Mount, MountTypeEnum};
use bollard::Docker;
use chrono::Utc;
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

pub struct DockerRuntime {
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
}

/// Configuration bundle for constructing a [`DockerRuntime`].
///
/// Groups the nine constructor parameters into a single struct to keep the
/// `new` call-site readable and satisfy the `clippy::too_many_arguments` lint.
pub struct DockerRuntimeConfig {
    pub bootstrap_script: String,
    pub socket_path: Option<String>,
    pub network_mode: Option<String>,
    pub orchestrator_url: String,
    pub nfs_server_host: Option<String>,
    pub nfs_port: u16,
    pub nfs_mountport: u16,
    pub event_bus: Arc<EventBus>,
    pub credential_resolver: Arc<dyn CredentialResolver>,
}

impl DockerRuntime {
    pub fn new(config: DockerRuntimeConfig) -> Result<Self, RuntimeError> {
        let DockerRuntimeConfig {
            bootstrap_script,
            socket_path,
            network_mode,
            orchestrator_url,
            nfs_server_host,
            nfs_port,
            nfs_mountport,
            event_bus,
            credential_resolver,
        } = config;
        // Resolve bootstrap script path to absolute path
        let bootstrap_path = if PathBuf::from(&bootstrap_script).is_absolute() {
            PathBuf::from(&bootstrap_script)
        } else {
            // Relative path - resolve from current directory
            std::env::current_dir()
                .map_err(|e| {
                    RuntimeError::SpawnFailed(format!("Failed to get current directory: {}", e))
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
        // Connect to Docker daemon (custom socket or auto-detect)
        let docker = if let Some(path) = socket_path {
            // Try custom socket path
            #[cfg(unix)]
            let result = Docker::connect_with_unix(&path, 120, bollard::API_DEFAULT_VERSION);

            #[cfg(windows)]
            let result = Docker::connect_with_named_pipe(&path, 120, bollard::API_DEFAULT_VERSION);

            result.map_err(|e| {
                RuntimeError::SpawnFailed(format!(
                    "Failed to connect to Docker at {}: {}\n\n\
                 Ensure Docker is running and the socket path is correct.",
                    path, e
                ))
            })?
        } else {
            // Auto-detect Docker connection
            Docker::connect_with_local_defaults()
                .map_err(|e| RuntimeError::SpawnFailed(format!(
                    "Failed to connect to Docker: {}\n\n\
                     Common causes:\n\
                     - Docker daemon not running (check: docker ps)\n\
                     - Permission denied accessing Docker socket\n\
                     - On Windows: Docker Desktop not started\n\
                     - On Linux: Current user not in 'docker' group\n\n\
                     Try:\n\
                     - Start Docker: systemctl start docker (Linux) or Docker Desktop (Windows/Mac)\n\
                     - Check permissions: ls -la /var/run/docker.sock\n\
                     - Add user to docker group: sudo usermod -aG docker $USER",
                    e
                )))?
        };
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
        })
    }

    /// Remove dangling (unused) Docker images to reclaim disk space (ADR-045).
    ///
    /// Calls `docker image prune --filter dangling=true`. Safe to call at any time;
    /// running containers are never affected because Docker only prunes images with
    /// no active consumer.
    pub async fn cleanup_images(&self) -> Result<(), RuntimeError> {
        use bollard::image::PruneImagesOptions;
        let mut filters = std::collections::HashMap::new();
        filters.insert("dangling", vec!["true"]);
        self.docker
            .prune_images(Some(PruneImagesOptions { filters }))
            .await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Image prune failed: {}", e)))?;
        Ok(())
    }

    /// Verify Docker daemon is accessible
    pub async fn healthcheck(&self) -> Result<(), RuntimeError> {
        self.docker.ping().await.map_err(|e| {
            RuntimeError::SpawnFailed(format!(
                "Cannot connect to Docker daemon: {}\n\n\
                 Docker healthcheck failed. Ensure Docker is running:\n\
                 - On Windows: Start Docker Desktop\n\
                 - On Linux: sudo systemctl start docker\n\
                 - On macOS: Start Docker Desktop\n\n\
                 Verify with: docker ps",
                e
            ))
        })?;
        Ok(())
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
                        Some(bollard::container::StatsOptions {
                            stream: false,
                            one_shot: true,
                        }),
                    )
                    .next()
                    .await
                {
                    Some(Ok(stats)) => {
                        // Calculate CPU percentage
                        let cpu_percent =
                            if let (Some(system_cpu_usage), Some(presystem_cpu_usage)) = (
                                stats.cpu_stats.system_cpu_usage,
                                stats.precpu_stats.system_cpu_usage,
                            ) {
                                let cpu_usage = &stats.cpu_stats.cpu_usage;
                                let precpu_usage = &stats.precpu_stats.cpu_usage;
                                let cpu_delta = cpu_usage
                                    .total_usage
                                    .saturating_sub(precpu_usage.total_usage)
                                    as f64;
                                let system_delta =
                                    system_cpu_usage.saturating_sub(presystem_cpu_usage) as f64;
                                let num_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;
                                if system_delta > 0.0 {
                                    (cpu_delta / system_delta) * num_cpus * 100.0
                                } else {
                                    0.0
                                }
                            } else {
                                0.0
                            };

                        // Get memory usage
                        let memory_bytes = stats.memory_stats.usage.unwrap_or(0);

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
impl AgentRuntime for DockerRuntime {
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
            .ensure_image(&image, config.image_pull_policy)
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

        let mut host_config = bollard::service::HostConfig {
            mounts: None, // Will be set below if volumes are specified (ADR-036: NFS volume mounts)
            network_mode: self.network_mode.clone(), // Optional Docker network (None = default)
            ..Default::default()
        };

        // Apply resource limits if specified
        if let Some(memory_bytes) = config.resources.memory_bytes {
            host_config.memory = Some(memory_bytes as i64);
        }
        if let Some(cpu_millis) = config.resources.cpu_millis {
            // Docker nano_cpus: 1 CPU = 1e9 nano CPUs, 1 milli CPU = 1e6 nano CPUs
            host_config.nano_cpus = Some((cpu_millis as i64) * 1_000_000);
        }

        // Apply NFS volume mounts if specified (ADR-036: Orchestrator Proxy Pattern)
        if !config.volumes.is_empty() {
            // Use explicit NFS server host if provided, otherwise extract from orchestrator_url
            // This separation is needed because:
            // - orchestrator_url is used by containers (Docker service name works: "aegis-runtime")
            // - NFS mounts happen at Docker daemon level on host (needs resolvable hostname: "127.0.0.1")
            let orchestrator_host = self.nfs_server_host.as_deref().unwrap_or_else(|| {
                // Fallback: Default to "127.0.0.1" which covers Native Linux and WSL2 deployments.
                // Prior behavior extracted the hostname from orchestrator_url (e.g., "aegis-runtime"),
                // but this fails for Docker deployments since the Docker Daemon on the host
                // cannot resolve internal container network names.
                "127.0.0.1"
            });

            let mounts: Vec<Mount> = config
                .volumes
                .iter()
                .map(|volume_mount| {
                    let container_path = volume_mount.mount_point.display().to_string();

                    // ADR-036: NFS Server Gateway configuration via local driver
                    // Mount options: addr={host},nfsvers=3,proto=tcp,soft,timeo=10,nolock
                    // - NFSv3 protocol (not v4, for simplicity)
                    // - nolock: No NLM support in Phase 1 (safe for single-agent-per-volume)
                    // - soft mount with 10-second timeout (fail gracefully on network issues)
                    // - TCP protocol for reliability

                    debug!(
                        "Configuring NFS mount: volume_id={}, path={}, mode={:?}, host={}",
                        volume_mount.volume_id,
                        container_path,
                        volume_mount.access_mode,
                        orchestrator_host
                    );

                    // Build NFS mount using local driver (standard Docker approach)
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
                        format!(":{}", volume_mount.remote_path), // remote_path contains /{tenant_id}/{volume_id}
                    );

                    Mount {
                        target: Some(container_path),
                        // Named volumes (source is set) support ReadOnly mode;
                        // anonymous volumes (source=None) do NOT — Docker returns
                        // HTTP 400 "must not set ReadOnly mode when using anonymous
                        // volumes".  Use a deterministic name derived from volume_id.
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

        // Removed container_config that was causing move issues and unused var warning

        let options = CreateContainerOptions {
            name: format!("aegis-agent-{}", uuid::Uuid::new_v4()),
            platform: None,
        };

        // Convert map to "KEY=VALUE" strings
        let mut env_vars: Vec<String> = config
            .env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
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
        let container_config = Config {
            image: Some(image.clone()),
            tty: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(cmd),
            env: Some(env_vars),
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

        self.keep_container_on_failure
            .write()
            .await
            .insert(id.clone(), config.keep_container_on_failure);

        self.docker
            .start_container(&id, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to start container: {}", e)))?;

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
            self.copy_bootstrap_to_container(&id).await?;
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
                        let content = String::from_utf8_lossy(&message).to_string();
                        debug!(
                            container_id = container_id,
                            stream = "stdout",
                            "Bootstrap output: {}",
                            content
                        );
                        stdout_logs.push(content);
                    }
                    Ok(LogOutput::StdErr { message }) => {
                        let content = String::from_utf8_lossy(&message).to_string();
                        warn!(
                            container_id = container_id,
                            stream = "stderr",
                            "Bootstrap stderr: {}",
                            content
                        );
                        stderr_logs.push(content);
                    }
                    _ => {}
                }
            }
        }

        // Get exit code from exec
        let exec_inspect =
            self.docker.inspect_exec(&exec.id).await.map_err(|e| {
                RuntimeError::ExecutionFailed(format!("Failed to inspect exec: {}", e))
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
                format!("Bootstrap script exited with code {}", exit_code)
            };
            Err(RuntimeError::ExecutionFailed(error_msg))
        } else {
            let result = serde_json::Value::String(stdout_logs.join(""));
            Ok(TaskOutput {
                result,
                logs: stderr_logs,
                tool_calls: vec![],
                exit_code,
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

        self.docker
            .remove_container(id.as_str(), Some(options))
            .await
            .map_err(|e| RuntimeError::TerminationFailed(e.to_string()))?;

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
            state: format!("{:?}", state),
            uptime_seconds: uptime,
            memory_usage_mb: mem,
            cpu_usage_percent: cpu,
        })
    }
}
// Private helper methods for DockerRuntime
impl DockerRuntime {
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
                RuntimeError::SpawnFailed(format!("Failed to check for bootstrap script: {}", e))
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
                RuntimeError::SpawnFailed(format!("Failed to inspect bootstrap check: {}", e))
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
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to set tar path: {}", e)))?;
        header.set_size(bootstrap_content.len() as u64);
        header.set_mode(0o755); // Make executable
        header.set_cksum();

        tar_builder
            .append(&header, bootstrap_content.as_slice())
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to create tar: {}", e)))?;

        let tar_data = tar_builder
            .into_inner()
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to finalize tar: {}", e)))?;

        // Upload to container at /usr/local/bin/
        let options = UploadToContainerOptions {
            path: "/usr/local/bin/",
            ..Default::default()
        };

        self.docker
            .upload_to_container(container_id, Some(options), tar_data.into())
            .await
            .map_err(|e| {
                RuntimeError::SpawnFailed(format!(
                    "Failed to copy bootstrap script to container: {}",
                    e
                ))
            })?;

        debug!(
            container_id = container_id,
            "Bootstrap script copied successfully"
        );
        Ok(())
    }
}
