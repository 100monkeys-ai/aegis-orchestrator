// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Domain Event Catalog (ADR-030)
//!
//! Single source of truth for all domain events emitted by AEGIS. Every bounded
//! context publishes events through the [`crate::infrastructure::event_bus::EventBus`]
//! using the [`crate::infrastructure::event_bus::DomainEvent`] wrapper enum.
//!
//! ## Event Groups
//!
//! | Enum | Bounded Context | Description |
//! |---|---|---|
//! | [`StorageEvent`] | BC-7 Storage Gateway | File-level audit trail from NFS FSAL (ADR-036) |
//! | [`AgentLifecycleEvent`] | BC-1 Agent Lifecycle | Agent manifest deploy/update/remove lifecycle |
//! | [`ExecutionEvent`] | BC-2 Execution | 100monkeys iteration loop progress and console I/O |
//! | [`WorkflowEvent`] | BC-3 Workflow | Temporal-backed FSM state transitions (ADR-015) |
//! | [`LearningEvent`] | BC-5 Cortex | Pattern weight reinforcement/decay signals |
//! | [`ValidationEvent`] | BC-2 Execution | Gradient validation scores and multi-judge consensus |
//! | [`VolumeEvent`] | BC-7 Storage Gateway | Volume lifecycle (create/attach/detach/delete/expire) |
//! | [`PolicyEvent`] | BC-4 Security Policy | Runtime policy violation records |
//! | [`ViolationType`] | BC-4 / BC-12 | Structured violation classification |
//! | [`MCPToolEvent`] | BC-12 SEAL / Tool Routing | MCP server lifecycle and tool invocation audit (ADR-033) |
//! | [`ImageManagementEvent`] | BC-2 Execution | Container image pull lifecycle and cache status (ADR-045) |
//! | [`CommandExecutionEvent`] | BC-2 Execution / Dispatch | In-container command execution via Dispatch Protocol (ADR-040) |
//! | [`IamEvent`] | BC-13 IAM & Identity Federation | OIDC authentication, realm lifecycle, JWKS cache events (ADR-041) |
//! | [`SecretEvent`] | BC-11 Secrets & Identity | Secret access, dynamic credential generation, and access denial audit (ADR-034) |
//! | [`TenantEvent`] | Multi-Tenant | Tenant lifecycle, audit, and quota events (ADR-056) |
//! | [`RateLimitEvent`] | Cross-cutting (ADR-072) | Rate limit rejection and warning threshold events |
//! | [`SwarmEvent`] | BC-6 Swarm Coordination | Swarm lifecycle, child spawning, lock, and broadcast events (ADR-039) |
//! | [`CredentialEvent`] | BC-11 Secrets & Identity | Credential binding lifecycle, grant, rotation, and access audit (ADR-078) |
//!
//! ## Phase 2 Note
//!
//! The EventBus is currently **in-memory only** (tokio broadcast channel). Persistent
//! event replay and external consumers (Kafka, NATS) are planned for Phase 2 per ADR-030.

use crate::domain::agent::{AgentManifest, AgentScope};
use crate::domain::credential::{
    CredentialBindingId, CredentialGrantId, CredentialProvider, CredentialType, GrantTarget,
};
// Re-export BC-7 git repo binding events so they sit alongside the other
// `*Event` enums in the single domain event catalog (ADR-081).
pub use super::git_repo::GitRepoEvent;
// Re-export BC-7 Vibe-Code Canvas events so they sit alongside the other
// `*Event` enums in the single domain event catalog (ADR-106).
pub use super::canvas::CanvasEvent;
// Re-export BC-7 Script persistence events so they sit alongside the other
// `*Event` enums in the single domain event catalog (ADR-110 §D7).
pub use super::script::ScriptEvent;
// Re-export team-tenancy events so they sit alongside the other `*Event`
// enums in the single domain event catalog (ADR-111).
pub use super::team::TeamEvent;
use crate::domain::execution::{CodeDiff, IterationError};
use crate::domain::runtime::InstanceId;
use crate::domain::secrets::AccessContext;
use crate::domain::shared_kernel::{
    AgentId, DispatchId, ExecutionId, ImagePullPolicy, NodeId, VolumeId,
};
use crate::domain::tenancy::TenantQuotaKind;
use crate::domain::tenant::TenantId;
use crate::domain::volume::StorageClass;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Canonical correlated activity envelope for task/agent follow surfaces.
///
/// This stays in the domain layer so downstream transports can serialize one
/// stable shape without rebuilding per-surface log payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorrelatedActivityEvent {
    pub event_type: String,
    pub category: String,
    pub timestamp: DateTime<Utc>,
    pub execution_id: Option<ExecutionId>,
    pub agent_id: Option<AgentId>,
    pub iteration: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

/// Whether a container image was retrieved from the local Docker cache or pulled
/// fresh from a remote registry (ADR-045).
///
/// Returned by [`crate::infrastructure::image_manager::DockerImageManager::ensure_image`]
/// and embedded in [`ImageManagementEvent::ImagePullCompleted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullSource {
    /// Image was already present in the local Docker daemon cache; no network I/O.
    Cached,
    /// Image was freshly pulled from the remote registry.
    Downloaded,
}

/// Container image lifecycle events emitted by [`crate::infrastructure::runtime::ContainerRuntime`]
/// during the `spawn()` path (ADR-045).
///
/// Published to the [`crate::infrastructure::event_bus::EventBus`] before/after every
/// Docker image pull so that the Zaru client, Cortex, and audit log can:
/// - Track which images were pulled vs cache-hit across executions
/// - Alert on systematic pull failures (misconfigured registry credentials, rate limits)
/// - Feed Cortex with image-reuse patterns for cost optimisation signals
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageManagementEvent {
    /// Raised immediately before the `DockerImageManager::ensure_image` call so
    /// that slow pulls are visible in the execution timeline.
    ImagePullStarted {
        execution_id: ExecutionId,
        image: String,
        pull_policy: ImagePullPolicy,
        started_at: DateTime<Utc>,
    },
    /// Raised after a successful `ensure_image` call; `source` distinguishes a
    /// cache-hit from an actual network pull.
    ImagePullCompleted {
        execution_id: ExecutionId,
        image: String,
        source: PullSource,
        duration_ms: u64,
        completed_at: DateTime<Utc>,
    },
    /// Raised when `ensure_image` returns an error so the execution can be failed
    /// immediately and the reason preserved for audit.
    ImagePullFailed {
        execution_id: ExecutionId,
        image: String,
        reason: String,
        failed_at: DateTime<Utc>,
    },
}

/// File-level audit events published by the NFS Server Gateway FSAL (ADR-036).
///
/// These complement [`VolumeEvent`] (which tracks volume lifecycle) by recording
/// individual POSIX file operations. Every variant is persisted to Postgres via
/// [`crate::application::storage_event_persister::StorageEventPersister`] and
/// consumed by the Cortex for file-access pattern learning.
///
/// # Security
///
/// `PathTraversalBlocked` and `FilesystemPolicyViolation` variants are the primary
/// forensic signal for detecting malicious or misconfigured agent behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageEvent {
    FileOpened {
        /// Agent execution that performed this operation. Mutually exclusive with
        /// `workflow_execution_id` — exactly one must be `Some`.
        execution_id: Option<ExecutionId>,
        /// Workflow execution that performed this operation (ContainerStep FUSE path).
        /// Mutually exclusive with `execution_id` — exactly one must be `Some`.
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        open_mode: String, // "read", "write", "read-write", "create"
        opened_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    FileRead {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        offset: u64,
        bytes_read: u64,
        duration_ms: u64,
        read_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    FileWritten {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        offset: u64,
        bytes_written: u64,
        duration_ms: u64,
        written_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    FileClosed {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        closed_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    DirectoryListed {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        entry_count: usize,
        listed_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    FileCreated {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        created_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    FileDeleted {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        path: String,
        deleted_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    PathTraversalBlocked {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        attempted_path: String,
        blocked_at: DateTime<Utc>,
    },
    FilesystemPolicyViolation {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        operation: String, // "read", "write", "delete"
        path: String,
        policy_rule: String,
        violated_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    QuotaExceeded {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        requested_bytes: u64,
        available_bytes: u64,
        exceeded_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
    UnauthorizedVolumeAccess {
        execution_id: Option<ExecutionId>,
        workflow_execution_id: Option<uuid::Uuid>,
        volume_id: VolumeId,
        attempted_at: DateTime<Utc>,
        /// Node that initiated the cross-node RPC (`None` for local operations).
        caller_node_id: Option<NodeId>,
        /// Node that executed the storage operation (`None` for local operations).
        host_node_id: Option<NodeId>,
    },
}

/// Agent manifest lifecycle events (BC-1 Agent Lifecycle Context).
///
/// Published by [`crate::application::lifecycle::StandardAgentLifecycleService`].
/// Consumers: Zaru client gRPC stream, Cortex (tracks agent version history).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum AgentLifecycleEvent {
    AgentDeployed {
        agent_id: AgentId,
        manifest: AgentManifest,
        deployed_at: DateTime<Utc>,
    },
    AgentPaused {
        agent_id: AgentId,
        paused_at: DateTime<Utc>,
    },
    AgentResumed {
        agent_id: AgentId,
        resumed_at: DateTime<Utc>,
    },
    AgentUpdated {
        agent_id: AgentId,
        old_version: String,
        new_version: String,
        updated_at: DateTime<Utc>,
    },
    AgentRemoved {
        agent_id: AgentId,
        removed_at: DateTime<Utc>,
    },
    AgentFailed {
        agent_id: AgentId,
        reason: String,
        failed_at: DateTime<Utc>,
    },
    AgentScopeChanged {
        agent_id: AgentId,
        agent_name: String,
        previous_scope: AgentScope,
        new_scope: AgentScope,
        previous_tenant_id: crate::domain::tenant::TenantId,
        new_tenant_id: crate::domain::tenant::TenantId,
        changed_by: String,
        changed_at: DateTime<Utc>,
    },
}

/// Execution and iteration-level events (BC-2 Execution Context / 100monkeys loop).
///
/// Published by [`crate::application::execution::StandardExecutionService`] via
/// [`crate::domain::supervisor::SupervisorObserver`] callbacks.
///
/// Key flow:
/// ```text
/// ExecutionStarted
///   └─ IterationStarted (N=1)
///       ├─ ConsoleOutput* (streaming)
///       ├─ LlmInteraction (one per LLM call)
///       ├─ InstanceSpawned / InstanceTerminated
///       └─ IterationCompleted | IterationFailed
///           └─ RefinementApplied? → IterationStarted (N+1)
/// ExecutionCompleted | ExecutionFailed | ExecutionCancelled
/// ```
///
/// The `Validation` variant wraps sub-events from the validation
/// bounded context to keep this enum as the single stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecutionEvent {
    ExecutionStarted {
        execution_id: ExecutionId,
        agent_id: AgentId,
        started_at: DateTime<Utc>,
    },
    IterationStarted {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        action: String,
        started_at: DateTime<Utc>,
    },
    IterationCompleted {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        output: String,
        completed_at: DateTime<Utc>,
    },
    IterationFailed {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        error: IterationError,
        failed_at: DateTime<Utc>,
    },
    RefinementApplied {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        code_diff: CodeDiff,
        applied_at: DateTime<Utc>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cortex_pattern_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cortex_pattern_category: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cortex_success_score: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cortex_solution_approach: Option<String>,
    },
    ExecutionCompleted {
        execution_id: ExecutionId,
        agent_id: AgentId,
        final_output: String,
        total_iterations: u8,
        completed_at: DateTime<Utc>,
    },
    ExecutionFailed {
        execution_id: ExecutionId,
        agent_id: AgentId,
        reason: String,
        total_iterations: u8,
        failed_at: DateTime<Utc>,
    },
    ExecutionCancelled {
        execution_id: ExecutionId,
        agent_id: AgentId,
        reason: Option<String>,
        cancelled_at: DateTime<Utc>,
    },
    /// The execution exceeded its wall-clock timeout and was forcibly terminated.
    /// Published by [`crate::application::execution::StandardExecutionService`]
    /// when `tokio::time::timeout` fires around the Supervisor loop.
    ExecutionTimedOut {
        execution_id: ExecutionId,
        agent_id: AgentId,
        timeout_seconds: u64,
        total_iterations: u8,
        timed_out_at: DateTime<Utc>,
    },
    ConsoleOutput {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        stream: String, // "stdout" or "stderr"
        content: String,
        timestamp: DateTime<Utc>,
    },
    LlmInteraction {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        provider: String,
        model: String,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
        prompt: String,
        response: String,
        timestamp: DateTime<Utc>,
    },
    InstanceSpawned {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        instance_id: InstanceId,
        spawned_at: DateTime<Utc>,
    },
    InstanceTerminated {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        instance_id: InstanceId,
        terminated_at: DateTime<Utc>,
    },
    /// A child execution was spawned by this execution (swarm coordination, ADR-039).
    /// `execution_id` is the parent; `child_execution_id` is the spawned child.
    ChildExecutionSpawned {
        execution_id: ExecutionId,
        agent_id: AgentId,
        parent_execution_id: ExecutionId,
        child_execution_id: ExecutionId,
        child_agent_id: AgentId,
        spawned_at: DateTime<Utc>,
    },
    /// A child execution completed (swarm coordination, ADR-039).
    /// `execution_id` is the parent; `child_execution_id` is the completed child.
    ChildExecutionCompleted {
        execution_id: ExecutionId,
        agent_id: AgentId,
        child_execution_id: ExecutionId,
        outcome: String,
        completed_at: DateTime<Utc>,
    },
    Validation(ValidationEvent),

    /// Output handler was started after execution completed (ADR-103).
    OutputHandlerStarted {
        execution_id: ExecutionId,
        /// Discriminant from [`crate::domain::output_handler::OutputHandlerConfig::handler_type_name`].
        handler_type: String,
    },

    /// Output handler completed successfully (ADR-103).
    OutputHandlerCompleted {
        execution_id: ExecutionId,
        handler_type: String,
        /// Optional return value from the handler (e.g. child execution final output).
        result: Option<String>,
    },

    /// Output handler failed (ADR-103).
    OutputHandlerFailed {
        execution_id: ExecutionId,
        handler_type: String,
        error: String,
    },
}

/// Workflow FSM lifecycle events (BC-3 Workflow Orchestration Context).
///
/// Published by Temporal event listener ([`crate::infrastructure::temporal_event_listener`])
/// as Temporal signals workflow progress. See ADR-015 (Workflow Engine Architecture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkflowEvent {
    WorkflowRegistered {
        workflow_id: crate::domain::workflow::WorkflowId,
        name: String,
        version: String,
        scope: crate::domain::workflow::WorkflowScope,
        registered_at: DateTime<Utc>,
    },
    WorkflowExecutionStarted {
        execution_id: ExecutionId,
        workflow_id: crate::domain::workflow::WorkflowId,
        started_at: DateTime<Utc>,
    },
    WorkflowStateEntered {
        execution_id: ExecutionId,
        state_name: String,
        entered_at: DateTime<Utc>,
    },
    WorkflowStateExited {
        execution_id: ExecutionId,
        state_name: String,
        output: serde_json::Value,
        exited_at: DateTime<Utc>,
    },
    WorkflowIterationStarted {
        execution_id: ExecutionId,
        iteration_number: u8,
        started_at: DateTime<Utc>,
    },
    WorkflowIterationCompleted {
        execution_id: ExecutionId,
        iteration_number: u8,
        output: serde_json::Value,
        completed_at: DateTime<Utc>,
    },
    WorkflowIterationFailed {
        execution_id: ExecutionId,
        iteration_number: u8,
        error: String,
        failed_at: DateTime<Utc>,
    },
    WorkflowExecutionCompleted {
        execution_id: ExecutionId,
        final_blackboard: serde_json::Value,
        artifacts: Option<serde_json::Value>,
        completed_at: DateTime<Utc>,
    },
    WorkflowExecutionFailed {
        execution_id: ExecutionId,
        reason: String,
        failed_at: DateTime<Utc>,
    },
    WorkflowExecutionCancelled {
        execution_id: ExecutionId,
        cancelled_at: DateTime<Utc>,
    },
    /// A workflow state triggered a child workflow execution (ADR-065)
    SubworkflowTriggered {
        /// Parent execution that initiated the child
        parent_execution_id: ExecutionId,
        /// Child execution that was started
        child_execution_id: ExecutionId,
        /// The child workflow being invoked
        child_workflow_id: crate::domain::workflow::WorkflowId,
        /// Whether the parent is waiting (blocking) or continuing (fire_and_forget)
        mode: String,
        /// State name in the parent workflow that triggered the child
        parent_state_name: String,
        triggered_at: DateTime<Utc>,
    },
    /// A blocking child workflow completed and its result was written to the parent blackboard (ADR-065)
    SubworkflowCompleted {
        parent_execution_id: ExecutionId,
        child_execution_id: ExecutionId,
        /// Blackboard key under which the child result was stored
        result_key: String,
        completed_at: DateTime<Utc>,
    },
    /// A child workflow failed (ADR-065)
    SubworkflowFailed {
        parent_execution_id: ExecutionId,
        child_execution_id: ExecutionId,
        reason: String,
        failed_at: DateTime<Utc>,
    },
    /// A workflow's visibility scope was changed (ADR-076)
    WorkflowScopeChanged {
        workflow_id: crate::domain::workflow::WorkflowId,
        workflow_name: String,
        previous_scope: crate::domain::workflow::WorkflowScope,
        new_scope: crate::domain::workflow::WorkflowScope,
        previous_tenant_id: crate::domain::tenant::TenantId,
        new_tenant_id: crate::domain::tenant::TenantId,
        changed_by: String,
        changed_at: DateTime<Utc>,
    },
    /// Intent-to-execution pipeline started (ADR-087)
    IntentExecutionPipelineStarted {
        pipeline_execution_id: ExecutionId,
        workflow_execution_id: ExecutionId,
        intent: String,
        language: crate::domain::workflow::ExecutionLanguage,
        workspace_volume_id: VolumeId,
        started_at: DateTime<Utc>,
    },
    /// Intent-to-execution pipeline completed successfully (ADR-087)
    IntentExecutionPipelineCompleted {
        pipeline_execution_id: ExecutionId,
        workflow_execution_id: ExecutionId,
        tenant_id: crate::domain::tenant::TenantId,
        final_result: String,
        duration_ms: u64,
        reused_existing_agent: bool,
        agent_similarity_score: Option<f32>,
        completed_at: DateTime<Utc>,
    },
    /// Intent-to-execution pipeline failed (ADR-087)
    IntentExecutionPipelineFailed {
        pipeline_execution_id: ExecutionId,
        workflow_execution_id: ExecutionId,
        failed_at_state: String,
        reason: String,
        failed_at: DateTime<Utc>,
    },
    /// A workflow definition was permanently deleted from the registry.
    WorkflowRemoved {
        workflow_id: crate::domain::workflow::WorkflowId,
        workflow_name: String,
        removed_at: DateTime<Utc>,
    },
}

/// Cortex pattern weight change events (BC-5 Cortex / Learning & Memory Context).
///
/// Published when the Cortex service updates a pattern's success score after
/// an execution completes. See ADR-018 (Weighted Cortex Memory) and
/// ADR-029 (Cortex Time-Decay Parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LearningEvent {
    PatternDiscovered {
        execution_id: ExecutionId,
        agent_id: AgentId,
        pattern_category: String,
        discovered_at: DateTime<Utc>,
    },
    PatternReinforced {
        execution_id: ExecutionId,
        agent_id: AgentId,
        delta: f64,
        reinforced_at: DateTime<Utc>,
    },
    PatternDecayed {
        execution_id: ExecutionId,
        agent_id: AgentId,
        delta: f64,
        decayed_at: DateTime<Utc>,
    },
}

/// Gradient validation events (BC-2 Execution Context, ADR-017).
///
/// Published by [`crate::application::validation_service::ValidationService`] after each
/// iteration is evaluated. `score` and `confidence` are both in `[0.0, 1.0]`.
/// For multi-judge runs, individual judge scores are aggregated into `MultiJudgeConsensus`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValidationEvent {
    GradientValidationPerformed {
        execution_id: ExecutionId,
        agent_id: AgentId,
        iteration_number: u8,
        score: f64,
        confidence: f64,
        validated_at: DateTime<Utc>,
    },
    MultiJudgeConsensus {
        execution_id: ExecutionId,
        agent_id: AgentId,
        judge_scores: Vec<(AgentId, f64)>,
        final_score: f64,
        confidence: f64,
        reached_at: DateTime<Utc>,
    },
}

/// Volume lifecycle events (BC-7 Storage Gateway Context, ADR-032).
///
/// Published by [`crate::application::volume_manager::VolumeService`] during volume
/// create/attach/detach/delete and TTL expiry. For per-file-operation events,
/// see [`StorageEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VolumeEvent {
    VolumeCreated {
        volume_id: VolumeId,
        execution_id: Option<ExecutionId>,
        storage_class: StorageClass,
        remote_path: String,
        size_limit_bytes: u64,
        created_at: DateTime<Utc>,
    },
    VolumeAttached {
        volume_id: VolumeId,
        instance_id: InstanceId,
        mount_point: String,
        access_mode: String,
        attached_at: DateTime<Utc>,
    },
    VolumeDetached {
        volume_id: VolumeId,
        instance_id: InstanceId,
        detached_at: DateTime<Utc>,
    },
    VolumeDeleted {
        volume_id: VolumeId,
        deleted_at: DateTime<Utc>,
    },
    VolumeExpired {
        volume_id: VolumeId,
        expired_at: DateTime<Utc>,
    },
    VolumeMountFailed {
        volume_id: VolumeId,
        instance_id: InstanceId,
        error: String,
        failed_at: DateTime<Utc>,
    },
    VolumeQuotaExceeded {
        volume_id: VolumeId,
        size_limit_bytes: u64,
        actual_bytes: u64,
        exceeded_at: DateTime<Utc>,
    },
    UserVolumeCreated {
        volume_id: VolumeId,
        owner_user_id: String,
        tenant_id: TenantId,
        label: String,
        size_limit_bytes: u64,
        created_at: DateTime<Utc>,
    },
    UserVolumeRenamed {
        volume_id: VolumeId,
        old_label: String,
        new_label: String,
        renamed_at: DateTime<Utc>,
    },
    UserVolumeDeleted {
        volume_id: VolumeId,
        owner_user_id: String,
        deleted_at: DateTime<Utc>,
    },
    UserVolumeQuotaWarning {
        owner_user_id: String,
        tenant_id: TenantId,
        usage_percent: f32,
        warned_at: DateTime<Utc>,
    },
}

/// Infrastructure-level security policy violation events (BC-4 Security Policy).
///
/// Published by the runtime policy enforcer when an agent container attempts to
/// violate its manifest-declared [`crate::domain::policy::SecurityPolicy`].
/// Distinct from [`MCPToolEvent::PolicyViolation`] which covers SEAL/MCP-level violations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyEvent {
    PolicyViolationAttempted {
        agent_id: AgentId,
        violation_type: String,
        details: String,
        attempted_at: DateTime<Utc>,
    },
    PolicyViolationBlocked {
        agent_id: AgentId,
        violation_type: String,
        details: String,
        blocked_at: DateTime<Utc>,
    },
}

/// Structured classification of policy violation types used in [`MCPToolEvent::PolicyViolation`]
/// and [`crate::domain::seal_session::SealSessionError`] audit records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ViolationType {
    /// The requested MCP tool is not declared in the agent's `SecurityContext` capabilities.
    ToolNotAllowed,
    /// The requested tool is explicitly listed in the `deny` section of the `SecurityContext`.
    ToolExplicitlyDenied,
    /// The agent has exceeded the per-tool rate limit defined in its `Capability`.
    RateLimitExceeded,
    /// The requested filesystem path falls outside the FSAL-enforced `FilesystemPolicy` boundaries.
    PathOutsideBoundary,
    /// A `../` traversal sequence was detected in the requested path before canonicalization.
    PathTraversalAttempt,
    /// The target network domain is not in the `NetworkPolicy` allowlist.
    DomainNotAllowed,
    /// A required argument was missing from the tool invocation payload.
    MissingRequiredArgument,
    /// The tool call exceeded the per-invocation timeout declared in the `Capability`.
    TimeoutExceeded,
}

/// MCP Tool server lifecycle and invocation audit events (BC-12 SEAL / Tool Routing, ADR-033).
///
/// Published by [`crate::application::tool_invocation_service::ToolInvocationService`] and
/// [`crate::infrastructure::tool_router::ToolRouter`]. Consumed by:
/// - The Cortex for tool-usage pattern learning (e.g. "always run `npm install`
///   after modifying `package.json`")
/// - The Zaru client for real-time execution visualization
/// - Security analytics for anomalous tool usage detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MCPToolEvent {
    // ========== Server Lifecycle Events ==========
    ServerRegistered {
        server_id: crate::domain::mcp::ToolServerId,
        name: String,
        capabilities: Vec<String>,
        registered_at: DateTime<Utc>,
    },

    ServerStarted {
        server_id: crate::domain::mcp::ToolServerId,
        name: String,
        process_id: u32,
        started_at: DateTime<Utc>,
    },

    ServerStopped {
        server_id: crate::domain::mcp::ToolServerId,
        name: String,
        stopped_at: DateTime<Utc>,
    },

    ServerFailed {
        server_id: crate::domain::mcp::ToolServerId,
        name: String,
        error: String,
        failed_at: DateTime<Utc>,
    },

    ServerUnhealthy {
        server_id: crate::domain::mcp::ToolServerId,
        last_healthy: Option<DateTime<Utc>>,
    },

    // ========== Tool Invocation Events ==========
    InvocationRequested {
        invocation_id: crate::domain::mcp::ToolInvocationId,
        execution_id: ExecutionId,
        agent_id: AgentId,
        tool_name: String,
        arguments: serde_json::Value,
        requested_at: DateTime<Utc>,
    },

    InvocationStarted {
        invocation_id: crate::domain::mcp::ToolInvocationId,
        execution_id: ExecutionId,
        agent_id: AgentId,
        server_id: crate::domain::mcp::ToolServerId,
        tool_name: String,
        started_at: DateTime<Utc>,
    },

    InvocationCompleted {
        invocation_id: crate::domain::mcp::ToolInvocationId,
        execution_id: ExecutionId,
        agent_id: AgentId,
        result: serde_json::Value,
        duration_ms: u64,
        completed_at: DateTime<Utc>,
    },

    InvocationFailed {
        invocation_id: crate::domain::mcp::ToolInvocationId,
        execution_id: ExecutionId,
        agent_id: AgentId,
        error: crate::domain::mcp::MCPError,
        failed_at: DateTime<Utc>,
    },

    // ========== Policy Violation Events ==========
    PolicyViolation {
        execution_id: ExecutionId,
        agent_id: AgentId,
        tool_name: String,
        violation_type: ViolationType,
        details: String,
        blocked_at: DateTime<Utc>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// BC-3 CI/CD Container Step Events  (ADR-050)
// ─────────────────────────────────────────────────────────────────────────────

/// Reason a ContainerRun or ParallelContainerRun step could not complete (ADR-050).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerRunFailureReason {
    /// The container exited with a non-zero exit code
    NonZeroExitCode { code: i32 },
    /// The step exceeded the configured wall-clock timeout
    TimeoutExpired { timeout_secs: u64 },
    /// The container image could not be pulled from the registry
    ImagePullFailed { image: String, error: String },
    /// A required volume could not be mounted
    VolumeMountFailed { volume: String, error: String },
    /// The container was killed due to memory or CPU exhaustion
    ResourceExhausted { detail: String },
}

/// Domain events for deterministic CI/CD container steps (BC-3, ADR-050).
///
/// Published by [`crate::application::run_container_step::RunContainerStepUseCase`].
/// Consumed by:
/// - The Synapse UI for real-time CI/CD step visualization alongside agent iterations
/// - Cortex for learning which build/test patterns succeed reliably
/// - Audit trail (SOC 2 / compliance)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerRunEvent {
    /// A container step was started — image pull and container creation initiated
    ContainerRunStarted {
        execution_id: ExecutionId,
        /// Logical workflow state name (e.g., "BUILD")
        state_name: String,
        /// Human-readable step label (e.g., "Compile and Build")
        step_name: String,
        image: String,
        command: Vec<String>,
        started_at: DateTime<Utc>,
    },

    /// A container step completed successfully (exit code 0)
    ContainerRunCompleted {
        execution_id: ExecutionId,
        state_name: String,
        step_name: String,
        exit_code: i32,
        stdout_bytes: u64,
        stderr_bytes: u64,
        duration_ms: u64,
        completed_at: DateTime<Utc>,
    },

    /// A container step failed (non-zero exit, timeout, image pull failure, etc.)
    ContainerRunFailed {
        execution_id: ExecutionId,
        state_name: String,
        step_name: String,
        reason: ContainerRunFailureReason,
        failed_at: DateTime<Utc>,
    },

    /// A ParallelContainerRun state finished aggregating all step results
    ParallelContainerRunAggregated {
        execution_id: ExecutionId,
        state_name: String,
        total_steps: u32,
        succeeded: u32,
        failed: u32,
        /// Serialized `ParallelCompletionStrategy` variant name
        strategy: String,
        aggregated_at: DateTime<Utc>,
    },
}

/// Commands executed inside the container via Dispatch Protocol (ADR-040).
///
/// Published at each phase of a `cmd.run` dispatch lifecycle. Consumed by Cortex
/// for command-pattern learning and by operators via the event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandExecutionEvent {
    /// The orchestrator dispatched an exec action to `bootstrap.py`.
    CommandExecutionStarted {
        execution_id: ExecutionId,
        dispatch_id: DispatchId,
        command: String,
        args: Vec<String>,
        cwd: String,
        timeout_secs: u32,
        started_at: DateTime<Utc>,
    },
    /// The bootstrap re-POSTed a completed dispatch result.
    CommandExecutionCompleted {
        execution_id: ExecutionId,
        dispatch_id: DispatchId,
        command: String,
        exit_code: i32,
        stdout_bytes: u64,
        stderr_bytes: u64,
        duration_ms: u64,
        truncated: bool,
        completed_at: DateTime<Utc>,
    },
    /// The dispatch failed at the protocol layer.
    CommandExecutionFailed {
        execution_id: ExecutionId,
        dispatch_id: DispatchId,
        command: String,
        reason: CommandFailureReason,
        failed_at: DateTime<Utc>,
    },
    /// A `cmd.run` tool call was blocked before dispatch by the security policy engine.
    CommandPolicyViolation {
        execution_id: ExecutionId,
        tool_call_id: String,
        command: String,
        args: Vec<String>,
        violation_type: crate::domain::security_context::PolicyViolation,
        blocked_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandFailureReason {
    /// The subprocess exited with a non-zero status code.
    NonZeroExitCode { code: i32 },
    /// The subprocess was killed because it exceeded the configured timeout.
    TimeoutExpired { timeout_secs: u32 },
    /// The orchestrator timed out waiting for `bootstrap.py` to re-POST the dispatch result.
    DispatchTimeout { waited_secs: u32 },
    /// `bootstrap.py` reported an unknown dispatch action type.
    UnknownAction { action: String },
}

/// SEAL session lifecycle and security events (BC-12 SEAL Protocol, ADR-035 §5).
///
/// Published by [`crate::application::attestation_service::AttestationServiceImpl`]
/// and [`crate::infrastructure::seal::audit::SealAuditLogger`].
/// Consumed by:
/// - Cortex for security pattern learning (e.g. detecting attestation storms)
/// - Zaru client for real-time security dashboard
/// - SOC 2 audit trail export (Phase 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SealEvent {
    /// Agent completed the SEAL attestation handshake and received a SecurityToken.
    AttestationCompleted {
        agent_id: AgentId,
        execution_id: ExecutionId,
        security_context_name: String,
        attested_at: DateTime<Utc>,
    },
    /// A new SealSession was created for an agent execution.
    SessionCreated {
        session_id: String,
        agent_id: AgentId,
        execution_id: ExecutionId,
        security_context_name: String,
        expires_at: DateTime<Utc>,
        created_at: DateTime<Utc>,
    },
    /// An SealSession was revoked (execution complete or security incident).
    SessionRevoked {
        session_id: String,
        agent_id: AgentId,
        reason: String,
        revoked_at: DateTime<Utc>,
    },
    /// A tool call was blocked by the SecurityContext policy engine.
    PolicyViolationBlocked {
        agent_id: AgentId,
        execution_id: ExecutionId,
        tool_name: String,
        violation_type: ViolationType,
        details: String,
        blocked_at: DateTime<Utc>,
    },
}

/// Stimulus ingestion and routing events (BC-8 Stimulus-Response Context, ADR-021).
///
/// Published by [`crate::application::stimulus::StandardStimulusService`].
/// Consumed by:
/// - Cortex for routing pattern learning (Stage 2 → Stage 1 promotion signals)
/// - Zaru client for stimulus audit dashboard
/// - ADR-030 event replay (Phase 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StimulusEvent {
    /// A stimulus arrived and passed authentication — before routing begins.
    StimulusReceived {
        stimulus_id: crate::domain::stimulus::StimulusId,
        /// Canonical source name (from `StimulusSource::name()`).
        source: String,
        received_at: DateTime<Utc>,
    },

    /// Routing succeeded and a workflow execution was started.
    StimulusClassified {
        stimulus_id: crate::domain::stimulus::StimulusId,
        /// UUID string of the WorkflowId that was selected.
        workflow_id: String,
        /// Confidence score: 1.0 for deterministic routes, [0.7, 1.0] for LLM.
        confidence: f64,
        /// "Deterministic" | "LlmClassified"
        routing_mode: String,
        classified_at: DateTime<Utc>,
    },

    /// The stimulus was rejected before or after routing.
    /// `reason` is a human-readable string (e.g. "low_confidence: 0.42", "hmac_invalid").
    StimulusRejected {
        stimulus_id: crate::domain::stimulus::StimulusId,
        reason: String,
        rejected_at: DateTime<Utc>,
    },

    /// The RouterAgent execution itself failed (distinct from a low-confidence classification).
    ClassificationFailed {
        stimulus_id: crate::domain::stimulus::StimulusId,
        error: String,
        failed_at: DateTime<Utc>,
    },
}

/// IAM & Identity Federation events (BC-13, ADR-041).
///
/// Published by `StandardOIDCIamService` during token validation,
/// realm lifecycle operations, and JWKS cache refresh cycles.
/// Consumed by:
/// - Cortex for security pattern learning (e.g. detecting brute-force token validation failures)
/// - Zaru client for IAM audit dashboard
/// - SOC 2 audit trail export (Phase 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IamEvent {
    // ─── Authentication Events ────────────────────────────────────────────────
    /// A JWT was successfully validated against a OIDC realm's JWKS.
    UserAuthenticated {
        sub: String,
        realm_slug: String,
        /// "consumer_user" | "operator" | "service_account" | "tenant_user"
        identity_kind: String,
        authenticated_at: DateTime<Utc>,
    },
    /// JWT validation failed (bad signature, expired, unknown issuer, etc.).
    TokenValidationFailed {
        /// None if the issuer claim could not be parsed.
        realm_slug: Option<String>,
        reason: String,
        attempted_at: DateTime<Utc>,
    },

    // ─── Realm Lifecycle ──────────────────────────────────────────────────────
    /// A new realm was registered in the trusted realm set.
    RealmRegistered {
        realm_slug: String,
        /// "system" | "consumer" | "tenant"
        realm_kind: String,
        issuer_url: String,
        registered_at: DateTime<Utc>,
    },
    /// A tenant realm was provisioned (Phase 2 — OIDC Admin API + secret namespace).
    TenantRealmProvisioned {
        tenant_slug: String,
        realm_id: String,
        /// Mirrors ADR-034 namespace alignment.
        secret_namespace: String,
        provisioned_at: DateTime<Utc>,
    },

    // ─── Service Account Lifecycle ────────────────────────────────────────────
    /// A service account OIDC client was created.
    ServiceAccountProvisioned {
        client_id: String,
        realm_slug: String,
        /// "client_credentials" | "authorization_code_pkce"
        grant_type: String,
        provisioned_at: DateTime<Utc>,
    },
    /// A service account was revoked.
    ServiceAccountRevoked {
        client_id: String,
        realm_slug: String,
        reason: String,
        revoked_at: DateTime<Utc>,
    },

    // ─── JWKS Cache Events ────────────────────────────────────────────────────
    /// The JWKS key set for a realm was successfully refreshed.
    JwksCacheRefreshed {
        realm_slug: String,
        key_count: usize,
        refreshed_at: DateTime<Utc>,
    },
    /// JWKS refresh failed (network error, invalid response, etc.).
    JwksCacheRefreshFailed {
        realm_slug: String,
        reason: String,
        failed_at: DateTime<Utc>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// BC-11 Secrets & Identity Management Events  (ADR-034)
// ─────────────────────────────────────────────────────────────────────────────

/// Domain events for the Secrets & Identity Management bounded context (BC-11).
///
/// These events form the cryptographic audit trail required by ADR-034 for
/// every secret access, dynamic credential generation, and policy denial.
/// They are consumed by the Cortex and the compliance reporting pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SecretEvent {
    // ─── Static Secret Access ─────────────────────────────────────────────────
    /// A static secret was successfully read from the store.
    SecretRetrieved {
        engine: String,
        path: String,
        access_context: AccessContext,
        retrieved_at: DateTime<Utc>,
    },
    /// A static secret was written (created or updated) in the store.
    SecretWritten {
        engine: String,
        path: String,
        access_context: AccessContext,
        written_at: DateTime<Utc>,
    },
    // ─── Dynamic Credentials ──────────────────────────────────────────────────
    /// A short-lived dynamic secret was generated by the database/PKI engine.
    DynamicSecretGenerated {
        engine: String,
        role: String,
        lease_id: String,
        lease_duration_secs: u64,
        access_context: AccessContext,
        generated_at: DateTime<Utc>,
    },
    /// An active lease was successfully renewed, extending its TTL.
    LeaseRenewed {
        lease_id: String,
        new_duration_secs: u64,
        access_context: AccessContext,
        renewed_at: DateTime<Utc>,
    },
    /// A lease was explicitly revoked before its natural expiry.
    LeaseRevoked {
        lease_id: String,
        access_context: AccessContext,
        revoked_at: DateTime<Utc>,
    },
    // ─── Security Violations ──────────────────────────────────────────────────
    /// A secret access was denied (permission error, missing path, policy block).
    SecretAccessDenied {
        engine: Option<String>,
        path: Option<String>,
        reason: String,
        access_context: AccessContext,
        denied_at: DateTime<Utc>,
    },
}

/// Tenant lifecycle and audit events (ADR-056)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TenantEvent {
    /// New tenant provisioned (Keycloak realm + OpenBao namespace + DB record)
    TenantProvisioned {
        tenant_slug: String,
        tier: String,
        keycloak_realm: String,
        openbao_namespace: String,
        provisioned_at: DateTime<Utc>,
    },
    /// Tenant suspended by operator
    TenantSuspended {
        tenant_slug: String,
        reason: String,
        suspended_at: DateTime<Utc>,
    },
    /// Tenant soft-deleted (30-day retention)
    TenantSoftDeleted {
        tenant_slug: String,
        deleted_at: DateTime<Utc>,
    },
    /// Tenant permanently deleted (after retention window or explicit hard-delete)
    TenantHardDeleted {
        tenant_id: TenantId,
        hard_deleted_at: DateTime<Utc>,
    },
    /// Admin accessed another tenant's data via X-Aegis-Tenant header.
    /// The admin's own tenant is implied by their authenticated identity.
    AdminCrossTenantAccess {
        admin_identity: String,
        target_tenant_id: TenantId,
        accessed_at: DateTime<Utc>,
    },
    /// Tenant quota exceeded
    TenantQuotaExceeded {
        tenant_slug: String,
        quota_kind: TenantQuotaKind,
        current_value: u64,
        limit: u64,
        exceeded_at: DateTime<Utc>,
    },
    /// A per-tenant resource quota was updated by an operator
    TenantQuotaUpdated {
        tenant_id: TenantId,
        quota_kind: TenantQuotaKind,
        old_limit: u64,
        new_limit: u64,
        updated_at: DateTime<Utc>,
        updated_by: String,
    },
    /// Consumer user's per-user tenant provisioned at signup (ADR-097)
    UserTenantProvisioned {
        tenant_slug: String,
        user_sub: String,
        tier: String,
        keycloak_realm: String,
        provisioned_at: DateTime<Utc>,
    },
}

/// Cross-context drift events — emitted whenever a cached external
/// identifier (Stripe customer/subscription, Keycloak user/realm) is
/// discovered to be missing or stale at the point of use. Observers can
/// route these to alerting, reconciliation jobs, or audit trails.
///
/// Producers treat cached IDs as hints, not contracts — every miss is
/// self-healed if possible and surfaced here for observability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DriftEvent {
    /// A Keycloak user referenced by a cached tenant/subscription no
    /// longer exists in the expected realm.
    KeycloakUserMissing {
        tenant_id: TenantId,
        user_sub: String,
        detected_at: DateTime<Utc>,
    },
    /// A Keycloak realm referenced by enterprise tenant sync no longer
    /// exists or cannot be listed.
    KeycloakRealmMissing {
        realm: String,
        tenant_id: Option<TenantId>,
        detected_at: DateTime<Utc>,
    },
    /// A Stripe customer referenced by a cached mapping no longer
    /// exists on Stripe's side (e.g. sandbox reset).
    StripeCustomerMissing {
        customer_id: String,
        tenant_id: Option<TenantId>,
        detected_at: DateTime<Utc>,
    },
    /// A Stripe subscription referenced by a cached mapping no longer
    /// exists.
    StripeSubscriptionMissing {
        subscription_id: String,
        tenant_id: TenantId,
        detected_at: DateTime<Utc>,
    },
    /// A tenant subscription row references a team tenant whose
    /// underlying team aggregate is missing — a dangling/orphaned
    /// subscription row.
    OrphanSubscription {
        tenant_id: TenantId,
        stripe_customer_id: String,
        detected_at: DateTime<Utc>,
    },
}

/// Rate limit enforcement events (ADR-072).
///
/// Published by [`crate::infrastructure::rate_limit::CompositeRateLimitEnforcer`] when a
/// request is rejected or when usage crosses the 80% warning threshold.
/// Consumed by:
/// - Alerting pipelines for real-time operator notification
/// - Audit trail for compliance and forensic analysis
/// - Cortex for usage pattern learning (Phase 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RateLimitEvent {
    /// Published when a rate limit check rejects a request.
    Exceeded {
        user_id: Option<String>,
        tenant_id: Option<String>,
        resource_type: String,
        bucket: String,
        limit: u64,
        counter: u64,
        timestamp: DateTime<Utc>,
    },
    /// Published when usage crosses 80% of a bucket limit.
    Warning {
        user_id: Option<String>,
        tenant_id: Option<String>,
        resource_type: String,
        bucket: String,
        limit: u64,
        current: u64,
        threshold_percent: u8,
        timestamp: DateTime<Utc>,
    },
}

/// Swarm coordination events (BC-6, ADR-039).
///
/// Published by `StandardSwarmService` for multi-agent lifecycle observability.
/// Feeds The Synapse's Glass Laboratory nested iteration sub-panels, the audit
/// trail, and Cortex swarm topology learning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SwarmEvent {
    /// A new swarm was created for coordinated multi-agent execution.
    SwarmCreated {
        swarm_id: uuid::Uuid,
        parent_execution_id: ExecutionId,
        created_at: DateTime<Utc>,
    },
    /// A child agent was spawned within a swarm.
    ChildSpawned {
        swarm_id: uuid::Uuid,
        agent_id: AgentId,
        execution_id: ExecutionId,
        spawned_at: DateTime<Utc>,
    },
    /// A swarm was dissolved (all children cancelled or completed).
    SwarmDissolved {
        swarm_id: uuid::Uuid,
        reason: String,
        dissolved_at: DateTime<Utc>,
    },
    /// A resource lock was acquired within a swarm.
    LockAcquired {
        swarm_id: uuid::Uuid,
        resource_id: String,
        holder: AgentId,
        execution_id: ExecutionId,
    },
    /// A resource lock was released within a swarm.
    LockReleased {
        swarm_id: uuid::Uuid,
        resource_id: String,
    },
    /// A message was broadcast to all swarm members.
    MessageBroadcast {
        swarm_id: uuid::Uuid,
        from: AgentId,
        recipient_count: usize,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// BC-11 Credential Binding Events  (ADR-078)
// ─────────────────────────────────────────────────────────────────────────────

/// Domain events for the Credential Binding bounded context (BC-11, ADR-078).
///
/// Published by the credential application service for every significant state
/// change on a [`crate::domain::credential::UserCredentialBinding`] aggregate.
/// Consumed by:
/// - Cortex for credential usage pattern learning
/// - Zaru client for the credential management dashboard
/// - SOC 2 audit trail export (Phase 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CredentialEvent {
    /// A new credential binding was created and stored in OpenBao.
    CredentialCreated {
        binding_id: CredentialBindingId,
        owner_user_id: String,
        tenant_id: TenantId,
        provider: CredentialProvider,
        credential_type: CredentialType,
    },
    /// A credential binding was explicitly revoked; all grants are cleared.
    CredentialRevoked {
        binding_id: CredentialBindingId,
        tenant_id: TenantId,
    },
    /// The underlying secret value was rotated in OpenBao; the binding id is
    /// stable but the secret path leaf has been updated.
    CredentialRotated {
        binding_id: CredentialBindingId,
        tenant_id: TenantId,
    },
    /// A new grant was added, allowing `target` to use the binding.
    CredentialGranted {
        binding_id: CredentialBindingId,
        grant_id: CredentialGrantId,
        target: GrantTarget,
        granted_by: String,
    },
    /// An existing grant was revoked, revoking `target`'s access.
    CredentialGrantRevoked {
        binding_id: CredentialBindingId,
        grant_id: CredentialGrantId,
    },
    /// An agent successfully retrieved the credential value for use in an
    /// execution — emitted for access audit and Cortex learning.
    CredentialAccessed {
        binding_id: CredentialBindingId,
        agent_id: AgentId,
        tenant_id: TenantId,
    },
}

#[cfg(test)]
mod tests_iam {
    use super::*;

    #[test]
    fn test_iam_event_user_authenticated_serialization() {
        let event = IamEvent::UserAuthenticated {
            sub: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            realm_slug: "zaru-consumer".to_string(),
            identity_kind: "consumer_user".to_string(),
            authenticated_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: IamEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, IamEvent::UserAuthenticated { .. }),
            "Expected UserAuthenticated variant"
        );
        let IamEvent::UserAuthenticated {
            sub, realm_slug, ..
        } = deserialized
        else {
            return;
        };
        assert_eq!(sub, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(realm_slug, "zaru-consumer");
    }

    #[test]
    fn test_iam_event_token_validation_failed_serialization() {
        let event = IamEvent::TokenValidationFailed {
            realm_slug: Some("aegis-system".to_string()),
            reason: "JWT expired".to_string(),
            attempted_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("TokenValidationFailed"));
        assert!(json.contains("JWT expired"));
    }

    #[test]
    fn test_iam_event_jwks_cache_refreshed_serialization() {
        let event = IamEvent::JwksCacheRefreshed {
            realm_slug: "aegis-system".to_string(),
            key_count: 2,
            refreshed_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("JwksCacheRefreshed"));
        assert!(json.contains("\"key_count\":2"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::execution::{ExecutionId, IterationError};
    use crate::domain::volume::{StorageClass, VolumeId};
    use chrono::Utc;

    // ── StorageEvent serialization ────────────────────────────────────────────

    #[test]
    fn test_storage_event_file_opened_serialization() {
        let exec_id = ExecutionId::new();
        let vol_id = VolumeId::new();
        let event = StorageEvent::FileOpened {
            execution_id: Some(exec_id),
            workflow_execution_id: None,
            volume_id: vol_id,
            path: "/workspace/file.txt".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: StorageEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, StorageEvent::FileOpened { .. }),
            "Expected FileOpened variant, got: {deserialized:?}"
        );
        let StorageEvent::FileOpened {
            path, open_mode, ..
        } = deserialized
        else {
            return;
        };
        assert_eq!(path, "/workspace/file.txt");
        assert_eq!(open_mode, "read");
    }

    #[test]
    fn test_storage_event_quota_exceeded_serialization() {
        let event = StorageEvent::QuotaExceeded {
            execution_id: Some(ExecutionId::new()),
            workflow_execution_id: None,
            volume_id: VolumeId::new(),
            requested_bytes: 1024,
            available_bytes: 0,
            exceeded_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("QuotaExceeded"));
    }

    // ── ExecutionEvent serialization ──────────────────────────────────────────

    #[test]
    fn test_execution_event_started_serialization() {
        let exec_id = ExecutionId::new();
        let agent_id = AgentId::new();
        let event = ExecutionEvent::ExecutionStarted {
            execution_id: exec_id,
            agent_id,
            started_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ExecutionEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, ExecutionEvent::ExecutionStarted { .. }),
            "Expected ExecutionStarted variant, got: {deserialized:?}"
        );
        let ExecutionEvent::ExecutionStarted { execution_id, .. } = deserialized else {
            return;
        };
        assert_eq!(execution_id, exec_id);
    }

    #[test]
    fn test_execution_event_completed_serialization() {
        let event = ExecutionEvent::ExecutionCompleted {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            final_output: "result".to_string(),
            total_iterations: 3,
            completed_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("ExecutionCompleted"));
        assert!(json.contains("result"));
    }

    #[test]
    fn test_execution_event_iteration_failed_serialization() {
        let event = ExecutionEvent::IterationFailed {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            iteration_number: 2,
            error: IterationError {
                message: "compile error".to_string(),
                details: None,
            },
            failed_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("IterationFailed"));
    }

    // ── ValidationEvent serialization ─────────────────────────────────────────

    #[test]
    fn test_validation_event_gradient_serialization() {
        let event = ValidationEvent::GradientValidationPerformed {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            iteration_number: 1,
            score: 0.9,
            confidence: 0.85,
            validated_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ValidationEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(
                deserialized,
                ValidationEvent::GradientValidationPerformed { .. }
            ),
            "Expected GradientValidationPerformed variant, got: {deserialized:?}"
        );
        let ValidationEvent::GradientValidationPerformed {
            score, confidence, ..
        } = deserialized
        else {
            return;
        };
        assert_eq!(score, 0.9);
        assert_eq!(confidence, 0.85);
    }

    #[test]
    fn test_validation_event_consensus_serialization() {
        let event = ValidationEvent::MultiJudgeConsensus {
            execution_id: ExecutionId::new(),
            agent_id: AgentId::new(),
            judge_scores: vec![(AgentId::new(), 0.9), (AgentId::new(), 0.85)],
            final_score: 0.875,
            confidence: 0.9,
            reached_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("MultiJudgeConsensus"));
    }

    // ── VolumeEvent serialization ─────────────────────────────────────────────

    #[test]
    fn test_volume_event_created_serialization() {
        let event = VolumeEvent::VolumeCreated {
            volume_id: VolumeId::new(),
            execution_id: Some(ExecutionId::new()),
            storage_class: StorageClass::persistent(),
            remote_path: "/volumes/test".to_string(),
            size_limit_bytes: 1024 * 1024 * 1024,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("VolumeCreated"));
    }

    #[test]
    fn test_volume_event_quota_exceeded_serialization() {
        let event = VolumeEvent::VolumeQuotaExceeded {
            volume_id: VolumeId::new(),
            size_limit_bytes: 1024,
            actual_bytes: 2048,
            exceeded_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("VolumeQuotaExceeded"));
    }

    // ── PolicyEvent serialization ─────────────────────────────────────────────

    #[test]
    fn test_policy_event_violation_serialization() {
        let event = PolicyEvent::PolicyViolationBlocked {
            agent_id: AgentId::new(),
            violation_type: "network".to_string(),
            details: "Attempted access to evil.com".to_string(),
            blocked_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PolicyEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, PolicyEvent::PolicyViolationBlocked { .. }),
            "Expected PolicyViolationBlocked variant, got: {deserialized:?}"
        );
        let PolicyEvent::PolicyViolationBlocked { violation_type, .. } = deserialized else {
            return;
        };
        assert_eq!(violation_type, "network");
    }

    // ── AgentLifecycleEvent ───────────────────────────────────────────────────

    #[test]
    fn test_agent_lifecycle_event_paused_serialization() {
        let event = AgentLifecycleEvent::AgentPaused {
            agent_id: AgentId::new(),
            paused_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("AgentPaused"));
    }
}
