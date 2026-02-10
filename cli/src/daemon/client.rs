// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! HTTP client for communicating with daemon API

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use uuid::Uuid;

use aegis_sdk::manifest::AgentManifest;



#[derive(Debug, Clone)]
pub struct DaemonClient {
    client: Client,
    base_url: String,
}

impl DaemonClient {
    pub fn new(port: u16) -> Result<Self> {
        let client = Client::builder()
            // No global timeout for CLI client as we need long-lived streams

            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            client,
            base_url: format!("http://localhost:{}", port),
        })
    }

    pub async fn deploy_agent(&self, manifest: AgentManifest) -> Result<Uuid> {
        let response = self
            .client
            .post(&format!("{}/api/agents", self.base_url))
            .json(&manifest)
            .send()
            .await
            .context("Failed to deploy agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to deploy agent: {}", error_text);
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
    ) -> Result<Uuid> {
        #[derive(Serialize)]
        struct ExecuteRequest {
            input: serde_json::Value,
        }

        let response = self
            .client
            .post(&format!("{}/api/agents/{}/execute", self.base_url, agent_id))
            .json(&ExecuteRequest { input })
            .send()
            .await
            .context("Failed to execute agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to execute agent: {}", error_text);
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
            .get(&format!("{}/api/executions/{}", self.base_url, execution_id))
            .send()
            .await
            .context("Failed to get execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get execution: {}", error_text);
        }

        response
            .json()
            .await
            .context("Failed to parse execution response")
    }

    pub async fn cancel_execution(&self, execution_id: Uuid) -> Result<()> {
        let response = self
            .client
            .post(&format!(
                "{}/api/executions/{}/cancel",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to cancel execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to cancel execution: {}", error_text);
        }

        Ok(())
    }

    pub async fn list_executions(
        &self,
        agent_id: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ExecutionInfo>> {
        let mut url = format!("{}/api/executions?limit={}", self.base_url, limit);
        if let Some(aid) = agent_id {
            url.push_str(&format!("&agent_id={}", aid));
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to list executions")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list executions: {}", error_text);
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
        let mut url = format!(
            "{}/api/executions/{}/events",
            self.base_url, execution_id
        );
        if follow {
            url.push_str("?follow=true");
        } else {
            url.push_str("?follow=false");
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to event stream")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stream logs: {}", error_text);
        }

        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Failed to read event stream chunk")?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                if line.starts_with("data: ") {
                    let json_str = &line[6..];
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(json_str) {
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

    pub async fn stream_agent_logs(
        &self,
        agent_id: Uuid,
        follow: bool,
        errors_only: bool,
        verbose: bool,
    ) -> Result<()> {
        let mut url = format!(
            "{}/api/agents/{}/events",
            self.base_url, agent_id
        );
        if follow {
            url.push_str("?follow=true");
        } else {
            url.push_str("?follow=false");
        }

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to agent event stream")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to stream agent logs: {}", error_text);
        }

        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Failed to read event stream chunk")?;
            let text = String::from_utf8_lossy(&chunk);

            for line in text.lines() {
                if line.starts_with("data: ") {
                    let json_str = &line[6..];
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(json_str) {
                         // Reuse the print logic? Yes.
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

    pub async fn delete_execution(&self, execution_id: Uuid) -> Result<()> {
        let response = self
            .client
            .delete(&format!(
                "{}/api/executions/{}",
                self.base_url, execution_id
            ))
            .send()
            .await
            .context("Failed to delete execution")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete execution: {}", error_text);
        }

        Ok(())
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let response = self
            .client
            .get(&format!("{}/api/agents", self.base_url))
            .send()
            .await
            .context("Failed to list agents")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to list agents: {}", error_text);
        }

        response
            .json()
            .await
            .context("Failed to parse agents response")
    }

    pub async fn get_agent(&self, agent_id: Uuid) -> Result<AgentManifest> {
        let response = self
            .client
            .get(&format!("{}/api/agents/{}", self.base_url, agent_id))
            .send()
            .await
            .context("Failed to get agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to get agent: {}", error_text);
        }

        response
            .json()
            .await
            .context("Failed to parse agent manifest")
    }

    pub async fn delete_agent(&self, agent_id: Uuid) -> Result<()> {
        let response = self
            .client
            .delete(&format!("{}/api/agents/{}", self.base_url, agent_id))
            .send()
            .await
            .context("Failed to delete agent")?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to delete agent: {}", error_text);
        }

        Ok(())
    }
    pub async fn lookup_agent(&self, name: &str) -> Result<Option<Uuid>> {
        let response = self
            .client
            .get(&format!("{}/api/agents/lookup/{}", self.base_url, name))
            .send()
            .await
            .context("Failed to lookup agent")?;

        if response.status() == 404 {
            return Ok(None);
        }

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Failed to lookup agent: {}", error_text);
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

fn is_error_event(event: &serde_json::Value) -> bool {
    matches!(
        event["event_type"].as_str(),
        Some("IterationFailed") | Some("ExecutionFailed") | Some("PolicyViolation")
    )
}

fn print_event(event: &serde_json::Value, verbose: bool) {
    use colored::Colorize;

    let event_type = event["event_type"].as_str().unwrap_or("Unknown");
    let timestamp = event["timestamp"].as_str().unwrap_or("");

    match event_type {
        "ExecutionStarted" => {
            println!(
                "{} {}",
                format!("[{}]", timestamp).dimmed(),
                "Execution started".bold()
            );
        }
        "IterationStarted" => {
            let iteration = event["iteration_number"].as_u64().unwrap_or(0);
            
            if verbose {
                let action = event["action"].as_str().unwrap_or("");
                println!(
                    "{} {} {} - {}",
                    format!("[{}]", timestamp).dimmed(),
                    "Iteration".yellow(),
                    iteration,
                    action
                );
            } else {
                println!(
                    "{} {} {}",
                    format!("[{}]", timestamp).dimmed(),
                    "Iteration".yellow(),
                    iteration
                );
            }
        }
        "IterationCompleted" => {
            let iteration = event["iteration_number"].as_u64().unwrap_or(0);
            let output = event["data"]["output"].as_str().unwrap_or("");
            
            if verbose {
                println!(
                    "{} {} {} {}\n{}",
                    format!("[{}]", timestamp).dimmed(),
                    "Iteration".yellow(),
                    iteration,
                    "completed".green(),
                    output.cyan()
                );
            } else {
                println!(
                    "{} {} {} {}",
                    format!("[{}]", timestamp).dimmed(),
                    "Iteration".yellow(),
                    iteration,
                    "completed".green()
                );
            }
        }
        "IterationFailed" => {
            let iteration = event["iteration_number"].as_u64().unwrap_or(0);
            let error = if let Some(err_obj) = event["data"]["error"].as_object() {
                err_obj["message"].as_str().unwrap_or(event["data"]["error"].as_str().unwrap_or("Unknown error"))
            } else {
                 event["data"]["error"].as_str().unwrap_or(event["error"].as_str().unwrap_or(""))
            };
            println!(
                "{} {} {} {} - {}",
                format!("[{}]", timestamp).dimmed(),
                "Iteration".yellow(),
                iteration,
                "failed".red(),
                error
            );
        }
        "ConsoleOutput" => {
            let stream = event["stream"].as_str().unwrap_or("stdout");
            let content = event["data"]["output"].as_str().unwrap_or("");
            let prefix = match stream {
                "stderr" => "[STDERR]".red(),
                "judge" => "[JUDGE]".magenta().bold(),
                _ => "[STDOUT]".cyan(),
            };
            println!("{} {}", prefix, content.trim_end());
        }
        "LlmInteraction" => {
            let model = event["data"]["model"].as_str().unwrap_or("unknown");
            
            if verbose {
                let response = event["data"]["response"].as_str().unwrap_or("");
                let prompt = event["data"]["prompt"].as_str().unwrap_or("");
                
                println!(
                    "{} {} [{}]",
                    format!("[{}]", timestamp).dimmed(),
                    "LLM Interaction".purple().bold(),
                    model
                );
                println!("{}", "PROMPT:".dimmed());
                println!("{}", prompt);
                println!("{}", "RESPONSE:".dimmed());
                println!("{}", response);
                println!("{}", "-".repeat(40).dimmed());
            } else {
                // Show model interaction indicator without response content
                println!(
                    "{} {} [{}]",
                    format!("[{}]", timestamp).dimmed(),
                    "LLM".purple(),
                    model
                );
            }
        }
        "ExecutionCompleted" => {
            if verbose {
                println!(
                    "{} {} {}",
                    format!("[{}]", timestamp).dimmed(),
                    event_type.cyan(),
                    serde_json::to_string_pretty(&event["data"]).unwrap_or_default()
                );
            } else {
                println!(
                    "{} {}",
                    format!("[{}]", timestamp).dimmed(),
                    "Execution completed".green().bold()
                );
            }
        }
        "ExecutionFailed" => {
            println!(
                "{} {} {}",
                format!("[{}]", timestamp).dimmed(),
                "Execution failed".red().bold(),
                event["data"]["error"].as_str().or(event["reason"].as_str()).unwrap_or("Unknown error")
            );
        }
        _ => {
            if event_type != "Unknown" {
                 println!(
                    "{} {} {}",
                    format!("[{}]", timestamp).dimmed(),
                    event_type.cyan(),
                    serde_json::to_string_pretty(&event["data"]).unwrap_or_default()
                );
            }
        }
    }
}
