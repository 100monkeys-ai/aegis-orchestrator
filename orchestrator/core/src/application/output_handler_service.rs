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
//! | `Container` | ADR-103 phase 2 — panics at runtime if used |
//! | `McpTool` | ADR-103 phase 2 — panics at runtime if used |
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
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

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
    async fn invoke(
        &self,
        config: &OutputHandlerConfig,
        final_output: &str,
        execution_id: &ExecutionId,
        tenant_id: &TenantId,
    ) -> Result<Option<String>>;
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
        execution_id: &ExecutionId,
        _tenant_id: &TenantId,
    ) -> Result<Option<String>> {
        let handler_type = config.handler_type_name();

        self.event_bus
            .publish_execution_event(ExecutionEvent::OutputHandlerStarted {
                execution_id: *execution_id,
                handler_type: handler_type.to_string(),
            });

        let result = match config {
            OutputHandlerConfig::Agent {
                agent_id,
                input_template,
                timeout_seconds,
                ..
            } => {
                invoke_agent_handler(
                    &self.execution_service,
                    &self.agent_lifecycle_service,
                    agent_id,
                    input_template.as_deref(),
                    final_output,
                    *execution_id,
                    *timeout_seconds,
                )
                .await
            }

            OutputHandlerConfig::Container { .. } => {
                todo!("Container output handler — ADR-103 phase 2")
            }

            OutputHandlerConfig::McpTool { .. } => {
                todo!("McpTool output handler — ADR-103 phase 2")
            }

            OutputHandlerConfig::Webhook {
                url,
                method,
                headers,
                body_template,
                timeout_seconds,
                retry: _,
                ..
            } => {
                invoke_webhook_handler(
                    url,
                    method,
                    headers,
                    body_template.as_deref(),
                    final_output,
                    *timeout_seconds,
                )
                .await
            }
        };

        match &result {
            Ok(output) => {
                self.event_bus
                    .publish_execution_event(ExecutionEvent::OutputHandlerCompleted {
                        execution_id: *execution_id,
                        handler_type: handler_type.to_string(),
                        result: output.clone(),
                    });
            }
            Err(e) => {
                self.event_bus
                    .publish_execution_event(ExecutionEvent::OutputHandlerFailed {
                        execution_id: *execution_id,
                        handler_type: handler_type.to_string(),
                        error: e.to_string(),
                    });
            }
        }

        result
    }
}

/// Spawn a child execution for the named agent and poll until it completes.
async fn invoke_agent_handler(
    execution_service: &Arc<dyn ExecutionService>,
    agent_lifecycle_service: &Arc<dyn AgentLifecycleService>,
    agent_id_str: &str,
    input_template: Option<&str>,
    final_output: &str,
    parent_execution_id: ExecutionId,
    timeout_seconds: Option<u64>,
) -> Result<Option<String>> {
    // Resolve the agent by name or ID. Use the system tenant so global agents are always
    // visible regardless of the execution's tenant context.
    let agents = agent_lifecycle_service
        .list_agents_visible_for_tenant(&TenantId::system())
        .await?;

    let agent = agents
        .into_iter()
        .find(|a| a.name == agent_id_str || a.id.0.to_string() == agent_id_str)
        .ok_or_else(|| anyhow!("Output handler agent '{}' not found", agent_id_str))?;

    // Build the child execution input.
    let input_value = if let Some(template) = input_template {
        // Render the Handlebars template against `{ "output": final_output }`.
        let mut hb = handlebars::Handlebars::new();
        hb.set_strict_mode(false);
        let ctx = serde_json::json!({ "output": final_output });
        let rendered = hb
            .render_template(template, &ctx)
            .unwrap_or_else(|_| final_output.to_string());
        serde_json::Value::String(rendered)
    } else {
        serde_json::Value::String(final_output.to_string())
    };

    let exec_input = ExecutionInput {
        intent: None,
        input: input_value,
    };

    let child_exec_id = execution_service
        .start_child_execution(agent.id, exec_input, parent_execution_id)
        .await?;

    // Poll for completion with configurable timeout (default: 300 seconds).
    let timeout_secs = timeout_seconds.unwrap_or(300);
    let poll_interval_ms = 500u64;
    let max_attempts = (timeout_secs * 1000) / poll_interval_ms;

    for _ in 0..max_attempts {
        let exec = execution_service.get_execution(child_exec_id).await?;
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
) -> Result<Option<String>> {
    // Render the body template if provided.
    let body = if let Some(template) = body_template {
        let mut hb = handlebars::Handlebars::new();
        hb.set_strict_mode(false);
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
