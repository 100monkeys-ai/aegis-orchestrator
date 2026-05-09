// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Output Handler Service (ADR-103)
//!
//! Application service that invokes the declared egress handler after an agent
//! execution completes.  Called by [`crate::application::execution::StandardExecutionService`]
//! immediately after `ExecutionCompleted` is published.
//!
//! ## Supported Variants
//!
//! | Variant | Status |
//! |---------|--------|
//! | `Agent` | Implemented — spawns child execution and polls to completion |
//! | `Webhook` | Implemented — HTTP POST via `reqwest` |
//! | `Container` | ADR-103 phase 2 — returns [`OutputHandlerError::NotYetImplemented`] |
//! | `McpTool` | ADR-103 phase 2 — returns [`OutputHandlerError::NotYetImplemented`] |
//!
//! Deferred variants MUST NOT panic the worker loop; they return a structured
//! [`OutputHandlerError::NotYetImplemented`] that maps to `gRPC UNIMPLEMENTED`
//! (HTTP 501 equivalent) at the daemon boundary and is surfaced on the
//! execution's status record when invoked from the async worker loop.
//!
//! ## Required/Optional Semantics
//!
//! When `required` is `true` on the handler config, an `Err` return from `invoke`
//! causes the caller to mark the execution as failed. When `required` is `false`
//! the caller logs the error but leaves the execution in the `Completed` state.

use crate::application::agent::AgentLifecycleService;
use crate::application::execution::ExecutionService;
use crate::domain::events::ExecutionEvent;
use crate::domain::execution::{ExecutionId, ExecutionInput, ExecutionStatus};
use crate::domain::output_handler::OutputHandlerConfig;
use crate::domain::shared_kernel::TenantId;
use anyhow::anyhow;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

/// Service-layer errors for [`OutputHandlerService`].
///
/// These map onto gRPC/HTTP status codes at the daemon boundary:
///
/// | Variant | gRPC | HTTP equivalent |
/// |---------|------|-----------------|
/// | [`OutputHandlerError::NotYetImplemented`] | `UNIMPLEMENTED` | `501 Not Implemented` |
/// | [`OutputHandlerError::Failed`]            | `INTERNAL`       | `500 Internal Server Error` |
#[derive(Debug, Error)]
pub enum OutputHandlerError {
    /// Handler variant is recognized but its implementation is deferred to a
    /// later ADR-103 phase. Returned by `Container` and `McpTool` variants
    /// today. MUST map to `501 Not Implemented` / gRPC `UNIMPLEMENTED` at the
    /// daemon boundary — never a panic.
    #[error("output handler not yet implemented: {0}")]
    NotYetImplemented(String),

    /// Handler invocation failed at runtime (network error, agent resolution
    /// failure, timeout, non-success HTTP status, etc.). Maps to gRPC
    /// `INTERNAL` / HTTP `500`.
    #[error("output handler failed: {0}")]
    Failed(String),
}

impl From<anyhow::Error> for OutputHandlerError {
    fn from(e: anyhow::Error) -> Self {
        OutputHandlerError::Failed(e.to_string())
    }
}

/// Primary interface for dispatching output handlers after execution completes.
#[async_trait]
pub trait OutputHandlerService: Send + Sync {
    /// Invoke the configured egress handler and return the handler's output, if any.
    ///
    /// - For `Agent` handlers the return value is the child execution's final output.
    /// - For `Webhook` handlers the return value is the HTTP response body.
    /// - `None` is returned when the handler produces no meaningful output.
    ///
    /// # Errors
    ///
    /// Returns an error if the handler invocation fails. The caller decides whether
    /// to propagate the failure based on [`OutputHandlerConfig::is_required`].
    /// Invoke the configured output handler.
    ///
    /// `parent_execution_id` is the agent execution that produced `final_output`.
    /// When `None` (e.g. ContainerRun or ParallelAgents states that have no single
    /// agent execution), Agent-type handlers spawn a standalone execution instead
    /// of a child execution.
    async fn invoke(
        &self,
        config: &OutputHandlerConfig,
        final_output: &str,
        parent_execution_id: Option<&ExecutionId>,
        tenant_id: &TenantId,
        intent: Option<&str>,
    ) -> Result<Option<String>, OutputHandlerError>;
}

/// Production implementation of [`OutputHandlerService`].
pub struct StandardOutputHandlerService {
    execution_service: Arc<dyn ExecutionService>,
    agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
    event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
}

impl StandardOutputHandlerService {
    pub fn new(
        execution_service: Arc<dyn ExecutionService>,
        agent_lifecycle_service: Arc<dyn AgentLifecycleService>,
        event_bus: Arc<crate::infrastructure::event_bus::EventBus>,
    ) -> Self {
        Self {
            execution_service,
            agent_lifecycle_service,
            event_bus,
        }
    }
}

#[async_trait]
impl OutputHandlerService for StandardOutputHandlerService {
    async fn invoke(
        &self,
        config: &OutputHandlerConfig,
        final_output: &str,
        parent_execution_id: Option<&ExecutionId>,
        tenant_id: &TenantId,
        intent: Option<&str>,
    ) -> Result<Option<String>, OutputHandlerError> {
        let handler_type = config.handler_type_name();

        // Use the parent execution ID for event correlation when available,
        // otherwise generate a synthetic one for observability.
        let correlation_id = parent_execution_id
            .copied()
            .unwrap_or_else(ExecutionId::new);

        self.event_bus
            .publish_execution_event(ExecutionEvent::OutputHandlerStarted {
                execution_id: correlation_id,
                handler_type: handler_type.to_string(),
            });

        let result: Result<Option<String>, OutputHandlerError> = match config {
            OutputHandlerConfig::Agent {
                agent_id,
                input_template,
                timeout_seconds,
                ..
            } => invoke_agent_handler(
                &self.execution_service,
                &self.agent_lifecycle_service,
                agent_id,
                input_template.as_deref(),
                final_output,
                parent_execution_id.copied(),
                *timeout_seconds,
                intent,
                tenant_id,
            )
            .await
            .map_err(OutputHandlerError::from),

            // ADR-103 phase 2: Container and McpTool variants are declared in
            // the domain type for forward compatibility but their executors
            // are not yet implemented. Return a structured error instead of
            // a `todo!()` panic so user-supplied agent YAMLs referencing
            // these variants fail cleanly at the execution boundary rather
            // than crashing the worker loop.
            OutputHandlerConfig::Container { .. } => Err(OutputHandlerError::NotYetImplemented(
                "Container output handler is deferred to ADR-103 phase 2".into(),
            )),

            OutputHandlerConfig::McpTool { .. } => Err(OutputHandlerError::NotYetImplemented(
                "McpTool output handler is deferred to ADR-103 phase 2".into(),
            )),

            OutputHandlerConfig::Webhook {
                url,
                method,
                headers,
                body_template,
                timeout_seconds,
                retry: _,
                ..
            } => invoke_webhook_handler(
                url,
                method,
                headers,
                body_template.as_deref(),
                final_output,
                *timeout_seconds,
            )
            .await
            .map_err(OutputHandlerError::from),
        };

        match &result {
            Ok(output) => {
                self.event_bus
                    .publish_execution_event(ExecutionEvent::OutputHandlerCompleted {
                        execution_id: correlation_id,
                        handler_type: handler_type.to_string(),
                        result: output.clone(),
                    });
            }
            Err(e) => {
                self.event_bus
                    .publish_execution_event(ExecutionEvent::OutputHandlerFailed {
                        execution_id: correlation_id,
                        handler_type: handler_type.to_string(),
                        error: e.to_string(),
                    });
            }
        }

        result
    }
}

/// Spawn an execution for the named agent and poll until it completes.
///
/// When `parent_execution_id` is `Some`, the execution is spawned as a child of
/// that parent (preserving hierarchy). When `None` (e.g. output handler on a
/// ContainerRun state that has no agent execution), a standalone root execution
/// is created instead.
#[allow(clippy::too_many_arguments)]
async fn invoke_agent_handler(
    execution_service: &Arc<dyn ExecutionService>,
    agent_lifecycle_service: &Arc<dyn AgentLifecycleService>,
    agent_id_str: &str,
    input_template: Option<&str>,
    final_output: &str,
    parent_execution_id: Option<ExecutionId>,
    timeout_seconds: Option<u64>,
    intent: Option<&str>,
    tenant_id: &TenantId,
) -> anyhow::Result<Option<String>> {
    // Resolve the agent by name or ID. Use the system tenant so global agents are always
    // visible regardless of the execution's tenant context.
    let agents = agent_lifecycle_service
        .list_agents_visible_for_tenant(&TenantId::system())
        .await?;

    let agent = agents
        .into_iter()
        .find(|a| a.name == agent_id_str || a.id.0.to_string() == agent_id_str)
        .ok_or_else(|| anyhow!("Output handler agent '{}' not found", agent_id_str))?;

    // Build the execution input. The synthetic payload must include `tenant_id`
    // because `StandardExecutionService::resolve_tenant_from_payload` requires it,
    // and the rendered prompt text is routed through `workflow_input` so
    // `extract_user_input` returns it as a `Value::String` for `{{input}}` rendering.
    let rendered_input = if let Some(template) = input_template {
        // Render the Handlebars template against `{ "output": final_output, "intent": ... }`.
        let mut hb = handlebars::Handlebars::new();
        hb.set_strict_mode(false);
        // Disable HTML escaping: the rendered template is fed to an LLM as plain
        // text, so backticks, quotes, and angle brackets in `{{output}}` /
        // `{{intent}}` must reach the agent verbatim. Mirrors the configuration
        // in `PromptTemplateEngine`.
        hb.register_escape_fn(handlebars::no_escape);
        let ctx = serde_json::json!({
            "output": final_output,
            "intent": intent.unwrap_or(""),
        });
        hb.render_template(template, &ctx)
            .unwrap_or_else(|_| final_output.to_string())
    } else {
        final_output.to_string()
    };

    let input_value = serde_json::json!({
        "tenant_id": tenant_id.to_string(),
        "workflow_input": rendered_input,
    });

    let exec_input = ExecutionInput {
        intent: None,
        input: input_value,
        workspace_volume_id: None,
        workspace_volume_mount_path: None,
        workspace_remote_path: None,
        workflow_execution_id: None,
        attachments: Vec::new(),
    };

    let child_exec_id = if let Some(parent_id) = parent_execution_id {
        execution_service
            .start_child_execution(agent.id, exec_input, parent_id)
            .await?
    } else {
        execution_service
            .start_execution(
                agent.id,
                exec_input,
                "aegis-system-operator".to_string(),
                None,
            )
            .await?
    };

    // Poll for completion with configurable timeout (default: 300 seconds).
    let timeout_secs = timeout_seconds.unwrap_or(300);
    let poll_interval_ms = 500u64;
    let max_attempts = (timeout_secs * 1000) / poll_interval_ms;

    for _ in 0..max_attempts {
        let exec = execution_service
            .get_execution_for_tenant(tenant_id, child_exec_id)
            .await?;
        match exec.status {
            ExecutionStatus::Completed => {
                let output = exec.iterations().last().and_then(|it| it.output.clone());
                return Ok(output);
            }
            ExecutionStatus::Failed | ExecutionStatus::Cancelled => {
                return Err(anyhow!(
                    "Output handler agent execution {} failed or was cancelled",
                    child_exec_id
                ));
            }
            _ => {
                tokio::time::sleep(Duration::from_millis(poll_interval_ms)).await;
            }
        }
    }

    Err(anyhow!(
        "Output handler agent execution timed out after {} seconds",
        timeout_secs
    ))
}

/// POST final output to a webhook URL using `reqwest`.
async fn invoke_webhook_handler(
    url: &str,
    method: &str,
    headers: &std::collections::HashMap<String, String>,
    body_template: Option<&str>,
    final_output: &str,
    timeout_seconds: Option<u64>,
) -> anyhow::Result<Option<String>> {
    // Render the body template if provided.
    let body = if let Some(template) = body_template {
        let mut hb = handlebars::Handlebars::new();
        hb.set_strict_mode(false);
        // Disable HTML escaping: webhook bodies are typically JSON or plain
        // text and must not have backticks/quotes/angle brackets mangled into
        // HTML entities.
        hb.register_escape_fn(handlebars::no_escape);
        let ctx = serde_json::json!({ "output": final_output });
        hb.render_template(template, &ctx)
            .unwrap_or_else(|_| final_output.to_string())
    } else {
        final_output.to_string()
    };

    let timeout = Duration::from_secs(timeout_seconds.unwrap_or(30));

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| anyhow!("Failed to build HTTP client: {}", e))?;

    let mut request_builder = match method.to_uppercase().as_str() {
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "PATCH" => client.patch(url),
        other => {
            return Err(anyhow!(
                "Unsupported webhook HTTP method '{}': expected POST, PUT, or PATCH",
                other
            ));
        }
    };

    for (key, value) in headers {
        request_builder = request_builder.header(key.as_str(), value.as_str());
    }

    let response = request_builder
        .header("Content-Type", "text/plain")
        .body(body)
        .send()
        .await
        .map_err(|e| anyhow!("Webhook delivery failed: {}", e))?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Webhook returned non-success status: {}",
            response.status()
        ));
    }

    let response_body = response.text().await.unwrap_or_default();

    Ok(if response_body.is_empty() {
        None
    } else {
        Some(response_body)
    })
}

#[cfg(test)]
mod tests {
    //! Tests for the synthetic execution payload built by `invoke_agent_handler`.
    //!
    //! These tests exist to lock in the regression fix for the output handler bug
    //! where `StandardExecutionService::resolve_tenant_from_payload` rejected the
    //! synthetic input because the payload was a bare JSON string with no
    //! `tenant_id` field. The current contract is that the synthetic payload is a
    //! JSON object containing `tenant_id` (so tenant resolution succeeds) and
    //! `workflow_input` (so the formatter agent receives the rendered prompt
    //! through the same channel as workflow-driven executions).
    use super::*;
    use crate::application::agent::AgentLifecycleService;
    use crate::domain::agent::{
        Agent, AgentId, AgentManifest, AgentSpec, AgentStatus, ImagePullPolicy, ManifestMetadata,
        RuntimeConfig, TaskConfig,
    };
    use crate::domain::execution::{Execution, ExecutionStatus, Iteration, IterationStatus};
    use crate::domain::shared_kernel::TenantId;
    use crate::infrastructure::event_bus::DomainEvent;
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn test_agent(name: &str) -> Agent {
        let manifest = AgentManifest {
            api_version: "100monkeys.ai/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: ManifestMetadata {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: None,
                labels: std::collections::HashMap::new(),
                annotations: std::collections::HashMap::new(),
            },
            spec: AgentSpec {
                runtime: RuntimeConfig {
                    language: Some("python".to_string()),
                    version: Some("3.11".to_string()),
                    image: None,
                    image_pull_policy: ImagePullPolicy::IfNotPresent,
                    isolation: "inherit".to_string(),
                    model: "default".to_string(),
                    temperature: None,
                },
                task: Some(TaskConfig {
                    instruction: Some("noop".to_string()),
                    prompt_template: None,
                    input_data: None,
                }),
                context: vec![],
                execution: None,
                security: None,
                schedule: None,
                tools: vec![],
                env: std::collections::HashMap::new(),
                volumes: vec![],
                advanced: None,
                input_schema: None,
                security_context: None,
                output_handler: None,
            },
        };
        let now = Utc::now();
        Agent {
            id: AgentId::new(),
            tenant_id: TenantId::default(),
            scope: crate::domain::agent::AgentScope::default(),
            name: name.to_string(),
            manifest,
            status: AgentStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }

    /// Lifecycle service that returns a single test agent matching `agent_name`.
    struct OneAgentLifecycle {
        agent: Agent,
    }

    #[async_trait]
    impl AgentLifecycleService for OneAgentLifecycle {
        async fn deploy_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _manifest: AgentManifest,
            _force: bool,
            _scope: crate::domain::agent::AgentScope,
            _caller_identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<AgentId> {
            unimplemented!("not exercised")
        }
        async fn get_agent_for_tenant(&self, _tenant_id: &TenantId, _id: AgentId) -> Result<Agent> {
            unimplemented!("not exercised")
        }
        async fn update_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: AgentId,
            _manifest: AgentManifest,
        ) -> Result<()> {
            unimplemented!("not exercised")
        }
        async fn delete_agent_for_tenant(&self, _tenant_id: &TenantId, _id: AgentId) -> Result<()> {
            unimplemented!("not exercised")
        }
        async fn list_agents_for_tenant(&self, _tenant_id: &TenantId) -> Result<Vec<Agent>> {
            Ok(vec![self.agent.clone()])
        }
        async fn list_agents_visible_for_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<Vec<Agent>> {
            Ok(vec![self.agent.clone()])
        }
        async fn lookup_agent_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _name: &str,
        ) -> Result<Option<AgentId>> {
            Ok(Some(self.agent.id))
        }
        async fn lookup_agent_visible_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _name: &str,
        ) -> Result<Option<AgentId>> {
            Ok(Some(self.agent.id))
        }
        async fn list_versions_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _agent_id: AgentId,
        ) -> Result<Vec<crate::domain::repository::AgentVersion>> {
            Ok(vec![])
        }
        async fn lookup_agent_for_tenant_with_version(
            &self,
            _tenant_id: &TenantId,
            _name: &str,
            _version: &str,
        ) -> Result<Option<AgentId>> {
            Ok(Some(self.agent.id))
        }
    }

    /// Captures the `ExecutionInput` passed to `start_execution` /
    /// `start_child_execution`, then reports the spawned execution as
    /// `Completed` so the polling loop in `invoke_agent_handler` exits
    /// immediately.
    struct CapturingExecutionService {
        captured: Mutex<Vec<ExecutionInput>>,
        exec_id: ExecutionId,
        tenant_id: TenantId,
        agent_id: Mutex<Option<AgentId>>,
    }

    impl CapturingExecutionService {
        fn new(tenant_id: TenantId) -> Self {
            Self {
                captured: Mutex::new(Vec::new()),
                exec_id: ExecutionId::new(),
                tenant_id,
                agent_id: Mutex::new(None),
            }
        }

        fn captured_inputs(&self) -> Vec<ExecutionInput> {
            self.captured.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl ExecutionService for CapturingExecutionService {
        async fn start_execution(
            &self,
            agent_id: AgentId,
            input: ExecutionInput,
            _security_context_name: String,
            _identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            *self.agent_id.lock().unwrap() = Some(agent_id);
            self.captured.lock().unwrap().push(input);
            Ok(self.exec_id)
        }

        async fn start_execution_with_id(
            &self,
            execution_id: ExecutionId,
            agent_id: AgentId,
            input: ExecutionInput,
            _security_context_name: String,
            _identity: Option<&crate::domain::iam::UserIdentity>,
        ) -> Result<ExecutionId> {
            *self.agent_id.lock().unwrap() = Some(agent_id);
            self.captured.lock().unwrap().push(input);
            Ok(execution_id)
        }

        async fn start_child_execution(
            &self,
            agent_id: AgentId,
            input: ExecutionInput,
            _parent_execution_id: ExecutionId,
        ) -> Result<ExecutionId> {
            *self.agent_id.lock().unwrap() = Some(agent_id);
            self.captured.lock().unwrap().push(input);
            Ok(self.exec_id)
        }

        async fn get_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            id: ExecutionId,
        ) -> Result<Execution> {
            let agent_id = self.agent_id.lock().unwrap().unwrap_or(AgentId::new());
            let mut exec = Execution::new_with_id(
                id,
                agent_id,
                ExecutionInput {
                    intent: None,
                    input: serde_json::Value::Null,
                    workspace_volume_id: None,
                    workspace_volume_mount_path: None,
                    workspace_remote_path: None,
                    workflow_execution_id: None,
                    attachments: Vec::new(),
                },
                1,
                "aegis-system-operator".to_string(),
            );
            exec.tenant_id = self.tenant_id.clone();
            exec.status = ExecutionStatus::Completed;
            exec.iterations.push(Iteration {
                number: 1,
                status: IterationStatus::Success,
                action: "format".to_string(),
                output: Some("formatted-output".to_string()),
                validation_results: None,
                error: None,
                code_changes: None,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                llm_interactions: Vec::new(),
                trajectory: None,
                policy_violations: Vec::new(),
            });
            Ok(exec)
        }

        async fn get_execution_unscoped(&self, id: ExecutionId) -> Result<Execution> {
            self.get_execution_for_tenant(&self.tenant_id, id).await
        }

        async fn get_iterations_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _exec_id: ExecutionId,
        ) -> Result<Vec<Iteration>> {
            Ok(Vec::new())
        }

        async fn cancel_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<()> {
            unimplemented!("not exercised")
        }

        async fn stream_execution(
            &self,
            _id: ExecutionId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<crate::domain::events::ExecutionEvent>> + Send>>>
        {
            unimplemented!("not exercised")
        }

        async fn stream_agent_events(
            &self,
            _id: AgentId,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<DomainEvent>> + Send>>> {
            unimplemented!("not exercised")
        }

        async fn list_executions_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _agent_id: Option<AgentId>,
            _workflow_id: Option<crate::domain::workflow::WorkflowId>,
            _limit: usize,
        ) -> Result<Vec<Execution>> {
            Ok(Vec::new())
        }

        async fn delete_execution_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<()> {
            unimplemented!("not exercised")
        }

        async fn record_llm_interaction(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _interaction: crate::domain::execution::LlmInteraction,
        ) -> Result<()> {
            Ok(())
        }

        async fn store_iteration_trajectory(
            &self,
            _execution_id: ExecutionId,
            _iteration: u8,
            _trajectory: Vec<crate::domain::execution::TrajectoryStep>,
        ) -> Result<()> {
            Ok(())
        }
    }

    /// Regression test for the bug where `OutputHandlerService::invoke_agent_handler`
    /// produced an `ExecutionInput.input` of `Value::String(...)` with no
    /// `tenant_id` field, causing `StandardExecutionService::start_execution` to
    /// reject the synthetic payload with "Missing required tenant_id in
    /// execution payload" and mark the parent workflow as Failed even though
    /// the upstream EXECUTE_CODE state had succeeded (observed on workflow
    /// execution f11d1b1e-7e38-4f99-977b-a0450e9e5a44).
    ///
    /// The fix wraps the rendered prompt in a JSON object carrying `tenant_id`
    /// (so tenant resolution succeeds) and `workflow_input` (so
    /// `extract_user_input` returns the prompt text verbatim for
    /// `{{input}}` rendering).
    #[tokio::test]
    async fn invoke_agent_handler_embeds_tenant_id_and_workflow_input_in_synthetic_payload() {
        let agent_name = "test-formatter";
        let agent = test_agent(agent_name);
        let lifecycle: Arc<dyn AgentLifecycleService> = Arc::new(OneAgentLifecycle { agent });
        let tenant = TenantId::from_string("u-test-tenant-12345").expect("valid tenant");

        let capturing = Arc::new(CapturingExecutionService::new(tenant.clone()));
        let exec_svc: Arc<dyn ExecutionService> = capturing.clone();

        let _ = invoke_agent_handler(
            &exec_svc,
            &lifecycle,
            agent_name,
            Some("Format this: {{output}} (intent: {{intent}})"),
            "raw-final-output",
            None,
            Some(5),
            Some("summarize"),
            &tenant,
        )
        .await
        .expect("invoke_agent_handler should succeed");

        let inputs = capturing.captured_inputs();
        assert_eq!(inputs.len(), 1, "exactly one execution should be started");
        let captured = &inputs[0];

        let payload = captured
            .input
            .as_object()
            .expect("synthetic payload must be a JSON object, not a bare string");

        let captured_tenant = payload
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .expect("synthetic payload must include tenant_id (regression)");
        assert_eq!(
            captured_tenant,
            tenant.to_string(),
            "tenant_id in synthetic payload must match the tenant passed into invoke_agent_handler"
        );

        let workflow_input = payload
            .get("workflow_input")
            .and_then(|v| v.as_str())
            .expect("synthetic payload must include workflow_input carrying the rendered prompt");
        assert_eq!(
            workflow_input, "Format this: raw-final-output (intent: summarize)",
            "workflow_input must contain the Handlebars-rendered prompt text"
        );
    }

    /// Regression: the local Handlebars instance in `invoke_agent_handler`
    /// must NOT HTML-escape the rendered template. The rendered string is
    /// fed to a downstream LLM agent as plain text, so backticks, double
    /// quotes, and angle brackets in `{{output}}` / `{{intent}}` must reach
    /// the agent verbatim. Previously, the Handlebars default escape function
    /// turned ``` ``` ``` into `&#x60;&#x60;&#x60;` and `"status"` into
    /// `&quot;status&quot;`, corrupting the formatter agent's input.
    #[tokio::test]
    async fn invoke_agent_handler_does_not_html_escape_rendered_template() {
        let agent_name = "test-formatter";
        let agent = test_agent(agent_name);
        let lifecycle: Arc<dyn AgentLifecycleService> = Arc::new(OneAgentLifecycle { agent });
        let tenant = TenantId::from_string("u-test-tenant-12345").expect("valid tenant");

        let capturing = Arc::new(CapturingExecutionService::new(tenant.clone()));
        let exec_svc: Arc<dyn ExecutionService> = capturing.clone();

        // Output containing every character that Handlebars' default
        // html_escape would mangle: backticks, double quotes, `<`, `>`, `&`.
        let raw_output = "```json\n{\"status\":\"ok\",\"x\":1,\"html\":\"<b>&amp;</b>\"}\n```";

        let _ = invoke_agent_handler(
            &exec_svc,
            &lifecycle,
            agent_name,
            Some("Render: {{output}} ({{intent}})"),
            raw_output,
            None,
            Some(5),
            Some("format & emit"),
            &tenant,
        )
        .await
        .expect("invoke_agent_handler should succeed");

        let inputs = capturing.captured_inputs();
        assert_eq!(inputs.len(), 1, "exactly one execution should be started");
        let captured = &inputs[0];

        let workflow_input = captured
            .input
            .as_object()
            .and_then(|p| p.get("workflow_input"))
            .and_then(|v| v.as_str())
            .expect("synthetic payload must include workflow_input");

        // Literal characters from the raw output must survive rendering.
        assert!(
            workflow_input.contains("```"),
            "rendered template must preserve raw backticks, got: {workflow_input}"
        );
        assert!(
            workflow_input.contains("\"status\""),
            "rendered template must preserve raw double quotes, got: {workflow_input}"
        );
        assert!(
            workflow_input.contains("<b>"),
            "rendered template must preserve raw angle brackets, got: {workflow_input}"
        );
        // Intent containing `&` must also survive verbatim.
        assert!(
            workflow_input.contains("format & emit"),
            "rendered template must preserve raw ampersand from intent, got: {workflow_input}"
        );

        // None of the HTML-entity escapes Handlebars would emit may appear,
        // unless they were literally in the input. (`&amp;` was in the raw
        // input, so it is allowed; the others were not.)
        assert!(
            !workflow_input.contains("&#x60;"),
            "backticks must not be HTML-escaped, got: {workflow_input}"
        );
        assert!(
            !workflow_input.contains("&quot;"),
            "double quotes must not be HTML-escaped, got: {workflow_input}"
        );
        assert!(
            !workflow_input.contains("&lt;"),
            "`<` must not be HTML-escaped, got: {workflow_input}"
        );
        assert!(
            !workflow_input.contains("&gt;"),
            "`>` must not be HTML-escaped, got: {workflow_input}"
        );
        // The intent's literal `&` must not be escaped to `&amp;`. The raw
        // output had a literal `&amp;` in it, so we count occurrences: the
        // input had exactly one `&amp;`, and the renderer must not introduce
        // additional `&amp;` sequences from the literal `&` in the intent.
        assert_eq!(
            workflow_input.matches("&amp;").count(),
            1,
            "no extra `&amp;` entities may be introduced by the renderer, got: {workflow_input}"
        );
    }
}
