// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::runtime::{
    AgentRuntime, InstanceId, TaskInput, TaskOutput, RuntimeError, InstanceStatus, RuntimeConfig
};
use async_trait::async_trait;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, StartContainerOptions, RemoveContainerOptions,
    LogOutput
};
use bollard::image::CreateImageOptions;
use bollard::exec::{CreateExecOptions, StartExecResults, StartExecOptions};
use futures::StreamExt;
use tracing::info;

pub struct DockerRuntime {
    docker: Docker,
    bootstrap_script: String,
    enable_disk_quotas: bool,
    network_mode: Option<String>,
    orchestrator_url: String,
}

impl DockerRuntime {
    pub fn new(bootstrap_script: String, socket_path: Option<String>, enable_disk_quotas: bool, network_mode: Option<String>, orchestrator_url: String) -> Result<Self, RuntimeError> {
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
        
        Ok(Self { docker, bootstrap_script, enable_disk_quotas, network_mode, orchestrator_url })
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
        let mut host_config = bollard::service::HostConfig {
            binds: Some(vec![
                format!("{}:/usr/local/bin/aegis-bootstrap", 
                    std::env::current_dir().unwrap().join(&self.bootstrap_script).to_str().unwrap()),
            ]),
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
        
        // Try to apply disk quotas if enabled and disk limit is specified
        // Disk quotas require: overlay2 storage driver + XFS with pquota mount option
        // Most systems (WSL, macOS, standard Linux) don't support this
        let mut disk_quota_requested = false;
        if self.enable_disk_quotas {
            if let Some(disk_bytes) = config.resources.disk_bytes {
                host_config.storage_opt = Some([("size".to_string(), disk_bytes.to_string())].into());
                disk_quota_requested = true;
            }
        } else if config.resources.disk_bytes.is_some() {
            // Disk quota requested but disabled in config
            tracing::debug!("Disk quota requested in agent manifest but disabled in node config (enable_disk_quotas=false)");
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

        // Keep container alive - actual agent execution happens via bootstrap script in execute()
        let cmd = vec!["tail".to_string(), "-f".to_string(), "/dev/null".to_string()];

        // Try to create container with disk quotas if requested
        let container_config = Config {
            image: Some(image.clone()),
            tty: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(cmd.clone()),
            env: Some(env_vars.clone()),
            host_config: Some(host_config.clone()),
            ..Default::default()
        };
        
        let res = match self.docker.create_container(Some(options.clone()), container_config).await {
            Ok(res) => res,
            Err(e) if disk_quota_requested && e.to_string().contains("storage-opt") => {
                // Disk quotas not supported on this system - retry without them
                tracing::warn!(
                    "Disk quotas requested but not supported by Docker on this system. \
                     Error: {}. Continuing without disk limits. \
                     (Requires overlay2 storage driver + XFS with pquota mount option)",
                    e
                );
                
                // Retry without storage_opt
                host_config.storage_opt = None;
                let retry_config = Config {
                    image: Some(image.clone()),
                    tty: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(cmd),
                    env: Some(env_vars),
                    host_config: Some(host_config),
                    ..Default::default()
                };
                
                self.docker.create_container(Some(options), retry_config).await
                    .map_err(|e| RuntimeError::SpawnFailed(e.to_string()))?
            }
            Err(e) => return Err(RuntimeError::SpawnFailed(e.to_string())),
        };

        let id = res.id;

        self.docker.start_container(&id, None::<StartContainerOptions<String>>).await
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to start container: {}", e)))?;

        info!("Spawned agent container: {}", id);
        Ok(InstanceId::new(id))
    }

    async fn execute(&self, id: &InstanceId, input: TaskInput) -> Result<TaskOutput, RuntimeError> {
        let container_id = id.as_str();

        // simple exec for now - just echo the prompt
        // Execute via bootstrap script
        let exec_config = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            // Pass input as argument. Note: Shell escaping might be needed if input has special chars.
            // Using list form avoids shell: ["python", ...]
            cmd: Some(vec![
                "python".to_string(), 
                "/usr/local/bin/aegis-bootstrap".to_string(),
                input.prompt.clone()
            ]),
            // env: Some(vec!["PYTHONUNBUFFERED=1".to_string()]), // Commented out to ensure inheritance from container
            ..Default::default()
        };

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
        
        match res {
            StartExecResults::Attached { mut output, .. } => {
                while let Some(msg) = output.next().await {
                    match msg {
                        Ok(LogOutput::StdOut { message }) => {
                            stdout_logs.push(String::from_utf8_lossy(&message).to_string());
                        },
                        Ok(LogOutput::StdErr { message }) => {
                            stderr_logs.push(String::from_utf8_lossy(&message).to_string());
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
