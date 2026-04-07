// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Intent-to-execution pipeline tool handlers (ADR-087).

use super::*;

impl ToolInvocationService {
    /// Handle `aegis.execute.intent` — start the intent-to-execution pipeline.
    pub(super) async fn invoke_aegis_execute_intent_tool(
        &self,
        args: &Value,
        security_context: &crate::domain::security_context::SecurityContext,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let intent = args.get("intent").and_then(|v| v.as_str()).ok_or_else(|| {
            SealSessionError::InvalidArguments(
                "aegis.execute.intent requires 'intent' string".to_string(),
            )
        })?;

        let inputs = args.get("inputs").cloned().unwrap_or(serde_json::json!({}));
        let volume_id = args
            .get("volume_id")
            .and_then(|v| v.as_str())
            .map(String::from);

        // ADR-087 D4: Free tier users may not supply a persistent volume_id.
        // Reject before any WorkflowExecution record is created.
        if volume_id.is_some() {
            let tier =
                crate::domain::iam::ZaruTier::from_security_context_name(&security_context.name);
            if tier == Some(crate::domain::iam::ZaruTier::Free) {
                return Err(SealSessionError::InvalidArguments(
                    "volume_id is not available on the Free tier".to_string(),
                ));
            }
        }

        let language = args
            .get("language")
            .and_then(|v| v.as_str())
            .unwrap_or("python");
        let timeout_seconds = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);

        let tenant_id = Self::resolve_tenant_arg(args)?;

        // Build IntentExecutionInput and serialize as workflow input
        let pipeline_input = crate::domain::workflow::IntentExecutionInput {
            intent: intent.to_string(),
            inputs,
            volume_id,
            language: serde_json::from_value(serde_json::json!(language)).unwrap_or_default(),
            timeout_seconds,
        };

        let lang = pipeline_input.language;
        let inputs_json = pipeline_input.inputs.to_string();
        let mut input = serde_json::to_value(&pipeline_input).map_err(|e| {
            SealSessionError::InternalError(format!(
                "Failed to serialize IntentExecutionInput: {e}"
            ))
        })?;

        // Inject derived fields so Handlebars templates in the workflow YAML can resolve them
        if let Some(obj) = input.as_object_mut() {
            obj.insert(
                "container_image".to_string(),
                serde_json::json!(lang.container_image()),
            );
            obj.insert(
                "runner".to_string(),
                serde_json::json!(lang.runner_command()),
            );
            obj.insert(
                "language_ext".to_string(),
                serde_json::json!(lang.file_extension()),
            );
            obj.insert("inputs_json".to_string(), serde_json::json!(inputs_json));
        }

        let start_use_case = match &self.start_workflow_execution_use_case {
            Some(uc) => uc,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.execute.intent",
                    "error": "Workflow execution service not configured"
                })));
            }
        };

        match start_use_case
            .start_execution(
                crate::application::start_workflow_execution::StartWorkflowExecutionRequest {
                    workflow_id: "builtin-intent-to-execution".to_string(),
                    input,
                    blackboard: None,
                    version: None,
                    tenant_id: Some(tenant_id),
                    security_context_name: Some("aegis-system-agent-runtime".to_string()),
                    intent: Some(intent.to_string()),
                },
            )
            .await
        {
            Ok(started) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.execute.intent",
                "pipeline_execution_id": started.execution_id,
                "status": "started",
                "stream_url": format!("/v1/executions/{}/stream", started.execution_id)
            }))),
            Err(e) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.execute.intent",
                "error": format!("Failed to start intent execution pipeline: {e:#}")
            }))),
        }
    }

    /// Handle `aegis.execute.status` — check pipeline execution status.
    pub(super) async fn invoke_aegis_execute_status_tool(
        &self,
        args: &Value,
    ) -> Result<ToolInvocationResult, SealSessionError> {
        let execution_id_str = args
            .get("pipeline_execution_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SealSessionError::InvalidArguments(
                    "aegis.execute.status requires 'pipeline_execution_id' string".to_string(),
                )
            })?;

        let tenant_id = Self::resolve_tenant_arg(args)?;
        let execution_id = crate::domain::execution::ExecutionId(
            uuid::Uuid::parse_str(execution_id_str).map_err(|error| {
                SealSessionError::InvalidArguments(format!(
                    "aegis.execute.status: invalid pipeline_execution_id UUID: {error}"
                ))
            })?,
        );

        let repo = match &self.workflow_execution_repo {
            Some(repo) => repo,
            None => {
                return Ok(ToolInvocationResult::Direct(serde_json::json!({
                    "tool": "aegis.execute.status",
                    "error": "Workflow execution repository not configured"
                })));
            }
        };

        match repo.find_by_id_for_tenant(&tenant_id, execution_id).await {
            Ok(Some(execution)) => {
                let status_str = format!("{:?}", execution.status).to_lowercase();
                let mut response = serde_json::json!({
                    "tool": "aegis.execute.status",
                    "pipeline_execution_id": execution_id_str,
                    "status": status_str,
                    "current_state": execution.current_state.as_str(),
                    "started_at": execution.started_at,
                });

                // Include final_result on completion
                if matches!(
                    execution.status,
                    crate::domain::execution::ExecutionStatus::Completed
                ) {
                    if let Some(result) = execution.blackboard.data().get("final_result") {
                        response["final_result"] = result.clone();
                    }
                }

                // Include reason on failure
                if matches!(
                    execution.status,
                    crate::domain::execution::ExecutionStatus::Failed
                ) {
                    if let Some(reason) = execution.blackboard.data().get("failure_reason") {
                        response["reason"] = reason.clone();
                    }
                }

                Ok(ToolInvocationResult::Direct(response))
            }
            Ok(None) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.execute.status",
                "error": format!("Pipeline execution '{execution_id_str}' not found")
            }))),
            Err(error) => Ok(ToolInvocationResult::Direct(serde_json::json!({
                "tool": "aegis.execute.status",
                "error": format!("Failed to query pipeline execution: {error}")
            }))),
        }
    }
}
