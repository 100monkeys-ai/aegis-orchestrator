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
}

impl DockerRuntime {
    pub fn new(bootstrap_script: String) -> Result<Self, RuntimeError> {
        // Increase timeout for image pulls (5 minutes)
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to connect to Docker: {}", e)))?;
        Ok(Self { docker, bootstrap_script })
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
                    // If image exists but inspect failed for some weird reason, we might still want to try creating
                    // but here we already know it doesn't exist or we want to pull.
                    return Err(RuntimeError::SpawnFailed(format!("Failed to pull image {}: {}", image, e)));
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
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
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
        if let Some(disk_bytes) = config.resources.disk_bytes {
            // Storage limits via storage_opt (requires overlay2 driver)
            host_config.storage_opt = Some([("size".to_string(), disk_bytes.to_string())].into());
        }

        // Removed container_config that was causing move issues and unused var warning

        let options = CreateContainerOptions {
            name: format!("aegis-agent-{}", uuid::Uuid::new_v4()),
            platform: None,
        };

        // Convert map to "KEY=VALUE" strings
        let env_vars: Vec<String> = config.env.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();

        // Determine command - use entrypoint if specified, otherwise default tail
        let cmd = if let Some(entrypoint) = &config.entrypoint {
            // If custom entrypoint specified, use it as the command
            vec![entrypoint.clone()]
        } else {
            // Default: keep container alive
            vec!["tail".to_string(), "-f".to_string(), "/dev/null".to_string()]
        };

        let res = self.docker.create_container(Some(options), Config {
            image: Some(image.clone()), // Clone image
            tty: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(cmd),
            env: Some(env_vars),
            host_config: Some(host_config),
            ..Default::default()
        }).await
            .map_err(|e| RuntimeError::SpawnFailed(e.to_string()))?;

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
