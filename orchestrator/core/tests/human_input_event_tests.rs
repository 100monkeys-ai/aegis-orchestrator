// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Human gate event integration tests
//!
//! A Human state of the Temporal worker emits `HumanInputRequested` with the
//! gate's rendered prompt and its default response. The listener persists it
//! among the run's events, where the run's event list reads it, and publishes
//! it to the event bus.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure (integration)
//! - **Purpose:** The worker's gate event reaches the run's persisted events

use aegis_orchestrator_core::domain::events::WorkflowEvent;
use aegis_orchestrator_core::domain::execution::ExecutionId;
use aegis_orchestrator_core::domain::repository::{RepositoryError, WorkflowExecutionRepository};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    StateKind, StateName, TransitionCondition, TransitionRule, Workflow, WorkflowExecution,
    WorkflowExecutionEventRecord, WorkflowId, WorkflowMetadata, WorkflowSpec, WorkflowState,
};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::repositories::InMemoryWorkflowExecutionRepository;
use aegis_orchestrator_core::infrastructure::{TemporalEventListener, TemporalEventPayload};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The in-memory execution repository, with the run's event list kept as the
/// Postgres store keeps it: one record per appended event, in sequence order.
struct EventListRepo {
    inner: InMemoryWorkflowExecutionRepository,
    events: Mutex<Vec<(ExecutionId, WorkflowExecutionEventRecord)>>,
}

impl EventListRepo {
    fn new() -> Self {
        Self {
            inner: InMemoryWorkflowExecutionRepository::new(),
            events: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl WorkflowExecutionRepository for EventListRepo {
    async fn save_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution: &WorkflowExecution,
    ) -> Result<(), RepositoryError> {
        self.inner.save_for_tenant(tenant_id, execution).await
    }

    async fn find_by_id_for_tenant(
        &self,
        tenant_id: &TenantId,
        id: ExecutionId,
    ) -> Result<Option<WorkflowExecution>, RepositoryError> {
        self.inner.find_by_id_for_tenant(tenant_id, id).await
    }

    async fn find_active_for_tenant(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.inner.find_active_for_tenant(tenant_id).await
    }

    async fn find_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: WorkflowId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.inner
            .find_by_workflow_for_tenant(tenant_id, workflow_id, limit, offset)
            .await
    }

    async fn list_paginated_for_tenant(
        &self,
        tenant_id: &TenantId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.inner
            .list_paginated_for_tenant(tenant_id, limit, offset)
            .await
    }

    async fn list_paginated_all(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
        self.inner.list_paginated_all(limit, offset).await
    }

    async fn count_by_workflow_for_tenant(
        &self,
        tenant_id: &TenantId,
        workflow_id: WorkflowId,
    ) -> Result<i64, RepositoryError> {
        self.inner
            .count_by_workflow_for_tenant(tenant_id, workflow_id)
            .await
    }

    async fn update_temporal_linkage_for_tenant(
        &self,
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        temporal_workflow_id: &str,
        temporal_run_id: &str,
    ) -> Result<(), RepositoryError> {
        self.inner
            .update_temporal_linkage_for_tenant(
                tenant_id,
                execution_id,
                temporal_workflow_id,
                temporal_run_id,
            )
            .await
    }

    async fn append_event(
        &self,
        execution_id: ExecutionId,
        sequence_number: i64,
        event_type: String,
        payload: serde_json::Value,
        iteration_number: Option<u8>,
    ) -> Result<(), RepositoryError> {
        let mut events = self.events.lock().unwrap();
        if events
            .iter()
            .any(|(id, record)| *id == execution_id && record.sequence == sequence_number)
        {
            return Ok(());
        }
        events.push((
            execution_id,
            WorkflowExecutionEventRecord {
                sequence: sequence_number,
                event_type,
                state_name: None,
                iteration_number,
                payload,
                recorded_at: chrono::Utc::now(),
            },
        ));
        Ok(())
    }

    async fn find_events_by_execution(
        &self,
        id: ExecutionId,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
        let mut records: Vec<_> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(execution_id, _)| *execution_id == id)
            .map(|(_, record)| record.clone())
            .collect();
        records.sort_by_key(|record| record.sequence);
        Ok(records.into_iter().skip(offset).take(limit).collect())
    }

    async fn find_tenant_id_by_execution(
        &self,
        id: ExecutionId,
    ) -> Result<Option<TenantId>, RepositoryError> {
        self.inner.find_tenant_id_by_execution(id).await
    }
}

fn gated_workflow() -> Workflow {
    let mut states = HashMap::new();
    states.insert(
        StateName::new("AWAIT_APPROVAL").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo gate".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![TransitionRule {
                condition: TransitionCondition::Always,
                target: StateName::new("END").unwrap(),
                feedback: None,
            }],
            timeout: None,
            max_state_visits: None,
        },
    );
    states.insert(
        StateName::new("END").unwrap(),
        WorkflowState {
            kind: StateKind::System {
                command: "echo end".to_string(),
                env: HashMap::new(),
                workdir: None,
            },
            transitions: vec![],
            timeout: None,
            max_state_visits: None,
        },
    );
    Workflow::new(
        WorkflowMetadata {
            name: "human-gate-events".to_string(),
            version: Some("1.0.0".to_string()),
            description: None,
            labels: HashMap::new(),
            annotations: HashMap::new(),
            input_schema: None,
            output_schema: None,
            output_template: None,
        },
        WorkflowSpec {
            initial_state: StateName::new("AWAIT_APPROVAL").unwrap(),
            context: HashMap::new(),
            states,
            storage: Default::default(),
            max_total_transitions: None,
            default_schedule: None,
            repositories: None,
        },
    )
    .unwrap()
}

/// One event as the worker's `emit` posts it to `/v1/temporal-events`.
fn worker_payload(
    execution_id: ExecutionId,
    workflow: &Workflow,
    sequence: i64,
    event_type: &str,
    extra: serde_json::Value,
) -> TemporalEventPayload {
    let mut body = json!({
        "event_type": event_type,
        "execution_id": execution_id.to_string(),
        "temporal_sequence_number": sequence,
        "workflow_id": workflow.id.to_string(),
        "timestamp": "2026-10-12T03:38:40.089Z",
    });
    for (key, value) in extra.as_object().expect("extra fields are an object") {
        body[key] = value.clone();
    }
    serde_json::from_value(body).expect("the worker's payload deserializes")
}

/// The run of 2026-10-12 failed at its first gate: the listener refused
/// HumanInputRequested as an unknown event type. The gate's event is accepted,
/// stands in the run's event list with its prompt and default response, and
/// is published for the run's watchers.
#[tokio::test]
async fn a_human_gate_event_stands_in_the_runs_event_list() {
    let tenant_id = TenantId::from_string("tenant-gate").unwrap();
    let workflow = gated_workflow();
    let execution_id = ExecutionId::new();
    let execution = WorkflowExecution::new(&workflow, execution_id, json!({"task": "add"}));
    let repo = Arc::new(EventListRepo::new());
    repo.save_for_tenant(&tenant_id, &execution).await.unwrap();

    let event_bus = Arc::new(EventBus::new(16));
    let mut receiver = event_bus.subscribe();
    let listener = TemporalEventListener::new(event_bus.clone(), repo.clone());

    listener
        .handle_event(worker_payload(
            execution_id,
            &workflow,
            6,
            "WorkflowStateEntered",
            json!({ "state_name": "AWAIT_APPROVAL" }),
        ))
        .await
        .expect("the state entered is accepted");
    listener
        .handle_event(worker_payload(
            execution_id,
            &workflow,
            7,
            "HumanInputRequested",
            json!({
                "prompt": "Approve the requirements for the add command?",
                "default_response": "reject",
            }),
        ))
        .await
        .expect("the gate's event is accepted");

    let events = repo
        .find_events_by_execution(execution_id, 100, 0)
        .await
        .unwrap();
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, vec!["WorkflowStateEntered", "HumanInputRequested"]);
    let gate = &events[1];
    assert_eq!(gate.sequence, 7);
    assert_eq!(
        gate.payload["prompt"],
        json!("Approve the requirements for the add command?")
    );
    assert_eq!(gate.payload["default_response"], json!("reject"));

    let mut published = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        published.push(event);
    }
    assert!(
        published.iter().any(|event| matches!(
            event,
            DomainEvent::Workflow(WorkflowEvent::WorkflowHumanInputRequested {
                execution_id: id,
                prompt,
                default_response,
                ..
            }) if *id == execution_id
                && prompt == "Approve the requirements for the add command?"
                && default_response.as_deref() == Some("reject")
        )),
        "the gate's event was not published: {published:?}"
    );
}
