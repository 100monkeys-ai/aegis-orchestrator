use super::*;

use crate::domain::execution::AttachmentRef;

/// The mark a workflow's `input_schema` sets on an input that names one of
/// the run's attached files (AEGIS ADR-143 S7b): `x-aegis-attachment-name:
/// true` on the input's property. An extension keyword, so the schema's
/// validation ignores it.
pub(super) const ATTACHMENT_NAME_MARK: &str = "x-aegis-attachment-name";

/// Fill and check each input `input_schema` marks as naming an attached
/// file, before the run starts (AEGIS ADR-143 S7a, S7b). Left out with
/// exactly one attachment, the input is that attachment's name; left out
/// with several, or naming no attached file, the run is refused with the
/// sentence returned. A run carrying no attachment is left to the step's
/// own refusal (S4), and an input of another type to the schema's.
fn fill_attachment_names(
    input_schema: Option<&Value>,
    input: &mut Value,
    attachments: &[AttachmentRef],
) -> Result<(), String> {
    let Some(properties) = input_schema
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    if attachments.is_empty() {
        return Ok(());
    }
    let names: Vec<&str> = attachments.iter().map(|a| a.name.as_str()).collect();
    let listed = names.join(", ");
    for (key, property) in properties {
        if property.get(ATTACHMENT_NAME_MARK) != Some(&Value::Bool(true)) {
            continue;
        }
        match input.get(key) {
            Some(Value::String(named)) => {
                if !names.contains(&named.as_str()) {
                    return Err(format!(
                        "No attached file is named '{named}'; the attached files are: {listed}."
                    ));
                }
            }
            None | Some(Value::Null) => {
                if names.len() > 1 {
                    return Err(format!(
                        "This run reads one recording and several files are attached ({listed}); name the one to transcribe in {key}."
                    ));
                }
                if let Some(obj) = input.as_object_mut() {
                    obj.insert(key.clone(), Value::String(names[0].to_string()));
                }
            }
            Some(_) => {}
        }
    }
    Ok(())
}

impl ToolInvocationService {
    /// Build a [`crate::application::workflow_scope::ScopeChangeRequester`] from tool args and the active security context.
    ///
    /// Callers MUST supply `user_id` and `roles` in the tool args so that the
    /// authorization check in [`crate::application::workflow_scope::WorkflowScopeService::change_scope`] evaluates
    /// against the *actual* caller identity.  When `roles` is omitted the
    /// requester is constructed with an empty role set — fail-closed — so the
    /// downstream authorization check will reject any privileged transition.
    fn resolve_scope_change_requester(
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
        tenant_id: &TenantId,
    ) -> Result<crate::application::workflow_scope::ScopeChangeRequester, SealSessionError> {
        let user_id = args
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or(&security_context.name)
            .to_string();

        let roles: Vec<String> = args
            .get("roles")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Ok(crate::application::workflow_scope::ScopeChangeRequester {
            user_id,
            roles,
            tenant_id: tenant_id.clone(),
        })
    }

    pub(super) async fn invoke_aegis_workflow_delete_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
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
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
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
        args: &mut Value,
        _security_context: &crate::domain::security_context::SecurityContext,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        // Zaru ADR-0028 W49b (3): the goal this run is started for, checked
        // before anything starts, as `aegis.workflow.generate` checks its own.
        let goal_id = match self
            .goal_for_start("aegis.workflow.run", args, caller_identity, _scope)
            .await?
        {
            Ok(goal_id) => goal_id,
            Err(refused) => return Ok(refused),
        };
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.workflow.run requires 'name' string".to_string(),
            )
        })?;

        let mut input = args.get("input").cloned().unwrap_or(serde_json::json!({}));
        // Zaru ADR-0055 D14: the dispatch's binding choices ride in the
        // input's reserved key `contexts`.
        super::context_args::carry_contexts(args, &mut input)?;
        super::repository_args::carry_repositories(args, &mut input)?;
        // AEGIS ADR-126, Update of 2026-10-07 (2), clauses 3 and 3a: the
        // conversation the run was started from, only as the facade wrote it.
        super::context_args::carry_conversation(args, &mut input);
        // AEGIS ADR-143 S3: the attachments ride in the run's
        // `input.attachments`, as `aegis.execute.intent` places them.
        let attachments = super::attachment_args::parse_attachments(args)?;
        if !attachments.is_empty() {
            let attachments_json = serde_json::to_value(&attachments).map_err(|e| {
                SealSessionError::InternalError(format!(
                    "Failed to serialize attachments for workflow run: {e}"
                ))
            })?;
            match input.as_object_mut() {
                Some(obj) => {
                    obj.insert("attachments".to_string(), attachments_json);
                }
                None => {
                    let original = std::mem::replace(&mut input, serde_json::Value::Null);
                    let mut wrapper = serde_json::Map::new();
                    wrapper.insert("value".to_string(), original);
                    wrapper.insert("attachments".to_string(), attachments_json);
                    input = serde_json::Value::Object(wrapper);
                }
            }
        }
        let blackboard = args.get("blackboard").cloned();
        let intent = args
            .get("intent")
            .and_then(|v| v.as_str())
            .map(String::from);
        let version = args
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);

        // AEGIS ADR-143 S7a, S7b: an input naming an attached file is
        // filled from the run's only attachment, or the run is refused here,
        // before any execution or container exists.
        if let Some(workflow) = self
            .workflow_for_run(&tenant_id, name, version.as_deref())
            .await
        {
            if let Err(sentence) = fill_attachment_names(
                workflow.metadata.input_schema.as_ref(),
                &mut input,
                &attachments,
            ) {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.run",
                    "error": sentence
                })));
            }
        }

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
            .start_execution_for_tenant(
                &tenant_id,
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: name.to_string(),
                    input,
                    blackboard,
                    version,
                    tenant_id: Some(tenant_id.clone()),
                    security_context_name: Some("aegis-system-agent-runtime".to_string()),
                    intent,
                },
                caller_identity,
            )
            .await
        {
            Ok(started) => {
                // Bound to its goal, so the goal's cancel reaches the run and
                // the goal's rounds see it.
                if let Some(closed) = self
                    .bind_to_goal(
                        goal_id,
                        &started.execution_id,
                        crate::domain::goal::BoundKind::Workflow,
                    )
                    .await
                {
                    return Ok(ToolInvocationResult::Direct(closed));
                }
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.run",
                    "execution_id": started.execution_id,
                    "status": "started"
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.run",
                "error": format!("Failed to start workflow: {e}")
            }))),
        }
    }

    /// The workflow `aegis.workflow.run` would start, resolved as the start
    /// resolves it: by id, by name and version, or by name. `None` when no
    /// repository is configured or none is found; the start then answers
    /// for itself.
    async fn workflow_for_run(
        &self,
        tenant_id: &TenantId,
        name: &str,
        version: Option<&str>,
    ) -> Option<Workflow> {
        let repo = self.workflow_repository.as_ref()?;
        let found = if let Ok(uuid) = uuid::Uuid::parse_str(name) {
            repo.find_by_id_for_tenant(
                tenant_id,
                crate::domain::workflow::WorkflowId::from_uuid(uuid),
            )
            .await
        } else if let Some(version) = version {
            repo.resolve_by_name_and_version(tenant_id, name, version)
                .await
        } else {
            repo.resolve_by_name(tenant_id, name).await
        };
        match found {
            Ok(workflow) => workflow,
            Err(e) => {
                tracing::warn!(workflow = name, error = %e, "The workflow to run could not be read before its start");
                None
            }
        }
    }

    pub(super) async fn invoke_aegis_workflow_execution_list_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
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
                    .resolve_by_name(&tenant_id, workflow_ref)
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
                            "summary": super::summary::summarize_intent(
                                &execution
                                    .input
                                    .get("intent")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned),
                            ),
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
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let execution_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.executions.get requires 'execution_id' string".to_string(),
                )
            })?;
        let execution_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(execution_id_str).map_err(|error| {
                SealSessionError::InvalidArguments(format!(
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
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let result = self
            .invoke_aegis_workflow_execution_get_tool(args, scope)
            .await?;
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

    /// Handle `aegis.workflow.wait` — block until a workflow execution reaches a terminal state.
    pub(super) async fn invoke_aegis_workflow_wait_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let execution_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.wait requires 'execution_id' string".to_string(),
                )
            })?;
        let execution_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(execution_id_str).map_err(|error| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.workflow.wait: invalid execution_id UUID: {error}"
                ))
            })?,
        );

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.wait",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let poll_interval = args
            .get("poll_interval_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(5);
        let timeout = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(300);

        let poll_duration = std::time::Duration::from_secs(poll_interval);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);

        loop {
            match repo.find_by_id_for_tenant(&tenant_id, execution_id).await {
                Ok(Some(execution)) => {
                    let status_str = format!("{:?}", execution.status).to_lowercase();
                    let is_terminal = matches!(
                        execution.status,
                        crate::domain::execution::ExecutionStatus::Completed
                            | crate::domain::execution::ExecutionStatus::Failed
                            | crate::domain::execution::ExecutionStatus::Cancelled
                    );

                    if is_terminal {
                        let mut answer = serde_json::json!({
                            "tool": "aegis.workflow.wait",
                            "execution_id": execution_id_str,
                            "workflow_id": execution.workflow_id.to_string(),
                            "status": status_str,
                            "current_state": execution.current_state.as_str(),
                            "blackboard": execution.blackboard.data(),
                            "last_output": execution.final_output,
                            "started_at": execution.started_at,
                            "last_transition_at": execution.last_transition_at,
                        });
                        // AEGIS ADR-005 I9 (P4): the files each completed
                        // step execution produced, in the steps' start
                        // order, each naming its execution. With no record
                        // read the key is absent: never "no files".
                        match &self.execution_repository {
                            Some(executions) => match executions
                                .find_by_workflow_execution_for_tenant(&tenant_id, execution_id.0)
                                .await
                            {
                                Ok(steps) => {
                                    let files: Vec<Value> = steps
                                        .iter()
                                        .filter(|step| {
                                            step.status
                                                == crate::domain::execution::ExecutionStatus::Completed
                                        })
                                        .flat_map(|step| {
                                            step.produced_files().iter().map(move |file| {
                                                serde_json::json!({
                                                    "execution_id": step.id.0.to_string(),
                                                    "path": file.path,
                                                    "size_bytes": file.size_bytes,
                                                    "content_type": file.content_type,
                                                })
                                            })
                                        })
                                        .collect();
                                    answer["produced_files"] = Value::Array(files);
                                }
                                Err(error) => tracing::warn!(
                                    workflow_execution_id = %execution_id_str,
                                    %error,
                                    "aegis.workflow.wait: step executions not read; \
                                     answering no produced_files"
                                ),
                            },
                            None => tracing::warn!(
                                workflow_execution_id = %execution_id_str,
                                "aegis.workflow.wait: no execution repository configured; \
                                 answering no produced_files"
                            ),
                        }
                        // AEGIS ADR-005 O6 (Q6): a failed run answers why, the
                        // last WorkflowExecutionFailed event's `error` (a step
                        // refused at start says "Execution refused: ...").
                        if execution.status == crate::domain::execution::ExecutionStatus::Failed {
                            const PAGE: usize = 500;
                            let mut offset = 0;
                            let mut reason: Option<String> = None;
                            loop {
                                match repo
                                    .find_events_by_execution(execution_id, PAGE, offset)
                                    .await
                                {
                                    Ok(events) => {
                                        for event in &events {
                                            if event.event_type == "WorkflowExecutionFailed" {
                                                if let Some(error) = event
                                                    .payload
                                                    .get("error")
                                                    .and_then(Value::as_str)
                                                {
                                                    reason = Some(error.to_string());
                                                }
                                            }
                                        }
                                        if events.len() < PAGE {
                                            break;
                                        }
                                        offset += PAGE;
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            workflow_execution_id = %execution_id_str,
                                            %error,
                                            "aegis.workflow.wait: events not read; answering no error"
                                        );
                                        break;
                                    }
                                }
                            }
                            if let Some(reason) = reason {
                                answer["error"] = Value::String(reason);
                            }
                        }
                        return Ok(ToolInvocationResult::Direct(answer));
                    }

                    if std::time::Instant::now() >= deadline {
                        return Ok(ToolInvocationResult::Direct(serde_json::json!({
                            "tool": "aegis.workflow.wait",
                            "execution_id": execution_id_str,
                            "status": status_str,
                            "current_state": execution.current_state.as_str(),
                            "timed_out": true,
                            "message": format!("Workflow execution still {} after {}s timeout", status_str, timeout),
                        })));
                    }

                    tokio::time::sleep(poll_duration).await;
                }
                Ok(None) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.wait",
                        "error": format!("Workflow execution '{execution_id_str}' not found")
                    })));
                }
                Err(error) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.wait",
                        "error": format!("Failed to query workflow execution: {error}")
                    })));
                }
            }
        }
    }

    pub(super) async fn invoke_aegis_workflow_generate_tool(
        &self,
        args: &mut Value,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        // AEGIS ADR-131 U6: the goal this generation is started for, checked
        // before anything starts.
        let goal_id = match self
            .goal_for_start("aegis.workflow.generate", args, caller_identity, _scope)
            .await?
        {
            Ok(goal_id) => goal_id,
            Err(refused) => return Ok(refused),
        };
        let input = args.get("input").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.workflow.generate requires 'input' string".to_string(),
            )
        })?;

        let mut payload = serde_json::json!({ "input": input });
        // Zaru ADR-0055 D14: the dispatch's binding choices ride in the
        // input's reserved key `contexts`.
        super::context_args::carry_contexts(args, &mut payload)?;
        super::repository_args::carry_repositories(args, &mut payload)?;
        // AEGIS ADR-126, Update of 2026-10-07 (2), clauses 3 and 3a: the
        // conversation the run was started from, only as the facade wrote it.
        super::context_args::carry_conversation(args, &mut payload);

        let start_use_case = match &self.start_workflow_execution_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.generate",
                    "error": "Workflow execution service not configured"
                })));
            }
        };

        // Started as the person who called the tool, its initiator: the
        // agents its states run act for that person (AEGIS ADR-132 H6).
        match start_use_case
            .start_execution_for_tenant(
                &tenant_id,
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: "builtin-workflow-generator".to_string(),
                    input: payload,
                    blackboard: None,
                    version: None,
                    tenant_id: Some(tenant_id.clone()),
                    security_context_name: Some("aegis-system-agent-runtime".to_string()),
                    intent: None,
                },
                caller_identity,
            )
            .await
        {
            Ok(started) => {
                if let Some(closed) = self
                    .bind_to_goal(
                        goal_id,
                        &started.execution_id,
                        crate::domain::goal::BoundKind::Workflow,
                    )
                    .await
                {
                    return Ok(ToolInvocationResult::Direct(closed));
                }
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.generate",
                    "execution_id": started.execution_id,
                    "status": "started"
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.generate",
                "error": format!("Failed to start workflow generation: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_logs_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.logs requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(&exec_id_str).map_err(|e| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.workflow.logs: invalid execution_id UUID: {e}"
                ))
            })?,
        );

        let repo = match &self.workflow_execution_repo {
            Some(r) => r.clone(),
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        let all_tenants = Self::reads_every_tenant(args, _scope);
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let fetched = if all_tenants {
            // AEGIS ADR-129 D17: under an active escalation, any tenant's
            // workflow execution by its id: its own tenant, then the read.
            match repo.find_tenant_id_by_execution(exec_id).await {
                Ok(Some(own_tenant)) => repo.find_by_id_for_tenant(&own_tenant, exec_id).await,
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            }
        } else {
            repo.find_by_id_for_tenant(&tenant_id, exec_id).await
        };
        let execution = match fetched {
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

        let page = match read_workflow_log_page(repo.as_ref(), exec_id, args).await {
            Ok(page) => page,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.logs",
                    "error": format!("Failed to fetch execution events: {e}")
                })));
            }
        };

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.logs",
            "execution_id": exec_id_str,
            "status": format!("{:?}", execution.status).to_lowercase(),
            "workflow_id": execution.workflow_id.to_string(),
            "events": page.events,
            "total": page.total,
            "limit": page.limit,
            "offset": page.offset,
            "has_more": page.has_more,
        })))
    }

    /// Handler for `aegis.workflow.cancel` — cancels a running workflow execution.
    pub(super) async fn invoke_aegis_workflow_cancel_tool(
        &self,
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution control is invoked. Without this gate any
        // caller could cancel any tenant's workflow execution by guessing its
        // UUID.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.cancel requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.workflow.cancel: invalid execution_id UUID: {e}"
                ))
            })?,
        );

        // Pre-flight tenant validation: a foreign-tenant execution must not
        // reach the control port. `find_by_id_for_tenant` returns `Ok(None)`
        // for an execution that does not belong to `tenant_id`.
        if let Some(repo) = &self.workflow_execution_repo {
            match repo.find_by_id_for_tenant(&tenant_id, exec_id).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.cancel",
                        "cancelled": false,
                        "error": format!("Workflow execution '{exec_id_str}' not found")
                    })));
                }
                Err(error) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.cancel",
                        "cancelled": false,
                        "error": format!("Failed to verify workflow execution tenancy: {error}")
                    })));
                }
            }
        }

        let port = match &self.workflow_execution_control {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.cancel",
                    "error": "Workflow execution control port not configured"
                })));
            }
        };

        // AEGIS ADR-131 U33a: a cancel of a workflow execution bound to an
        // open goal closes the goal `cancelled` before this call answers.
        match crate::application::goal_service::cancel_ending_its_goal(
            self.goal_service.as_deref(),
            &tenant_id,
            exec_id,
            port.cancel_workflow_execution(&tenant_id, exec_id),
        )
        .await
        {
            Ok(()) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.cancel",
                "cancelled": true,
                "execution_id": exec_id_str
            }))),
            // C3, finding 4: the execution was cancelled but its goal is
            // still open; the answer says so, naming the goal.
            Err(e @ crate::application::goal_service::CancelPathError::GoalStillOpen { .. }) => {
                Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.cancel",
                    "cancelled": true,
                    "execution_id": exec_id_str,
                    "goal_still_open": e.goal_still_open().map(|g| g.to_string()),
                    "error": e.to_string()
                })))
            }
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.cancel",
                "cancelled": false,
                "error": format!("Failed to cancel workflow execution: {e}")
            }))),
        }
    }

    /// Handler for `aegis.workflow.signal` — sends human input to a paused workflow.
    ///
    /// The signal names who answered as `answered_by`: the person's sub, or a
    /// service account's client id; none when the call acts for nobody.
    pub(super) async fn invoke_aegis_workflow_signal_tool(
        &self,
        args: &mut Value,
        caller_identity: Option<&crate::domain::iam::UserIdentity>,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: signal injection is more dangerous than read — without the
        // tenant gate any caller could deliver a human-input payload into a
        // paused workflow waiting on someone else's tenant. Bind/verify the
        // requested tenant against the authenticated scope first.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.signal requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.workflow.signal: invalid execution_id UUID: {e}"
                ))
            })?,
        );

        let response = args
            .get("response")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.signal requires 'response' string".to_string(),
                )
            })?
            .to_string();
        let feedback = args
            .get("feedback")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // Pre-flight tenant validation against the workflow execution
        // repository — a foreign-tenant execution must not receive a signal.
        if let Some(repo) = &self.workflow_execution_repo {
            match repo.find_by_id_for_tenant(&tenant_id, exec_id).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.signal",
                        "signalled": false,
                        "error": format!("Workflow execution '{exec_id_str}' not found")
                    })));
                }
                Err(error) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.signal",
                        "signalled": false,
                        "error": format!("Failed to verify workflow execution tenancy: {error}")
                    })));
                }
            }
        }

        let port = match &self.workflow_execution_control {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.signal",
                    "error": "Workflow execution control port not configured"
                })));
            }
        };

        let answered_by = signal_answerer(caller_identity);
        match port
            .signal_workflow_execution(
                &tenant_id,
                exec_id,
                &response,
                feedback.as_deref(),
                answered_by.as_deref(),
            )
            .await
        {
            Ok(()) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.signal",
                "signalled": true,
                "execution_id": exec_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.signal",
                "signalled": false,
                "error": format!("Failed to signal workflow execution: {e}")
            }))),
        }
    }

    /// Handler for `aegis.workflow.remove` — removes a workflow execution record.
    pub(super) async fn invoke_aegis_workflow_remove_tool(
        &self,
        args: &mut Value,
        scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        // ADR-097: bind/verify the requested tenant against the authenticated
        // scope before any execution data is deleted. Without this gate any
        // caller could delete the audit/history record of another tenant's
        // workflow execution by guessing its UUID.
        let tenant_id = Self::enforce_tenant_arg(args, scope)?;

        let exec_id_str = args
            .get("execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.workflow.remove requires 'execution_id' string".to_string(),
                )
            })?;

        let exec_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(exec_id_str).map_err(|e| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.workflow.remove: invalid execution_id UUID: {e}"
                ))
            })?,
        );

        // Pre-flight tenant validation: a foreign-tenant execution must not
        // be deleted via this surface.
        if let Some(repo) = &self.workflow_execution_repo {
            match repo.find_by_id_for_tenant(&tenant_id, exec_id).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.remove",
                        "removed": false,
                        "error": format!("Workflow execution '{exec_id_str}' not found")
                    })));
                }
                Err(error) => {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.remove",
                        "removed": false,
                        "error": format!("Failed to verify workflow execution tenancy: {error}")
                    })));
                }
            }
        }

        let port = match &self.workflow_execution_control {
            Some(p) => p,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.remove",
                    "error": "Workflow execution control port not configured"
                })));
            }
        };

        match port.remove_workflow_execution(&tenant_id, exec_id).await {
            Ok(()) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.remove",
                "removed": true,
                "execution_id": exec_id_str
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.remove",
                "removed": false,
                "error": format!("Failed to remove workflow execution: {e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_list_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.list",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        let scope_filter = args.get("scope").and_then(|v| v.as_str());

        let workflows = match scope_filter {
            Some("global") => repo.list_global().await.map_err(|e| {
                SealSessionError::InternalError(format!("Failed to list workflows: {e}"))
            })?,
            Some("visible") => repo.list_visible(&tenant_id).await.map_err(|e| {
                SealSessionError::InternalError(format!("Failed to list workflows: {e}"))
            })?,
            _ => repo.list_all_for_tenant(&tenant_id).await.map_err(|e| {
                SealSessionError::InternalError(format!("Failed to list workflows: {e}"))
            })?,
        };

        let entries: Vec<serde_json::Value> = workflows
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id.0.to_string(),
                    "name": w.metadata.name,
                    "version": w.metadata.version.clone().unwrap_or_default(),
                    "scope": w.scope.to_string(),
                    "initial_state": w.spec.initial_state,
                    "description": w.metadata.description,
                    "labels": w.metadata.labels,
                    "input_schema": w.metadata.input_schema,
                })
            })
            .collect();

        Ok(ToolInvocationResult::Direct(serde_json::json!({
            "tool": "aegis.workflow.list",
            "count": entries.len(),
            "workflows": entries
        })))
    }

    pub(super) async fn invoke_aegis_workflow_promote_tool(
        &self,
        args: &mut Value,
        security_context: &crate::domain::security_context::SecurityContext,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.workflow.promote requires 'name' string".to_string(),
            )
        })?;

        let target_scope_str = args
            .get("target_scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global");

        let target_scope: crate::domain::workflow::WorkflowScope =
            target_scope_str.parse().map_err(|_| {
                SealSessionError::InvalidArguments(format!(
                    "Invalid target_scope '{target_scope_str}': expected 'tenant' or 'global'"
                ))
            })?;

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.promote",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        // Resolve workflow by name or ID
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
                                "tool": "aegis.workflow.promote",
                                "error": "Workflow not found"
                            })));
                        }
                    }
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.promote",
                        "error": "Workflow not found"
                    })));
                }
            }
        };

        let requester = Self::resolve_scope_change_requester(args, security_context, &tenant_id)?;

        let scope_service = crate::application::workflow_scope::WorkflowScopeService::new(
            repo.clone(),
            self.event_bus.clone(),
        );

        match scope_service
            .change_scope(workflow.id, target_scope.clone(), &requester)
            .await
        {
            Ok(()) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.promote",
                "promoted": true,
                "workflow_id": workflow.id.0.to_string(),
                "name": workflow.metadata.name,
                "previous_scope": workflow.scope.to_string(),
                "new_scope": target_scope.to_string()
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.promote",
                "promoted": false,
                "error": format!("{e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_demote_tool(
        &self,
        args: &mut Value,
        security_context: &crate::domain::security_context::SecurityContext,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.workflow.demote requires 'name' string".to_string(),
            )
        })?;

        let target_scope_str = args
            .get("target_scope")
            .and_then(|v| v.as_str())
            .unwrap_or("tenant");

        let target_scope: crate::domain::workflow::WorkflowScope =
            target_scope_str.parse().map_err(|_| {
                SealSessionError::InvalidArguments(format!(
                    "Invalid target_scope '{target_scope_str}': expected 'tenant' or 'global'"
                ))
            })?;

        let repo = match &self.workflow_repository {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.demote",
                    "error": "Workflow repository not configured"
                })));
            }
        };

        // Resolve workflow by name or ID
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
                                "tool": "aegis.workflow.demote",
                                "error": "Workflow not found"
                            })));
                        }
                    }
                } else {
                    return Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.demote",
                        "error": "Workflow not found"
                    })));
                }
            }
        };

        let requester = Self::resolve_scope_change_requester(args, security_context, &tenant_id)?;

        let scope_service = crate::application::workflow_scope::WorkflowScopeService::new(
            repo.clone(),
            self.event_bus.clone(),
        );

        match scope_service
            .change_scope(workflow.id, target_scope.clone(), &requester)
            .await
        {
            Ok(()) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.demote",
                "demoted": true,
                "workflow_id": workflow.id.0.to_string(),
                "name": workflow.metadata.name,
                "previous_scope": workflow.scope.to_string(),
                "new_scope": target_scope.to_string()
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.workflow.demote",
                "demoted": false,
                "error": format!("{e}")
            }))),
        }
    }

    pub(super) async fn invoke_aegis_workflow_export_tool(
        &self,
        args: &mut Value,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
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

        let workflow = match repo.resolve_by_name(&tenant_id, name).await {
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
        args: &mut Value,
        _execution_id: crate::domain::execution::ExecutionId,
        _agent_id: AgentId,
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
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
            .register_workflow_for_tenant(
                &tenant_id,
                manifest_yaml,
                force,
                crate::domain::workflow::WorkflowScope::Tenant,
            )
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
                        SealSessionError::InternalError(format!(
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

    /// Validate a workflow manifest deterministically and by its judges, then
    /// register it in the caller's tenant, resolved from the scope as
    /// `aegis.workflow.update` resolves it; a tenant the caller may not name
    /// is refused before anything is validated or registered.
    pub(super) async fn invoke_aegis_workflow_create_tool(
        &self,
        args: &mut Value,
        execution_id: crate::domain::execution::ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        tool_audit_history: &[TrajectoryStep],
        _scope: &crate::domain::iam::TenantScope,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let manifest_yaml = args
            .get("manifest_yaml")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
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

        let tenant_id = Self::enforce_tenant_arg(args, _scope)?;

        let workflow = match WorkflowParser::parse_yaml(&manifest_yaml) {
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
            SealSessionError::ConfigurationError(
                "Workflow semantic validation service not configured".to_string(),
            )
        })?;
        let register_workflow_use_case =
            self.register_workflow_use_case.as_ref().ok_or_else(|| {
                SealSessionError::ConfigurationError(
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
                .lookup_agent_visible_for_tenant(&tenant_id, judge_name)
                .await
                .map_err(|e| {
                    SealSessionError::InternalError(format!(
                        "Failed to lookup judge agent '{judge_name}': {e}"
                    ))
                })?
                .ok_or_else(|| {
                    SealSessionError::NotFound(format!("Judge agent '{judge_name}' not found"))
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

        // A judge that fails is the manifest's semantic validation failing,
        // answered to the caller in the words the judges' failure gives.
        let semantic_consensus = match validation_service
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
                &tenant_id,
            )
            .await
        {
            Ok(consensus) => consensus,
            Err(e) => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.workflow.create",
                    "deterministic_validation": {"passed": true},
                    "semantic_validation": {
                        "passed": false,
                        "error": format!("Workflow semantic validation failed: {e:#}"),
                    },
                    "deployed": false
                })));
            }
        };

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
            .register_workflow_for_tenant(
                &tenant_id,
                &manifest_yaml,
                force,
                crate::domain::workflow::WorkflowScope::Tenant,
            )
            .await
        {
            Ok(registered) => {
                let persisted_path = self
                    .persist_generated_manifest(
                        "workflows",
                        &registered.name,
                        &registered.version,
                        &manifest_yaml,
                    )
                    .map_err(|e| {
                        SealSessionError::InternalError(format!(
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
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("already exists") {
                    let wf_version = workflow.metadata.version.clone().unwrap_or_default();
                    Ok(ToolInvocationResult::Direct(serde_json::json!({
                        "tool": "aegis.workflow.create",
                        "status": "conflict",
                        "error": format!(
                            "Workflow '{}' version '{}' is already registered. Increment the version (e.g. to '{}.1') in the manifest and retry.",
                            workflow.metadata.name,
                            wf_version,
                            wf_version,
                        ),
                        "hint": "increment_version"
                    })))
                } else {
                    Ok(ToolInvocationResult::Direct(serde_json::json!({
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
                    })))
                }
            }
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

/// One page of a workflow run's persisted events, in sequence order, with
/// the paging values that were applied.
#[derive(Debug)]
struct WorkflowLogPage {
    events: Vec<crate::domain::workflow::WorkflowExecutionEventRecord>,
    total: usize,
    limit: usize,
    offset: usize,
    has_more: bool,
}

/// The page size of `aegis.workflow.logs` when the caller names none.
const WORKFLOW_LOG_DEFAULT_LIMIT: usize = 50;
/// The largest page `aegis.workflow.logs` answers.
const WORKFLOW_LOG_MAX_LIMIT: usize = 200;
/// The page size the run's events are counted by.
const WORKFLOW_LOG_COUNT_PAGE: usize = 500;

/// Read the page that `aegis.workflow.logs`'s `limit` (default 50, at most
/// 200) and `offset` (default 0) arguments name of a workflow run's events,
/// in sequence order, with the run's total and whether events follow the
/// page.
async fn read_workflow_log_page(
    repo: &dyn crate::domain::repository::WorkflowExecutionRepository,
    execution_id: crate::domain::execution::ExecutionId,
    args: &Value,
) -> Result<WorkflowLogPage, crate::domain::repository::RepositoryError> {
    let limit: usize = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        .unwrap_or(WORKFLOW_LOG_DEFAULT_LIMIT)
        .min(WORKFLOW_LOG_MAX_LIMIT);
    let offset: usize = args
        .get("offset")
        .and_then(|v| v.as_u64())
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        .unwrap_or(0);
    let events = repo
        .find_events_by_execution(execution_id, limit, offset)
        .await?;
    // A page that holds events but is short of the limit ends the trail, so
    // the total is known from it; otherwise the trail is counted.
    let total = if !events.is_empty() && events.len() < limit {
        offset + events.len()
    } else {
        let mut counted = 0;
        loop {
            let read = repo
                .find_events_by_execution(execution_id, WORKFLOW_LOG_COUNT_PAGE, counted)
                .await?
                .len();
            counted += read;
            if read < WORKFLOW_LOG_COUNT_PAGE {
                break counted;
            }
        }
    };
    let has_more = offset.saturating_add(events.len()) < total;
    Ok(WorkflowLogPage {
        events,
        total,
        limit,
        offset,
        has_more,
    })
}

#[cfg(test)]
mod workflow_log_page_tests {
    //! `aegis.workflow.logs` maps its `limit` and `offset` arguments onto the
    //! run's events in sequence order and answers the values it applied, the
    //! run's total and whether more follow.

    use super::read_workflow_log_page;
    use crate::domain::execution::ExecutionId;
    use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
    use crate::domain::tenant::TenantId;
    use crate::domain::workflow::{WorkflowExecution, WorkflowExecutionEventRecord, WorkflowId};
    use serde_json::json;

    /// A run's event trail of `events` events, sequences 1 to `events`; the
    /// repository pages it as the Postgres query does.
    struct Trail(Vec<WorkflowExecutionEventRecord>);

    impl Trail {
        fn of(events: i64) -> Self {
            Self(
                (1..=events)
                    .map(|sequence| WorkflowExecutionEventRecord {
                        sequence,
                        event_type: "WorkflowStateEntered".to_string(),
                        state_name: None,
                        iteration_number: None,
                        payload: json!({}),
                        recorded_at: chrono::Utc::now(),
                    })
                    .collect(),
            )
        }
    }

    #[async_trait::async_trait]
    impl WorkflowExecutionRepository for Trail {
        async fn save_for_tenant(
            &self,
            _: &TenantId,
            _: &WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn find_by_id_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<Option<WorkflowExecution>, RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn find_active_for_tenant(
            &self,
            _: &TenantId,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn find_by_workflow_for_tenant(
            &self,
            _: &TenantId,
            _: WorkflowId,
            _: usize,
            _: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn list_paginated_for_tenant(
            &self,
            _: &TenantId,
            _: usize,
            _: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn list_paginated_all(
            &self,
            _: usize,
            _: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn count_by_workflow_for_tenant(
            &self,
            _: &TenantId,
            _: WorkflowId,
        ) -> Result<i64, RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn update_temporal_linkage_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
            _: &str,
            _: &str,
        ) -> Result<(), RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn append_event(
            &self,
            _: ExecutionId,
            _: i64,
            _: String,
            _: serde_json::Value,
            _: Option<u8>,
        ) -> Result<(), RepositoryError> {
            unreachable!("not read by the logs page")
        }
        async fn find_events_by_execution(
            &self,
            _: ExecutionId,
            limit: usize,
            offset: usize,
        ) -> Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
            Ok(self.0.iter().skip(offset).take(limit).cloned().collect())
        }
        async fn find_tenant_id_by_execution(
            &self,
            _: ExecutionId,
        ) -> Result<Option<TenantId>, RepositoryError> {
            unreachable!("not read by the logs page")
        }
    }

    fn sequences(page: &super::WorkflowLogPage) -> Vec<i64> {
        page.events.iter().map(|event| event.sequence).collect()
    }

    #[tokio::test]
    async fn limit_one_answers_one_event_and_says_more_follow() {
        let args = json!({"execution_id": ExecutionId::new().to_string(), "limit": 1});
        let page = read_workflow_log_page(&Trail::of(7), ExecutionId::new(), &args)
            .await
            .unwrap();
        println!("aegis.workflow.logs limit 1 over seven events answered {page:?}");
        assert_eq!(sequences(&page), vec![1], "one event, the first");
        assert_eq!(page.limit, 1, "the limit applied is echoed");
        assert_eq!(page.offset, 0);
        assert_eq!(page.total, 7, "the run's total, not the page's length");
        assert!(page.has_more, "six more events follow");
    }

    #[tokio::test]
    async fn offset_five_answers_the_events_from_sequence_six() {
        let args = json!({"execution_id": ExecutionId::new().to_string(), "offset": 5});
        let page = read_workflow_log_page(&Trail::of(7), ExecutionId::new(), &args)
            .await
            .unwrap();
        println!("aegis.workflow.logs offset 5 over seven events answered {page:?}");
        assert_eq!(sequences(&page), vec![6, 7]);
        assert_eq!(page.offset, 5);
        assert_eq!(page.total, 7);
        assert!(!page.has_more, "the last event is in the page");
    }

    #[tokio::test]
    async fn no_limit_answers_fifty_and_a_larger_limit_is_bounded_at_two_hundred() {
        let trail = Trail::of(250);
        let page = read_workflow_log_page(&trail, ExecutionId::new(), &json!({}))
            .await
            .unwrap();
        assert_eq!(page.limit, 50, "the default limit");
        assert_eq!(page.events.len(), 50);
        assert_eq!(page.total, 250);
        assert!(page.has_more);

        let page = read_workflow_log_page(&trail, ExecutionId::new(), &json!({"limit": 500}))
            .await
            .unwrap();
        assert_eq!(page.limit, 200, "the limit is bounded at 200");
        assert_eq!(page.events.len(), 200);
        assert!(page.has_more);
    }
}

/// Who answers a gate through `aegis.workflow.signal`: a person's sub, or a
/// service account's client id; `None` when the call acts for nobody.
fn signal_answerer(identity: Option<&crate::domain::iam::UserIdentity>) -> Option<String> {
    identity.map(|identity| match &identity.identity_kind {
        crate::domain::iam::IdentityKind::ServiceAccount { client_id } => client_id.clone(),
        _ => identity.sub.clone(),
    })
}

/// `aegis.workflow.run`'s start: its run bound to the goal it is started
/// for (Zaru ADR-0028 W49b (3)), and an input naming an attached file
/// filled from the run's only attachment, or the run refused before any
/// execution exists (AEGIS ADR-143 S7a, S7b).
#[cfg(test)]
mod workflow_run_tests {
    use super::*;
    use crate::application::ports::WorkflowExecutionControlPort;
    use crate::application::start_workflow_execution::{
        StartWorkflowExecutionRequest, StartedWorkflowExecution,
    };
    use crate::domain::agent::AgentId;
    use crate::domain::events::ExecutionEvent;
    use crate::domain::execution::{Execution, ExecutionId, Iteration};
    use crate::domain::goal::{GoalId, GoalRepository, GoalState};
    use crate::domain::iam::{IdentityKind, TenantScope, UserIdentity, ZaruTier};
    use crate::domain::node_config::GoalsConfig;
    use crate::domain::security_context::SecurityContext;
    use crate::domain::workflow::StateKind;
    use crate::infrastructure::event_bus::DomainEvent;
    use crate::infrastructure::repositories::postgres_goal::InMemoryGoalRepository;
    use crate::infrastructure::repositories::{
        InMemoryAgentRepository, InMemoryVolumeRepository, InMemoryWorkflowRepository,
    };
    use crate::infrastructure::seal::session_repository::InMemorySealSessionRepository;
    use crate::infrastructure::storage::LocalHostStorageProvider;
    use futures::Stream;
    use serde_json::json;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;

    const OWNER: &str = "1a2b0000-workflow-run-owner";

    const TRANSCRIBE_AUDIO: &str =
        include_str!("../../../../../cli/templates/workflows/transcribe-audio.yaml");

    fn attachment(name: &str, mime_type: &str) -> Value {
        json!({
            "volume_id": uuid::Uuid::new_v4().to_string(),
            "path": format!("uploads/{name}"),
            "name": name,
            "mime_type": mime_type,
            "size": 2_525_169,
        })
    }

    fn identity(sub: &str) -> UserIdentity {
        UserIdentity {
            sub: sub.to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::for_consumer_user(sub).unwrap(),
            },
        }
    }

    fn scope(sub: &str) -> TenantScope {
        TenantScope::new(
            TenantId::for_consumer_user(sub).unwrap(),
            identity(sub).identity_kind,
        )
    }

    fn zaru_free() -> SecurityContext {
        SecurityContext {
            name: "zaru-free".to_string(),
            description: "consumer".to_string(),
            capabilities: vec![],
            deny_list: vec![],
            metadata: crate::domain::security_context::SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    /// No agent execution is started by `aegis.workflow.run`.
    struct NoExecutions;

    #[async_trait::async_trait]
    impl ExecutionService for NoExecutions {
        async fn start_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_execution_with_id(
            &self,
            _: ExecutionId,
            _: AgentId,
            _: ExecutionInput,
            _: String,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn start_child_execution(
            &self,
            _: AgentId,
            _: ExecutionInput,
            _: ExecutionId,
        ) -> anyhow::Result<ExecutionId> {
            anyhow::bail!("not exercised")
        }
        async fn get_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Execution> {
            anyhow::bail!("Execution not found")
        }
        async fn get_execution_unscoped(&self, _: ExecutionId) -> anyhow::Result<Execution> {
            anyhow::bail!("Execution not found")
        }
        async fn get_iterations_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<Vec<Iteration>> {
            anyhow::bail!("not exercised")
        }
        async fn cancel_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn stream_execution(
            &self,
            _: ExecutionId,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<ExecutionEvent>> + Send>>>
        {
            anyhow::bail!("not exercised")
        }
        async fn stream_agent_events(
            &self,
            _: AgentId,
        ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<DomainEvent>> + Send>>>
        {
            anyhow::bail!("not exercised")
        }
        async fn list_executions_for_tenant(
            &self,
            _: &TenantId,
            _: Option<AgentId>,
            _: Option<crate::domain::workflow::WorkflowId>,
            _: usize,
        ) -> anyhow::Result<Vec<Execution>> {
            Ok(vec![])
        }
        async fn delete_execution_for_tenant(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn record_llm_interaction(
            &self,
            _: ExecutionId,
            _: u8,
            _: crate::domain::execution::LlmInteraction,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
        async fn store_iteration_trajectory(
            &self,
            _: ExecutionId,
            _: u8,
            _: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("not exercised")
        }
    }

    /// Records each workflow cancel, as the daemon's port would request it.
    #[derive(Default)]
    struct WorkflowControl {
        cancels: StdMutex<Vec<ExecutionId>>,
    }

    #[async_trait::async_trait]
    impl WorkflowExecutionControlPort for WorkflowControl {
        async fn cancel_workflow_execution(
            &self,
            _: &TenantId,
            execution_id: ExecutionId,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.cancels.lock().unwrap().push(execution_id);
            Ok(())
        }
        async fn signal_workflow_execution(
            &self,
            _: &TenantId,
            _: ExecutionId,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Err("not exercised".into())
        }
        async fn remove_workflow_execution(
            &self,
            _: &TenantId,
            _: ExecutionId,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Err("not exercised".into())
        }
    }

    /// Starts a workflow execution as the use case would, keeping each
    /// request; a request it keeps is an execution created.
    #[derive(Default)]
    struct Starts {
        requests: StdMutex<Vec<StartWorkflowExecutionRequest>>,
    }

    #[async_trait::async_trait]
    impl crate::application::start_workflow_execution::StartWorkflowExecutionUseCase for Starts {
        async fn start_execution_for_tenant(
            &self,
            _: &TenantId,
            request: StartWorkflowExecutionRequest,
            _: Option<&UserIdentity>,
        ) -> anyhow::Result<StartedWorkflowExecution> {
            let workflow_id = request.workflow_id.clone();
            self.requests.lock().unwrap().push(request);
            Ok(StartedWorkflowExecution {
                execution_id: ExecutionId::new().to_string(),
                workflow_id,
                temporal_run_id: "run".to_string(),
                status: "running".to_string(),
                started_at: chrono::Utc::now(),
            })
        }
    }

    struct NoOpPublisher;

    #[async_trait::async_trait]
    impl crate::domain::fsal::EventPublisher for NoOpPublisher {
        async fn publish_storage_event(&self, _event: crate::domain::events::StorageEvent) {}
    }

    struct Harness {
        service: ToolInvocationService,
        workflows: Arc<WorkflowControl>,
        starts: Arc<Starts>,
        goals: Arc<InMemoryGoalRepository>,
        workflow_repository: Arc<InMemoryWorkflowRepository>,
    }

    fn harness() -> Harness {
        let router = Arc::new(ToolRouter::new(ToolRouter::builtin_dispatchers()));
        let storage_root =
            std::env::temp_dir().join(format!("aegis-workflow-run-{}", uuid::Uuid::new_v4()));
        let fsal = Arc::new(AegisFSAL::new(
            Arc::new(LocalHostStorageProvider::new(&storage_root).unwrap()),
            Arc::new(InMemoryVolumeRepository::new()),
            Arc::new(parking_lot::RwLock::new(HashMap::new())),
            Arc::new(NoOpPublisher),
        ));
        let event_bus = Arc::new(EventBus::new(256));
        let workflows = Arc::new(WorkflowControl::default());
        let starts = Arc::new(Starts::default());
        let goals = Arc::new(InMemoryGoalRepository::new());
        let workflow_repository = Arc::new(InMemoryWorkflowRepository::new());
        let goal_service = Arc::new(crate::application::goal_service::GoalService::new(
            goals.clone(),
            event_bus.clone(),
            GoalsConfig::default(),
        ));
        let service = ToolInvocationService::new(
            Arc::new(InMemorySealSessionRepository::new()),
            Arc::new(
                crate::infrastructure::security_context::InMemorySecurityContextRepository::new(),
            ),
            Arc::new(SealMiddleware::new()),
            router,
            fsal,
            NfsVolumeRegistry::new(),
            Arc::new(InMemoryAgentRepository::new()),
            Arc::new(NoExecutions),
            Arc::new(crate::infrastructure::web_tools::ReqwestWebToolAdapter::unconfigured()),
            event_bus,
            None,
        )
        .with_goals(goal_service)
        .with_workflow_execution_control(workflows.clone())
        .with_workflow_execution(starts.clone())
        .with_workflow_repository(workflow_repository.clone());
        Harness {
            service,
            workflows,
            starts,
            goals,
            workflow_repository,
        }
    }

    fn direct(result: ToolInvocationResult) -> Value {
        match result {
            ToolInvocationResult::Direct(v) => v,
            other => panic!("answers directly, not {other:?}"),
        }
    }

    impl Harness {
        async fn goal_create(&self) -> GoalId {
            let answer = direct(
                self.service
                    .invoke_aegis_goal_create_tool(
                        &json!({
                            "statement": "Transcribe the recording.",
                            "client_ref": "conversation-1",
                            "channel": "web",
                        }),
                        Some(&identity(OWNER)),
                        &scope(OWNER),
                    )
                    .await
                    .unwrap(),
            );
            GoalId::from_string(answer["goal_id"].as_str().unwrap()).unwrap()
        }

        async fn workflow_run(&self, mut args: Value) -> Value {
            direct(
                self.service
                    .invoke_aegis_workflow_run_tool(
                        &mut args,
                        &zaru_free(),
                        Some(&identity(OWNER)),
                        &scope(OWNER),
                    )
                    .await
                    .unwrap(),
            )
        }

        fn created(&self) -> usize {
            self.starts.requests.lock().unwrap().len()
        }

        /// The built-in `transcribe-audio`, as its template reads, visible
        /// to the caller.
        async fn with_transcribe_audio(&self) -> Workflow {
            let workflow =
                WorkflowParser::parse_yaml(TRANSCRIBE_AUDIO).expect("the template parses");
            crate::domain::repository::WorkflowRepository::save_for_tenant(
                self.workflow_repository.as_ref(),
                &TenantId::for_consumer_user(OWNER).unwrap(),
                &workflow,
            )
            .await
            .unwrap();
            workflow
        }

        async fn goal_cancel(&self, goal_id: GoalId) -> Value {
            direct(
                self.service
                    .invoke_aegis_goal_cancel_tool(
                        &json!({"goal_id": goal_id.to_string(), "reason": "Stopped by you"}),
                        &zaru_free(),
                        Some(&identity(OWNER)),
                        &scope(OWNER),
                    )
                    .await
                    .unwrap(),
            )
        }
    }

    /// W49b (3): a run `aegis.workflow.run` starts with a `goal_id` is bound
    /// to that goal, as `aegis.workflow.generate` binds its run, so the
    /// goal's cancel reaches it.
    #[tokio::test]
    async fn workflow_run_with_a_goal_binds_its_run_and_goal_cancel_then_cancels_it() {
        let h = harness();
        let goal = h.goal_create().await;

        let started = h
            .workflow_run(json!({
                "name": "email-outreach",
                "input": {"target": "queued"},
                "goal_id": goal.to_string(),
            }))
            .await;
        assert_eq!(started["status"], "started", "{started}");
        assert_eq!(h.starts.requests.lock().unwrap().len(), 1);
        let run = ExecutionId::from_string(started["execution_id"].as_str().unwrap()).unwrap();

        let bound = h.goals.list_bound(goal).await.unwrap();
        println!("the goal's bound executions after aegis.workflow.run: {bound:?}");
        assert_eq!(
            bound.iter().map(|b| b.execution_id).collect::<Vec<_>>(),
            vec![run],
            "aegis.workflow.run did not bind its run to the goal it was started for"
        );

        let answer = h.goal_cancel(goal).await;
        println!("aegis.goal.cancel answered: {answer}");
        assert_eq!(answer["state"], "cancelled", "{answer}");
        assert_eq!(
            *h.workflows.cancels.lock().unwrap(),
            vec![run],
            "aegis.goal.cancel did not cancel the run aegis.workflow.run started for the goal"
        );
        let goal_row = h.goals.find_goal(goal).await.unwrap().unwrap();
        assert_eq!(goal_row.state, GoalState::Cancelled);
    }

    /// S7a: `audio_path` left out, with exactly one attachment, names that
    /// attachment, so the step's command reads `/input/<its name>`.
    #[tokio::test]
    async fn transcribe_audio_with_one_attachment_and_no_audio_path_renders_input_that_name() {
        let h = harness();
        let workflow = h.with_transcribe_audio().await;

        let started = h
            .workflow_run(json!({
                "name": "transcribe-audio",
                "input": {},
                "attachments": [attachment("DIALOGUE.ogg", "audio/ogg")],
            }))
            .await;
        assert_eq!(started["status"], "started", "{started}");
        assert_eq!(h.created(), 1);

        let input = h.starts.requests.lock().unwrap()[0].input.clone();
        let command = workflow
            .spec
            .states
            .values()
            .find_map(|state| match &state.kind {
                StateKind::ContainerRun { command, .. } => Some(command.clone()),
                _ => None,
            })
            .expect("transcribe-audio has its model step");
        let rendered: Vec<String> = command
            .iter()
            .map(|arg| {
                handlebars::Handlebars::new()
                    .render_template(arg, &json!({ "input": input }))
                    .unwrap()
            })
            .collect();
        println!("the step's command renders as {rendered:?}");
        assert_eq!(
            rendered[1], "/input/DIALOGUE.ogg",
            "the run's only attachment is not the file the step reads: {input}"
        );
    }

    /// S7b: several attachments and no `audio_path` is refused at the
    /// start, before any execution, with one sentence naming the files.
    #[tokio::test]
    async fn transcribe_audio_with_two_attachments_and_no_audio_path_is_refused_before_any_execution(
    ) {
        let h = harness();
        h.with_transcribe_audio().await;

        let answer = h
            .workflow_run(json!({
                "name": "transcribe-audio",
                "input": {},
                "attachments": [
                    attachment("DIALOGUE.ogg", "audio/ogg"),
                    attachment("notes.wav", "audio/wav"),
                ],
            }))
            .await;
        println!("aegis.workflow.run answered: {answer}");
        assert_eq!(
            answer["error"],
            "This run reads one recording and several files are attached (DIALOGUE.ogg, notes.wav); name the one to transcribe in audio_path.",
            "{answer}"
        );
        assert!(answer.get("execution_id").is_none(), "{answer}");
        assert_eq!(h.created(), 0, "an execution was created for a refused run");
    }

    /// S7b: an `audio_path` naming no attached file is refused at the
    /// start, before any execution, with one sentence naming the files.
    #[tokio::test]
    async fn transcribe_audio_naming_no_attached_file_is_refused_before_any_execution() {
        let h = harness();
        h.with_transcribe_audio().await;

        let answer = h
            .workflow_run(json!({
                "name": "transcribe-audio",
                "input": {"audio_path": "recording.ogg"},
                "attachments": [attachment("DIALOGUE.ogg", "audio/ogg")],
            }))
            .await;
        println!("aegis.workflow.run answered: {answer}");
        assert_eq!(
            answer["error"],
            "No attached file is named 'recording.ogg'; the attached files are: DIALOGUE.ogg.",
            "{answer}"
        );
        assert!(answer.get("execution_id").is_none(), "{answer}");
        assert_eq!(h.created(), 0, "an execution was created for a refused run");
    }
}
