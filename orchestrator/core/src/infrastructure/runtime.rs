// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::runtime::{
    AgentRuntime, InstanceId, TaskInput, TaskOutput, RuntimeError, InstanceStatus, RuntimeConfig
};
use async_trait::async_trait;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, StartContainerOptions, RemoveContainerOptions,
    LogOutput, UploadToContainerOptions
};
use bollard::image::CreateImageOptions;
use bollard::exec::{CreateExecOptions, StartExecResults, StartExecOptions};
use futures::StreamExt;
use tracing::{info, debug, warn};
use std::path::PathBuf;

pub struct DockerRuntime {
    docker: Docker,
    bootstrap_script_path: PathBuf,  // Store as PathBuf for better path handling
    network_mode: Option<String>,
    orchestrator_url: String,
}

impl DockerRuntime {
    pub fn new(bootstrap_script: String, socket_path: Option<String>, network_mode: Option<String>, orchestrator_url: String) -> Result<Self, RuntimeError> {
        // Resolve bootstrap script path to absolute path
        let bootstrap_path = if PathBuf::from(&bootstrap_script).is_absolute() {
            PathBuf::from(&bootstrap_script)
        } else {
            // Relative path - resolve from current directory
            std::env::current_dir()
                .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to get current directory: {}", e)))?
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
            
            result.map_err(|e| RuntimeError::SpawnFailed(format!(
                "Failed to connect to Docker at {}: {}\n\n\
                 Ensure Docker is running and the socket path is correct.",
                path, e
            )))?
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
        
        Ok(Self { 
            docker, 
            bootstrap_script_path: bootstrap_path,
            network_mode, 
            orchestrator_url 
        })
    }
    
    /// Verify Docker daemon is accessible
    pub async fn healthcheck(&self) -> Result<(), RuntimeError> {
        self.docker.ping().await
            .map_err(|e| RuntimeError::SpawnFailed(format!(
                "Cannot connect to Docker daemon: {}\n\n\
                 Docker healthcheck failed. Ensure Docker is running:\n\
                 - On Windows: Start Docker Desktop\n\
                 - On Linux: sudo systemctl start docker\n\
                 - On macOS: Start Docker Desktop\n\n\
                 Verify with: docker ps",
                e
            )))?;
        Ok(())
    }

    async fn get_container_stats(&self, _id: &str) -> Option<(f64, u64, u64)> {
        // TODO: Implement actual stats collection
        // For now return dummy values
        Some((0.0, 0, 0))
    }
}

#[async_trait]
impl AgentRuntime for DockerRuntime {
    async fn spawn(&self, config: RuntimeConfig) -> Result<InstanceId, RuntimeError> {
        // Validate isolation mode first
        config.validate_isolation()?;
        
        let image = config.to_image();
        
        // Check if image exists locally first
        let image_exists = self.docker.inspect_image(&image).await.is_ok();
        
        // Decide whether to pull
        let should_pull = if image_exists {
            false
        } else if config.autopull {
            info!("Image {} not found locally, will attempt to pull", image);
            true
        } else {
            false
        };

        if should_pull {
            info!("Pulling image: {}", image);
            let options = Some(CreateImageOptions {
                from_image: image.clone(),
                ..Default::default()
            });

            let mut stream = self.docker.create_image(options, None, None);
            while let Some(result) = stream.next().await {
                if let Err(e) = result {
                    let error_msg = format!(
                        "Failed to pull image {}: {}\n\n\
                         Common causes:\n\
                         - Docker daemon not responding (connection issue)\n\
                         - No internet connectivity to Docker Hub\n\
                         - Image name is incorrect or doesn't exist\n\
                         - Corporate proxy/firewall blocking Docker Hub\n\
                         - Registry authentication required\n\n\
                         Try manually: docker pull {}",
                        image, e, image
                    );
                    return Err(RuntimeError::SpawnFailed(error_msg));
                }
            }
            info!("Successfully pulled image: {}", image);
        } else if !image_exists && !config.autopull {
             return Err(RuntimeError::SpawnFailed(format!("Image {} not found locally and autopull is disabled", image)));
        }
        
        // Validate isolation mode first
        config.validate_isolation()?;

        let image = config.to_image();

        // Build host config with resource limits
        debug!("Preparing to copy bootstrap script into container");
        
        let mut host_config = bollard::service::HostConfig {
            binds: None,  // Will be set below if volumes are specified
            network_mode: self.network_mode.clone(),  // Optional Docker network (None = default)
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
        
        // Apply volume bind mounts if specified
        if !config.volumes.is_empty() {
            let binds: Vec<String> = config.volumes.iter().map(|mount| {
                // Derive host path from volume_id
                // Convention: /var/lib/aegis/storage/<volume-id>
                let host_path = format!("/var/lib/aegis/storage/{}", mount.volume_id);
                let container_path = mount.mount_point.display();
                let mode = match mount.access_mode {
                    crate::domain::volume::AccessMode::ReadOnly => "ro",
                    crate::domain::volume::AccessMode::ReadWrite => "rw",
                };
                
                let bind_spec = format!("{}:{}:{}", host_path, container_path, mode);
                debug!("Mounting volume: {}", bind_spec);
                bind_spec
            }).collect();
            
            host_config.binds = Some(binds);
            info!("Configured {} volume mount(s) for container", config.volumes.len());
        }

        // Removed container_config that was causing move issues and unused var warning

        let options = CreateContainerOptions {
            name: format!("aegis-agent-{}", uuid::Uuid::new_v4()),
            platform: None,
        };

        // Convert map to "KEY=VALUE" strings
        let mut env_vars: Vec<String> = config.env.iter()
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
        let cmd = vec!["tail".to_string(), "-f".to_string(), "/dev/null".to_string()];

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
        let res = self.docker.create_container(Some(options), container_config).await
            .map_err(|e| RuntimeError::SpawnFailed(e.to_string()))?;

        let id = res.id;

        self.docker.start_container(&id, None::<StartContainerOptions<String>>).await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to start container: {}", e)))?;

        info!("Spawned agent container: {}", id);
        
        // Copy bootstrap script into the container
        debug!(container_id = &id, "Copying bootstrap script into container");
        self.copy_bootstrap_to_container(&id).await?;
        
        Ok(InstanceId::new(id))
    }

    async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput, RuntimeError> {
        let container_id = id.as_str();

        debug!(
            container_id = container_id,
            prompt_len = input.prompt.len(),
            "Executing bootstrap script in container"
        );

        // simple exec for now - just echo the prompt
        // Execute via bootstrap script
        let exec_config = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            // Pass input as argument. Note: Shell escaping might be needed if input has special chars.
            // Using list form avoids shell: ["python", ...")
            cmd: Some(vec![
                "python".to_string(), 
                "/usr/local/bin/aegis-bootstrap".to_string(),
                input.prompt.clone()
            ]),
            // env: Some(vec!["PYTHONUNBUFFERED=1".to_string()]), // Commented out to ensure inheritance from container
            ..Default::default()
        };
        
        debug!(container_id = container_id, "Exec command: python /usr/local/bin/aegis-bootstrap <prompt>");

        let exec = self.docker.create_exec(container_id, exec_config).await
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

        let start_opts = StartExecOptions {
            detach: false,
            ..Default::default()
        };

        let res = self.docker.start_exec(&exec.id, Some(start_opts)).await
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

        let mut stdout_logs = Vec::new();
        let mut stderr_logs = Vec::new();
        
        debug!(container_id = container_id, "Starting bootstrap.py execution");
        
        match res {
            StartExecResults::Attached { mut output, .. } => {
                while let Some(msg) = output.next().await {
                    match msg {
                        Ok(LogOutput::StdOut { message }) => {
                            let content = String::from_utf8_lossy(&message).to_string();
                            debug!(container_id = container_id, stream = "stdout", "Bootstrap output: {}", content);
                            stdout_logs.push(content);
                        },
                        Ok(LogOutput::StdErr { message }) => {
                            let content = String::from_utf8_lossy(&message).to_string();
                            warn!(container_id = container_id, stream = "stderr", "Bootstrap stderr: {}", content);
                            stderr_logs.push(content);
                        },
                        _ => {}
                    }
                }
            },
            _ => {}
        }

        // Get exit code from exec
        let exec_inspect = self.docker.inspect_exec(&exec.id).await
            .map_err(|e| RuntimeError::ExecutionFailed(format!("Failed to inspect exec: {}", e)))?;
        
        let exit_code = exec_inspect.exit_code.unwrap_or(0);
        
        debug!(
            container_id = container_id,
            exit_code = exit_code,
            stdout_lines = stdout_logs.len(),
            stderr_lines = stderr_logs.len(),
            "Bootstrap execution completed"
        );

        let result = serde_json::Value::String(stdout_logs.join(""));
        
        Ok(TaskOutput {
            result,
            logs: stderr_logs,
            tool_calls: vec![],
            exit_code,
        })
    }

    async fn terminate(&self, id: &InstanceId) -> Result<(), RuntimeError> {
        let options = RemoveContainerOptions {
            force: true,
            ..Default::default()
        };

        self.docker.remove_container(id.as_str(), Some(options)).await
            .map_err(|e| RuntimeError::TerminationFailed(e.to_string()))?;
            
        info!("Terminated agent container: {}", id.as_str());
        Ok(())
    }

    async fn status(&self, id: &InstanceId) -> Result<InstanceStatus, RuntimeError> {
        let inspect = self.docker.inspect_container(id.as_str(), None).await
            .map_err(|e| RuntimeError::InstanceNotFound(e.to_string()))?;
            
        let state = inspect.state.and_then(|s| s.status).unwrap_or(bollard::models::ContainerStateStatusEnum::DEAD);
        
        let (cpu, mem, uptime) = self.get_container_stats(id.as_str()).await.unwrap_or((0.0, 0, 0));

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
        
        let check_exec = self.docker.create_exec(container_id, check_exists).await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to check for bootstrap script: {}", e)))?;
        
        let start_opts = StartExecOptions {
            detach: false,
            ..Default::default()
        };
        
        // Execute the test command
        let _ = self.docker.start_exec(&check_exec.id, Some(start_opts)).await;
        
        // Check exit code - 0 means file exists, non-zero means it doesn't
        let check_inspect = self.docker.inspect_exec(&check_exec.id).await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to inspect bootstrap check: {}", e)))?;
        
        let exit_code = check_inspect.exit_code.unwrap_or(1);
        
        if exit_code == 0 {
            debug!(container_id = container_id, "Bootstrap script already exists in container, skipping copy");
            return Ok(());
        }
        
        debug!(container_id = container_id, "Bootstrap script not found in container, copying from host");
        
        // Read bootstrap script content
        let bootstrap_content = std::fs::read(&self.bootstrap_script_path)
            .map_err(|e| RuntimeError::SpawnFailed(format!(
                "Failed to read bootstrap script at {}: {}",
                self.bootstrap_script_path.display(),
                e
            )))?;
        
        // Create a tar archive with the bootstrap script
        let mut tar_builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_path("aegis-bootstrap").map_err(|e| 
            RuntimeError::SpawnFailed(format!("Failed to set tar path: {}", e))
        )?;
        header.set_size(bootstrap_content.len() as u64);
        header.set_mode(0o755); // Make executable
        header.set_cksum();
        
        tar_builder.append(&header, bootstrap_content.as_slice())
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to create tar: {}", e)))?;
        
        let tar_data = tar_builder.into_inner()
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to finalize tar: {}", e)))?;
        
        // Upload to container at /usr/local/bin/
        let options = UploadToContainerOptions {
            path: "/usr/local/bin/",
            ..Default::default()
        };
        
        self.docker.upload_to_container(container_id, Some(options), tar_data.into())
            .await
            .map_err(|e| RuntimeError::SpawnFailed(format!(
                "Failed to copy bootstrap script to container: {}", e
            )))?;
        
        debug!(container_id = container_id, "Bootstrap script copied successfully");
        Ok(())
    }
}