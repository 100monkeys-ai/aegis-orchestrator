use super::*;

impl ToolInvocationService {
    pub(super) async fn invoke_aegis_workflow_delete_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.delete requires 'name' string".to_string(),
            )
        })?;

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.delete",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        // Find by name or ID
        let workflow_id = match repo.find_by_name_for_tenant(&tenant_id, name).await {
            Ok(Some(w)) => w.id,
            _ => {
                if let Ok(uuid) = uuid::Uuid::parse_str(name) {
                    crate::domain::workflow::WorkflowId(uuid)
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.delete",
                        "error": "Workflow not found"
                    })));
                }
            }
        };

        match repo.delete_for_tenant(&tenant_id, workflow_id).await {
            Ok(_) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.delete",
                "deleted": true,
                "workflow_id": workflow_id.0.to_string()
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.delete",
                "deleted": false,
                "error": format!("Failed to delete workflow: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_validate_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.validate requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(workflow) => workflow,
            Err(error) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.validate",
                    "valid": false,
                    "deterministic_validation": {
                        "passed": false,
                        "error": format!("Workflow parser validation failed: {error}"),
                    },
                })));
            }
        };

        if let Err(error) = WorkflowValidator::check_for_cycles(&workflow) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.validate",
                "valid": false,
                "deterministic_validation": {
                    "passed": false,
                    "error": format!("Workflow cycle validation failed: {error}"),
                },
            })));
        }

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.validate",
            "valid": true,
            "deterministic_validation": { "passed": true },
            "workflow": {
                "workflow_id": workflow.id.to_string(),
                "name": workflow.metadata.name,
                "version": workflow.metadata.version.clone().unwrap_or_default(),
                "state_count": workflow.spec.states.len(),
                "initial_state": workflow.spec.initial_state.as_str(),
            },
        })))
    }

    pub(super) async fn invoke_aegis_workflow_run_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.run requires 'name' string".to_string(),
            )
        })?;

        let input = args.get("input").cloned().unwrap_or(serde_json::json!({}));
        let blackboard = args.get("blackboard").cloned();

        let start_use_case = match &self.start_workflow_execution_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.run",
                    "error": "Workflow execution service not configured"
                })));
            }
        };

        match start_use_case
            .start_execution(
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: name.to_string(),
                    input,
                    blackboard,
                    tenant_id: Some(tenant_id.clone()),
                },
            )
            .await
        {
            Ok(started) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.run",
                "execution_id": started.workflow_id,
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.run",
                "error": format!("Failed to start workflow: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_execution_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let limit: usize = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(200))
            .unwrap_or(20);
        let offset: usize = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.list",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let executions = if let Some(workflow_ref) =
            args.get("workflow_id").and_then(|v| v.as_str())
        {
            let workflow_id = if let Ok(uuid) = uuid::Uuid::parse_str(workflow_ref) {
                crate::domain::workflow::WorkflowId(uuid)
            } else if let Some(workflow_repo) = &self.workflow_repository {
                match workflow_repo
                    .find_by_name_for_tenant(&tenant_id, workflow_ref)
                    .await
                {
                    Ok(Some(workflow)) => workflow.id,
                    Ok(None) => {
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.workflow.executions.list",
                            "error": format!("Workflow '{workflow_ref}' not found")
                        })));
                    }
                    Err(error) => {
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.workflow.executions.list",
                            "error": format!("Failed to resolve workflow '{workflow_ref}': {error}")
                        })));
                    }
                }
            } else {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.list",
                    "error": format!("workflow_id '{workflow_ref}' is not a UUID and workflow repository is not configured")
                })));
            };

            repo.find_by_workflow_for_tenant(&tenant_id, workflow_id, limit, offset)
                .await
        } else {
            repo.list_paginated_for_tenant(&tenant_id, limit, offset)
                .await
        };

        match executions {
            Ok(executions) => {
                let items: Vec<Value> = executions
                    .into_iter()
                    .map(|execution| {
                        serde_json::json!({
                            "execution_id": execution.id.to_string(),
                            "workflow_id": execution.workflow_id.to_string(),
                            "status": format!("{:?}", execution.status).to_lowercase(),
                            "current_state": execution.current_state.as_str(),
                            "started_at": execution.started_at,
                            "last_transition_at": execution.last_transition_at,
                        })
                    })
                    .collect();

                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.list",
                    "count": items.len(),
                    "executions": items,
                    "limit": limit,
                    "offset": offset,
                })))
            }
            Err(error) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.list",
                "error": format!("Failed to list workflow executions: {error}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_execution_get_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let execution_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::InvalidArguments(
                    "aegis.workflow.executions.get requires 'execution_id' string".to_string(),
                )
            })?;
        let execution_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(execution_id_str).map_err(|error| {
                SmcpSessionError::InvalidArguments(format!(
                    "aegis.workflow.executions.get: invalid execution_id UUID: {error}"
                ))
            })?,
        );

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.executions.get",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        match repo.find_by_id_for_tenant(&tenant_id, execution_id).await {
            Ok(Some(execution)) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.get",
                "execution": {
                    "execution_id": execution.id.to_string(),
                    "workflow_id": execution.workflow_id.to_string(),
                    "status": format!("{:?}", execution.status).to_lowercase(),
                    "current_state": execution.current_state.as_str(),
                    "input": execution.input,
                    "blackboard": execution.blackboard.data(),
                    "state_outputs": execution.state_outputs,
                    "started_at": execution.started_at,
                    "last_transition_at": execution.last_transition_at,
                }
            }))),
            Ok(None) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.get",
                "error": format!("Workflow execution '{execution_id_str}' not found")
            }))),
            Err(error) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.executions.get",
                "error": format!("Failed to fetch workflow execution: {error}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_status_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let result = self.invoke_aegis_workflow_execution_get_tool(args).await?;
        Ok(match result {
            ToolInvocationResult::Direct(payload) => {
                let mut payload = payload;
                if let Some(tool) = payload.get_mut("tool") {
                    *tool = serde_json::Value::String("aegis.workflow.status".to_string());
                }
                ToolInvocationResult::Direct(payload)
            }
            ToolInvocationResult::DispatchRequired(action) => {
                ToolInvocationResult::DispatchRequired(action)
            }
        })
    }

    pub(super) async fn invoke_aegis_workflow_generate_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let input = args.get("input").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.generate requires 'input' string".to_string(),
            )
        })?;

        let start_use_case = match &self.start_workflow_execution_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.generate",
                    "error": "Workflow execution service not configured"
                })));
            }
        };

        match start_use_case
            .start_execution(
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: "builtin-workflow-generator".to_string(),
                    input: serde_json::json!({ "input": input }),
                    blackboard: None,
                    tenant_id: Some(tenant_id),
                },
            )
            .await
        {
            Ok(started) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.generate",
                "execution_id": started.workflow_id,
                "status": "started"
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.generate",
                "error": format!("Failed to start workflow generation: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_logs_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::InvalidArguments(
                    "aegis.workflow.logs requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SmcpSessionError::InvalidArguments(format!(
                    "aegis.workflow.logs: invalid execution_id UUID: {e}"
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

        let repo = match &self.workflow_execution_repo {
            Some(r) => r.clone(),
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let tenant_id = Self::resolve_tenant_arg(args)?;
        let execution = match repo.find_by_id_for_tenant(&tenant_id, exec_id).await {
            Ok(Some(e)) => e,
            Ok(None) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Workflow execution '{exec_id_str}' not found")
                })));
            }
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Failed to fetch execution: {e}")
                })));
            }
        };

        let events = match repo.find_events_by_execution(exec_id, limit, offset).await {
            Ok(evts) => evts,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Failed to fetch execution events: {e}")
                })));
            }
        };

        let total = events.len();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.logs",
            "execution_id": exec_id_str,
            "status": format!("{:?}", execution.status).to_lowercase(),
            "workflow_id": execution.workflow_id.to_string(),
            "events": events,
            "total": total,
            "limit": limit,
            "offset": offset,
        })))
    }

    pub(super) async fn invoke_aegis_workflow_list_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.list",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let workflows = repo.list_all_for_tenant(&tenant_id).await.map_err(|e| {
            SmcpSessionError::SignatureVerificationFailed(format!("Failed to list workflows: {e}"))
        })?;

        let entries: Vec<serde_json::Value> = workflows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id.0.to_string(),
                    "name": w.metadata.name,
                    "version": w.metadata.version.clone().unwrap_or_default(),
                    "initial_state": w.spec.initial_state,
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.list",
            "count": entries.len(),
            "workflows": entries
        })))
    }

    pub(super) async fn invoke_aegis_workflow_export_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "aegis.workflow.export requires 'name' string".to_string(),
            )
        })?;

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.export",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let workflow = match repo.find_by_name_for_tenant(&tenant_id, name).await {
            Ok(Some(w)) => w,
            _ => {
                if let Ok(uuid) = uuid::Uuid::parse_str(name) {
                    match repo
                        .find_by_id_for_tenant(
                            &tenant_id,
                            crate::domain::workflow::WorkflowId(uuid),
                        )
                        .await
                    {
                        Ok(Some(w)) => w,
                        _ => {
                            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                                "tool": "aegis.workflow.export",
                                "error": "Workflow not found"
                            })));
                        }
                    }
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.export",
                        "error": "Workflow not found or invalid UUID"
                    })));
                }
            }
        };

        let yaml = serde_yaml::to_string(&workflow).unwrap_or_default();
        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.export",
            "name": workflow.metadata.name,
            "manifest_yaml": yaml
        })))
    }

    pub(super) async fn invoke_aegis_workflow_update_tool(
        &self,
        args: &Value,
        _execution_id: crate::domain::execution::ExecutionId,
        _agent_id: AgentId,
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let tenant_id = Self::resolve_tenant_arg(args)?;
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.update requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "updated": false,
                    "deterministic_validation": {
                        "passed": false,
                        "error": format!("Workflow parser validation failed: {}", e),
                    },
                    "error": format!("Workflow parser validation failed: {}", e),
                })));
            }
        };

        if let Err(e) = WorkflowValidator::check_for_cycles(&workflow) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.update",
                "updated": false,
                "deterministic_validation": {
                    "passed": false,
                    "error": format!("Workflow cycle validation failed: {}", e),
                },
                "error": format!("Workflow cycle validation failed: {}", e),
            })));
        }

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "deterministic_validation": { "passed": true },
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let _existing = match repo
            .find_by_name_for_tenant(&tenant_id, &workflow.metadata.name)
            .await
        {
            Ok(Some(w)) => w,
            _ => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "updated": false,
                    "deterministic_validation": { "passed": true },
                    "error": "Workflow not found"
                })));
            }
        };

        let register_workflow_use_case = match &self.register_workflow_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "deterministic_validation": { "passed": true },
                    "error": "Workflow registration use case not configured"
                })));
            }
        };

        match register_workflow_use_case
            .register_workflow_for_tenant(&tenant_id, manifest_yaml, force)
            .await
        {
            Ok(meta) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "workflows",
                        &meta.name,
                        &meta.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SmcpSessionError::SignatureVerificationFailed(format!(
                            "Workflow updated but failed to persist manifest: {e}"
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.update",
                    "updated": true,
                    "deterministic_validation": { "passed": true },
                    "name": meta.name,
                    "workflow_id": meta.workflow_id,
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.update",
                "updated": false,
                "deterministic_validation": { "passed": true },
                "error": format!("Workflow update failed: {}", e)
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_create_tool(
        &self,
        args: &Value,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
    ) -> Result<ToolInvocationResult, SmcpSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "aegis.workflow.create requires 'manifest_yaml' string".to_string(),
                )
            })?;

        let task_context = args
            .get("task_context")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let min_score = args
            .get("min_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.8);
        let min_confidence = args
            .get("min_confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.7);

        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

        let workflow = match WorkflowParser::parse_yaml(manifest_yaml) {
            Ok(w) => w,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create",
                    "deterministic_validation": {
                        "passed": false,
                        "error": format!("Workflow parser validation failed: {}", e),
                    },
                    "semantic_validation": {"passed": false},
                    "deployed": false
                })));
            }
        };

        if let Err(e) = WorkflowValidator::check_for_cycles(&workflow) {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {
                    "passed": false,
                    "error": format!("Workflow cycle validation failed: {}", e),
                },
                "semantic_validation": {"passed": false},
                "deployed": false
            })));
        }

        let semantic_guard_violations =
            collect_thresholded_transition_semantic_violations(&workflow);
        if !semantic_guard_violations.is_empty() {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": false,
                    "error": "Thresholded validator states must use explicit score-based routing. Use `score_below` for low-score outcomes and do not mix `on_success` with `score_*` transitions in the same state.",
                    "violations": semantic_guard_violations,
                },
                "deployed": false
            })));
        }

        let validation_service = self.validation_service.as_ref().ok_or_else(|| {
            SmcpSessionError::SignatureVerificationFailed(
                "Workflow semantic validation service not configured".to_string(),
            )
        })?;
        let register_workflow_use_case =
            self.register_workflow_use_case.as_ref().ok_or_else(|| {
                SmcpSessionError::SignatureVerificationFailed(
                    "Workflow registration use case not configured".to_string(),
                )
            })?;

        let judge_names: Vec<String> = args
            .get("judge_agents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_else(|| vec!["code-quality-judge".to_string()]);

        if judge_names.is_empty() {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": false,
                    "error": "No judge agents provided",
                },
                "deployed": false
            })));
        }

        let mut judges: Vec<(AgentId, f64)> = Vec::with_capacity(judge_names.len());
        for judge_name in &judge_names {
            let judge_id = self
                .agent_lifecycle
                .lookup_agent(judge_name)
                .await
                .map_err(|e| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "Failed to lookup judge agent '{judge_name}': {e}"
                    ))
                })?
                .ok_or_else(|| {
                    SmcpSessionError::SignatureVerificationFailed(format!(
                        "Judge agent '{judge_name}' not found"
                    ))
                })?;
            judges.push((judge_id, 1.0));
        }

        let criteria = if task_context.is_empty() {
            "Evaluate this workflow manifest for correctness, transition safety, DDD alignment, and production readiness. Thresholded validator states must use explicit score-based routing: require `score_below` for low-score outcomes, and reject any state that mixes `on_success` with `score_*` transitions."
                .to_string()
        } else {
            format!(
                "Task Context:\n{task_context}\n\nEvaluate this workflow manifest for correctness, transition safety, DDD alignment, and production readiness. Thresholded validator states must use explicit score-based routing: require `score_below` for low-score outcomes, and reject any state that mixes `on_success` with `score_*` transitions."
            )
        };

        let semantic_request = ValidationRequest {
            content: manifest_yaml.to_string(),
            criteria,
            context: Some(serde_json::json!({
                "validation_context": "workflow_create",
                "workflow_name": workflow.metadata.name,
                "state_count": workflow.spec.states.len(),
                "force": force,
                "task_context": task_context,
                "iteration_number": iteration_number,
                "tool_audit_history": Self::build_tool_audit_history(
                    execution_id,
                    iteration_number,
                    tool_audit_history,
                ),
            })),
        };

        let semantic_consensus = validation_service
            .validate_with_judges(
                execution_id,
                agent_id,
                0,
                semantic_request,
                judges,
                Some(ConsensusConfig {
                    strategy: ConsensusStrategy::WeightedAverage,
                    threshold: Some(min_score),
                    min_agreement_confidence: Some(min_confidence),
                    n: None,
                    min_judges_required: 1,
                    confidence_weighting: Some(ConfidenceWeighting::default()),
                }),
                90,
                500,
            )
            .await
            .map_err(|e| {
                SmcpSessionError::SignatureVerificationFailed(format!(
                    "Workflow semantic validation failed: {e}"
                ))
            })?;

        let semantic_passed = semantic_consensus.final_score >= min_score
            && semantic_consensus.consensus_confidence >= min_confidence;

        if !semantic_passed {
            return Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": false,
                    "score": semantic_consensus.final_score,
                    "confidence": semantic_consensus.consensus_confidence,
                    "thresholds": {
                        "min_score": min_score,
                        "min_confidence": min_confidence
                    },
                    "strategy": semantic_consensus.strategy,
                },
                "deployed": false
            })));
        }

        match register_workflow_use_case
            .register_workflow(manifest_yaml, force)
            .await
        {
            Ok(registered) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "workflows",
                        &registered.name,
                        &registered.version,
                        manifest_yaml,
                    )
                    .map_err(|e| {
                        SmcpSessionError::SignatureVerificationFailed(format!(
                            "Workflow deployed but failed to persist manifest: {e}"
                        ))
                    })?;
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create",
                    "deterministic_validation": {"passed": true},
                    "semantic_validation": {
                        "passed": true,
                        "score": semantic_consensus.final_score,
                        "confidence": semantic_consensus.consensus_confidence,
                        "strategy": semantic_consensus.strategy,
                    },
                    "deployed": true,
                    "workflow": {
                        "workflow_id": registered.workflow_id,
                        "name": registered.name,
                        "version": registered.version,
                        "status": registered.status
                    },
                    "manifest_yaml": manifest_yaml,
                    "manifest_path": persisted_path
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.create",
                "deterministic_validation": {"passed": true},
                "semantic_validation": {
                    "passed": true,
                    "score": semantic_consensus.final_score,
                    "confidence": semantic_consensus.consensus_confidence,
                    "strategy": semantic_consensus.strategy,
                },
                "deployed": false,
                "errors": [format!("Workflow registration failed: {}", e)]
            }))),
        }
    }

    pub(super) fn persist_generated_manifest(
        &self,
        kind: &str,
        name: &str,
        version: &str,
        manifest_yaml: &str,
    ) -> std::io::Result<Option<String>> {
        let Some(root) = &self.generated_manifests_root else {
            return Ok(None);
        };

        let safe_name = sanitize_segment(name);
        let safe_version = sanitize_segment(version);
        let target_dir = root.join(kind).join(safe_name);
        std::fs::create_dir_all(&target_dir)?;

        let file_path = target_dir.join(format!("{safe_version}.yaml"));
        std::fs::write(&file_path, manifest_yaml)?;
        Ok(Some(path_to_string(&file_path)))
    }
}

pub(super) fn sanitize_segment(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "unversioned".to_string();
    }

    let sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Prevent path traversal patterns after character substitution.
    // Treat empty or traversal-like segments as a safe default.
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." || sanitized.contains("..") {
        "unversioned".to_string()
    } else {
        sanitized
    }
}

pub(super) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(super) fn collect_thresholded_transition_semantic_violations(
    workflow: &Workflow,
) -> Vec<String> {
    workflow
        .spec
        .states
        .iter()
        .flat_map(|(state_name, state)| {
            let has_thresholded_score_transition = state
                .transitions
                .iter()
                .any(|transition| is_thresholded_score_transition(&transition.condition));
            if !has_thresholded_score_transition {
                return Vec::new();
            }

            let has_on_success = state
                .transitions
                .iter()
                .any(|transition| matches!(transition.condition, TransitionCondition::OnSuccess));
            let has_score_below = state
                .transitions
                .iter()
                .any(|transition| matches!(transition.condition, TransitionCondition::ScoreBelow { .. }));

            let mut violations = Vec::new();
            if has_on_success {
                violations.push(format!(
                    "State '{}' mixes `on_success` with score-based transitions; low-score success paths must be explicit.",
                    state_name
                ));
            }
            if !has_score_below {
                violations.push(format!(
                    "State '{}' uses score-based transitions but has no explicit `score_below` branch for low-score outcomes.",
                    state_name
                ));
            }

            violations
        })
        .collect()
}

pub(super) fn is_thresholded_score_transition(condition: &TransitionCondition) -> bool {
    matches!(
        condition,
        TransitionCondition::ScoreAbove { .. }
            | TransitionCondition::ScoreBelow { .. }
            | TransitionCondition::ScoreBetween { .. }
            | TransitionCondition::ScoreAndConfidenceAbove { .. }
    )
}
