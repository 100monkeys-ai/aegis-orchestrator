use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_task_execute_tool(
        &self,
        args: &Value,
        _security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let agent_ref = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.task.execute requires 'agent_id' string".to_string(),
                )
            })?;

        let mut input = args.get("input").cloned().unwrap_or(serde_json::json!({}));
        let intent = args
            .get("intent")
            .and_then(|v| v.as_str())
            .map(String::from);
        let version = args.get("version").and_then(|v| v.as_str());

        // Resolve and inject the caller's tenant_id into the payload so that
        // start_execution (and any cluster forwarding) picks up the correct tenant.
        let tenant_id = Self::resolve_tenant_arg(args)?;
        if let Some(map) = input.as_object_mut() {
            map.entry("tenant_id")
                .or_insert_with(|| serde_json::Value::String(tenant_id.to_string()));
        }

        let agent_id = if let Ok(uuid) = uuid::Uuid::parse_str(agent_ref) {
            if version.is_some() {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.execute",
                    "error": "version parameter is only supported when identifying agents by name, not UUID"
                })));
            }
            crate::domain::agent::AgentId(uuid)
        } else if let Some(ver) = version {
            match self
                .agent_lifecycle
                .lookup_agent_for_tenant_with_version(&tenant_id, agent_ref, ver)
                .await
            {
                Ok(Some(id)) => id,
                Ok(None) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.task.execute",
                        "error": format!("Agent '{agent_ref}' version '{ver}' not found")
                    })));
                }
                Err(e) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.task.execute",
                        "error": format!("Agent '{agent_ref}' version '{ver}' not found: {e}")
                    })));
                }
            }
        } else {
            match self
                .agent_lifecycle
                .lookup_agent_visible_for_tenant(&tenant_id, agent_ref)
                .await
            {
                Ok(Some(id)) => id,
                _ => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.task.execute",
                        "error": format!("Agent '{agent_ref}' not found")
                    })));
                }
            }
        };

        match self
            .execution_service
            .start_execution(
                agent_id,
                crate::domain::execution::ExecutionInput {
                    intent,
                    input,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                },
                "aegis-system-agent-runtime".to_string(),
                None,
            )
            .await
        {
            Ok(exec_id) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.execute",
                "execution_id": exec_id.to_string(),
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.execute",
                "error": format!("Failed to start task execution: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_task_status_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.task.status requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SealSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.get_execution_unscoped(exec_id).await {
            Ok(exec) => {
                let last_iter = exec.iterations().last();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.status",
                    "execution_id": exec_id_str,
                    "agent_id": exec.agent_id.0.to_string(),
                    "status": format!("{:?}", exec.status).to_lowercase(),
                    "started_at": exec.started_at,
                    "ended_at": exec.ended_at,
                    "iteration_count": exec.iterations().len(),
                    "last_output": last_iter.and_then(|i| i.output.as_ref()),
                    "last_error": last_iter.and_then(|i| i.error.as_ref().map(|e| format!("{e:?}")))
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.status",
                "error": format!("Failed to get execution: {e}")
            }))),
        }
    }

    /// Blocking poll tool — waits for an execution to reach a terminal state.
    /// Polls every `poll_interval_seconds` (default 10s) up to `timeout_seconds` (default 300s).
    /// Returns the final execution status, output, and error (if any).
    pub(super) async fn invoke_aegis_task_wait_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.task.wait requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str)
                .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid UUID: {e}")))?,
        );

        let poll_interval = args
            .get("poll_interval_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(10);
        let timeout = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(600);

        let poll_duration = std::time::Duration::from_secs(poll_interval);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);

        loop {
            match self.execution_service.get_execution_unscoped(exec_id).await {
                Ok(exec) => {
                    let status_str = format!("{:?}", exec.status).to_lowercase();
                    let is_terminal = matches!(
                        exec.status,
                        crate::domain::execution::ExecutionStatus::Completed
                            | crate::domain::execution::ExecutionStatus::Failed
                            | crate::domain::execution::ExecutionStatus::Cancelled
                    );

                    if is_terminal {
                        let last_iter = exec.iterations().last();
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.task.wait",
                            "execution_id": exec_id_str,
                            "agent_id": exec.agent_id.0.to_string(),
                            "status": status_str,
                            "started_at": exec.started_at,
                            "ended_at": exec.ended_at,
                            "iteration_count": exec.iterations().len(),
                            "last_output": last_iter.and_then(|i| i.output.as_ref()),
                            "last_error": last_iter.and_then(|i| i.error.as_ref().map(|e| format!("{e:?}")))
                        })));
                    }

                    if std::time::Instant::now() >= deadline {
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.task.wait",
                            "execution_id": exec_id_str,
                            "status": status_str,
                            "timed_out": true,
                            "message": format!("Execution still {} after {}s timeout", status_str, timeout),
                            "iteration_count": exec.iterations().len()
                        })));
                    }

                    tokio::time::sleep(poll_duration).await;
                }
                Err(e) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.task.wait",
                        "execution_id": exec_id_str,
                        "error": format!("Failed to get execution: {e}")
                    })));
                }
            }
        }
    }

    pub(super) async fn invoke_aegis_task_logs_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.task.logs requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.task.logs: invalid execution_id UUID: {e}"
                ))
            })?,
        );

        let limit: usize = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(50);
        let offset: usize = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);

        let execution = match self.execution_service.get_execution_unscoped(exec_id).await {
            Ok(execution) => execution,
            Err(error) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.logs",
                    "error": format!("Failed to fetch execution: {error}")
                })));
            }
        };

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo.clone(),
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.logs",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let events = match repo.find_events_by_execution(exec_id, limit, offset).await {
            Ok(events) => events,
            Err(error) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.logs",
                    "error": format!("Failed to fetch execution events: {error}")
                })));
            }
        };

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.task.logs",
            "execution_id": exec_id_str,
            "agent_id": execution.agent_id.0.to_string(),
            "status": format!("{:?}", execution.status).to_lowercase(),
            "events": events,
            "total": events.len(),
            "limit": limit,
            "offset": offset,
        })))
    }

    pub(super) async fn invoke_aegis_task_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(crate::domain::agent::AgentId);

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

        match self
            .execution_service
            .list_executions(agent_id, limit)
            .await
        {
            Ok(executions) => {
                let entries: Vec<serde_json::Value> = executions
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "id": e.id.0.to_string(),
                            "agent_id": e.agent_id.0.to_string(),
                            "status": format!("{:?}", e.status).to_lowercase(),
                            "started_at": e.started_at
                        })
                    })
                    .collect();

                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.list",
                    "count": entries.len(),
                    "executions": entries
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.list",
                "error": format!("Failed to list executions: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_task_cancel_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.task.cancel requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SealSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.cancel_execution(exec_id).await {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.cancel",
                "cancelled": true,
                "execution_id": exec_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.cancel",
                "cancelled": false,
                "error": format!("Failed to cancel execution: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_task_remove_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::SignatureVerificationFailed(
                    "aegis.task.remove requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SealSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.delete_execution(exec_id).await {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.remove",
                "removed": true,
                "execution_id": exec_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.remove",
                "removed": false,
                "error": format!("Failed to remove execution: {e}")
            }))),
        }
    }
}
