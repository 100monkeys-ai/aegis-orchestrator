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
//! | [`MCPToolEvent`] | BC-12 SMCP / Tool Routing | MCP server lifecycle and tool invocation audit (ADR-033) |
//! | [`ImageManagementEvent`] | BC-2 Execution | Container image pull lifecycle and cache status (ADR-045) |
//!
//! ## Phase 2 Note
//!
//! The EventBus is currently **in-memory only** (tokio broadcast channel). Persistent
//! event replay and external consumers (Kafka, NATS) are planned for Phase 2 per ADR-030.

use crate::domain::agent::{AgentId, AgentManifest, ImagePullPolicy};
use crate::domain::execution::{CodeDiff, ExecutionId, IterationError};
use crate::domain::runtime::InstanceId;
use crate::domain::volume::{StorageClass, VolumeId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

/// Container image lifecycle events emitted by [`crate::infrastructure::runtime::DockerRuntime`]
/// during the `spawn()` path (ADR-045).
///
/// Published to the [`crate::infrastructure::event_bus::EventBus`] before/after every
/// Docker image pull so that the Control Plane, Cortex, and audit log can:
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
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        open_mode: String, // "read", "write", "read-write", "create"
        opened_at: DateTime<Utc>,
    },
    FileRead {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        offset: u64,
        bytes_read: u64,
        duration_ms: u64,
        read_at: DateTime<Utc>,
    },
    FileWritten {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        offset: u64,
        bytes_written: u64,
        duration_ms: u64,
        written_at: DateTime<Utc>,
    },
    FileClosed {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        closed_at: DateTime<Utc>,
    },
    DirectoryListed {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        entry_count: usize,
        listed_at: DateTime<Utc>,
    },
    FileCreated {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        created_at: DateTime<Utc>,
    },
    FileDeleted {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        path: String,
        deleted_at: DateTime<Utc>,
    },
    PathTraversalBlocked {
        execution_id: ExecutionId,
        attempted_path: String,
        blocked_at: DateTime<Utc>,
    },
    FilesystemPolicyViolation {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        operation: String, // "read", "write", "delete"
        path: String,
        policy_rule: String,
        violated_at: DateTime<Utc>,
    },
    QuotaExceeded {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        requested_bytes: u64,
        available_bytes: u64,
        exceeded_at: DateTime<Utc>,
    },
    UnauthorizedVolumeAccess {
        execution_id: ExecutionId,
        volume_id: VolumeId,
        attempted_at: DateTime<Utc>,
    },
}

/// Agent manifest lifecycle events (BC-1 Agent Lifecycle Context).
///
/// Published by [`crate::application::lifecycle::StandardAgentLifecycleService`].
/// Consumers: Control Plane gRPC stream, Cortex (tracks agent version history).
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
    Validation(ValidationEvent),
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
}

/// Infrastructure-level security policy violation events (BC-4 Security Policy).
///
/// Published by the runtime policy enforcer when an agent container attempts to
/// violate its manifest-declared [`crate::domain::policy::SecurityPolicy`].
/// Distinct from [`MCPToolEvent::PolicyViolation`] which covers SMCP/MCP-level violations.
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
/// and [`crate::domain::smcp_session::SmcpSessionError`] audit records.
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

/// MCP Tool server lifecycle and invocation audit events (BC-12 SMCP / Tool Routing, ADR-033).
///
/// Published by [`crate::application::tool_invocation_service::ToolInvocationService`] and
/// [`crate::infrastructure::tool_router::ToolRouter`]. Consumed by:
/// - The Cortex for tool-usage pattern learning (e.g. "always run `npm install`
///   after modifying `package.json`")
/// - The Control Plane for real-time execution visualization
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

/// SMCP session lifecycle and security events (BC-12 SMCP Protocol, ADR-035 §5).
///
/// Published by [`crate::application::attestation_service::AttestationServiceImpl`]
/// and [`crate::infrastructure::smcp::audit::SmcpAuditLogger`].
/// Consumed by:
/// - Cortex for security pattern learning (e.g. detecting attestation storms)
/// - Control Plane for real-time security dashboard
/// - SOC 2 audit trail export (Phase 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SmcpEvent {
    /// Agent completed the SMCP attestation handshake and received a SecurityToken.
    AttestationCompleted {
        agent_id: AgentId,
        execution_id: ExecutionId,
        security_context_name: String,
        attested_at: DateTime<Utc>,
    },
    /// A new SmcpSession was created for an agent execution.
    SessionCreated {
        session_id: String,
        agent_id: AgentId,
        execution_id: ExecutionId,
        security_context_name: String,
        expires_at: DateTime<Utc>,
        created_at: DateTime<Utc>,
    },
    /// An SmcpSession was revoked (execution complete or security incident).
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
/// - Control Plane for stimulus audit dashboard
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
            execution_id: exec_id,
            volume_id: vol_id,
            path: "/workspace/file.txt".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: StorageEvent = serde_json::from_str(&json).unwrap();
        let StorageEvent::FileOpened {
            path, open_mode, ..
        } = deserialized
        else {
            panic!("Expected FileOpened variant, got: {:?}", deserialized);
        };
        assert_eq!(path, "/workspace/file.txt");
        assert_eq!(open_mode, "read");
    }

    #[test]
    fn test_storage_event_quota_exceeded_serialization() {
        let event = StorageEvent::QuotaExceeded {
            execution_id: ExecutionId::new(),
            volume_id: VolumeId::new(),
            requested_bytes: 1024,
            available_bytes: 0,
            exceeded_at: Utc::now(),
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
        let ExecutionEvent::ExecutionStarted { execution_id, .. } = deserialized else {
            panic!("Expected ExecutionStarted variant, got: {:?}", deserialized);
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
        let ValidationEvent::GradientValidationPerformed {
            score, confidence, ..
        } = deserialized
        else {
            panic!(
                "Expected GradientValidationPerformed variant, got: {:?}",
                deserialized
            );
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
        let PolicyEvent::PolicyViolationBlocked { violation_type, .. } = deserialized else {
            panic!(
                "Expected PolicyViolationBlocked variant, got: {:?}",
                deserialized
            );
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
