use super::attachment_args::parse_attachments;
use super::*;

impl ToolInvocationService {
    /// Whether this read reaches every tenant: an active operator
    /// escalation and no `tenant_id` named (AEGIS ADR-129 D17). A named
    /// tenant is read as that tenant, through `enforce_tenant_arg`.
    pub(super) fn reads_every_tenant(
        args: &Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> bool {
        scope.is_escalated() && args.get("tenant_id").is_none() && args.get("tenant").is_none()
    }

    pub(super) async fn invoke_aegis_task_execute_tool(
        &self,
        args: &mut Value,
        _security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let agent_ref = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.task.execute requires 'agent_id' string".to_string(),
                )
            })?;

        // AEGIS ADR-131 U6: the goal this execution is started for, checked
        // before anything starts.
        let goal_id = match self
            .goal_for_start("aegis.task.execute", args, caller_identity, _scope)
            .await?
        {
            Ok(goal_id) => goal_id,
            Err(refused) => return Ok(refused),
        };

        let mut input = args.get("input").cloned().unwrap_or(serde_json::json!({}));
        let intent = args
            .get("intent")
            .and_then(|v| v.as_str())
            .map(String::from);
        let version = args
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // ADR-113: parse attachments from the SEAL JSON-RPC tool call args so
        // they reach `ExecutionInput.attachments` and get merged into
        // `input.attachments` by `prepare_execution_input` downstream.
        let attachments = parse_attachments(args)?;
        // Zaru ADR-0055 D14: the dispatch's binding choices ride in the
        // input's reserved key `contexts`.
        super::context_args::carry_contexts(args, &mut input)?;
        super::repository_args::carry_repositories(args, &mut input)?;
        // AEGIS ADR-126, Update of 2026-10-07 (2), clauses 3 and 3a: the
        // conversation the run was started from, only as the facade wrote it.
        super::context_args::carry_conversation(args, &mut input);

        // Resolve and inject the caller's tenant_id into the payload so that
        // start_execution (and any cluster forwarding) picks up the correct tenant.
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        if let Some(map) = input.as_object_mut() {
            map.entry("tenant_id")
                .or_insert_with(|| serde_json::Value::String(tenant_id.to_string()));
        }

        let agent_id = if let Ok(uuid) = uuid::Uuid::parse_str(&agent_ref) {
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
                .lookup_agent_for_tenant_with_version(&tenant_id, &agent_ref, &ver)
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
                .lookup_agent_visible_for_tenant(&tenant_id, &agent_ref)
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
                    workflow_execution_id: None,
                    attachments,
                },
                "aegis-system-agent-runtime".to_string(),
                caller_identity,
            )
            .await
        {
            Ok(exec_id) => {
                if let Some(closed) = self
                    .bind_to_goal(
                        goal_id,
                        &exec_id.to_string(),
                        crate::domain::goal::BoundKind::Agent,
                    )
                    .await
                {
                    return Ok(ToolInvocationResult::Direct(closed));
                }
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.execute",
                    "execution_id": exec_id.to_string(),
                    "status": "started"
                })))
            }
            Err(e) => {
                // AEGIS ADR-005 O6: a refused start answers the refusal itself.
                let error = match e.downcast_ref::<crate::domain::execution::ExecutionError>() {
                    Some(refused @ crate::domain::execution::ExecutionError::Refused(_)) => {
                        refused.to_string()
                    }
                    _ => format!("Failed to start task execution: {e}"),
                };
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.execute",
                    "error": error
                })))
            }
        }
    }

    pub(super) async fn invoke_aegis_task_status_tool(
        &self,
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution data is touched. Without this gate any
        // caller could read any tenant's execution by guessing its UUID.
        let all_tenants = Self::reads_every_tenant(args, scope);
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.task.status requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str)
                .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid UUID: {e}")))?,
        );

        let fetched = if all_tenants {
            // AEGIS ADR-129 D17: under an active escalation, any tenant's
            // execution by its id, as the REST route reads it for an operator.
            self.execution_service.get_execution_unscoped(exec_id).await
        } else {
            self.execution_service
                .get_execution_for_tenant(&tenant_id, exec_id)
                .await
        };
        match fetched {
            Ok(exec) => {
                let last_iter = exec.iterations().last();
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.status",
                    "execution_id": exec_id_str,
                    "agent_id": exec.agent_id.0.to_string(),
                    "tenant_id": exec.tenant_id.as_str(),
                    "status": format!("{:?}", exec.status).to_lowercase(),
                    "started_at": exec.started_at,
                    "ended_at": exec.ended_at,
                    "iteration_count": exec.iterations().len(),
                    "last_output": last_iter.and_then(|i| i.output.as_ref()),
                    "produced_files": exec.produced_files(),
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
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution data is touched. Without this gate any
        // caller could poll any tenant's execution by guessing its UUID.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

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
            match self
                .execution_service
                .get_execution_for_tenant(&tenant_id, exec_id)
                .await
            {
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
                            "produced_files": exec.produced_files(),
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
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution data is touched. Without this gate any
        // caller could read any tenant's execution events by guessing its UUID.
        let all_tenants = Self::reads_every_tenant(args, scope);
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

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

        // The tenant gate is enforced here: get_execution_for_tenant returns
        // an error when the execution doesn't belong to `tenant_id`, so a
        // foreign-tenant UUID cannot reach the unscoped events query below.
        let fetched = if all_tenants {
            // AEGIS ADR-129 D17: under an active escalation, any tenant's.
            self.execution_service.get_execution_unscoped(exec_id).await
        } else {
            self.execution_service
                .get_execution_for_tenant(&tenant_id, exec_id)
                .await
        };
        let execution = match fetched {
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
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope and thread it through the repository query. Prior to this
        // fix the captured tenant was discarded and the handler invoked the
        // unscoped `list_executions`, which routed to the global
        // `TenantId::consumer()` singleton — leaking every consumer-tier
        // execution across all callers.
        let all_tenants = Self::reads_every_tenant(args, scope);
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(crate::domain::agent::AgentId);

        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

        let listed = if all_tenants && agent_id.is_none() {
            // AEGIS ADR-129 D17: under an active escalation and without an
            // `agent_id`, the most recent executions of every tenant, each
            // carrying its `tenant_id`, as the REST route lists them for an
            // operator (`list_recent_all_paginated`).
            match &self.execution_repository {
                Some(repo) => repo
                    .list_recent_all_paginated(limit, 0)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}")),
                None => Err(anyhow::anyhow!(
                    "the execution store is not configured for the all-tenant list"
                )),
            }
        } else {
            self.execution_service
                .list_executions_for_tenant(&tenant_id, agent_id, None, limit)
                .await
        };
        match listed {
            Ok(executions) => {
                let entries: Vec<serde_json::Value> = executions
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "id": e.id.0.to_string(),
                            "agent_id": e.agent_id.0.to_string(),
                            "tenant_id": e.tenant_id.as_str(),
                            "status": format!("{:?}", e.status).to_lowercase(),
                            "started_at": e.started_at,
                            "ended_at": e.ended_at,
                            "iteration_count": e.iterations().len(),
                            "summary": super::summary::summarize_intent(&e.input.intent),
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
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution data is touched. Without this gate any
        // caller could cancel any tenant's execution by guessing its UUID.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.task.cancel requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str)
                .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid UUID: {e}")))?,
        );

        // AEGIS ADR-131 U33a: a cancel of an execution bound to an open goal
        // closes the goal `cancelled` before this call answers, so no round
        // re-dispatches the work the person stopped.
        match crate::application::goal_service::cancel_ending_its_goal(
            self.goal_service.as_deref(),
            &tenant_id,
            exec_id,
            self.execution_service
                .cancel_execution_for_tenant(&tenant_id, exec_id),
        )
        .await
        {
            // A cancel of an execution that had already ended leaves it as it
            // was; the answer carries the state the execution is in after the
            // cancel, so the caller sees "failed" (with the reason in
            // aegis.task.status) rather than a cancellation that did not happen.
            Ok(_) => match self
                .execution_service
                .get_execution_for_tenant(&tenant_id, exec_id)
                .await
            {
                Ok(exec) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.cancel",
                    "cancelled": exec.status == crate::domain::execution::ExecutionStatus::Cancelled,
                    "status": format!("{:?}", exec.status).to_lowercase(),
                    "execution_id": exec_id_str
                }))),
                Err(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.cancel",
                    "cancelled": true,
                    "execution_id": exec_id_str
                }))),
            },
            // C3, finding 4: the execution was cancelled but its goal is
            // still open; the answer says so, naming the goal.
            Err(e @ crate::application::goal_service::CancelPathError::GoalStillOpen { .. }) => {
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.task.cancel",
                    "cancelled": true,
                    "execution_id": exec_id_str,
                    "goal_still_open": e.goal_still_open().map(|g| g.to_string()),
                    "error": e.to_string()
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.task.cancel",
                "cancelled": false,
                "error": format!("Failed to cancel execution: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_task_remove_tool(
        &self,
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution data is touched. Without this gate any
        // caller could delete any tenant's execution by guessing its UUID.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.task.remove requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str)
                .map_err(|e| SealSessionError::InvalidArguments(format!("Invalid UUID: {e}")))?,
        );

        match self
            .execution_service
            .delete_execution_for_tenant(&tenant_id, exec_id)
            .await
        {
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
