// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

use aegis_orchestrator_core::domain::events::{
    PolicyEvent, SmcpEvent, StimulusEvent, StorageEvent,
};
use aegis_orchestrator_core::domain::stimulus::StimulusId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};

const DEFAULT_CAPACITY: usize = 512;

#[derive(Debug, Clone, Serialize)]
pub struct StimulusView {
    pub stimulus_id: String,
    pub source: String,
    pub status: String,
    pub workflow_id: Option<String>,
    pub confidence: Option<f64>,
    pub routing_mode: Option<String>,
    pub rejection_reason: Option<String>,
    pub classification_error: Option<String>,
    pub received_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecurityIncidentView {
    pub category: String,
    pub severity: String,
    pub agent_id: Option<String>,
    pub execution_id: Option<String>,
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
    pub details: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageViolationView {
    pub category: String,
    pub execution_id: String,
    pub volume_id: Option<String>,
    pub path: Option<String>,
    pub operation: Option<String>,
    pub details: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Default)]
struct OperatorReadModelState {
    stimuli: HashMap<StimulusId, StimulusView>,
    security_incidents: VecDeque<SecurityIncidentView>,
}

#[derive(Clone)]
pub struct OperatorReadModelStore {
    state: Arc<RwLock<OperatorReadModelState>>,
    capacity: usize,
}

impl OperatorReadModelStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(RwLock::new(OperatorReadModelState::default())),
            capacity: DEFAULT_CAPACITY,
        })
    }

    pub fn spawn_collector(event_bus: Arc<EventBus>) -> Arc<Self> {
        let store = Self::new();
        let collector = store.clone();
        tokio::spawn(async move {
            let mut receiver = event_bus.subscribe();
            while let Ok(event) = receiver.recv().await {
                collector.record_event(event).await;
            }
        });
        store
    }

    pub async fn record_event(&self, event: DomainEvent) {
        let mut state = self.state.write().await;
        match event {
            DomainEvent::Stimulus(stimulus_event) => {
                apply_stimulus_event(&mut state.stimuli, &stimulus_event);
            }
            DomainEvent::Smcp(smcp_event) => {
                if let Some(incident) = security_incident_from_smcp(&smcp_event) {
                    push_bounded(&mut state.security_incidents, incident, self.capacity);
                }
            }
            DomainEvent::Policy(policy_event) => {
                if let Some(incident) = security_incident_from_policy(&policy_event) {
                    push_bounded(&mut state.security_incidents, incident, self.capacity);
                }
            }
            _ => {}
        }
    }

    pub async fn list_stimuli(&self) -> Vec<StimulusView> {
        let state = self.state.read().await;
        let mut items: Vec<StimulusView> = state.stimuli.values().cloned().collect();
        items.sort_by(|a, b| b.last_event_at.cmp(&a.last_event_at));
        items
    }

    pub async fn get_stimulus(&self, stimulus_id: StimulusId) -> Option<StimulusView> {
        let state = self.state.read().await;
        state.stimuli.get(&stimulus_id).cloned()
    }

    pub async fn list_security_incidents(&self) -> Vec<SecurityIncidentView> {
        let state = self.state.read().await;
        state.security_incidents.iter().cloned().collect()
    }
}

/// Build a `StorageViolationView` from a security-related `StorageEvent`.
///
/// This function supports `StorageEvent` variants that represent storage policy
/// violations as well as security-relevant audit events (for example, blocked path
/// traversals or filesystem policy violations). Callers may pass any `StorageEvent`
/// variant that is explicitly handled by this function's `match` expression.
pub fn storage_violation_event_view(event: &StorageEvent) -> StorageViolationView {
    match event {
        StorageEvent::PathTraversalBlocked {
            execution_id,
            attempted_path,
            blocked_at,
        } => StorageViolationView {
            category: "path_traversal_blocked".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: None,
            path: Some(attempted_path.clone()),
            operation: Some("path_traversal".to_string()),
            details: "Path traversal was blocked before reaching storage".to_string(),
            occurred_at: *blocked_at,
        },
        StorageEvent::FilesystemPolicyViolation {
            execution_id,
            volume_id,
            operation,
            path,
            policy_rule,
            violated_at,
        } => StorageViolationView {
            category: "filesystem_policy_violation".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some(operation.clone()),
            details: format!("Policy rule violated: {policy_rule}"),
            occurred_at: *violated_at,
        },
        StorageEvent::QuotaExceeded {
            execution_id,
            volume_id,
            requested_bytes,
            available_bytes,
            exceeded_at,
        } => StorageViolationView {
            category: "quota_exceeded".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: None,
            operation: Some("quota".to_string()),
            details: format!(
                "Requested {requested_bytes} bytes but only {available_bytes} bytes were available"
            ),
            occurred_at: *exceeded_at,
        },
        StorageEvent::UnauthorizedVolumeAccess {
            execution_id,
            volume_id,
            attempted_at,
        } => StorageViolationView {
            category: "unauthorized_volume_access".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: None,
            operation: Some("access".to_string()),
            details: "Unauthorized volume access was blocked".to_string(),
            occurred_at: *attempted_at,
        },
        StorageEvent::FileOpened {
            execution_id,
            volume_id,
            path,
            open_mode,
            opened_at,
        } => StorageViolationView {
            category: "file_opened".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some(open_mode.clone()),
            details: "File open audit event".to_string(),
            occurred_at: *opened_at,
        },
        StorageEvent::FileRead {
            execution_id,
            volume_id,
            path,
            read_at,
            ..
        } => StorageViolationView {
            category: "file_read".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some("read".to_string()),
            details: "File read audit event".to_string(),
            occurred_at: *read_at,
        },
        StorageEvent::FileWritten {
            execution_id,
            volume_id,
            path,
            written_at,
            ..
        } => StorageViolationView {
            category: "file_written".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some("write".to_string()),
            details: "File write audit event".to_string(),
            occurred_at: *written_at,
        },
        StorageEvent::FileClosed {
            execution_id,
            volume_id,
            path,
            closed_at,
        } => StorageViolationView {
            category: "file_closed".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some("close".to_string()),
            details: "File close audit event".to_string(),
            occurred_at: *closed_at,
        },
        StorageEvent::DirectoryListed {
            execution_id,
            volume_id,
            path,
            listed_at,
            ..
        } => StorageViolationView {
            category: "directory_listed".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some("list".to_string()),
            details: "Directory listing audit event".to_string(),
            occurred_at: *listed_at,
        },
        StorageEvent::FileCreated {
            execution_id,
            volume_id,
            path,
            created_at,
        } => StorageViolationView {
            category: "file_created".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some("create".to_string()),
            details: "File creation audit event".to_string(),
            occurred_at: *created_at,
        },
        StorageEvent::FileDeleted {
            execution_id,
            volume_id,
            path,
            deleted_at,
        } => StorageViolationView {
            category: "file_deleted".to_string(),
            execution_id: execution_id.0.to_string(),
            volume_id: Some(volume_id.0.to_string()),
            path: Some(path.clone()),
            operation: Some("delete".to_string()),
            details: "File deletion audit event".to_string(),
            occurred_at: *deleted_at,
        },
    }
}

fn stimulus_event_id(event: &StimulusEvent) -> StimulusId {
    match event {
        StimulusEvent::StimulusReceived { stimulus_id, .. }
        | StimulusEvent::StimulusClassified { stimulus_id, .. }
        | StimulusEvent::StimulusRejected { stimulus_id, .. }
        | StimulusEvent::ClassificationFailed { stimulus_id, .. } => *stimulus_id,
    }
}

fn apply_stimulus_event(stimuli: &mut HashMap<StimulusId, StimulusView>, event: &StimulusEvent) {
    let id = stimulus_event_id(event);
    match event {
        StimulusEvent::StimulusReceived {
            stimulus_id,
            source,
            received_at,
        } => {
            stimuli.insert(
                *stimulus_id,
                StimulusView {
                    stimulus_id: stimulus_id.to_string(),
                    source: source.clone(),
                    status: "received".to_string(),
                    workflow_id: None,
                    confidence: None,
                    routing_mode: None,
                    rejection_reason: None,
                    classification_error: None,
                    received_at: *received_at,
                    last_event_at: *received_at,
                },
            );
        }
        StimulusEvent::StimulusClassified {
            workflow_id,
            confidence,
            routing_mode,
            classified_at,
            ..
        } => {
            let entry = stimuli.entry(id).or_insert_with(|| StimulusView {
                stimulus_id: id.to_string(),
                source: "unknown".to_string(),
                status: "classified".to_string(),
                workflow_id: None,
                confidence: None,
                routing_mode: None,
                rejection_reason: None,
                classification_error: None,
                received_at: *classified_at,
                last_event_at: *classified_at,
            });
            entry.status = "classified".to_string();
            entry.workflow_id = Some(workflow_id.clone());
            entry.confidence = Some(*confidence);
            entry.routing_mode = Some(routing_mode.clone());
            entry.last_event_at = *classified_at;
        }
        StimulusEvent::StimulusRejected {
            reason,
            rejected_at,
            ..
        } => {
            let entry = stimuli.entry(id).or_insert_with(|| StimulusView {
                stimulus_id: id.to_string(),
                source: "unknown".to_string(),
                status: "rejected".to_string(),
                workflow_id: None,
                confidence: None,
                routing_mode: None,
                rejection_reason: None,
                classification_error: None,
                received_at: *rejected_at,
                last_event_at: *rejected_at,
            });
            entry.status = "rejected".to_string();
            entry.rejection_reason = Some(reason.clone());
            entry.last_event_at = *rejected_at;
        }
        StimulusEvent::ClassificationFailed {
            error, failed_at, ..
        } => {
            let entry = stimuli.entry(id).or_insert_with(|| StimulusView {
                stimulus_id: id.to_string(),
                source: "unknown".to_string(),
                status: "classification_failed".to_string(),
                workflow_id: None,
                confidence: None,
                routing_mode: None,
                rejection_reason: None,
                classification_error: None,
                received_at: *failed_at,
                last_event_at: *failed_at,
            });
            entry.status = "classification_failed".to_string();
            entry.classification_error = Some(error.clone());
            entry.last_event_at = *failed_at;
        }
    }
}

fn security_incident_from_smcp(event: &SmcpEvent) -> Option<SecurityIncidentView> {
    match event {
        SmcpEvent::PolicyViolationBlocked {
            agent_id,
            execution_id,
            tool_name,
            violation_type,
            details,
            blocked_at,
        } => Some(SecurityIncidentView {
            category: "smcp_policy_violation_blocked".to_string(),
            severity: "high".to_string(),
            agent_id: Some(agent_id.0.to_string()),
            execution_id: Some(execution_id.0.to_string()),
            session_id: None,
            tool_name: Some(tool_name.clone()),
            details: format!("{violation_type:?}: {details}"),
            occurred_at: *blocked_at,
        }),
        SmcpEvent::SessionRevoked {
            session_id,
            agent_id,
            reason,
            revoked_at,
        } => Some(SecurityIncidentView {
            category: "smcp_session_revoked".to_string(),
            severity: "medium".to_string(),
            agent_id: Some(agent_id.0.to_string()),
            execution_id: None,
            session_id: Some(session_id.clone()),
            tool_name: None,
            details: reason.clone(),
            occurred_at: *revoked_at,
        }),
        _ => None,
    }
}

fn security_incident_from_policy(event: &PolicyEvent) -> Option<SecurityIncidentView> {
    match event {
        PolicyEvent::PolicyViolationAttempted {
            agent_id,
            violation_type,
            details,
            attempted_at,
        } => Some(SecurityIncidentView {
            category: "policy_violation_attempted".to_string(),
            severity: "medium".to_string(),
            agent_id: Some(agent_id.0.to_string()),
            execution_id: None,
            session_id: None,
            tool_name: Some(violation_type.clone()),
            details: details.clone(),
            occurred_at: *attempted_at,
        }),
        PolicyEvent::PolicyViolationBlocked {
            agent_id,
            violation_type,
            details,
            blocked_at,
        } => Some(SecurityIncidentView {
            category: "policy_violation_blocked".to_string(),
            severity: "high".to_string(),
            agent_id: Some(agent_id.0.to_string()),
            execution_id: None,
            session_id: None,
            tool_name: Some(violation_type.clone()),
            details: details.clone(),
            occurred_at: *blocked_at,
        }),
    }
}

fn push_bounded<T>(queue: &mut VecDeque<T>, item: T, capacity: usize) {
    queue.push_front(item);
    while queue.len() > capacity {
        queue.pop_back();
    }
}
