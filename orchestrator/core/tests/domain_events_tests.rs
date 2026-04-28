// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Domain Events Tests
//!
//! Comprehensive unit tests for the domain event catalog (ADR-030) and the
//! `DomainEvent` wrapper enum in the event bus. Covers construction, field
//! access, `execution_id()` / `agent_id()` extraction, and serde round-trips
//! for every major event group.

use aegis_orchestrator_core::domain::events::{
    AgentLifecycleEvent, ExecutionEvent, SealEvent, StimulusEvent, StorageEvent, ValidationEvent,
    ViolationType, VolumeEvent, WorkflowEvent,
};
use aegis_orchestrator_core::domain::execution::{CodeDiff, IterationError};
use aegis_orchestrator_core::domain::runtime::InstanceId;
use aegis_orchestrator_core::domain::shared_kernel::{
    AgentId, ExecutionId, StimulusId, VolumeId, WorkflowId,
};
use aegis_orchestrator_core::domain::volume::StorageClass;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

// ═══════════════════════════════════════════════════════════════════════════════
// 1. DomainEvent::execution_id() — returns correct ID for execution events,
//    None for events without an execution scope.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn execution_id_returns_some_for_execution_started() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = DomainEvent::Execution(ExecutionEvent::ExecutionStarted {
        execution_id: eid,
        agent_id: aid,
        started_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), Some(eid));
}

#[test]
fn execution_id_returns_some_for_iteration_completed() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = DomainEvent::Execution(ExecutionEvent::IterationCompleted {
        execution_id: eid,
        agent_id: aid,
        iteration_number: 2,
        output: "done".to_string(),
        completed_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), Some(eid));
}

#[test]
fn execution_id_returns_some_for_validation_event() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = DomainEvent::Execution(ExecutionEvent::Validation(
        ValidationEvent::GradientValidationPerformed {
            execution_id: eid,
            agent_id: aid,
            iteration_number: 1,
            score: 0.88,
            confidence: 0.95,
            validated_at: Utc::now(),
        },
    ));
    assert_eq!(event.execution_id(), Some(eid));
}

#[test]
fn execution_id_returns_none_for_agent_lifecycle() {
    let aid = AgentId::new();
    let event = DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentPaused {
        agent_id: aid,
        paused_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), None);
}

#[test]
fn execution_id_returns_none_for_stimulus_event() {
    let event = DomainEvent::Stimulus(StimulusEvent::StimulusReceived {
        stimulus_id: StimulusId::new(),
        source: "webhook".to_string(),
        received_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), None);
}

#[test]
fn execution_id_returns_some_for_workflow_execution_started() {
    let eid = ExecutionId::new();
    let wid = WorkflowId::new();
    let event = DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionStarted {
        execution_id: eid,
        workflow_id: wid,
        started_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), Some(eid));
}

#[test]
fn execution_id_returns_none_for_workflow_registered() {
    let wid = WorkflowId::new();
    let event = DomainEvent::Workflow(WorkflowEvent::WorkflowRegistered {
        workflow_id: wid,
        tenant_id: aegis_orchestrator_core::domain::tenant::TenantId::consumer(),
        name: "build-pipeline".to_string(),
        version: "1.0.0".to_string(),
        scope: aegis_orchestrator_core::domain::workflow::WorkflowScope::default(),
        registered_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), None);
}

#[test]
fn execution_id_returns_some_for_storage_event() {
    let eid = ExecutionId::new();
    let vid = VolumeId::new();
    let event = DomainEvent::Storage(StorageEvent::FileOpened {
        execution_id: Some(eid),
        workflow_execution_id: None,
        volume_id: vid,
        path: "/workspace/main.py".to_string(),
        open_mode: "read".to_string(),
        opened_at: Utc::now(),
        caller_node_id: None,
        host_node_id: None,
    });
    assert_eq!(event.execution_id(), Some(eid));
}

#[test]
fn execution_id_returns_some_for_seal_session_created() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = DomainEvent::Seal(SealEvent::SessionCreated {
        session_id: "sess-abc".to_string(),
        agent_id: aid,
        execution_id: eid,
        security_context_name: "aegis-system-operator".to_string(),
        expires_at: Utc::now(),
        created_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), Some(eid));
}

#[test]
fn execution_id_returns_none_for_seal_session_revoked() {
    let aid = AgentId::new();
    let event = DomainEvent::Seal(SealEvent::SessionRevoked {
        session_id: "sess-abc".to_string(),
        agent_id: aid,
        reason: "execution complete".to_string(),
        revoked_at: Utc::now(),
    });
    assert_eq!(event.execution_id(), None);
}

// ═══════════════════════════════════════════════════════════════════════════════
// 2. DomainEvent::agent_id() — returns correct ID for agent events, None for
//    events without an agent scope.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn agent_id_returns_some_for_agent_deployed() {
    let aid = AgentId::new();
    // Construct a minimal AgentManifest via serde (Kubernetes-style structure).
    let manifest_json = serde_json::json!({
        "apiVersion": "100monkeys.ai/v1",
        "kind": "Agent",
        "metadata": {
            "name": "test-agent",
            "version": "0.1.0"
        },
        "spec": {
            "runtime": {
                "language": "python",
                "version": "3.11"
            }
        }
    });
    let manifest: aegis_orchestrator_core::domain::agent::AgentManifest =
        serde_json::from_value(manifest_json).unwrap();
    let event = DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed {
        agent_id: aid,
        tenant_id: aegis_orchestrator_core::domain::tenant::TenantId::consumer(),
        manifest,
        deployed_at: Utc::now(),
    });
    assert_eq!(event.agent_id(), Some(aid));
}

#[test]
fn agent_id_returns_some_for_execution_started() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = DomainEvent::Execution(ExecutionEvent::ExecutionStarted {
        execution_id: eid,
        agent_id: aid,
        started_at: Utc::now(),
    });
    assert_eq!(event.agent_id(), Some(aid));
}

#[test]
fn agent_id_returns_some_for_seal_attestation_completed() {
    let aid = AgentId::new();
    let eid = ExecutionId::new();
    let event = DomainEvent::Seal(SealEvent::AttestationCompleted {
        agent_id: aid,
        execution_id: eid,
        security_context_name: "restricted".to_string(),
        attested_at: Utc::now(),
    });
    assert_eq!(event.agent_id(), Some(aid));
}

#[test]
fn agent_id_returns_none_for_workflow_event() {
    let eid = ExecutionId::new();
    let event = DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionCompleted {
        execution_id: eid,
        final_blackboard: serde_json::json!({}),
        artifacts: None,
        completed_at: Utc::now(),
    });
    assert_eq!(event.agent_id(), None);
}

#[test]
fn agent_id_returns_none_for_volume_event() {
    let vid = VolumeId::new();
    let event = DomainEvent::Volume(VolumeEvent::VolumeDeleted {
        volume_id: vid,
        deleted_at: Utc::now(),
    });
    assert_eq!(event.agent_id(), None);
}

#[test]
fn agent_id_returns_none_for_stimulus_event() {
    let event = DomainEvent::Stimulus(StimulusEvent::StimulusRejected {
        stimulus_id: StimulusId::new(),
        reason: "low_confidence: 0.42".to_string(),
        rejected_at: Utc::now(),
    });
    assert_eq!(event.agent_id(), None);
}

// ═══════════════════════════════════════════════════════════════════════════════
// 3. ExecutionEvent variants — construction, field access, serialization.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn execution_event_started_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = ExecutionEvent::ExecutionStarted {
        execution_id: eid,
        agent_id: aid,
        started_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::ExecutionStarted {
            execution_id,
            agent_id,
            ..
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(agent_id, aid);
        }
        other => panic!("Expected ExecutionStarted, got {other:?}"),
    }
}

#[test]
fn execution_event_iteration_failed_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = ExecutionEvent::IterationFailed {
        execution_id: eid,
        agent_id: aid,
        iteration_number: 3,
        error: IterationError {
            message: "syntax error".to_string(),
            details: Some("line 42".to_string()),
        },
        failed_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::IterationFailed {
            iteration_number,
            error,
            ..
        } => {
            assert_eq!(iteration_number, 3);
            assert_eq!(error.message, "syntax error");
            assert_eq!(error.details, Some("line 42".to_string()));
        }
        other => panic!("Expected IterationFailed, got {other:?}"),
    }
}

#[test]
fn execution_event_refinement_applied_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = ExecutionEvent::RefinementApplied {
        execution_id: eid,
        agent_id: aid,
        iteration_number: 2,
        code_diff: CodeDiff {
            file_path: "src/main.rs".to_string(),
            diff: "+fn main() {}".to_string(),
        },
        applied_at: Utc::now(),
        cortex_pattern_id: Some("pat-001".to_string()),
        cortex_pattern_category: Some("error_handling".to_string()),
        cortex_success_score: Some(0.92),
        cortex_solution_approach: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::RefinementApplied {
            code_diff,
            cortex_pattern_id,
            cortex_solution_approach,
            ..
        } => {
            assert_eq!(code_diff.file_path, "src/main.rs");
            assert_eq!(cortex_pattern_id, Some("pat-001".to_string()));
            assert!(cortex_solution_approach.is_none());
        }
        other => panic!("Expected RefinementApplied, got {other:?}"),
    }
}

#[test]
fn execution_event_console_output_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = ExecutionEvent::ConsoleOutput {
        execution_id: eid,
        agent_id: aid,
        iteration_number: 1,
        stream: "stderr".to_string(),
        content: "warning: unused variable".to_string(),
        timestamp: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("stderr"));
    assert!(json.contains("unused variable"));
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::ConsoleOutput {
            stream, content, ..
        } => {
            assert_eq!(stream, "stderr");
            assert!(content.contains("unused variable"));
        }
        other => panic!("Expected ConsoleOutput, got {other:?}"),
    }
}

#[test]
fn execution_event_instance_spawned_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let iid = InstanceId::new("container-abc123");
    let event = ExecutionEvent::InstanceSpawned {
        execution_id: eid,
        agent_id: aid,
        iteration_number: 1,
        instance_id: iid.clone(),
        spawned_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::InstanceSpawned { instance_id, .. } => {
            assert_eq!(instance_id.0, "container-abc123");
        }
        other => panic!("Expected InstanceSpawned, got {other:?}"),
    }
}

#[test]
fn execution_event_child_spawned_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let child_eid = ExecutionId::new();
    let child_aid = AgentId::new();
    let event = ExecutionEvent::ChildExecutionSpawned {
        execution_id: eid,
        agent_id: aid,
        parent_execution_id: eid,
        child_execution_id: child_eid,
        child_agent_id: child_aid,
        spawned_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::ChildExecutionSpawned {
            child_execution_id,
            child_agent_id,
            ..
        } => {
            assert_eq!(child_execution_id, child_eid);
            assert_eq!(child_agent_id, child_aid);
        }
        other => panic!("Expected ChildExecutionSpawned, got {other:?}"),
    }
}

#[test]
fn execution_event_timed_out_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = ExecutionEvent::ExecutionTimedOut {
        execution_id: eid,
        agent_id: aid,
        timeout_seconds: 300,
        total_iterations: 5,
        timed_out_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ExecutionEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ExecutionEvent::ExecutionTimedOut {
            timeout_seconds,
            total_iterations,
            ..
        } => {
            assert_eq!(timeout_seconds, 300);
            assert_eq!(total_iterations, 5);
        }
        other => panic!("Expected ExecutionTimedOut, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 4. ValidationEvent variants.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn validation_event_gradient_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = ValidationEvent::GradientValidationPerformed {
        execution_id: eid,
        agent_id: aid,
        iteration_number: 1,
        score: 0.88,
        confidence: 0.92,
        validated_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ValidationEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ValidationEvent::GradientValidationPerformed {
            score, confidence, ..
        } => {
            assert!((score - 0.88).abs() < f64::EPSILON);
            assert!((confidence - 0.92).abs() < f64::EPSILON);
        }
        other => panic!("Expected GradientValidationPerformed, got {other:?}"),
    }
}

#[test]
fn validation_event_multi_judge_consensus_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let judge1 = AgentId::new();
    let judge2 = AgentId::new();
    let event = ValidationEvent::MultiJudgeConsensus {
        execution_id: eid,
        agent_id: aid,
        judge_scores: vec![(judge1, 0.90), (judge2, 0.85)],
        final_score: 0.875,
        confidence: 0.88,
        reached_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: ValidationEvent = serde_json::from_str(&json).unwrap();
    match deser {
        ValidationEvent::MultiJudgeConsensus {
            judge_scores,
            final_score,
            ..
        } => {
            assert_eq!(judge_scores.len(), 2);
            assert!((final_score - 0.875).abs() < f64::EPSILON);
        }
        other => panic!("Expected MultiJudgeConsensus, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 5. AgentLifecycleEvent.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn agent_lifecycle_paused_serde_roundtrip() {
    let aid = AgentId::new();
    let event = AgentLifecycleEvent::AgentPaused {
        agent_id: aid,
        paused_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: AgentLifecycleEvent = serde_json::from_str(&json).unwrap();
    match deser {
        AgentLifecycleEvent::AgentPaused { agent_id, .. } => {
            assert_eq!(agent_id, aid);
        }
        other => panic!("Expected AgentPaused, got {other:?}"),
    }
}

#[test]
fn agent_lifecycle_updated_serde_roundtrip() {
    let aid = AgentId::new();
    let event = AgentLifecycleEvent::AgentUpdated {
        agent_id: aid,
        tenant_id: aegis_orchestrator_core::domain::tenant::TenantId::consumer(),
        old_version: "1.0.0".to_string(),
        new_version: "1.1.0".to_string(),
        updated_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: AgentLifecycleEvent = serde_json::from_str(&json).unwrap();
    match deser {
        AgentLifecycleEvent::AgentUpdated {
            old_version,
            new_version,
            ..
        } => {
            assert_eq!(old_version, "1.0.0");
            assert_eq!(new_version, "1.1.0");
        }
        other => panic!("Expected AgentUpdated, got {other:?}"),
    }
}

#[test]
fn agent_lifecycle_removed_serde_roundtrip() {
    let aid = AgentId::new();
    let event = AgentLifecycleEvent::AgentRemoved {
        agent_id: aid,
        tenant_id: aegis_orchestrator_core::domain::tenant::TenantId::consumer(),
        removed_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: AgentLifecycleEvent = serde_json::from_str(&json).unwrap();
    match deser {
        AgentLifecycleEvent::AgentRemoved { agent_id, .. } => assert_eq!(agent_id, aid),
        other => panic!("Expected AgentRemoved, got {other:?}"),
    }
}

#[test]
fn agent_lifecycle_failed_serde_roundtrip() {
    let aid = AgentId::new();
    let event = AgentLifecycleEvent::AgentFailed {
        agent_id: aid,
        reason: "OOM killed".to_string(),
        failed_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: AgentLifecycleEvent = serde_json::from_str(&json).unwrap();
    match deser {
        AgentLifecycleEvent::AgentFailed { reason, .. } => assert_eq!(reason, "OOM killed"),
        other => panic!("Expected AgentFailed, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 6. WorkflowEvent.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn workflow_event_state_entered_serde_roundtrip() {
    let eid = ExecutionId::new();
    let event = WorkflowEvent::WorkflowStateEntered {
        execution_id: eid,
        state_name: "BUILD".to_string(),
        entered_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: WorkflowEvent = serde_json::from_str(&json).unwrap();
    match deser {
        WorkflowEvent::WorkflowStateEntered { state_name, .. } => {
            assert_eq!(state_name, "BUILD");
        }
        other => panic!("Expected WorkflowStateEntered, got {other:?}"),
    }
}

#[test]
fn workflow_event_subworkflow_triggered_serde_roundtrip() {
    let parent_eid = ExecutionId::new();
    let child_eid = ExecutionId::new();
    let child_wid = WorkflowId::new();
    let event = WorkflowEvent::SubworkflowTriggered {
        parent_execution_id: parent_eid,
        child_execution_id: child_eid,
        child_workflow_id: child_wid,
        mode: "blocking".to_string(),
        parent_state_name: "DEPLOY".to_string(),
        triggered_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: WorkflowEvent = serde_json::from_str(&json).unwrap();
    match deser {
        WorkflowEvent::SubworkflowTriggered {
            parent_execution_id,
            child_execution_id,
            mode,
            ..
        } => {
            assert_eq!(parent_execution_id, parent_eid);
            assert_eq!(child_execution_id, child_eid);
            assert_eq!(mode, "blocking");
        }
        other => panic!("Expected SubworkflowTriggered, got {other:?}"),
    }
}

#[test]
fn workflow_event_execution_cancelled_serde_roundtrip() {
    let eid = ExecutionId::new();
    let event = WorkflowEvent::WorkflowExecutionCancelled {
        execution_id: eid,
        cancelled_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: WorkflowEvent = serde_json::from_str(&json).unwrap();
    match deser {
        WorkflowEvent::WorkflowExecutionCancelled { execution_id, .. } => {
            assert_eq!(execution_id, eid);
        }
        other => panic!("Expected WorkflowExecutionCancelled, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7. StorageEvent — file operation audit events.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn storage_event_file_written_serde_roundtrip() {
    let eid = ExecutionId::new();
    let vid = VolumeId::new();
    let event = StorageEvent::FileWritten {
        execution_id: Some(eid),
        workflow_execution_id: None,
        volume_id: vid,
        path: "/workspace/output.txt".to_string(),
        offset: 0,
        bytes_written: 1024,
        duration_ms: 12,
        written_at: Utc::now(),
        caller_node_id: None,
        host_node_id: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StorageEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StorageEvent::FileWritten {
            bytes_written,
            path,
            ..
        } => {
            assert_eq!(bytes_written, 1024);
            assert_eq!(path, "/workspace/output.txt");
        }
        other => panic!("Expected FileWritten, got {other:?}"),
    }
}

#[test]
fn storage_event_path_traversal_blocked_serde_roundtrip() {
    let eid = ExecutionId::new();
    let event = StorageEvent::PathTraversalBlocked {
        execution_id: Some(eid),
        workflow_execution_id: None,
        attempted_path: "../../../etc/passwd".to_string(),
        blocked_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StorageEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StorageEvent::PathTraversalBlocked { attempted_path, .. } => {
            assert!(attempted_path.contains("../"));
        }
        other => panic!("Expected PathTraversalBlocked, got {other:?}"),
    }
}

#[test]
fn storage_event_quota_exceeded_serde_roundtrip() {
    let eid = ExecutionId::new();
    let vid = VolumeId::new();
    let event = StorageEvent::QuotaExceeded {
        execution_id: Some(eid),
        workflow_execution_id: None,
        volume_id: vid,
        requested_bytes: 10_000_000,
        available_bytes: 500_000,
        exceeded_at: Utc::now(),
        caller_node_id: None,
        host_node_id: None,
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StorageEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StorageEvent::QuotaExceeded {
            requested_bytes,
            available_bytes,
            ..
        } => {
            assert_eq!(requested_bytes, 10_000_000);
            assert_eq!(available_bytes, 500_000);
        }
        other => panic!("Expected QuotaExceeded, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8. VolumeEvent — volume lifecycle events.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn volume_event_created_serde_roundtrip() {
    let vid = VolumeId::new();
    let eid = ExecutionId::new();
    let event = VolumeEvent::VolumeCreated {
        volume_id: vid,
        execution_id: Some(eid),
        storage_class: StorageClass::Persistent,
        remote_path: "/mnt/seaweedfs/volumes/abc".to_string(),
        size_limit_bytes: 1_073_741_824,
        created_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: VolumeEvent = serde_json::from_str(&json).unwrap();
    match deser {
        VolumeEvent::VolumeCreated {
            volume_id,
            execution_id,
            size_limit_bytes,
            ..
        } => {
            assert_eq!(volume_id, vid);
            assert_eq!(execution_id, Some(eid));
            assert_eq!(size_limit_bytes, 1_073_741_824);
        }
        other => panic!("Expected VolumeCreated, got {other:?}"),
    }
}

#[test]
fn volume_event_attached_serde_roundtrip() {
    let vid = VolumeId::new();
    let iid = InstanceId::new("inst-xyz");
    let event = VolumeEvent::VolumeAttached {
        volume_id: vid,
        instance_id: iid,
        mount_point: "/workspace".to_string(),
        access_mode: "read-write".to_string(),
        attached_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: VolumeEvent = serde_json::from_str(&json).unwrap();
    match deser {
        VolumeEvent::VolumeAttached {
            mount_point,
            access_mode,
            ..
        } => {
            assert_eq!(mount_point, "/workspace");
            assert_eq!(access_mode, "read-write");
        }
        other => panic!("Expected VolumeAttached, got {other:?}"),
    }
}

#[test]
fn volume_event_expired_serde_roundtrip() {
    let vid = VolumeId::new();
    let event = VolumeEvent::VolumeExpired {
        volume_id: vid,
        expired_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: VolumeEvent = serde_json::from_str(&json).unwrap();
    match deser {
        VolumeEvent::VolumeExpired { volume_id, .. } => assert_eq!(volume_id, vid),
        other => panic!("Expected VolumeExpired, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 9. StimulusEvent — stimulus routing events.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn stimulus_event_received_serde_roundtrip() {
    let sid = StimulusId::new();
    let event = StimulusEvent::StimulusReceived {
        stimulus_id: sid,
        source: "github-webhook".to_string(),
        received_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StimulusEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StimulusEvent::StimulusReceived { source, .. } => {
            assert_eq!(source, "github-webhook");
        }
        other => panic!("Expected StimulusReceived, got {other:?}"),
    }
}

#[test]
fn stimulus_event_classified_serde_roundtrip() {
    let sid = StimulusId::new();
    let event = StimulusEvent::StimulusClassified {
        stimulus_id: sid,
        workflow_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        confidence: 0.95,
        routing_mode: "Deterministic".to_string(),
        classified_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StimulusEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StimulusEvent::StimulusClassified {
            confidence,
            routing_mode,
            ..
        } => {
            assert!((confidence - 0.95).abs() < f64::EPSILON);
            assert_eq!(routing_mode, "Deterministic");
        }
        other => panic!("Expected StimulusClassified, got {other:?}"),
    }
}

#[test]
fn stimulus_event_rejected_serde_roundtrip() {
    let sid = StimulusId::new();
    let event = StimulusEvent::StimulusRejected {
        stimulus_id: sid,
        reason: "hmac_invalid".to_string(),
        rejected_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StimulusEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StimulusEvent::StimulusRejected { reason, .. } => {
            assert_eq!(reason, "hmac_invalid");
        }
        other => panic!("Expected StimulusRejected, got {other:?}"),
    }
}

#[test]
fn stimulus_event_classification_failed_serde_roundtrip() {
    let sid = StimulusId::new();
    let event = StimulusEvent::ClassificationFailed {
        stimulus_id: sid,
        error: "LLM timeout".to_string(),
        failed_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: StimulusEvent = serde_json::from_str(&json).unwrap();
    match deser {
        StimulusEvent::ClassificationFailed { error, .. } => {
            assert_eq!(error, "LLM timeout");
        }
        other => panic!("Expected ClassificationFailed, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 10. SealEvent — SEAL session events.
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn seal_event_session_created_serde_roundtrip() {
    let aid = AgentId::new();
    let eid = ExecutionId::new();
    let event = SealEvent::SessionCreated {
        session_id: "sess-123".to_string(),
        agent_id: aid,
        execution_id: eid,
        security_context_name: "aegis-system-operator".to_string(),
        expires_at: Utc::now(),
        created_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: SealEvent = serde_json::from_str(&json).unwrap();
    match deser {
        SealEvent::SessionCreated {
            session_id,
            agent_id,
            ..
        } => {
            assert_eq!(session_id, "sess-123");
            assert_eq!(agent_id, aid);
        }
        other => panic!("Expected SessionCreated, got {other:?}"),
    }
}

#[test]
fn seal_event_session_revoked_serde_roundtrip() {
    let aid = AgentId::new();
    let event = SealEvent::SessionRevoked {
        session_id: "sess-123".to_string(),
        agent_id: aid,
        reason: "security incident".to_string(),
        revoked_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: SealEvent = serde_json::from_str(&json).unwrap();
    match deser {
        SealEvent::SessionRevoked { reason, .. } => {
            assert_eq!(reason, "security incident");
        }
        other => panic!("Expected SessionRevoked, got {other:?}"),
    }
}

#[test]
fn seal_event_policy_violation_blocked_serde_roundtrip() {
    let aid = AgentId::new();
    let eid = ExecutionId::new();
    let event = SealEvent::PolicyViolationBlocked {
        agent_id: aid,
        execution_id: eid,
        tool_name: "fs_write".to_string(),
        violation_type: ViolationType::ToolNotAllowed,
        details: "tool not in allowlist".to_string(),
        blocked_at: Utc::now(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let deser: SealEvent = serde_json::from_str(&json).unwrap();
    match deser {
        SealEvent::PolicyViolationBlocked {
            tool_name, details, ..
        } => {
            assert_eq!(tool_name, "fs_write");
            assert!(details.contains("allowlist"));
        }
        other => panic!("Expected PolicyViolationBlocked, got {other:?}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 11. DomainEvent serde round-trip (the tagged wrapper).
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn domain_event_execution_serde_roundtrip() {
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let event = DomainEvent::Execution(ExecutionEvent::ExecutionCompleted {
        execution_id: eid,
        agent_id: aid,
        final_output: "success".to_string(),
        total_iterations: 3,
        completed_at: Utc::now(),
    });
    let json = serde_json::to_string(&event).unwrap();
    // DomainEvent uses `#[serde(tag = "type")]` so the JSON must contain the tag.
    assert!(json.contains("\"type\":\"execution\""));
    let deser: DomainEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.execution_id(), Some(eid));
    assert_eq!(deser.agent_id(), Some(aid));
}

#[test]
fn domain_event_agent_lifecycle_serde_roundtrip() {
    let aid = AgentId::new();
    let event = DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentResumed {
        agent_id: aid,
        resumed_at: Utc::now(),
    });
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"agent_lifecycle\""));
    let deser: DomainEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.agent_id(), Some(aid));
    assert_eq!(deser.execution_id(), None);
}

#[test]
fn domain_event_stimulus_serde_roundtrip() {
    let sid = StimulusId::new();
    let event = DomainEvent::Stimulus(StimulusEvent::StimulusReceived {
        stimulus_id: sid,
        source: "cron".to_string(),
        received_at: Utc::now(),
    });
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"stimulus\""));
    let deser: DomainEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.execution_id(), None);
    assert_eq!(deser.agent_id(), None);
}

#[test]
fn domain_event_volume_serde_roundtrip() {
    let vid = VolumeId::new();
    let event = DomainEvent::Volume(VolumeEvent::VolumeDeleted {
        volume_id: vid,
        deleted_at: Utc::now(),
    });
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"volume\""));
    let deser: DomainEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.execution_id(), None);
    assert_eq!(deser.agent_id(), None);
}

#[test]
fn domain_event_seal_serde_roundtrip() {
    let aid = AgentId::new();
    let eid = ExecutionId::new();
    let event = DomainEvent::Seal(SealEvent::AttestationCompleted {
        agent_id: aid,
        execution_id: eid,
        security_context_name: "sandbox".to_string(),
        attested_at: Utc::now(),
    });
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"seal\""));
    let deser: DomainEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(deser.execution_id(), Some(eid));
    assert_eq!(deser.agent_id(), Some(aid));
}

// ═══════════════════════════════════════════════════════════════════════════════
// 12. EventBus async publish/subscribe.
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn event_bus_publish_and_receive_execution_event() {
    let bus = Arc::new(EventBus::with_default_capacity());
    let eid = ExecutionId::new();
    let aid = AgentId::new();
    let mut receiver = bus.subscribe_execution(eid);

    bus.publish_execution_event(ExecutionEvent::ExecutionStarted {
        execution_id: eid,
        agent_id: aid,
        started_at: Utc::now(),
    });

    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("timed out waiting for event")
        .expect("channel closed");

    match event {
        ExecutionEvent::ExecutionStarted {
            execution_id,
            agent_id,
            ..
        } => {
            assert_eq!(execution_id, eid);
            assert_eq!(agent_id, aid);
        }
        other => panic!("Expected ExecutionStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn event_bus_filters_by_execution_id() {
    let bus = Arc::new(EventBus::with_default_capacity());
    let eid_target = ExecutionId::new();
    let eid_other = ExecutionId::new();
    let aid = AgentId::new();
    let mut receiver = bus.subscribe_execution(eid_target);

    // Publish an event for a different execution first.
    bus.publish_execution_event(ExecutionEvent::ExecutionStarted {
        execution_id: eid_other,
        agent_id: aid,
        started_at: Utc::now(),
    });

    // Publish the target event.
    bus.publish_execution_event(ExecutionEvent::ExecutionCompleted {
        execution_id: eid_target,
        agent_id: aid,
        final_output: "done".to_string(),
        total_iterations: 1,
        completed_at: Utc::now(),
    });

    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("timed out waiting for event")
        .expect("channel closed");

    match event {
        ExecutionEvent::ExecutionCompleted {
            execution_id,
            final_output,
            ..
        } => {
            assert_eq!(execution_id, eid_target);
            assert_eq!(final_output, "done");
        }
        other => panic!("Expected ExecutionCompleted for target, got {other:?}"),
    }
}

#[tokio::test]
async fn event_bus_subscriber_count() {
    let bus = EventBus::with_default_capacity();
    assert_eq!(bus.subscriber_count(), 0);

    let _r1 = bus.subscribe();
    assert_eq!(bus.subscriber_count(), 1);

    let _r2 = bus.subscribe();
    assert_eq!(bus.subscriber_count(), 2);

    drop(_r1);
    // broadcast::Receiver updates count on next send, so count may still show 2
    // until the next publish cycle. Just verify we can subscribe and it increments.
}
