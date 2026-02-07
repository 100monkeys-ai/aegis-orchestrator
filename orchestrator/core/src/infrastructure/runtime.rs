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
}

impl DockerRuntime {
    pub fn new() -> Result<Self, RuntimeError> {
        let docker = Docker::connect_with_local_defaults()
            .map_err(|e| RuntimeError::SpawnFailed(format!("Failed to connect to Docker: {}", e)))?;
        Ok(Self { docker })
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
        let image = config.image.clone();
        
        // Check autopull configuration
        let should_pull = if config.autopull {
            // If autopull is true, we verify existence and pull if missing.
            // (We could also support "always" pull here if we had a policy enum, but bool usually means "ensure exists")
            self.docker.inspect_image(&image).await.is_err()
        } else {
            // If autopull is false, we assume it exists. 
            // If it doesn't, create_container will fail later, which is expected.
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
                    return Err(RuntimeError::SpawnFailed(format!("Failed to pull image {}: {}", image, e)));
                }
            }
            info!("Successfully pulled image: {}", image);
        }
        


        let host_config = bollard::service::HostConfig {
            binds: Some(vec![
                format!("{}:/usr/local/bin/aegis-bootstrap", std::env::current_dir().unwrap().join("assets/bootstrap.py").to_str().unwrap()),
            ]),
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
            ..Default::default()
        };

        // Removed container_config that was causing move issues and unused var warning

        let options = CreateContainerOptions {
            name: format!("aegis-agent-{}", uuid::Uuid::new_v4()),
            platform: None,
        };

        let res = self.docker.create_container(Some(options), Config {
            image: Some(image.clone()), // Clone image
            tty: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(vec!["tail".to_string(), "-f".to_string(), "/dev/null".to_string()]),
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
            env: Some(vec![
                // Ensure PYTHONPATH or other envs if needed
                "PYTHONUNBUFFERED=1".to_string(),
                // We should pass execution ID if we had it in TaskInput? 
                // Currently TaskInput doesn't have it.
                // But we can add it or ignore for now.
            ]),
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

        let result = serde_json::Value::String(stdout_logs.join(""));
        
        Ok(TaskOutput {
            result,
            logs: stderr_logs,
            tool_calls: vec![],
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
