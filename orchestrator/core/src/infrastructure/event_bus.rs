// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Domain Event Bus (ADR-030)
//!
//! In-memory pub/sub event bus based on `tokio::sync::broadcast`. Every domain
//! aggregate publishes [`DomainEvent`]s here; infrastructure adapters (CLI
//! streamer, SSE endpoint, Cortex indexer) subscribe independently.
//!
//! ## Event Flow
//!
//! ```text
//! Execution aggregate
//!   │  publish_execution_event(IterationCompleted { .. })
//!   ▼
//! EventBus  (broadcast channel, capacity 1000)
//!   ├── CLI subscriber  →  prints progress to terminal
//!   ├── SSE subscriber  →  streams to Control Plane WebSocket
//!   └── Cortex subscriber → indexes RefinementApplied patterns
//! ```
//!
//! ## Bounded Context Coverage
//!
//! The [`DomainEvent`] enum wraps events from **all** bounded contexts so a
//! single subscriber type can observe the whole system:
//!
//! | Variant | Source BC | ADR |
//! |---------|-----------|-----|
//! | `AgentLifecycle` | BC-1 | — |
//! | `Execution` | BC-2 | ADR-005 |
//! | `Workflow` | BC-3 | ADR-015 |
//! | `Volume` | BC-7 | ADR-032 |
//! | `Storage` | BC-7 | ADR-036 |
//! | `MCP` | BC-4/Tools | ADR-033 |
//! | `Learning` / `Cortex` | BC-5 | ADR-018 |
//! | `Policy` | BC-4 | — |
//! | `Stimulus` | BC-8 | ADR-021 |
//! | `ImageManagement` | BC-2 | ADR-045 |
//! | `Iam` | BC-13 | ADR-041 |
//! | `Secrets` | BC-11 | ADR-034 |
//!
//! # Code Quality Principles
//!
//! - Keep publish/subscribe semantics in-memory and explicit for the current runtime model.
//! - Preserve typed domain events; do not smuggle transport-specific payloads through the bus.
//! - Fail predictably on lag or delivery issues so subscribers can make their own recovery choices.
//!
//! ## Phase Notes
//!
//! ⚠️ Phase 1 — In-memory only; events are lost on orchestrator restart.
//! Phase 2 will add a persistent event store for replay capability (ADR-030).
//!
//! See ADR-030 (Event Bus Architecture).

// Event Bus Implementation - Pub/Sub for Domain Events
//
// Provides in-memory event streaming using tokio broadcast channels.
// Enables real-time event streaming to CLI, SSE endpoints, and observers.
//
// For MVP: In-memory only (events lost on restart)
// Phase 2: Add persistent event store for replay capability

use crate::domain::agent::AgentId;
use crate::domain::events::{
    AgentLifecycleEvent, ContainerRunEvent, ExecutionEvent, IamEvent, ImageManagementEvent,
    LearningEvent, MCPToolEvent, PolicyEvent, SecretEvent, SmcpEvent, StimulusEvent, StorageEvent,
    ValidationEvent, VolumeEvent, WorkflowEvent,
};
use crate::domain::execution::ExecutionId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::warn;

fn domain_event_type(event: &DomainEvent) -> &'static str {
    match event {
        DomainEvent::AgentLifecycle(_) => "agent_lifecycle",
        DomainEvent::Execution(_) => "execution",
        DomainEvent::Workflow(_) => "workflow",
        DomainEvent::Learning(_) => "learning",
        DomainEvent::Policy(_) => "policy",
        DomainEvent::Volume(_) => "volume",
        DomainEvent::Storage(_) => "storage",
        DomainEvent::MCP(_) => "mcp",
        DomainEvent::Smcp(_) => "smcp",
        DomainEvent::Stimulus(_) => "stimulus",
        DomainEvent::ImageManagement(_) => "image_management",
        DomainEvent::Iam(_) => "iam",
        DomainEvent::Secrets(_) => "secrets",
        DomainEvent::ContainerRun(_) => "container_run",
    }
}

/// Unified domain event type for the event bus
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    AgentLifecycle(AgentLifecycleEvent),
    Execution(ExecutionEvent),
    Workflow(WorkflowEvent),
    Learning(LearningEvent),
    Policy(crate::domain::events::PolicyEvent),
    Volume(VolumeEvent),
    Storage(StorageEvent),
    MCP(MCPToolEvent),
    Smcp(SmcpEvent),
    /// BC-8 Stimulus-Response events (ADR-021)
    Stimulus(StimulusEvent),
    /// BC-2 container image pull lifecycle events (ADR-045)
    ImageManagement(ImageManagementEvent),
    /// BC-13 IAM & Identity Federation events (ADR-041)
    Iam(IamEvent),
    /// BC-11 Secrets & Identity Management events (ADR-034)
    Secrets(SecretEvent),
    /// BC-3 CI/CD container step lifecycle events (ADR-050)
    ContainerRun(ContainerRunEvent),
}

impl DomainEvent {
    pub fn execution_id(&self) -> Option<ExecutionId> {
        match self {
            DomainEvent::AgentLifecycle(_) | DomainEvent::Stimulus(_) | DomainEvent::Iam(_) => None,
            DomainEvent::Execution(event) => Some(match event {
                ExecutionEvent::ExecutionStarted { execution_id, .. }
                | ExecutionEvent::IterationStarted { execution_id, .. }
                | ExecutionEvent::IterationCompleted { execution_id, .. }
                | ExecutionEvent::IterationFailed { execution_id, .. }
                | ExecutionEvent::RefinementApplied { execution_id, .. }
                | ExecutionEvent::ExecutionCompleted { execution_id, .. }
                | ExecutionEvent::ExecutionFailed { execution_id, .. }
                | ExecutionEvent::ExecutionCancelled { execution_id, .. }
                | ExecutionEvent::ExecutionTimedOut { execution_id, .. }
                | ExecutionEvent::ConsoleOutput { execution_id, .. }
                | ExecutionEvent::LlmInteraction { execution_id, .. }
                | ExecutionEvent::InstanceSpawned { execution_id, .. }
                | ExecutionEvent::InstanceTerminated { execution_id, .. } => *execution_id,
                ExecutionEvent::Validation(validation) => match validation {
                    ValidationEvent::GradientValidationPerformed { execution_id, .. }
                    | ValidationEvent::MultiJudgeConsensus { execution_id, .. } => *execution_id,
                },
            }),
            DomainEvent::Workflow(event) => Some(match event {
                WorkflowEvent::WorkflowExecutionStarted { execution_id, .. }
                | WorkflowEvent::WorkflowStateEntered { execution_id, .. }
                | WorkflowEvent::WorkflowStateExited { execution_id, .. }
                | WorkflowEvent::WorkflowIterationStarted { execution_id, .. }
                | WorkflowEvent::WorkflowIterationCompleted { execution_id, .. }
                | WorkflowEvent::WorkflowIterationFailed { execution_id, .. }
                | WorkflowEvent::WorkflowExecutionCompleted { execution_id, .. }
                | WorkflowEvent::WorkflowExecutionFailed { execution_id, .. }
                | WorkflowEvent::WorkflowExecutionCancelled { execution_id, .. } => *execution_id,
                WorkflowEvent::WorkflowRegistered { .. } => return None,
            }),
            DomainEvent::Learning(event) => Some(match event {
                LearningEvent::PatternDiscovered { execution_id, .. }
                | LearningEvent::PatternReinforced { execution_id, .. }
                | LearningEvent::PatternDecayed { execution_id, .. } => *execution_id,
            }),
            DomainEvent::Policy(_) => None,
            DomainEvent::Volume(event) => match event {
                VolumeEvent::VolumeCreated { execution_id, .. } => *execution_id,
                VolumeEvent::VolumeAttached { .. }
                | VolumeEvent::VolumeDetached { .. }
                | VolumeEvent::VolumeDeleted { .. }
                | VolumeEvent::VolumeExpired { .. }
                | VolumeEvent::VolumeMountFailed { .. }
                | VolumeEvent::VolumeQuotaExceeded { .. } => None,
            },
            DomainEvent::Storage(event) => Some(match event {
                StorageEvent::FileOpened { execution_id, .. }
                | StorageEvent::FileRead { execution_id, .. }
                | StorageEvent::FileWritten { execution_id, .. }
                | StorageEvent::FileClosed { execution_id, .. }
                | StorageEvent::DirectoryListed { execution_id, .. }
                | StorageEvent::FileCreated { execution_id, .. }
                | StorageEvent::FileDeleted { execution_id, .. }
                | StorageEvent::PathTraversalBlocked { execution_id, .. }
                | StorageEvent::FilesystemPolicyViolation { execution_id, .. }
                | StorageEvent::QuotaExceeded { execution_id, .. }
                | StorageEvent::UnauthorizedVolumeAccess { execution_id, .. } => *execution_id,
            }),
            DomainEvent::MCP(event) => match event {
                MCPToolEvent::InvocationRequested { execution_id, .. }
                | MCPToolEvent::InvocationCompleted { execution_id, .. }
                | MCPToolEvent::InvocationFailed { execution_id, .. }
                | MCPToolEvent::PolicyViolation { execution_id, .. } => Some(*execution_id),
                MCPToolEvent::ServerRegistered { .. }
                | MCPToolEvent::ServerStarted { .. }
                | MCPToolEvent::ServerStopped { .. }
                | MCPToolEvent::ServerFailed { .. }
                | MCPToolEvent::ServerUnhealthy { .. }
                | MCPToolEvent::InvocationStarted { .. } => None,
            },
            DomainEvent::Smcp(event) => match event {
                SmcpEvent::AttestationCompleted { execution_id, .. }
                | SmcpEvent::SessionCreated { execution_id, .. }
                | SmcpEvent::PolicyViolationBlocked { execution_id, .. } => Some(*execution_id),
                SmcpEvent::SessionRevoked { .. } => None,
            },
            DomainEvent::ImageManagement(event) => Some(match event {
                ImageManagementEvent::ImagePullStarted { execution_id, .. }
                | ImageManagementEvent::ImagePullCompleted { execution_id, .. }
                | ImageManagementEvent::ImagePullFailed { execution_id, .. } => *execution_id,
            }),
            DomainEvent::Secrets(event) => match event {
                SecretEvent::SecretRetrieved { access_context, .. }
                | SecretEvent::SecretWritten { access_context, .. }
                | SecretEvent::DynamicSecretGenerated { access_context, .. }
                | SecretEvent::LeaseRenewed { access_context, .. }
                | SecretEvent::LeaseRevoked { access_context, .. }
                | SecretEvent::SecretAccessDenied { access_context, .. } => {
                    access_context.execution_id
                }
            },
            DomainEvent::ContainerRun(event) => Some(match event {
                ContainerRunEvent::ContainerRunStarted { execution_id, .. }
                | ContainerRunEvent::ContainerRunCompleted { execution_id, .. }
                | ContainerRunEvent::ContainerRunFailed { execution_id, .. }
                | ContainerRunEvent::ParallelContainerRunAggregated { execution_id, .. } => {
                    *execution_id
                }
            }),
        }
    }

    pub fn agent_id(&self) -> Option<AgentId> {
        match self {
            DomainEvent::AgentLifecycle(event) => Some(match event {
                AgentLifecycleEvent::AgentDeployed { agent_id, .. }
                | AgentLifecycleEvent::AgentPaused { agent_id, .. }
                | AgentLifecycleEvent::AgentResumed { agent_id, .. }
                | AgentLifecycleEvent::AgentUpdated { agent_id, .. }
                | AgentLifecycleEvent::AgentRemoved { agent_id, .. }
                | AgentLifecycleEvent::AgentFailed { agent_id, .. } => *agent_id,
            }),
            DomainEvent::Execution(event) => Some(match event {
                ExecutionEvent::ExecutionStarted { agent_id, .. }
                | ExecutionEvent::IterationStarted { agent_id, .. }
                | ExecutionEvent::IterationCompleted { agent_id, .. }
                | ExecutionEvent::IterationFailed { agent_id, .. }
                | ExecutionEvent::RefinementApplied { agent_id, .. }
                | ExecutionEvent::ExecutionCompleted { agent_id, .. }
                | ExecutionEvent::ExecutionFailed { agent_id, .. }
                | ExecutionEvent::ExecutionCancelled { agent_id, .. }
                | ExecutionEvent::ExecutionTimedOut { agent_id, .. }
                | ExecutionEvent::ConsoleOutput { agent_id, .. }
                | ExecutionEvent::LlmInteraction { agent_id, .. }
                | ExecutionEvent::InstanceSpawned { agent_id, .. }
                | ExecutionEvent::InstanceTerminated { agent_id, .. } => *agent_id,
                ExecutionEvent::Validation(validation) => match validation {
                    ValidationEvent::GradientValidationPerformed { agent_id, .. }
                    | ValidationEvent::MultiJudgeConsensus { agent_id, .. } => *agent_id,
                },
            }),
            DomainEvent::Workflow(_) | DomainEvent::Volume(_) | DomainEvent::Storage(_) => None,
            DomainEvent::Learning(event) => Some(match event {
                LearningEvent::PatternDiscovered { agent_id, .. }
                | LearningEvent::PatternReinforced { agent_id, .. }
                | LearningEvent::PatternDecayed { agent_id, .. } => *agent_id,
            }),
            DomainEvent::Policy(event) => Some(match event {
                PolicyEvent::PolicyViolationAttempted { agent_id, .. }
                | PolicyEvent::PolicyViolationBlocked { agent_id, .. } => *agent_id,
            }),
            DomainEvent::MCP(event) => match event {
                MCPToolEvent::InvocationRequested { agent_id, .. }
                | MCPToolEvent::InvocationCompleted { agent_id, .. }
                | MCPToolEvent::InvocationFailed { agent_id, .. }
                | MCPToolEvent::PolicyViolation { agent_id, .. } => Some(*agent_id),
                MCPToolEvent::ServerRegistered { .. }
                | MCPToolEvent::ServerStarted { .. }
                | MCPToolEvent::ServerStopped { .. }
                | MCPToolEvent::ServerFailed { .. }
                | MCPToolEvent::ServerUnhealthy { .. }
                | MCPToolEvent::InvocationStarted { .. } => None,
            },
            DomainEvent::Smcp(event) => Some(match event {
                SmcpEvent::AttestationCompleted { agent_id, .. }
                | SmcpEvent::SessionCreated { agent_id, .. }
                | SmcpEvent::SessionRevoked { agent_id, .. }
                | SmcpEvent::PolicyViolationBlocked { agent_id, .. } => *agent_id,
            }),
            DomainEvent::Stimulus(_) | DomainEvent::ImageManagement(_) | DomainEvent::Iam(_) => {
                None
            }
            DomainEvent::Secrets(event) => match event {
                SecretEvent::SecretRetrieved { access_context, .. }
                | SecretEvent::SecretWritten { access_context, .. }
                | SecretEvent::DynamicSecretGenerated { access_context, .. }
                | SecretEvent::LeaseRenewed { access_context, .. }
                | SecretEvent::LeaseRevoked { access_context, .. }
                | SecretEvent::SecretAccessDenied { access_context, .. } => access_context.agent_id,
            },
            DomainEvent::ContainerRun(_) => None,
        }
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            DomainEvent::AgentLifecycle(event) => match event {
                AgentLifecycleEvent::AgentDeployed { deployed_at, .. } => *deployed_at,
                AgentLifecycleEvent::AgentPaused { paused_at, .. } => *paused_at,
                AgentLifecycleEvent::AgentResumed { resumed_at, .. } => *resumed_at,
                AgentLifecycleEvent::AgentUpdated { updated_at, .. } => *updated_at,
                AgentLifecycleEvent::AgentRemoved { removed_at, .. } => *removed_at,
                AgentLifecycleEvent::AgentFailed { failed_at, .. } => *failed_at,
            },
            DomainEvent::Execution(event) => match event {
                ExecutionEvent::ExecutionStarted { started_at, .. } => *started_at,
                ExecutionEvent::IterationStarted { started_at, .. } => *started_at,
                ExecutionEvent::IterationCompleted { completed_at, .. } => *completed_at,
                ExecutionEvent::IterationFailed { failed_at, .. } => *failed_at,
                ExecutionEvent::RefinementApplied { applied_at, .. } => *applied_at,
                ExecutionEvent::ExecutionCompleted { completed_at, .. } => *completed_at,
                ExecutionEvent::ExecutionFailed { failed_at, .. } => *failed_at,
                ExecutionEvent::ExecutionCancelled { cancelled_at, .. } => *cancelled_at,
                ExecutionEvent::ExecutionTimedOut { timed_out_at, .. } => *timed_out_at,
                ExecutionEvent::ConsoleOutput { timestamp, .. } => *timestamp,
                ExecutionEvent::LlmInteraction { timestamp, .. } => *timestamp,
                ExecutionEvent::InstanceSpawned { spawned_at, .. } => *spawned_at,
                ExecutionEvent::InstanceTerminated { terminated_at, .. } => *terminated_at,
                ExecutionEvent::Validation(validation) => match validation {
                    ValidationEvent::GradientValidationPerformed { validated_at, .. } => {
                        *validated_at
                    }
                    ValidationEvent::MultiJudgeConsensus { reached_at, .. } => *reached_at,
                },
            },
            DomainEvent::Workflow(event) => match event {
                WorkflowEvent::WorkflowRegistered { registered_at, .. } => *registered_at,
                WorkflowEvent::WorkflowExecutionStarted { started_at, .. } => *started_at,
                WorkflowEvent::WorkflowStateEntered { entered_at, .. } => *entered_at,
                WorkflowEvent::WorkflowStateExited { exited_at, .. } => *exited_at,
                WorkflowEvent::WorkflowIterationStarted { started_at, .. } => *started_at,
                WorkflowEvent::WorkflowIterationCompleted { completed_at, .. } => *completed_at,
                WorkflowEvent::WorkflowIterationFailed { failed_at, .. } => *failed_at,
                WorkflowEvent::WorkflowExecutionCompleted { completed_at, .. } => *completed_at,
                WorkflowEvent::WorkflowExecutionFailed { failed_at, .. } => *failed_at,
                WorkflowEvent::WorkflowExecutionCancelled { cancelled_at, .. } => *cancelled_at,
            },
            DomainEvent::Learning(event) => match event {
                LearningEvent::PatternDiscovered { discovered_at, .. } => *discovered_at,
                LearningEvent::PatternReinforced { reinforced_at, .. } => *reinforced_at,
                LearningEvent::PatternDecayed { decayed_at, .. } => *decayed_at,
            },
            DomainEvent::Policy(event) => match event {
                PolicyEvent::PolicyViolationAttempted { attempted_at, .. } => *attempted_at,
                PolicyEvent::PolicyViolationBlocked { blocked_at, .. } => *blocked_at,
            },
            DomainEvent::Volume(event) => match event {
                VolumeEvent::VolumeCreated { created_at, .. } => *created_at,
                VolumeEvent::VolumeAttached { attached_at, .. } => *attached_at,
                VolumeEvent::VolumeDetached { detached_at, .. } => *detached_at,
                VolumeEvent::VolumeDeleted { deleted_at, .. } => *deleted_at,
                VolumeEvent::VolumeExpired { expired_at, .. } => *expired_at,
                VolumeEvent::VolumeMountFailed { failed_at, .. } => *failed_at,
                VolumeEvent::VolumeQuotaExceeded { exceeded_at, .. } => *exceeded_at,
            },
            DomainEvent::Storage(event) => match event {
                StorageEvent::FileOpened { opened_at, .. } => *opened_at,
                StorageEvent::FileRead { read_at, .. } => *read_at,
                StorageEvent::FileWritten { written_at, .. } => *written_at,
                StorageEvent::FileClosed { closed_at, .. } => *closed_at,
                StorageEvent::DirectoryListed { listed_at, .. } => *listed_at,
                StorageEvent::FileCreated { created_at, .. } => *created_at,
                StorageEvent::FileDeleted { deleted_at, .. } => *deleted_at,
                StorageEvent::PathTraversalBlocked { blocked_at, .. } => *blocked_at,
                StorageEvent::FilesystemPolicyViolation { violated_at, .. } => *violated_at,
                StorageEvent::QuotaExceeded { exceeded_at, .. } => *exceeded_at,
                StorageEvent::UnauthorizedVolumeAccess { attempted_at, .. } => *attempted_at,
            },
            DomainEvent::MCP(event) => match event {
                MCPToolEvent::ServerRegistered { registered_at, .. } => *registered_at,
                MCPToolEvent::ServerStarted { started_at, .. } => *started_at,
                MCPToolEvent::ServerStopped { stopped_at, .. } => *stopped_at,
                MCPToolEvent::ServerFailed { failed_at, .. } => *failed_at,
                MCPToolEvent::ServerUnhealthy { last_healthy, .. } => {
                    last_healthy.unwrap_or_else(Utc::now)
                }
                MCPToolEvent::InvocationRequested { requested_at, .. } => *requested_at,
                MCPToolEvent::InvocationStarted { started_at, .. } => *started_at,
                MCPToolEvent::InvocationCompleted { completed_at, .. } => *completed_at,
                MCPToolEvent::InvocationFailed { failed_at, .. } => *failed_at,
                MCPToolEvent::PolicyViolation { blocked_at, .. } => *blocked_at,
            },
            DomainEvent::Smcp(event) => match event {
                SmcpEvent::AttestationCompleted { attested_at, .. } => *attested_at,
                SmcpEvent::SessionCreated { created_at, .. } => *created_at,
                SmcpEvent::SessionRevoked { revoked_at, .. } => *revoked_at,
                SmcpEvent::PolicyViolationBlocked { blocked_at, .. } => *blocked_at,
            },
            DomainEvent::Stimulus(event) => match event {
                StimulusEvent::StimulusReceived { received_at, .. } => *received_at,
                StimulusEvent::StimulusClassified { classified_at, .. } => *classified_at,
                StimulusEvent::StimulusRejected { rejected_at, .. } => *rejected_at,
                StimulusEvent::ClassificationFailed { failed_at, .. } => *failed_at,
            },
            DomainEvent::ImageManagement(event) => match event {
                ImageManagementEvent::ImagePullStarted { started_at, .. } => *started_at,
                ImageManagementEvent::ImagePullCompleted { completed_at, .. } => *completed_at,
                ImageManagementEvent::ImagePullFailed { failed_at, .. } => *failed_at,
            },
            DomainEvent::Iam(event) => match event {
                IamEvent::UserAuthenticated {
                    authenticated_at, ..
                } => *authenticated_at,
                IamEvent::TokenValidationFailed { attempted_at, .. } => *attempted_at,
                IamEvent::RealmRegistered { registered_at, .. } => *registered_at,
                IamEvent::TenantRealmProvisioned { provisioned_at, .. } => *provisioned_at,
                IamEvent::ServiceAccountProvisioned { provisioned_at, .. } => *provisioned_at,
                IamEvent::ServiceAccountRevoked { revoked_at, .. } => *revoked_at,
                IamEvent::JwksCacheRefreshed { refreshed_at, .. } => *refreshed_at,
                IamEvent::JwksCacheRefreshFailed { failed_at, .. } => *failed_at,
            },
            DomainEvent::Secrets(event) => match event {
                SecretEvent::SecretRetrieved { retrieved_at, .. } => *retrieved_at,
                SecretEvent::SecretWritten { written_at, .. } => *written_at,
                SecretEvent::DynamicSecretGenerated { generated_at, .. } => *generated_at,
                SecretEvent::LeaseRenewed { renewed_at, .. } => *renewed_at,
                SecretEvent::LeaseRevoked { revoked_at, .. } => *revoked_at,
                SecretEvent::SecretAccessDenied { denied_at, .. } => *denied_at,
            },
            DomainEvent::ContainerRun(event) => match event {
                ContainerRunEvent::ContainerRunStarted { started_at, .. } => *started_at,
                ContainerRunEvent::ContainerRunCompleted { completed_at, .. } => *completed_at,
                ContainerRunEvent::ContainerRunFailed { failed_at, .. } => *failed_at,
                ContainerRunEvent::ParallelContainerRunAggregated { aggregated_at, .. } => {
                    *aggregated_at
                }
            },
        }
    }

    pub fn event_type_name(&self) -> &'static str {
        match self {
            DomainEvent::AgentLifecycle(event) => match event {
                AgentLifecycleEvent::AgentDeployed { .. } => "agent_deployed",
                AgentLifecycleEvent::AgentPaused { .. } => "agent_paused",
                AgentLifecycleEvent::AgentResumed { .. } => "agent_resumed",
                AgentLifecycleEvent::AgentUpdated { .. } => "agent_updated",
                AgentLifecycleEvent::AgentRemoved { .. } => "agent_removed",
                AgentLifecycleEvent::AgentFailed { .. } => "agent_failed",
            },
            DomainEvent::Execution(event) => match event {
                ExecutionEvent::ExecutionStarted { .. } => "execution_started",
                ExecutionEvent::IterationStarted { .. } => "iteration_started",
                ExecutionEvent::IterationCompleted { .. } => "iteration_completed",
                ExecutionEvent::IterationFailed { .. } => "iteration_failed",
                ExecutionEvent::RefinementApplied { .. } => "refinement_applied",
                ExecutionEvent::ExecutionCompleted { .. } => "execution_completed",
                ExecutionEvent::ExecutionFailed { .. } => "execution_failed",
                ExecutionEvent::ExecutionCancelled { .. } => "execution_cancelled",
                ExecutionEvent::ExecutionTimedOut { .. } => "execution_timed_out",
                ExecutionEvent::ConsoleOutput { .. } => "console_output",
                ExecutionEvent::LlmInteraction { .. } => "llm_interaction",
                ExecutionEvent::InstanceSpawned { .. } => "instance_spawned",
                ExecutionEvent::InstanceTerminated { .. } => "instance_terminated",
                ExecutionEvent::Validation(validation) => match validation {
                    ValidationEvent::GradientValidationPerformed { .. } => {
                        "gradient_validation_performed"
                    }
                    ValidationEvent::MultiJudgeConsensus { .. } => "multi_judge_consensus",
                },
            },
            DomainEvent::Workflow(event) => match event {
                WorkflowEvent::WorkflowRegistered { .. } => "workflow_registered",
                WorkflowEvent::WorkflowExecutionStarted { .. } => "workflow_execution_started",
                WorkflowEvent::WorkflowStateEntered { .. } => "workflow_state_entered",
                WorkflowEvent::WorkflowStateExited { .. } => "workflow_state_exited",
                WorkflowEvent::WorkflowIterationStarted { .. } => "workflow_iteration_started",
                WorkflowEvent::WorkflowIterationCompleted { .. } => "workflow_iteration_completed",
                WorkflowEvent::WorkflowIterationFailed { .. } => "workflow_iteration_failed",
                WorkflowEvent::WorkflowExecutionCompleted { .. } => "workflow_execution_completed",
                WorkflowEvent::WorkflowExecutionFailed { .. } => "workflow_execution_failed",
                WorkflowEvent::WorkflowExecutionCancelled { .. } => "workflow_execution_cancelled",
            },
            DomainEvent::Learning(event) => match event {
                LearningEvent::PatternDiscovered { .. } => "pattern_discovered",
                LearningEvent::PatternReinforced { .. } => "pattern_reinforced",
                LearningEvent::PatternDecayed { .. } => "pattern_decayed",
            },
            DomainEvent::Policy(event) => match event {
                PolicyEvent::PolicyViolationAttempted { .. } => "policy_violation_attempted",
                PolicyEvent::PolicyViolationBlocked { .. } => "policy_violation_blocked",
            },
            DomainEvent::Volume(event) => match event {
                VolumeEvent::VolumeCreated { .. } => "volume_created",
                VolumeEvent::VolumeAttached { .. } => "volume_attached",
                VolumeEvent::VolumeDetached { .. } => "volume_detached",
                VolumeEvent::VolumeDeleted { .. } => "volume_deleted",
                VolumeEvent::VolumeExpired { .. } => "volume_expired",
                VolumeEvent::VolumeMountFailed { .. } => "volume_mount_failed",
                VolumeEvent::VolumeQuotaExceeded { .. } => "volume_quota_exceeded",
            },
            DomainEvent::Storage(event) => match event {
                StorageEvent::FileOpened { .. } => "file_opened",
                StorageEvent::FileRead { .. } => "file_read",
                StorageEvent::FileWritten { .. } => "file_written",
                StorageEvent::FileClosed { .. } => "file_closed",
                StorageEvent::DirectoryListed { .. } => "directory_listed",
                StorageEvent::FileCreated { .. } => "file_created",
                StorageEvent::FileDeleted { .. } => "file_deleted",
                StorageEvent::PathTraversalBlocked { .. } => "path_traversal_blocked",
                StorageEvent::FilesystemPolicyViolation { .. } => "filesystem_policy_violation",
                StorageEvent::QuotaExceeded { .. } => "quota_exceeded",
                StorageEvent::UnauthorizedVolumeAccess { .. } => "unauthorized_volume_access",
            },
            DomainEvent::MCP(event) => match event {
                MCPToolEvent::ServerRegistered { .. } => "tool_server_registered",
                MCPToolEvent::ServerStarted { .. } => "tool_server_started",
                MCPToolEvent::ServerStopped { .. } => "tool_server_stopped",
                MCPToolEvent::ServerFailed { .. } => "tool_server_failed",
                MCPToolEvent::ServerUnhealthy { .. } => "tool_server_unhealthy",
                MCPToolEvent::InvocationRequested { .. } => "tool_invocation_requested",
                MCPToolEvent::InvocationStarted { .. } => "tool_invocation_started",
                MCPToolEvent::InvocationCompleted { .. } => "tool_invocation_completed",
                MCPToolEvent::InvocationFailed { .. } => "tool_invocation_failed",
                MCPToolEvent::PolicyViolation { .. } => "tool_policy_violation",
            },
            DomainEvent::Smcp(event) => match event {
                SmcpEvent::AttestationCompleted { .. } => "smcp_attestation_completed",
                SmcpEvent::SessionCreated { .. } => "smcp_session_created",
                SmcpEvent::SessionRevoked { .. } => "smcp_session_revoked",
                SmcpEvent::PolicyViolationBlocked { .. } => "smcp_policy_violation_blocked",
            },
            DomainEvent::Stimulus(event) => match event {
                StimulusEvent::StimulusReceived { .. } => "stimulus_received",
                StimulusEvent::StimulusClassified { .. } => "stimulus_classified",
                StimulusEvent::StimulusRejected { .. } => "stimulus_rejected",
                StimulusEvent::ClassificationFailed { .. } => "stimulus_classification_failed",
            },
            DomainEvent::ImageManagement(event) => match event {
                ImageManagementEvent::ImagePullStarted { .. } => "image_pull_started",
                ImageManagementEvent::ImagePullCompleted { .. } => "image_pull_completed",
                ImageManagementEvent::ImagePullFailed { .. } => "image_pull_failed",
            },
            DomainEvent::Iam(event) => match event {
                IamEvent::UserAuthenticated { .. } => "user_authenticated",
                IamEvent::TokenValidationFailed { .. } => "token_validation_failed",
                IamEvent::RealmRegistered { .. } => "realm_registered",
                IamEvent::TenantRealmProvisioned { .. } => "tenant_realm_provisioned",
                IamEvent::ServiceAccountProvisioned { .. } => "service_account_provisioned",
                IamEvent::ServiceAccountRevoked { .. } => "service_account_revoked",
                IamEvent::JwksCacheRefreshed { .. } => "jwks_cache_refreshed",
                IamEvent::JwksCacheRefreshFailed { .. } => "jwks_cache_refresh_failed",
            },
            DomainEvent::Secrets(event) => match event {
                SecretEvent::SecretRetrieved { .. } => "secret_retrieved",
                SecretEvent::SecretWritten { .. } => "secret_written",
                SecretEvent::DynamicSecretGenerated { .. } => "dynamic_secret_generated",
                SecretEvent::LeaseRenewed { .. } => "lease_renewed",
                SecretEvent::LeaseRevoked { .. } => "lease_revoked",
                SecretEvent::SecretAccessDenied { .. } => "secret_access_denied",
            },
            DomainEvent::ContainerRun(event) => match event {
                ContainerRunEvent::ContainerRunStarted { .. } => "container_run_started",
                ContainerRunEvent::ContainerRunCompleted { .. } => "container_run_completed",
                ContainerRunEvent::ContainerRunFailed { .. } => "container_run_failed",
                ContainerRunEvent::ParallelContainerRunAggregated { .. } => {
                    "parallel_container_run_aggregated"
                }
            },
        }
    }

    pub fn category(&self) -> &'static str {
        match self {
            DomainEvent::AgentLifecycle(_) => "agent_lifecycle",
            DomainEvent::Execution(_) => "execution",
            DomainEvent::Workflow(_) => "workflow",
            DomainEvent::Learning(_) => "learning",
            DomainEvent::Policy(_) => "policy",
            DomainEvent::Volume(_) => "volume",
            DomainEvent::Storage(_) => "storage",
            DomainEvent::MCP(_) => "mcp",
            DomainEvent::Smcp(_) => "smcp",
            DomainEvent::Stimulus(_) => "stimulus",
            DomainEvent::ImageManagement(_) => "image_management",
            DomainEvent::Iam(_) => "iam",
            DomainEvent::Secrets(_) => "secrets",
            DomainEvent::ContainerRun(_) => "container_run",
        }
    }

    pub fn iteration_number(&self) -> Option<u8> {
        match self {
            DomainEvent::Execution(event) => match event {
                ExecutionEvent::IterationStarted {
                    iteration_number, ..
                }
                | ExecutionEvent::IterationCompleted {
                    iteration_number, ..
                }
                | ExecutionEvent::IterationFailed {
                    iteration_number, ..
                }
                | ExecutionEvent::RefinementApplied {
                    iteration_number, ..
                }
                | ExecutionEvent::ConsoleOutput {
                    iteration_number, ..
                }
                | ExecutionEvent::LlmInteraction {
                    iteration_number, ..
                }
                | ExecutionEvent::InstanceSpawned {
                    iteration_number, ..
                }
                | ExecutionEvent::InstanceTerminated {
                    iteration_number, ..
                } => Some(*iteration_number),
                ExecutionEvent::Validation(validation) => match validation {
                    ValidationEvent::GradientValidationPerformed {
                        iteration_number, ..
                    } => Some(*iteration_number),
                    ValidationEvent::MultiJudgeConsensus { .. } => None,
                },
                ExecutionEvent::ExecutionStarted { .. }
                | ExecutionEvent::ExecutionCompleted { .. }
                | ExecutionEvent::ExecutionFailed { .. }
                | ExecutionEvent::ExecutionCancelled { .. }
                | ExecutionEvent::ExecutionTimedOut { .. } => None,
            },
            DomainEvent::Workflow(WorkflowEvent::WorkflowIterationStarted {
                iteration_number,
                ..
            })
            | DomainEvent::Workflow(WorkflowEvent::WorkflowIterationCompleted {
                iteration_number,
                ..
            })
            | DomainEvent::Workflow(WorkflowEvent::WorkflowIterationFailed {
                iteration_number,
                ..
            }) => Some(*iteration_number),
            DomainEvent::Workflow(_) => None,
            _ => None,
        }
    }

    pub fn stage(&self) -> Option<&'static str> {
        match self {
            DomainEvent::AgentLifecycle(_) => Some("agent"),
            DomainEvent::Execution(event) => Some(match event {
                ExecutionEvent::ExecutionStarted { .. }
                | ExecutionEvent::ExecutionCompleted { .. }
                | ExecutionEvent::ExecutionFailed { .. }
                | ExecutionEvent::ExecutionCancelled { .. }
                | ExecutionEvent::ExecutionTimedOut { .. } => "execution",
                ExecutionEvent::IterationStarted { .. }
                | ExecutionEvent::IterationCompleted { .. }
                | ExecutionEvent::IterationFailed { .. }
                | ExecutionEvent::RefinementApplied { .. }
                | ExecutionEvent::Validation(_) => "iteration",
                ExecutionEvent::ConsoleOutput { .. } => "console",
                ExecutionEvent::LlmInteraction { .. } => "llm",
                ExecutionEvent::InstanceSpawned { .. }
                | ExecutionEvent::InstanceTerminated { .. } => "runtime",
            }),
            DomainEvent::Workflow(_) => Some("workflow"),
            DomainEvent::Learning(_) => Some("learning"),
            DomainEvent::Policy(_) | DomainEvent::Smcp(_) => Some("security"),
            DomainEvent::Volume(_) | DomainEvent::Storage(_) => Some("storage"),
            DomainEvent::MCP(_) => Some("tooling"),
            DomainEvent::Stimulus(_) => Some("stimulus"),
            DomainEvent::ImageManagement(_) => Some("runtime"),
            DomainEvent::Iam(_) => Some("identity"),
            DomainEvent::Secrets(_) => Some("secrets"),
            DomainEvent::ContainerRun(_) => Some("container"),
        }
    }
}

/// Event bus for publishing and subscribing to domain events
#[derive(Clone)]
pub struct EventBus {
    sender: Arc<broadcast::Sender<DomainEvent>>,
}

impl EventBus {
    /// Create a new event bus with specified channel capacity
    /// Capacity determines how many events can be buffered before dropping old ones
    /// Default: 1000 events
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender: Arc::new(sender),
        }
    }

    /// Create event bus with default capacity (1000)
    pub fn with_default_capacity() -> Self {
        Self::new(1000)
    }

    /// Publish an agent lifecycle event
    pub fn publish_agent_event(&self, event: AgentLifecycleEvent) {
        self.publish(DomainEvent::AgentLifecycle(event));
    }

    /// Publish an execution event
    pub fn publish_execution_event(&self, event: ExecutionEvent) {
        self.publish(DomainEvent::Execution(event));
    }

    /// Publish a workflow event
    pub fn publish_workflow_event(&self, event: WorkflowEvent) {
        self.publish(DomainEvent::Workflow(event));
    }

    /// Publish a learning event
    pub fn publish_learning_event(&self, event: LearningEvent) {
        self.publish(DomainEvent::Learning(event));
    }

    /// Publish a volume event
    pub fn publish_volume_event(&self, event: VolumeEvent) {
        self.publish(DomainEvent::Volume(event));
    }

    /// Publish a storage event (ADR-036)
    pub fn publish_storage_event(&self, event: StorageEvent) {
        self.publish(DomainEvent::Storage(event));
    }

    /// Publish an MCP event (ADR-033/035)
    pub fn publish_mcp_event(&self, event: MCPToolEvent) {
        self.publish(DomainEvent::MCP(event));
    }

    /// Publish an SMCP session/security event (ADR-035)
    pub fn publish_smcp_event(&self, event: SmcpEvent) {
        self.publish(DomainEvent::Smcp(event));
    }

    /// Publish a stimulus ingestion/routing event (BC-8 ADR-021)
    pub fn publish_stimulus_event(&self, event: StimulusEvent) {
        self.publish(DomainEvent::Stimulus(event));
    }

    /// Publish a CI/CD container step event (ADR-050)
    pub fn publish_container_run_event(&self, event: ContainerRunEvent) {
        self.publish(DomainEvent::ContainerRun(event));
    }

    /// Publish a container image management event (BC-2 ADR-045)
    pub fn publish_image_event(&self, event: ImageManagementEvent) {
        self.publish(DomainEvent::ImageManagement(event));
    }

    /// Publish an IAM event (BC-13 ADR-041)
    pub fn publish_iam_event(&self, event: IamEvent) {
        self.publish(DomainEvent::Iam(event));
    }

    /// Publish a secrets management event (BC-11 ADR-034)
    pub fn publish_secret_event(&self, event: SecretEvent) {
        self.publish(DomainEvent::Secrets(event));
    }

    /// Publish a domain event to all subscribers
    fn publish(&self, event: DomainEvent) {
        let event_type = domain_event_type(&event);
        metrics::counter!("aegis_event_bus_events_total", "event_type" => event_type).increment(1);
        if self.sender.send(event).is_err() {
            metrics::counter!(
                "aegis_event_bus_delivery_failures_total",
                "reason" => "no_receivers"
            )
            .increment(1);
        }
    }

    /// Subscribe to all domain events
    /// Returns a receiver that can be used to listen for events
    pub fn subscribe(&self) -> EventReceiver {
        let receiver = self.sender.subscribe();
        EventReceiver { receiver }
    }

    /// Subscribe and filter for specific execution ID
    /// Useful for streaming logs for a single execution
    pub fn subscribe_execution(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ExecutionEventReceiver {
        let receiver = self.sender.subscribe();
        ExecutionEventReceiver {
            receiver,
            execution_id,
        }
    }

    /// Subscribe and filter for any domain event correlated to a specific execution ID.
    pub fn subscribe_execution_domain(
        &self,
        execution_id: crate::domain::execution::ExecutionId,
    ) -> ExecutionDomainEventReceiver {
        ExecutionDomainEventReceiver {
            receiver: self.sender.subscribe(),
            execution_id,
        }
    }

    /// Subscribe to `WorkflowEvent`s scoped to a specific workflow execution.
    ///
    /// The receiver will only yield events whose `execution_id` matches `id`.
    pub fn subscribe_workflow_execution(
        &self,
        id: crate::domain::execution::ExecutionId,
    ) -> WorkflowEventReceiver {
        WorkflowEventReceiver {
            receiver: self.sender.subscribe(),
            execution_id: id,
        }
    }

    /// Subscribe and filter for specific agent ID
    pub fn subscribe_agent(&self, agent_id: crate::domain::agent::AgentId) -> AgentEventReceiver {
        let receiver = self.sender.subscribe();
        AgentEventReceiver { receiver, agent_id }
    }

    /// Get the number of active subscribers
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

/// Receiver for all domain events
pub struct EventReceiver {
    receiver: broadcast::Receiver<DomainEvent>,
}

impl EventReceiver {
    /// Receive the next event (blocks until event is available)
    pub async fn recv(&mut self) -> Result<DomainEvent, EventBusError> {
        self.receiver.recv().await.map_err(|e| match e {
            broadcast::error::RecvError::Closed => EventBusError::Closed,
            broadcast::error::RecvError::Lagged(n) => {
                warn!("Event receiver lagged by {} events", n);
                metrics::counter!("aegis_event_bus_receiver_lag_total").increment(n);
                EventBusError::Lagged(n)
            }
        })
    }

    /// Try to receive an event without blocking
    pub fn try_recv(&mut self) -> Result<DomainEvent, EventBusError> {
        self.receiver.try_recv().map_err(|e| match e {
            broadcast::error::TryRecvError::Empty => EventBusError::Empty,
            broadcast::error::TryRecvError::Closed => EventBusError::Closed,
            broadcast::error::TryRecvError::Lagged(n) => {
                warn!("Event receiver lagged by {} events", n);
                metrics::counter!("aegis_event_bus_receiver_lag_total").increment(n);
                EventBusError::Lagged(n)
            }
        })
    }
}

/// Receiver for execution-specific events (filtered)
pub struct ExecutionEventReceiver {
    receiver: broadcast::Receiver<DomainEvent>,
    execution_id: crate::domain::execution::ExecutionId,
}

/// Receiver for all domain events correlated to a specific execution.
pub struct ExecutionDomainEventReceiver {
    receiver: broadcast::Receiver<DomainEvent>,
    execution_id: crate::domain::execution::ExecutionId,
}

impl ExecutionDomainEventReceiver {
    pub async fn recv(&mut self) -> Result<DomainEvent, EventBusError> {
        loop {
            let event = self.receiver.recv().await.map_err(|e| match e {
                broadcast::error::RecvError::Closed => EventBusError::Closed,
                broadcast::error::RecvError::Lagged(n) => {
                    warn!("Event receiver lagged by {} events", n);
                    metrics::counter!("aegis_event_bus_receiver_lag_total").increment(n);
                    EventBusError::Lagged(n)
                }
            })?;

            if event.execution_id() == Some(self.execution_id) {
                return Ok(event);
            }
        }
    }
}

impl ExecutionEventReceiver {
    /// Receive the next execution event for the specified execution ID
    /// Filters out events from other executions
    pub async fn recv(&mut self) -> Result<ExecutionEvent, EventBusError> {
        loop {
            let event = self.receiver.recv().await.map_err(|e| match e {
                broadcast::error::RecvError::Closed => EventBusError::Closed,
                broadcast::error::RecvError::Lagged(n) => {
                    warn!("Event receiver lagged by {} events", n);
                    metrics::counter!("aegis_event_bus_receiver_lag_total").increment(n);
                    EventBusError::Lagged(n)
                }
            })?;

            // Filter for execution events matching our ID
            if let DomainEvent::Execution(exec_event) = event {
                if self.matches_execution(&exec_event) {
                    return Ok(exec_event);
                }
            }
            // Continue loop if event doesn't match
        }
    }

    fn matches_execution(&self, event: &ExecutionEvent) -> bool {
        match event {
            ExecutionEvent::ExecutionStarted { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::IterationStarted { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::IterationCompleted { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::IterationFailed { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::RefinementApplied { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::ExecutionCompleted { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::ExecutionFailed { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::ExecutionCancelled { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::ConsoleOutput { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::LlmInteraction { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::InstanceSpawned { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::InstanceTerminated { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::ExecutionTimedOut { execution_id, .. } => {
                execution_id == &self.execution_id
            }
            ExecutionEvent::Validation(e) => match e {
                ValidationEvent::GradientValidationPerformed { execution_id, .. } => {
                    execution_id == &self.execution_id
                }
                ValidationEvent::MultiJudgeConsensus { execution_id, .. } => {
                    execution_id == &self.execution_id
                }
            },
        }
    }
}

/// Filtered receiver for workflow-execution–scoped domain events.
pub struct WorkflowEventReceiver {
    receiver: broadcast::Receiver<DomainEvent>,
    execution_id: crate::domain::execution::ExecutionId,
}

impl WorkflowEventReceiver {
    /// Receive the next `WorkflowEvent` for the specified execution ID.
    /// Filters out events from other executions and non-workflow events.
    pub async fn recv(&mut self) -> Result<crate::domain::events::WorkflowEvent, EventBusError> {
        loop {
            match self.receiver.recv().await {
                Ok(DomainEvent::Workflow(ev)) => {
                    if self.matches_execution(&ev) {
                        return Ok(ev);
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("WorkflowEventReceiver lagged by {} events", n);
                    metrics::counter!("aegis_event_bus_receiver_lag_total").increment(n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(EventBusError::Closed);
                }
            }
        }
    }

    fn matches_execution(&self, event: &crate::domain::events::WorkflowEvent) -> bool {
        use crate::domain::events::WorkflowEvent;
        match event {
            WorkflowEvent::WorkflowExecutionStarted { execution_id, .. }
            | WorkflowEvent::WorkflowStateEntered { execution_id, .. }
            | WorkflowEvent::WorkflowStateExited { execution_id, .. }
            | WorkflowEvent::WorkflowIterationStarted { execution_id, .. }
            | WorkflowEvent::WorkflowIterationCompleted { execution_id, .. }
            | WorkflowEvent::WorkflowIterationFailed { execution_id, .. }
            | WorkflowEvent::WorkflowExecutionCompleted { execution_id, .. }
            | WorkflowEvent::WorkflowExecutionFailed { execution_id, .. }
            | WorkflowEvent::WorkflowExecutionCancelled { execution_id, .. } => {
                *execution_id == self.execution_id
            }
            _ => false,
        }
    }
}

/// Receiver for agent-specific events (filtered)
pub struct AgentEventReceiver {
    receiver: broadcast::Receiver<DomainEvent>,
    agent_id: crate::domain::agent::AgentId,
}

impl AgentEventReceiver {
    /// Receive the next event for the specified agent ID
    pub async fn recv(&mut self) -> Result<DomainEvent, EventBusError> {
        loop {
            let event = self.receiver.recv().await.map_err(|e| match e {
                broadcast::error::RecvError::Closed => EventBusError::Closed,
                broadcast::error::RecvError::Lagged(n) => {
                    warn!("Event receiver lagged by {} events", n);
                    metrics::counter!("aegis_event_bus_receiver_lag_total").increment(n);
                    EventBusError::Lagged(n)
                }
            })?;

            if self.matches_agent(&event) {
                return Ok(event);
            }
        }
    }

    fn matches_agent(&self, event: &DomainEvent) -> bool {
        match event {
            DomainEvent::AgentLifecycle(e) => match e {
                AgentLifecycleEvent::AgentDeployed { agent_id, .. } => agent_id == &self.agent_id,
                AgentLifecycleEvent::AgentPaused { agent_id, .. } => agent_id == &self.agent_id,
                AgentLifecycleEvent::AgentResumed { agent_id, .. } => agent_id == &self.agent_id,
                AgentLifecycleEvent::AgentUpdated { agent_id, .. } => agent_id == &self.agent_id,
                AgentLifecycleEvent::AgentRemoved { agent_id, .. } => agent_id == &self.agent_id,
                AgentLifecycleEvent::AgentFailed { agent_id, .. } => agent_id == &self.agent_id,
            },
            DomainEvent::Execution(e) => match e {
                ExecutionEvent::ExecutionStarted { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::IterationStarted { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::IterationCompleted { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::IterationFailed { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::RefinementApplied { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::ExecutionCompleted { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::ExecutionFailed { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::ExecutionCancelled { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::ConsoleOutput { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::LlmInteraction { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::InstanceSpawned { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::InstanceTerminated { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::ExecutionTimedOut { agent_id, .. } => agent_id == &self.agent_id,
                ExecutionEvent::Validation(v) => match v {
                    ValidationEvent::GradientValidationPerformed { agent_id, .. } => {
                        agent_id == &self.agent_id
                    }
                    ValidationEvent::MultiJudgeConsensus { agent_id, .. } => {
                        agent_id == &self.agent_id
                    }
                },
            },
            DomainEvent::Learning(e) => match e {
                LearningEvent::PatternDiscovered { agent_id, .. } => agent_id == &self.agent_id,
                LearningEvent::PatternReinforced { agent_id, .. } => agent_id == &self.agent_id,
                LearningEvent::PatternDecayed { agent_id, .. } => agent_id == &self.agent_id,
            },
            DomainEvent::Workflow(_) => false, // Workflow events are system-wide, not per-agent
            DomainEvent::Policy(e) => match e {
                PolicyEvent::PolicyViolationAttempted { agent_id, .. } => {
                    agent_id == &self.agent_id
                }
                PolicyEvent::PolicyViolationBlocked { agent_id, .. } => agent_id == &self.agent_id,
            },
            DomainEvent::Volume(_) => false, // Not agent-filterable: carries execution_id, not agent_id
            DomainEvent::Storage(_) => false, // Not agent-filterable: carries execution_id, not agent_id
            DomainEvent::MCP(e) => match e {
                MCPToolEvent::InvocationRequested { agent_id, .. } => agent_id == &self.agent_id,
                MCPToolEvent::InvocationCompleted { agent_id, .. } => agent_id == &self.agent_id,
                MCPToolEvent::InvocationFailed { agent_id, .. } => agent_id == &self.agent_id,
                MCPToolEvent::PolicyViolation { agent_id, .. } => agent_id == &self.agent_id,
                // Server lifecycle and InvocationStarted events are global (no agent_id)
                MCPToolEvent::ServerRegistered { .. }
                | MCPToolEvent::ServerStarted { .. }
                | MCPToolEvent::ServerStopped { .. }
                | MCPToolEvent::ServerFailed { .. }
                | MCPToolEvent::ServerUnhealthy { .. }
                | MCPToolEvent::InvocationStarted { .. } => false,
            },
            DomainEvent::Smcp(e) => match e {
                SmcpEvent::AttestationCompleted { agent_id, .. } => agent_id == &self.agent_id,
                SmcpEvent::SessionCreated { agent_id, .. } => agent_id == &self.agent_id,
                SmcpEvent::SessionRevoked { agent_id, .. } => agent_id == &self.agent_id,
                SmcpEvent::PolicyViolationBlocked { agent_id, .. } => agent_id == &self.agent_id,
            },
            DomainEvent::Stimulus(_) => false, // Stimulus events are system-wide, not per-agent
            DomainEvent::ImageManagement(_) => false, // Image management events are system-wide, not per-agent
            DomainEvent::Iam(_) => false,             // IAM events are system-wide, not per-agent
            DomainEvent::Secrets(_) => false, // Secrets events are system-wide, not per-agent
            DomainEvent::ContainerRun(_) => false, // ContainerRun events are keyed by execution_id, not agent_id
        }
    }
}

/// Errors that can occur when receiving events
#[derive(Debug, thiserror::Error)]
pub enum EventBusError {
    #[error("Event bus is closed")]
    Closed,

    #[error("No events available")]
    Empty,

    #[error("Receiver lagged by {0} events (events were dropped)")]
    Lagged(u64),
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[tokio::test]
    async fn test_event_bus_publish_subscribe() {
        let event_bus = EventBus::new(10);
        let mut receiver = event_bus.subscribe();

        let agent_id = crate::domain::agent::AgentId::new();
        let event = AgentLifecycleEvent::AgentDeployed {
            agent_id,
            manifest: crate::domain::agent::AgentManifest {
                api_version: "100monkeys.ai/v1".to_string(),
                kind: "Agent".to_string(),
                metadata: crate::domain::agent::ManifestMetadata {
                    name: "test-agent".to_string(),
                    version: "1.0.0".to_string(),
                    description: None,
                    labels: std::collections::HashMap::new(),
                    annotations: std::collections::HashMap::new(),
                },
                spec: crate::domain::agent::AgentSpec {
                    runtime: crate::domain::agent::RuntimeConfig {
                        language: Some("python".to_string()),
                        version: Some("3.11".to_string()),
                        image: None,
                        image_pull_policy: crate::domain::agent::ImagePullPolicy::IfNotPresent,
                        isolation: "docker".to_string(),
                        model: "default".to_string(),
                    },
                    task: None,
                    context: vec![],
                    execution: None,
                    security: None,
                    schedule: None,
                    tools: vec![],
                    env: std::collections::HashMap::new(),
                    volumes: vec![],
                    advanced: None,
                },
            },
            deployed_at: Utc::now(),
        };

        event_bus.publish_agent_event(event.clone());

        let received = receiver.recv().await.unwrap();
        assert!(
            matches!(
                received,
                DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed { .. })
            ),
            "Expected AgentDeployed event, got {received:?}"
        );
        let DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed {
            agent_id: id, ..
        }) = received
        else {
            return;
        };
        assert_eq!(id, agent_id);
    }

    #[tokio::test]
    async fn test_execution_event_filtering() {
        let event_bus = EventBus::new(10);
        let execution_id = crate::domain::execution::ExecutionId::new();
        let other_execution_id = crate::domain::execution::ExecutionId::new();
        let agent_id = crate::domain::agent::AgentId::new();

        let mut receiver = event_bus.subscribe_execution(execution_id);

        // Publish event for different execution (should be filtered out)
        event_bus.publish_execution_event(ExecutionEvent::ExecutionStarted {
            execution_id: other_execution_id,
            agent_id,
            started_at: Utc::now(),
        });

        // Publish event for our execution (should be received)
        event_bus.publish_execution_event(ExecutionEvent::ExecutionStarted {
            execution_id,
            agent_id,
            started_at: Utc::now(),
        });

        let received = receiver.recv().await.unwrap();
        assert!(
            matches!(received, ExecutionEvent::ExecutionStarted { .. }),
            "Expected ExecutionStarted event, got {received:?}"
        );
        let ExecutionEvent::ExecutionStarted {
            execution_id: id, ..
        } = received
        else {
            return;
        };
        assert_eq!(id, execution_id);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let event_bus = EventBus::new(10);
        let mut receiver1 = event_bus.subscribe();
        let mut receiver2 = event_bus.subscribe();

        assert_eq!(event_bus.subscriber_count(), 2);

        let agent_id = crate::domain::agent::AgentId::new();
        event_bus.publish_agent_event(AgentLifecycleEvent::AgentPaused {
            agent_id,
            paused_at: Utc::now(),
        });

        // Both receivers should get the event
        let _ = receiver1.recv().await.unwrap();
        let _ = receiver2.recv().await.unwrap();
    }

    #[tokio::test]
    async fn test_execution_domain_receiver_includes_storage_events() {
        let event_bus = EventBus::new(10);
        let execution_id = crate::domain::execution::ExecutionId::new();
        let mut receiver = event_bus.subscribe_execution_domain(execution_id);

        event_bus.publish_storage_event(StorageEvent::FileOpened {
            execution_id,
            volume_id: crate::domain::volume::VolumeId::new(),
            path: "/workspace/src/main.rs".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
        });

        let received = receiver.recv().await.unwrap();
        match received {
            DomainEvent::Storage(StorageEvent::FileOpened {
                execution_id: id, ..
            }) => assert_eq!(id, execution_id),
            other => panic!("Expected storage event, got {other:?}"),
        }
    }

    #[test]
    fn test_domain_event_helpers_cover_validation_and_secrets() {
        let execution_id = crate::domain::execution::ExecutionId::new();
        let agent_id = crate::domain::agent::AgentId::new();

        let validation = DomainEvent::Execution(ExecutionEvent::Validation(
            ValidationEvent::GradientValidationPerformed {
                execution_id,
                agent_id,
                iteration_number: 2,
                score: 0.91,
                confidence: 0.82,
                validated_at: Utc::now(),
            },
        ));
        assert_eq!(validation.execution_id(), Some(execution_id));
        assert_eq!(validation.agent_id(), Some(agent_id));
        assert_eq!(validation.iteration_number(), Some(2));
        assert_eq!(
            validation.event_type_name(),
            "gradient_validation_performed"
        );
        assert_eq!(validation.category(), "execution");
        assert_eq!(validation.stage(), Some("iteration"));

        let secret = DomainEvent::Secrets(SecretEvent::SecretRetrieved {
            engine: "kv".to_string(),
            path: "secret/data/test".to_string(),
            access_context: crate::domain::secrets::AccessContext::for_execution(
                "orch-1",
                execution_id,
                agent_id,
            ),
            retrieved_at: Utc::now(),
        });
        assert_eq!(secret.execution_id(), Some(execution_id));
        assert_eq!(secret.agent_id(), Some(agent_id));
        assert_eq!(secret.category(), "secrets");
        assert_eq!(secret.stage(), Some("secrets"));
    }
}
