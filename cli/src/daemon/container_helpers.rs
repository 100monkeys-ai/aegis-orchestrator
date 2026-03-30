// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Docker container reaping logic for orphaned agent containers.

use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};

use aegis_orchestrator_core::domain::execution::{ExecutionId, ExecutionStatus};
use aegis_orchestrator_core::domain::runtime::{AgentRuntime, InstanceId};
use aegis_orchestrator_core::infrastructure::runtime::{ContainerRuntime, ManagedAgentContainer};

pub(crate) fn managed_container_reap_reason(
    container: &ManagedAgentContainer,
    execution_status: Option<ExecutionStatus>,
) -> Option<&'static str> {
    if container.debug_retain {
        return None;
    }

    match execution_status {
        None => Some("missing_execution_record"),
        Some(ExecutionStatus::Running) if container.state.as_deref() == Some("running") => None,
        Some(ExecutionStatus::Running) => Some("container_not_running"),
        Some(_) => Some("execution_not_running"),
    }
}

pub(crate) async fn cleanup_orphaned_agent_containers(
    runtime: Arc<ContainerRuntime>,
    execution_repo: Arc<dyn aegis_orchestrator_core::domain::repository::ExecutionRepository>,
) -> Result<usize> {
    let containers = runtime.list_managed_agent_containers().await?;
    let mut reaped = 0usize;

    for container in containers {
        if container.debug_retain {
            continue;
        }

        let execution_status = match container.execution_id.as_deref() {
            Some(raw_execution_id) => match ExecutionId::from_string(raw_execution_id) {
                Ok(execution_id) => match execution_repo.find_by_id(execution_id).await {
                    Ok(Some(execution)) => Some(execution.status),
                    Ok(None) => None,
                    Err(error) => {
                        warn!(
                            container_id = %container.id,
                            execution_id = raw_execution_id,
                            error = %error,
                            "Failed to look up execution for managed container; skipping"
                        );
                        continue;
                    }
                },
                Err(error) => {
                    warn!(
                        container_id = %container.id,
                        execution_id = raw_execution_id,
                        error = %error,
                        "Managed container has invalid execution_id label; treating as orphan"
                    );
                    None
                }
            },
            None => None,
        };

        let Some(reason) = managed_container_reap_reason(&container, execution_status) else {
            continue;
        };

        let instance_id = InstanceId::new(container.id.clone());
        info!(
            container_id = %container.id,
            execution_id = container.execution_id.as_deref().unwrap_or("missing"),
            container_state = container.state.as_deref().unwrap_or("unknown"),
            reason,
            "Reaping orphaned managed agent container"
        );

        if let Err(error) = runtime.terminate(&instance_id).await {
            warn!(
                container_id = %container.id,
                error = %error,
                "Failed to reap orphaned managed agent container"
            );
            continue;
        }

        reaped += 1;
    }

    Ok(reaped)
}
