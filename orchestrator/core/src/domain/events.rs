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
