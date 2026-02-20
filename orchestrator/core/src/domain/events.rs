// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use crate::domain::agent::{AgentId, AgentManifest};
use crate::domain::execution::{ExecutionId, IterationError, CodeDiff};
use crate::domain::runtime::InstanceId;
use crate::domain::volume::{VolumeId, StorageClass};

/// Storage Gateway file-level events (ADR-036)
///
/// These events provide file-level audit trail for NFS Server Gateway operations,
/// complementing volume lifecycle events (VolumeEvent).
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


#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Cortex(aegis_cortex::domain::events::CortexEvent),
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LearningEvent {
    // Basic placeholder derived from AGENTS.md
    PatternDiscovered { 
        execution_id: ExecutionId,
        discovered_at: DateTime<Utc>
    } 
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValidationEvent {
    GradientValidationPerformed {
        execution_id: ExecutionId,
        iteration_number: u8,
        score: f64,
        confidence: f64,
        validated_at: DateTime<Utc>,
    },
    MultiJudgeConsensus {
        execution_id: ExecutionId,
        judge_scores: Vec<(AgentId, f64)>,
        final_score: f64,
        confidence: f64,
        reached_at: DateTime<Utc>,
    },
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PolicyEvent {
    PolicyViolationAttempted {
        violation_type: String,
        details: String,
        attempted_at: DateTime<Utc>,
    },
    PolicyViolationBlocked {
        violation_type: String,
        details: String,
        blocked_at: DateTime<Utc>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ViolationType {
    ToolNotAllowed,
    ToolExplicitlyDenied,
    RateLimitExceeded,
    PathOutsideBoundary,
    PathTraversalAttempt,
    DomainNotAllowed,
    MissingRequiredArgument,
    TimeoutExceeded,
}

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
        result: serde_json::Value,
        duration_ms: u64,
        completed_at: DateTime<Utc>,
    },
    
    InvocationFailed {
        invocation_id: crate::domain::mcp::ToolInvocationId,
        execution_id: ExecutionId,
        error: crate::domain::mcp::MCPError,
        failed_at: DateTime<Utc>,
    },
    
    // ========== Policy Violation Events ==========
    
    PolicyViolation {
        execution_id: ExecutionId,
        tool_name: String,
        violation_type: ViolationType,
        details: String,
        blocked_at: DateTime<Utc>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::execution::{ExecutionId, IterationError};
    use crate::domain::volume::{VolumeId, StorageClass};
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
        if let StorageEvent::FileOpened { path, open_mode, .. } = deserialized {
            assert_eq!(path, "/workspace/file.txt");
            assert_eq!(open_mode, "read");
        } else {
            panic!("unexpected variant");
        }
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
        if let ExecutionEvent::ExecutionStarted { execution_id, .. } = deserialized {
            assert_eq!(execution_id, exec_id);
        } else {
            panic!("unexpected variant");
        }
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
            iteration_number: 1,
            score: 0.9,
            confidence: 0.85,
            validated_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: ValidationEvent = serde_json::from_str(&json).unwrap();
        if let ValidationEvent::GradientValidationPerformed { score, confidence, .. } = deserialized {
            assert_eq!(score, 0.9);
            assert_eq!(confidence, 0.85);
        } else {
            panic!("unexpected variant");
        }
    }

    #[test]
    fn test_validation_event_consensus_serialization() {
        let event = ValidationEvent::MultiJudgeConsensus {
            execution_id: ExecutionId::new(),
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
            violation_type: "network".to_string(),
            details: "Attempted access to evil.com".to_string(),
            blocked_at: Utc::now(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: PolicyEvent = serde_json::from_str(&json).unwrap();
        if let PolicyEvent::PolicyViolationBlocked { violation_type, .. } = deserialized {
            assert_eq!(violation_type, "network");
        } else {
            panic!("unexpected variant");
        }
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
