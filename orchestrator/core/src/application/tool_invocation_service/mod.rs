// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

mod agents;
mod audit;
mod discovery;
mod execute;
mod facade;
mod gateway;
mod runtime;
mod storage;
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
use crate::application::ports::{
    AgentActivityPort, ExternalWebToolPort, WorkflowExecutionControlPort,
};
use crate::application::register_workflow::RegisterWorkflowUseCase;
use crate::application::schema_registry::SchemaRegistry;
use crate::application::start_workflow_execution::StartWorkflowExecutionUseCase;
use crate::application::tool_catalog::StandardToolCatalog;
use crate::application::validation_service::ValidationService;
use crate::domain::agent::AgentId;
use crate::domain::dispatch::DispatchAction;
use crate::domain::events::{MCPToolEvent, ViolationType};
use crate::domain::execution::{ExecutionInput, TrajectoryStep};
use crate::domain::fsal::AegisFSAL;
use crate::domain::mcp::{
    MCPError, PolicyViolation, ToolInputContract, ToolInvocationId, ToolServerId,
};
use crate::domain::runtime_registry::StandardRuntimeRegistry;
use crate::domain::seal_session::{EnvelopeVerifier, SealSessionError};
use crate::domain::seal_session_repository::SealSessionRepository;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::tenant::TenantId;
use crate::domain::validation::extract_json_from_text;
use crate::domain::workflow::{
    ConfidenceWeighting, ConsensusConfig, ConsensusStrategy, TransitionCondition, Workflow,
    WorkflowValidator,
};
use crate::domain::{repository, validation::ValidationRequest};
use crate::infrastructure::agent_manifest_parser::AgentManifestParser;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::seal::middleware::SealMiddleware;
use crate::infrastructure::seal_gateway_proto::gateway_invocation_service_client::GatewayInvocationServiceClient;
use crate::infrastructure::seal_gateway_proto::{
    FsalMount, InvokeCliRequest, InvokeWorkflowRequest, ListToolsRequest,
};
use crate::infrastructure::tool_router::ToolRouter;
use crate::infrastructure::workflow_parser::WorkflowParser;

const JUDGE_POLL_INTERVAL_MS: u64 = 500;
const COMPACT_JSON_INLINE_LIMIT: usize = 256;
const COMPACT_STRING_PREVIEW_LIMIT: usize = 96;
const COMPACT_ERROR_PREVIEW_LIMIT: usize = 3;

#[derive(Debug)]
pub enum ToolInvocationResult {
    Direct(Value),
    DispatchRequired(DispatchAction),
}

pub struct ToolInvocationService {
    seal_session_repo: Arc<dyn SealSessionRepository>,
    security_context_repo: Arc<dyn SecurityContextRepository>,
    seal_middleware: Arc<SealMiddleware>,
    tool_router: Arc<ToolRouter>,
    /// AegisFSAL security boundary for all storage operations (ADR-033 Path 1, ADR-047)
    /// Delegates to the appropriate StorageProvider (SeaweedFS, OpenDAL, SEAL, LocalHost)
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
    /// Optional ADR-053 SEAL gateway URL from node config.
    seal_gateway_url: Option<String>,
    /// Schema registry for builtin schema.get / schema.validate tools.
    schema_registry: Arc<SchemaRegistry>,
    /// Optional port for workflow execution control (cancel, signal, remove).
    workflow_execution_control: Option<Arc<dyn WorkflowExecutionControlPort>>,
    /// Optional port for agent-level activity logs.
    agent_activity: Option<Arc<dyn AgentActivityPort>>,
    /// Optional tool catalog for aegis.tools.list / aegis.tools.search discovery.
    tool_catalog: Option<Arc<StandardToolCatalog>>,
    /// Optional discovery service for semantic search over agents and workflows (ADR-075).
    discovery_service: Option<Arc<dyn crate::application::discovery_service::DiscoveryService>>,
    /// Optional StandardRuntime registry for aegis.runtime.list tool.
    runtime_registry: Option<Arc<StandardRuntimeRegistry>>,
    /// File operations service for post-mortem execution file reads (aegis.execution.file)
    /// and user-volume file operations (aegis.file.*).
    file_operations_service:
        Option<Arc<crate::application::file_operations_service::FileOperationsService>>,
    /// User volume lifecycle service for aegis.volume.* tools.
    user_volume_service: Option<Arc<crate::application::user_volume_service::UserVolumeService>>,
    /// Git repository binding service for aegis.git.* tools.
    git_repo_service: Option<Arc<crate::application::git_repo_service::GitRepoService>>,
    /// Script service for aegis.script.* tools.
    script_service: Option<Arc<crate::application::script_service::ScriptService>>,
}
