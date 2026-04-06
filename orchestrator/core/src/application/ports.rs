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
use crate::domain::seal_session::SealSessionError;

/// Parameters for starting a workflow execution.
///
/// Bundles all arguments to [`WorkflowEnginePort::start_workflow`] into a single
/// struct, keeping the trait method within clippy's `too_many_arguments` limit.
pub struct StartWorkflowParams<'a> {
    pub workflow_id: &'a str,
    pub execution_id: ExecutionId,
    pub tenant_id: &'a str,
    pub input: HashMap<String, Value>,
    pub blackboard: Option<HashMap<String, Value>>,
    pub security_context_name: Option<String>,
    pub intent: Option<String>,
}

#[async_trait]
pub trait WorkflowEnginePort: Send + Sync {
    async fn register_workflow(
        &self,
        definition: &TemporalWorkflowDefinition,
    ) -> anyhow::Result<()>;

    async fn start_workflow(&self, params: StartWorkflowParams<'_>) -> anyhow::Result<String>;
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
    ) -> Result<ToolInvocationResult, SealSessionError>;

    async fn fetch(
        &self,
        request: WebFetchRequest,
    ) -> Result<ToolInvocationResult, SealSessionError>;
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
    pub jti: Option<String>,
    pub sub: String,
    pub scp: String,
    pub wid: String,
    pub tenant_id: Option<String>,
    pub task_summary: Option<String>,
}

pub trait SecurityTokenIssuerPort: Send + Sync {
    fn issue(&self, claims: &mut AttestationTokenClaims) -> anyhow::Result<String>;
}

/// Port for verifying that a container is currently running.
///
/// Used during SEAL attestation (ADR-035 §4.1) to bind the issued
/// `SecurityToken` to an actively-running workload, preventing replay
/// attacks where a token is requested for a container that has already
/// exited or never started.
#[async_trait]
pub trait ContainerVerificationPort: Send + Sync {
    /// Returns `Ok(())` if the container with the given ID is currently running.
    ///
    /// # Errors
    ///
    /// Returns an error if the container is not running, cannot be found,
    /// or the container runtime is unreachable.
    async fn verify_container_running(&self, container_id: &str) -> anyhow::Result<()>;
}

/// Port for workflow execution control operations (cancel, signal, remove).
///
/// Infrastructure adapters implement this to interact with the workflow engine
/// (e.g. Temporal) for lifecycle management of running workflow executions.
#[async_trait]
pub trait WorkflowExecutionControlPort: Send + Sync {
    async fn cancel_workflow_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn signal_workflow_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
        response: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_workflow_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Port for retrieving agent-level activity log snapshots.
///
/// Infrastructure adapters implement this to fetch agent event history
/// from whatever storage backend is in use.
#[async_trait]
pub trait AgentActivityPort: Send + Sync {
    async fn agent_logs_snapshot(
        &self,
        agent_id: uuid::Uuid,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>>;
}

/// Port for cascade-cancelling a swarm when a parent execution is cancelled.
/// Implemented by the swarm crate; injected into StandardExecutionService.
#[async_trait]
pub trait SwarmCancellationPort: Send + Sync {
    /// Look up the swarm associated with this execution and cancel it.
    /// Returns Ok(()) if no swarm is associated or the cancellation succeeds.
    async fn cascade_cancel_for_execution(&self, execution_id: ExecutionId) -> anyhow::Result<()>;
}

/// Client for communicating with the SEAL gateway's control plane (ADR-088 §A8).
///
/// Pre-creates SEAL sessions on the gateway before container start so that
/// agents have valid credentials immediately, eliminating the shared-default
/// security context fallback.
#[async_trait]
pub trait SealGatewayClient: Send + Sync {
    /// Pre-create a SEAL session on the gateway before container start.
    async fn create_session(
        &self,
        request: SealSessionCreateRequest,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Request to pre-create a SEAL session on the gateway (ADR-088 §A8).
#[derive(Debug, Clone, Serialize)]
pub struct SealSessionCreateRequest {
    pub execution_id: String,
    pub agent_id: String,
    pub security_context: String,
    pub public_key_b64: String,
    pub security_token: String,
    pub session_status: String,
    pub expires_at: String,
    pub allowed_tool_patterns: Vec<String>,
}
