// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Application-layer ports for external integrations.
//!
//! These traits define anti-corruption boundaries consumed by use-cases.
//! Infrastructure adapters implement these ports.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::application::temporal_mapper::TemporalWorkflowDefinition;
use crate::application::tool_invocation_service::ToolInvocationResult;
use crate::domain::execution::ExecutionId;
use crate::domain::smcp_session::SmcpSessionError;

#[async_trait]
pub trait WorkflowEnginePort: Send + Sync {
    async fn register_workflow(
        &self,
        definition: &TemporalWorkflowDefinition,
    ) -> anyhow::Result<()>;

    async fn start_workflow(
        &self,
        workflow_id: &str,
        execution_id: ExecutionId,
        input: HashMap<String, Value>,
        blackboard: Option<HashMap<String, Value>>,
    ) -> anyhow::Result<String>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryStepCommand {
    pub tool_name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreTrajectoryPatternCommand {
    pub task_signature: String,
    pub steps: Vec<TrajectoryStepCommand>,
    pub success_score: f64,
}

#[async_trait]
pub trait CortexPatternPort: Send + Sync {
    async fn store_trajectory_pattern(
        &self,
        request: StoreTrajectoryPatternCommand,
    ) -> anyhow::Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchRequest {
    pub query: String,
    pub max_results: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebFetchRequest {
    pub url: String,
    pub to_markdown: bool,
    pub follow_redirects: bool,
    pub timeout_secs: u64,
}

#[async_trait]
pub trait ExternalWebToolPort: Send + Sync {
    async fn search(
        &self,
        request: WebSearchRequest,
    ) -> Result<ToolInvocationResult, SmcpSessionError>;

    async fn fetch(
        &self,
        request: WebFetchRequest,
    ) -> Result<ToolInvocationResult, SmcpSessionError>;
}

#[derive(Debug, Clone)]
pub enum TokenAudience {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct AttestationTokenClaims {
    pub agent_id: String,
    pub execution_id: String,
    pub security_context: String,
    pub iss: Option<String>,
    pub aud: Option<TokenAudience>,
    pub exp: Option<i64>,
    pub iat: Option<i64>,
    pub nbf: Option<i64>,
}

pub trait SecurityTokenIssuerPort: Send + Sync {
    fn issue(&self, claims: &mut AttestationTokenClaims) -> anyhow::Result<String>;
}
