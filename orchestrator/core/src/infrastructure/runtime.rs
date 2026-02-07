use crate::domain::runtime::{
    AgentRuntime, InstanceId, TaskInput, TaskOutput, RuntimeError, InstanceStatus, RuntimeConfig
};
use async_trait::async_trait;
use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, StartContainerOptions, RemoveContainerOptions,
    LogOutput
};
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
        
        // Ensure image exists (pull if needed)
        // TODO: Add pull logic
        
        let container_config = Config {
            image: Some(image),
            tty: Some(true), // Keep it alive
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(vec!["tail".to_string(), "-f".to_string(), "/dev/null".to_string()]), // Keep alive command
            ..Default::default()
        };

        let options = CreateContainerOptions {
            name: format!("aegis-agent-{}", uuid::Uuid::new_v4()),
            platform: None,
        };

        let res = self.docker.create_container(Some(options), container_config).await
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
        let exec_config = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            cmd: Some(vec!["echo".to_string(), input.prompt.clone()]),
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

        let mut logs = Vec::new();
        match res {
            StartExecResults::Attached { mut output, .. } => {
                while let Some(msg) = output.next().await {
                    match msg {
                        Ok(LogOutput::StdOut { message }) => {
                            logs.push(String::from_utf8_lossy(&message).to_string());
                        },
                        Ok(LogOutput::StdErr { message }) => {
                            logs.push(String::from_utf8_lossy(&message).to_string());
                        },
                        _ => {}
                    }
                }
            },
            _ => {}
        }

        // Mock output for now since we rely on the agent process to actually run
        let result = serde_json::json!({
            "status": "success",
            "executed_prompt": input.prompt
        });

        Ok(TaskOutput {
            result,
            logs,
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
