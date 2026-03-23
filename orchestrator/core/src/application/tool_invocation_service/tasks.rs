use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_task_execute_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let agent_ref = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.execute requires 'agent_id' string".to_string(),
                )
            })?;

        let input = args.get("input").cloned().unwrap_or(serde_json::json!({}));

        let agent_id = if let Ok(uuid) = uuid::Uuid::parse_str(agent_ref) {
            crate::domain::agent::AgentId(uuid)
        } else {
            match self.agent_lifecycle.lookup_agent(agent_ref).await {
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
                    intent: None,
                    payload: input,
                },
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
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.status requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
            )?);

        match self.execution_service.get_execution(exec_id).await {
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

    pub(super) async fn invoke_aegis_task_logs_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::InvalidArguments(
                    "aegis.task.logs requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SmcpSessionError::InvalidArguments(format!(
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

        let execution = match self.execution_service.get_execution(exec_id).await {
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
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
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
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.cancel requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
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
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.task.remove requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id =
            crate::domain::execution::ExecutionId(uuid::Uuid::parse_str(exec_id_str).map_err(
                |e| SmcpSessionError::SignatureVerificationFailed(format!("Invalid UUID: {e}")),
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
