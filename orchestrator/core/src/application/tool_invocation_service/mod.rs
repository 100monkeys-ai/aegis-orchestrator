// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

mod agents;
mod audit;
mod facade;
mod gateway;
mod system;
mod tasks;
#[cfg(test)]
mod tests;
mod workflows;

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::application::agent::AgentLifecycleService;
use crate::application::execution::ExecutionService;
use crate::application::nfs_gateway::NfsVolumeRegistry;
use crate::application::ports::ExternalWebToolPort;
use crate::application::register_workflow::RegisterWorkflowUseCase;
use crate::application::schema_registry::SchemaRegistry;
use crate::application::start_workflow_execution::StartWorkflowExecutionUseCase;
use crate::application::validation_service::ValidationService;
use crate::domain::agent::AgentId;
use crate::domain::dispatch::DispatchAction;
use crate::domain::events::{MCPToolEvent, ViolationType};
use crate::domain::execution::{ExecutionInput, TrajectoryStep};
use crate::domain::fsal::AegisFSAL;
use crate::domain::mcp::{
    MCPError, PolicyViolation, ToolInputContract, ToolInvocationId, ToolServerId,
};
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::smcp_session::{EnvelopeVerifier, SmcpSessionError};
use crate::domain::smcp_session_repository::SmcpSessionRepository;
use crate::domain::tenant::TenantId;
use crate::domain::validation::extract_json_from_text;
use crate::domain::workflow::{
    ConfidenceWeighting, ConsensusConfig, ConsensusStrategy, TransitionCondition, Workflow,
    WorkflowValidator,
};
use crate::domain::{repository, validation::ValidationRequest};
use crate::infrastructure::agent_manifest_parser::AgentManifestParser;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::smcp::middleware::SmcpMiddleware;
use crate::infrastructure::smcp_gateway_proto::gateway_invocation_service_client::GatewayInvocationServiceClient;
use crate::infrastructure::smcp_gateway_proto::{
    FsalMount, InvokeCliRequest, InvokeWorkflowRequest, ListToolsRequest,
};
use crate::infrastructure::tool_router::ToolRouter;
use crate::infrastructure::workflow_parser::WorkflowParser;

const JUDGE_POLL_INTERVAL_MS: u64 = 500;
const COMPACT_JSON_INLINE_LIMIT: usize = 256;
const COMPACT_STRING_PREVIEW_LIMIT: usize = 96;
const COMPACT_ERROR_PREVIEW_LIMIT: usize = 3;

pub enum ToolInvocationResult {
    Direct(Value),
    DispatchRequired(DispatchAction),
}

pub struct ToolInvocationService {
    smcp_session_repo: Arc<dyn SmcpSessionRepository>,
    security_context_repo: Arc<dyn SecurityContextRepository>,
    smcp_middleware: Arc<SmcpMiddleware>,
    tool_router: Arc<ToolRouter>,
    /// AegisFSAL security boundary for all storage operations (ADR-033 Path 1, ADR-047)
    /// Delegates to the appropriate StorageProvider (SeaweedFS, OpenDAL, SMCP, LocalHost)
    fsal: Arc<AegisFSAL>,
    /// Volume registry for resolving execution_id -> volume context
    volume_registry: NfsVolumeRegistry,
    /// Agent lifecycle service for semantic validation
    agent_lifecycle: Arc<dyn AgentLifecycleService>,
    /// Execution service for spawning inner-loop judges
    execution_service: Arc<dyn ExecutionService>,
    /// Adapter for external web tools (Path 2).
    web_tool_port: Arc<dyn ExternalWebToolPort>,
    /// Event bus for MCP invocation and policy audit events.
    event_bus: Arc<EventBus>,
    /// Optional workflow registration use case for built-in aegis workflow authoring tools.
    register_workflow_use_case: Option<Arc<dyn RegisterWorkflowUseCase>>,
    /// Optional validation service for semantic stage in workflow authoring tools.
    validation_service: Option<Arc<ValidationService>>,
    /// Optional workflow repository for managing workflows.
    workflow_repository: Option<Arc<dyn repository::WorkflowRepository>>,
    /// Optional workflow execution repository for `aegis.workflow.logs` and `aegis.task.logs`.
    workflow_execution_repo: Option<Arc<dyn repository::WorkflowExecutionRepository>>,
    /// Optional workflow execution use case.
    start_workflow_execution_use_case: Option<Arc<dyn StartWorkflowExecutionUseCase>>,
    /// Optional root directory for persisting generated manifests on disk.
    generated_manifests_root: Option<PathBuf>,
    /// Optional path to node-config.yaml for system.config tool.
    node_config_path: Option<PathBuf>,
    /// Optional ADR-053 SMCP gateway URL from node config.
    smcp_gateway_url: Option<String>,
    /// Schema registry for builtin schema.get / schema.validate tools.
    schema_registry: Arc<SchemaRegistry>,
}
