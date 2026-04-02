// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! HTTP client for communicating with daemon API
//!
//! # Architecture
//!
//! - **Layer:** Interface / Presentation Layer
//! - **Purpose:** Implements internal responsibilities for client

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_stream::StreamExt;
use tracing::info;
use uuid::Uuid;

use aegis_orchestrator_core::domain::events::CorrelatedActivityEvent;
use aegis_orchestrator_sdk::AgentManifest;

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkflowListResponse {
    Wrapped { workflows: Vec<serde_json::Value> },
    Bare(Vec<serde_json::Value>),
}

#[derive(Debug, Clone)]
pub struct DaemonClient {
    client: Client,
    base_url: String,
}

impl DaemonClient {
    pub fn new(host: &str, port: u16) -> Result<Self> {
        let client = Client::builder()
            // No global timeout for CLI client as we need long-lived streams
            .build()
            .context("Failed to create HTTP client")?;

        let base_url = if host.starts_with("http://") || host.starts_with("https://") {
            format!("{host}:{port}")
        } else {
            format!("http://{host}:{port}")
        };

        Ok(Self { client, base_url })
    }

    pub async fn deploy_agent(&self, manifest: AgentManifest, force: bool) -> Result<Uuid> {
        let url = if force {
            format!("{}/v1/agents?force=true", self.base_url)
        } else {
            format!("{}/v1/agents", self.base_url)
        };
        let response = self
            .client
            .post(url)
            .json(&manifest)
            .send()
            .await
            .context("Failed to deploy agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to deploy agent: {error_text}");
        }

        #[derive(Deserialize)]
        struct DeployResponse {
            agent_id: Uuid,
        }

        let deploy_response: DeployResponse = response
            .json()
            .await
            .context("Failed to parse deploy response")?;

        Ok(deploy_response.agent_id)
    }

    pub async fn execute_agent(
        &self,
        agent_id: Uuid,
        input: serde_json::Value,
        context_overrides: Option<serde_json::Value>,
        version: Option<&str>,
    ) -> Result<Uuid> {
        #[derive(Serialize)]
        struct ExecuteRequest {
            input: serde_json::Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            context_overrides: Option<serde_json::Value>,
        }

        let mut url = format!("{}/v1/agents/{}/execute", self.base_url, agent_id);
        if let Some(ver) = version {
            url.push_str(&format!("?version={ver}"));
        }

        let response = self
            .client
            .post(url)
            .json(&ExecuteRequest {
                input,
                context_overrides,
            })
            .send()
            .await
            .context("Failed to execute agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to execute agent: {error_text}");
        }

        #[derive(Deserialize)]
        struct ExecuteResponse {
            execution_id: Uuid,
        }

        let exec_response: ExecuteResponse = response
            .json()
            .await
            .context("Failed to parse execute response")?;

        Ok(exec_response.execution_id)
    }

    pub async fn get_execution(&self, execution_id: Uuid) -> Result<ExecutionInfo> {
        let response = self
            .client
            .get(format!("{}/v1/executions/{}", self.base_url, execution_id))
            .send()
            .await
            .context("Failed to get execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get execution: {error_text}");
        }

        response
            .json()
            .await
            .context("Failed to parse execution response")
    }

    pub async fn cancel_execution(&self, execution_id: Uuid) -> Result<()> {
        let response = self
            .client
            .post(format!(
                "{}/v1/executions/{}/cancel",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to cancel execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to cancel execution: {error_text}");
        }

        Ok(())
    }

    pub async fn list_executions(
        &self,
        agent_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ExecutionInfo>> {
        let mut url = format!("{}/v1/executions?limit={}", self.base_url, limit);
        if let Some(aid) = agent_id {
            url.push_str(&format!("&agent_id={aid}"));
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to list executions")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list executions: {error_text}");
        }

        response
            .json()
            .await
            .context("Failed to parse executions response")
    }

    pub async fn stream_logs(
        &self,
        execution_id: Uuid,
        follow: bool,
        errors_only: bool,
        verbose: bool,
    ) -> Result<()> {
        let mut url = format!("{}/v1/executions/{}/events", self.base_url, execution_id);
        if follow {
            url.push_str("?follow=true");
        } else {
            url.push_str("?follow=false");
        }
        if verbose {
            url.push_str("&verbose=true");
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to event stream")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stream logs: {error_text}");
        }

        stream_correlated_events(response, errors_only, verbose).await
    }

    pub async fn stream_agent_logs(
        &self,
        agent_id: Uuid,
        follow: bool,
        errors_only: bool,
        verbose: bool,
    ) -> Result<()> {
        let mut url = format!("{}/v1/agents/{}/events", self.base_url, agent_id);
        if follow {
            url.push_str("?follow=true");
        } else {
            url.push_str("?follow=false");
        }
        if verbose {
            url.push_str("&verbose=true");
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to agent event stream")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stream agent logs: {error_text}");
        }

        stream_correlated_events(response, errors_only, verbose).await
    }

    pub async fn delete_execution(&self, execution_id: Uuid) -> Result<()> {
        let response = self
            .client
            .delete(format!("{}/v1/executions/{}", self.base_url, execution_id))
            .send()
            .await
            .context("Failed to delete execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete execution: {error_text}");
        }

        Ok(())
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let response = self
            .client
            .get(format!("{}/v1/agents", self.base_url))
            .send()
            .await
            .context("Failed to list agents")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list agents: {error_text}");
        }

        response
            .json()
            .await
            .context("Failed to parse agents response")
    }

    pub async fn get_agent(&self, agent_id: Uuid) -> Result<AgentManifest> {
        let response = self
            .client
            .get(format!("{}/v1/agents/{}", self.base_url, agent_id))
            .send()
            .await
            .context("Failed to get agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get agent: {error_text}");
        }

        #[derive(Debug, Deserialize)]
        struct GetAgentResponse {
            manifest: AgentManifest,
        }

        let wrapper: GetAgentResponse = response
            .json()
            .await
            .context("Failed to parse agent manifest")?;
        Ok(wrapper.manifest)
    }

    pub async fn delete_agent(&self, agent_id: Uuid) -> Result<()> {
        let response = self
            .client
            .delete(format!("{}/v1/agents/{}", self.base_url, agent_id))
            .send()
            .await
            .context("Failed to delete agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete agent: {error_text}");
        }

        Ok(())
    }
    pub async fn lookup_agent(&self, name: &str) -> Result<Option<Uuid>> {
        let response = self
            .client
            .get(format!("{}/v1/agents/lookup/{}", self.base_url, name))
            .send()
            .await
            .context("Failed to lookup agent")?;

        if response.status() == 404 {
            return Ok(None);
        }

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to lookup agent: {error_text}");
        }

        #[derive(Deserialize)]
        struct LookupResponse {
            id: Uuid,
        }

        let lookup_response: LookupResponse = response
            .json()
            .await
            .context("Failed to parse lookup response")?;

        Ok(Some(lookup_response.id))
    }

    pub async fn lookup_workflow(&self, name: &str) -> Result<Option<Uuid>> {
        let workflows = self.list_workflows().await?;
        Ok(workflows.iter().find_map(|workflow| {
            let workflow_name = workflow.get("name")?.as_str()?;
            if workflow_name == name {
                workflow
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|raw| Uuid::parse_str(raw).ok())
            } else {
                None
            }
        }))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExecutionInfo {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub status: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentInfo {
    pub id: Uuid,
    pub name: String,
    pub version: String,
    pub description: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkflowExecutionInfo {
    pub execution_id: Uuid,
    pub workflow_id: Uuid,
    #[serde(default)]
    pub workflow_name: Option<String>,
    pub status: String,
    #[serde(default)]
    pub current_state: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub last_transition_at: Option<String>,
    #[serde(default)]
    pub temporal_workflow_id: Option<String>,
    #[serde(default)]
    pub temporal_run_id: Option<String>,
    #[serde(default)]
    pub blackboard: Option<Value>,
    #[serde(default)]
    pub state_outputs: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkflowLogEvent {
    pub execution_id: Uuid,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_name: Option<String>,
    pub event_type: String,
    pub message: String,
    #[serde(default)]
    pub state_name: Option<String>,
    #[serde(default)]
    pub iteration_number: Option<u8>,
    pub timestamp: String,
    #[serde(default)]
    pub details: Value,
    #[serde(default)]
    pub temporal_workflow_id: Option<String>,
    #[serde(default)]
    pub temporal_run_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkflowLogOptions {
    pub transitions_only: bool,
    pub errors_only: bool,
    pub verbose: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkflowLogsResponse {
    events: Vec<WorkflowLogEvent>,
}

async fn stream_correlated_events(
    response: reqwest::Response,
    errors_only: bool,
    verbose: bool,
) -> Result<()> {
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read event stream chunk")?;
        let text = String::from_utf8_lossy(&chunk);

        for line in text.lines() {
            if let Some(json_str) = line.strip_prefix("data: ") {
                if let Ok(event) = serde_json::from_str::<CorrelatedActivityEvent>(json_str) {
                    if errors_only && !is_error_event(&event) {
                        continue;
                    }
                    print_event(&event, verbose);
                }
            }
        }
    }

    Ok(())
}

async fn stream_workflow_events(
    response: reqwest::Response,
    options: WorkflowLogOptions,
) -> Result<()> {
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read workflow event stream chunk")?;
        let text = String::from_utf8_lossy(&chunk);

        for line in text.lines() {
            if let Some(json_str) = line.strip_prefix("data: ") {
                if let Ok(event) = serde_json::from_str::<WorkflowLogEvent>(json_str) {
                    if should_skip_workflow_event(&event, options) {
                        continue;
                    }
                    info!(
                        execution_id = %event.execution_id,
                        event_type = %event.event_type,
                        "{}",
                        format_workflow_log_event(&event, options.verbose).trim_end()
                    );
                }
            }
        }
    }

    Ok(())
}

fn should_skip_workflow_event(event: &WorkflowLogEvent, options: WorkflowLogOptions) -> bool {
    (options.transitions_only && !is_transition_workflow_event(event))
        || (options.errors_only && !is_error_workflow_event(event))
}

fn is_transition_workflow_event(event: &WorkflowLogEvent) -> bool {
    matches!(
        canonical_event_type(&event.event_type).as_str(),
        "workflow_execution_started"
            | "workflow_state_entered"
            | "workflow_state_exited"
            | "workflow_execution_completed"
            | "workflow_execution_failed"
            | "workflow_execution_cancelled"
    )
}

fn is_error_workflow_event(event: &WorkflowLogEvent) -> bool {
    matches!(
        canonical_event_type(&event.event_type).as_str(),
        "workflow_iteration_failed" | "workflow_execution_failed"
    )
}

pub(crate) fn format_workflow_log_event(event: &WorkflowLogEvent, verbose: bool) -> String {
    let mut output = format!("[{}] {}", event.timestamp, event.message);

    if let Some(iteration) = event.iteration_number {
        output.push_str(&format!(" (iteration {iteration})"));
    }

    if !verbose {
        output.push('\n');
        return output;
    }

    output.push('\n');
    if let Some(workflow_name) = &event.workflow_name {
        output.push_str(&format!("  Workflow:   {workflow_name}\n"));
    }
    if let Some(workflow_id) = event.workflow_id {
        output.push_str(&format!("  Workflow ID: {workflow_id}\n"));
    }
    output.push_str(&format!(
        "  Event:      {}\n",
        canonical_event_type(&event.event_type)
    ));
    if let Some(state_name) = &event.state_name {
        output.push_str(&format!("  State:      {state_name}\n"));
    }
    if let Some(iteration) = event.iteration_number {
        output.push_str(&format!("  Iteration:  {iteration}\n"));
    }
    if let Some(temporal_workflow_id) = &event.temporal_workflow_id {
        output.push_str(&format!("  Temporal workflow: {temporal_workflow_id}\n"));
    }
    if let Some(temporal_run_id) = &event.temporal_run_id {
        output.push_str(&format!("  Temporal run:      {temporal_run_id}\n"));
    }
    if event.details != Value::Null && !event.details.is_null() {
        let rendered = serde_json::to_string_pretty(&event.details)
            .unwrap_or_else(|_| event.details.to_string());
        output.push_str("  Details:\n");
        for line in rendered.lines() {
            output.push_str("    ");
            output.push_str(line);
            output.push('\n');
        }
    }

    output
}

fn is_error_event(event: &CorrelatedActivityEvent) -> bool {
    let event_type = canonical_event_type(&event.event_type);

    matches!(
        event_type.as_str(),
        "iteration_failed"
            | "execution_failed"
            | "execution_timed_out"
            | "workflow_iteration_failed"
            | "workflow_execution_failed"
            | "volume_mount_failed"
            | "volume_quota_exceeded"
            | "filesystem_policy_violation"
            | "path_traversal_blocked"
            | "quota_exceeded"
            | "unauthorized_volume_access"
            | "policy_violation"
            | "invocation_failed"
            | "policy_violation_blocked"
            | "server_failed"
            | "server_unhealthy"
            | "container_run_failed"
            | "image_pull_failed"
            | "classification_failed"
            | "stimulus_rejected"
            | "secret_access_denied"
            | "token_validation_failed"
            | "jwks_cache_refresh_failed"
            | "agent_failed"
    )
}

fn extract_iteration_error_message(event: &CorrelatedActivityEvent) -> String {
    if let Some(msg) = event
        .details
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return msg.to_string();
    }

    if let Some(msg) = event
        .details
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| event.details.get("reason").and_then(Value::as_str))
    {
        return msg.to_string();
    }

    if !event.message.is_empty() {
        return event.message.clone();
    }

    "Unknown error".to_string()
}

fn print_event(event: &CorrelatedActivityEvent, verbose: bool) {
    info!(
        event_type = %event.event_type,
        category = %event.category,
        "{}",
        format_event(event, verbose)
    );
}

fn format_event(event: &CorrelatedActivityEvent, verbose: bool) -> String {
    use colored::Colorize;

    let event_type = canonical_event_type(&event.event_type);
    let timestamp = event.timestamp.to_rfc3339();
    let header = format!("[{timestamp}]").dimmed().to_string();
    let category = format!("[{}]", event.category).dimmed().to_string();

    match event_type.as_str() {
        "console_output" => {
            let stream = event
                .details
                .get("stream")
                .and_then(Value::as_str)
                .unwrap_or("stdout");
            let content = event
                .details
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or(&event.message);
            let prefix = match stream {
                "stderr" => "[STDERR]".red().to_string(),
                "judge" => "[JUDGE]".magenta().bold().to_string(),
                _ => "[STDOUT]".cyan().to_string(),
            };
            format!("{prefix} {}", content.trim_end())
        }
        "llm_interaction" if verbose => {
            let model = event
                .details
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let prompt = event
                .details
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("");
            let response = event
                .details
                .get("response")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!(
                "{header} {category} {} [{model}]\n{}\n{prompt}\n{}\n{response}\n{}",
                "LLM Interaction".purple().bold(),
                "PROMPT:".dimmed(),
                "RESPONSE:".dimmed(),
                "-".repeat(40).dimmed()
            )
        }
        "llm_interaction" => {
            let model = event
                .details
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("{header} {category} {} [{model}]", "LLM".purple())
        }
        "iteration_failed" => {
            let error = extract_iteration_error_message(event);
            format!(
                "{header} {category} {}",
                format!("Iteration failed: {error}").red().bold()
            )
        }
        "execution_failed" | "execution_timed_out" => {
            format!("{header} {category} {}", event.message.red().bold())
        }
        "execution_completed" if !verbose => format!(
            "{header} {category} {}",
            "Execution completed".green().bold()
        ),
        _ if verbose => {
            let base = format!("{header} {category} {}", event.message);
            let pretty_details = format_details(&event.details);
            if pretty_details.is_empty() {
                base
            } else {
                format!("{base}\n{pretty_details}")
            }
        }
        _ => format!("{header} {category} {}", event.message),
    }
}

fn canonical_event_type(event_type: &str) -> String {
    if event_type.bytes().any(|byte| byte.is_ascii_uppercase()) {
        let mut canonical = String::with_capacity(event_type.len() + 4);
        for (index, ch) in event_type.chars().enumerate() {
            if ch.is_ascii_uppercase() {
                if index != 0 {
                    canonical.push('_');
                }
                canonical.push(ch.to_ascii_lowercase());
            } else {
                canonical.push(ch);
            }
        }
        canonical
    } else {
        event_type.to_string()
    }
}

fn format_details(details: &Value) -> String {
    match details {
        Value::Null => String::new(),
        Value::Object(map) if map.is_empty() => String::new(),
        _ => serde_json::to_string_pretty(details).unwrap_or_default(),
    }
}

// ============================================================================
// Workflow Management Methods
// ============================================================================

impl DaemonClient {
    /// Deploy a workflow from a file with optional force overwrite.
    /// Deploy a workflow from a file with optional force overwrite and scope.
    pub async fn deploy_workflow_with_force_and_scope(
        &self,
        file: &std::path::Path,
        force: bool,
        scope: Option<&str>,
    ) -> Result<()> {
        let workflow_yaml =
            std::fs::read_to_string(file).context("Failed to read workflow file")?;
        self.deploy_workflow_manifest_with_force_and_scope(&workflow_yaml, force, scope)
            .await
    }

    /// Deploy a workflow from YAML content with optional force overwrite and scope.
    pub async fn deploy_workflow_manifest_with_force_and_scope(
        &self,
        workflow_yaml: &str,
        force: bool,
        scope: Option<&str>,
    ) -> Result<()> {
        let mut params = Vec::new();
        if force {
            params.push("force=true".to_string());
        }
        if let Some(s) = scope {
            params.push(format!("scope={s}"));
        }
        let query = if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        };
        let url = format!("{}/v1/workflows{query}", self.base_url);

        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/x-yaml")
            .body(workflow_yaml.to_string())
            .send()
            .await
            .context("Failed to deploy workflow")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to deploy workflow: {error_text}");
        }

        Ok(())
    }

    /// Deploy a workflow from YAML content with optional force overwrite.
    pub async fn deploy_workflow_manifest_with_force(
        &self,
        workflow_yaml: &str,
        force: bool,
    ) -> Result<()> {
        self.deploy_workflow_manifest_with_force_and_scope(workflow_yaml, force, None)
            .await
    }

    /// Run a workflow
    pub async fn run_workflow(
        &self,
        name: &str,
        input: serde_json::Value,
        blackboard: Option<serde_json::Value>,
        version: Option<&str>,
    ) -> Result<Uuid> {
        #[derive(Serialize)]
        struct RunRequest {
            input: serde_json::Value,
            #[serde(skip_serializing_if = "Option::is_none")]
            blackboard: Option<serde_json::Value>,
        }

        let mut url = format!("{}/v1/workflows/{}/run", self.base_url, name);
        if let Some(ver) = version {
            url.push_str(&format!("?version={ver}"));
        }

        let response = self
            .client
            .post(url)
            .json(&RunRequest { input, blackboard })
            .send()
            .await
            .context("Failed to run workflow")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to run workflow: {error_text}");
        }

        #[derive(Deserialize)]
        struct RunResponse {
            execution_id: Uuid,
        }

        let run_response: RunResponse = response
            .json()
            .await
            .context("Failed to parse run response")?;

        Ok(run_response.execution_id)
    }

    /// List all workflows
    pub async fn list_workflows(&self) -> Result<Vec<serde_json::Value>> {
        let response = self
            .client
            .get(format!("{}/v1/workflows", self.base_url))
            .send()
            .await
            .context("Failed to list workflows")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list workflows: {error_text}");
        }

        let list_response: WorkflowListResponse = response
            .json()
            .await
            .context("Failed to parse list response")?;

        let workflows = match list_response {
            WorkflowListResponse::Wrapped { workflows } => workflows,
            WorkflowListResponse::Bare(workflows) => workflows,
        };

        Ok(workflows)
    }

    /// List workflows with optional scope filter or visible-all mode.
    pub async fn list_workflows_with_scope(
        &self,
        scope: Option<&str>,
        visible: bool,
    ) -> Result<Vec<serde_json::Value>> {
        let mut params = Vec::new();
        if let Some(s) = scope {
            params.push(format!("scope={s}"));
        }
        if visible {
            params.push("visible=true".to_string());
        }
        let query = if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        };
        let url = format!("{}/v1/workflows{query}", self.base_url);

        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to list workflows")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list workflows: {error_text}");
        }

        let list_response: WorkflowListResponse = response
            .json()
            .await
            .context("Failed to parse list response")?;

        let workflows = match list_response {
            WorkflowListResponse::Wrapped { workflows } => workflows,
            WorkflowListResponse::Bare(workflows) => workflows,
        };

        Ok(workflows)
    }

    /// Change workflow scope (promote/demote).
    pub async fn change_workflow_scope(
        &self,
        name_or_id: &str,
        target_scope: &str,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/v1/workflows/{}/scope", self.base_url, name_or_id);
        let body = serde_json::json!({ "target_scope": target_scope });

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to change workflow scope")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to change workflow scope: {error_text}");
        }

        let result: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse scope change response")?;

        Ok(result)
    }

    /// List workflow executions (paginated, newest first)
    pub async fn list_workflow_executions(
        &self,
        limit: usize,
        workflow_id: Option<uuid::Uuid>,
    ) -> Result<Vec<WorkflowExecutionInfo>> {
        let mut url = format!("{}/v1/workflows/executions?limit={}", self.base_url, limit);
        if let Some(wid) = workflow_id {
            url.push_str(&format!("&workflow_id={wid}"));
        }

        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to list workflow executions")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list workflow executions: {error_text}");
        }

        let executions: Vec<WorkflowExecutionInfo> = response
            .json()
            .await
            .context("Failed to parse workflow executions response")?;

        Ok(executions)
    }

    /// Describe a workflow (get YAML definition)
    pub async fn describe_workflow(&self, name: &str) -> Result<String> {
        let response = self
            .client
            .get(format!("{}/v1/workflows/{}", self.base_url, name))
            .send()
            .await
            .context("Failed to describe workflow")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to describe workflow: {error_text}");
        }

        let workflow_yaml = response
            .text()
            .await
            .context("Failed to read workflow YAML")?;

        Ok(workflow_yaml)
    }

    /// Delete a workflow
    pub async fn delete_workflow(&self, name: &str) -> Result<()> {
        let response = self
            .client
            .delete(format!("{}/v1/workflows/{}", self.base_url, name))
            .send()
            .await
            .context("Failed to delete workflow")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete workflow: {error_text}");
        }

        Ok(())
    }

    pub async fn get_workflow_execution(
        &self,
        execution_id: Uuid,
    ) -> Result<WorkflowExecutionInfo> {
        let response = self
            .client
            .get(format!(
                "{}/v1/workflows/executions/{}",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to get workflow execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get workflow execution: {error_text}");
        }

        response
            .json()
            .await
            .context("Failed to parse workflow execution response")
    }

    pub async fn signal_workflow_execution(
        &self,
        execution_id: Uuid,
        response_text: &str,
    ) -> Result<()> {
        let response = self
            .client
            .post(format!(
                "{}/v1/workflows/executions/{}/signal",
                self.base_url, execution_id
            ))
            .json(&serde_json::json!({ "response": response_text }))
            .send()
            .await
            .context("Failed to signal workflow execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to signal workflow execution: {error_text}");
        }

        Ok(())
    }

    pub async fn cancel_workflow_execution(&self, execution_id: Uuid) -> Result<()> {
        let response = self
            .client
            .post(format!(
                "{}/v1/workflows/executions/{}/cancel",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to cancel workflow execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to cancel workflow execution: {error_text}");
        }

        Ok(())
    }

    pub async fn remove_workflow_execution(&self, execution_id: Uuid) -> Result<()> {
        let response = self
            .client
            .delete(format!(
                "{}/v1/workflows/executions/{}",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to remove workflow execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to remove workflow execution: {error_text}");
        }

        Ok(())
    }

    pub async fn stream_workflow_logs(
        &self,
        execution_id: Uuid,
        options: WorkflowLogOptions,
    ) -> Result<()> {
        let response = self
            .client
            .get(format!(
                "{}/v1/workflows/executions/{}/logs/stream",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to connect to workflow log stream")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stream workflow logs: {error_text}");
        }

        stream_workflow_events(response, options).await
    }

    pub async fn get_workflow_logs(
        &self,
        execution_id: Uuid,
        options: WorkflowLogOptions,
    ) -> Result<Vec<WorkflowLogEvent>> {
        let response = self
            .client
            .get(format!(
                "{}/v1/workflows/executions/{}/logs",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to get workflow logs")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get workflow logs: {error_text}");
        }

        let payload: WorkflowLogsResponse = response
            .json()
            .await
            .context("Failed to parse workflow logs response")?;

        Ok(payload
            .events
            .into_iter()
            .filter(|event| !should_skip_workflow_event(event, options))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_iteration_error_message, format_event, is_error_event, CorrelatedActivityEvent,
        WorkflowListResponse,
    };
    use chrono::Utc;
    use serde_json::{json, Value};
    use uuid::Uuid;

    #[test]
    fn test_error_object_with_message_precedence() {
        let event = CorrelatedActivityEvent {
            event_type: "IterationFailed".to_string(),
            category: "execution".to_string(),
            timestamp: Utc::now(),
            execution_id: None,
            agent_id: None,
            iteration: Some(1),
            stage: None,
            message: "top-level error".to_string(),
            details: json!({
                "error": {
                    "message": "object message",
                    "code": "SOME_CODE"
                }
            }),
        };

        let msg = extract_iteration_error_message(&event);
        assert_eq!(msg, "object message");
    }

    #[test]
    fn test_error_string_in_data_precedence() {
        let event = CorrelatedActivityEvent {
            event_type: "IterationFailed".to_string(),
            category: "execution".to_string(),
            timestamp: Utc::now(),
            execution_id: None,
            agent_id: None,
            iteration: Some(1),
            stage: None,
            message: "top-level error".to_string(),
            details: json!({
                "error": "data error string"
            }),
        };

        let msg = extract_iteration_error_message(&event);
        assert_eq!(msg, "data error string");
    }

    #[test]
    fn test_error_string_top_level_precedence() {
        let event = CorrelatedActivityEvent {
            event_type: "IterationFailed".to_string(),
            category: "execution".to_string(),
            timestamp: Utc::now(),
            execution_id: None,
            agent_id: None,
            iteration: Some(1),
            stage: None,
            message: "top-level error".to_string(),
            details: json!({}),
        };

        let msg = extract_iteration_error_message(&event);
        assert_eq!(msg, "top-level error");
    }

    #[test]
    fn test_error_fallback_unknown() {
        let event = CorrelatedActivityEvent {
            event_type: "IterationFailed".to_string(),
            category: "execution".to_string(),
            timestamp: Utc::now(),
            execution_id: None,
            agent_id: None,
            iteration: Some(1),
            stage: None,
            message: String::new(),
            details: json!({
                "error": {
                    "not_message": "no message field here"
                }
            }),
        };

        let msg = extract_iteration_error_message(&event);
        assert_eq!(msg, "Unknown error");
    }

    #[test]
    fn detects_backend_failures_as_errors() {
        let event = CorrelatedActivityEvent {
            event_type: "FilesystemPolicyViolation".to_string(),
            category: "storage".to_string(),
            timestamp: Utc::now(),
            execution_id: None,
            agent_id: None,
            iteration: None,
            stage: None,
            message: "Filesystem policy violation".to_string(),
            details: json!({}),
        };

        assert!(is_error_event(&event));
    }

    #[test]
    fn verbose_output_includes_structured_details() {
        let event = CorrelatedActivityEvent {
            event_type: "PolicyViolation".to_string(),
            category: "mcp".to_string(),
            timestamp: Utc::now(),
            execution_id: None,
            agent_id: None,
            iteration: None,
            stage: Some("fs.write".to_string()),
            message: "Tool policy violation blocked: fs.write".to_string(),
            details: json!({
                "tool_name": "fs.write",
                "details": "attempted to write outside workspace"
            }),
        };

        let rendered = format_event(&event, true);
        assert!(rendered.contains("Tool policy violation blocked: fs.write"));
        assert!(rendered.contains("\"tool_name\": \"fs.write\""));
    }

    #[test]
    fn parses_typed_correlated_activity_event() {
        let payload = json!({
            "event_type": "ContainerRunFailed",
            "category": "container_run",
            "timestamp": Utc::now(),
            "execution_id": Uuid::nil(),
            "agent_id": Value::Null,
            "iteration": Value::Null,
            "stage": "BUILD",
            "message": "Container step failed: Compile",
            "details": { "step_name": "Compile" }
        })
        .to_string();

        let parsed: CorrelatedActivityEvent = serde_json::from_str(&payload).expect("must parse");
        assert_eq!(parsed.event_type, "ContainerRunFailed");
        assert_eq!(parsed.category, "container_run");
        assert_eq!(parsed.stage.as_deref(), Some("BUILD"));
    }

    #[test]
    fn parses_wrapped_workflow_list_response() {
        let payload = r#"{"workflows":[{"name":"alpha"}]}"#;
        let parsed: WorkflowListResponse = serde_json::from_str(payload).expect("must parse");

        match parsed {
            WorkflowListResponse::Wrapped { workflows } => assert_eq!(workflows.len(), 1),
            WorkflowListResponse::Bare(_) => panic!("expected wrapped workflow list"),
        }
    }

    #[test]
    fn parses_bare_workflow_list_response() {
        let payload = r#"[{"name":"alpha"}]"#;
        let parsed: WorkflowListResponse = serde_json::from_str(payload).expect("must parse");

        match parsed {
            WorkflowListResponse::Bare(workflows) => assert_eq!(workflows.len(), 1),
            WorkflowListResponse::Wrapped { .. } => panic!("expected bare workflow list"),
        }
    }

    #[test]
    fn parses_empty_bare_workflow_list_response() {
        let payload = "[]";
        let parsed: WorkflowListResponse = serde_json::from_str(payload).expect("must parse");

        match parsed {
            WorkflowListResponse::Bare(workflows) => assert_eq!(workflows.len(), 0),
            WorkflowListResponse::Wrapped { .. } => panic!("expected bare workflow list"),
        }
    }
}
