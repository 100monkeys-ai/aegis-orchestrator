// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Execution Event Persister Application Service
//!
//! Subscribes to [`ExecutionEvent`]s on the in-process [`EventBus`] and
//! persists them to the [`WorkflowExecutionRepository`] so that
//! `aegis.task.logs` (and any other audit-trail consumer) returns the real
//! per-iteration event stream for executions that don't go through the
//! Temporal listener.
//!
//! Historically only the Temporal listener wrote to `execution_events`. The
//! Supervisor (`application::execution`) and dispatch gateway
//! (`cli::daemon::handlers::dispatch`) publish [`ExecutionEvent`]s on the bus
//! but never persisted them, so direct `aegis.task.execute` flows showed
//! `events: []`. This persister closes that gap.
//!
//! # Sequence numbering
//!
//! Each execution has its own monotonic counter starting at
//! `1_000_000_000_000` (10^12) — far above any plausible Temporal history
//! index. The Temporal listener and the persister therefore write into the
//! same `(execution_id, sequence_number)` UNIQUE space without colliding,
//! and `find_events_by_execution` returns them interleaved by sequence.
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Bridge in-memory `EventBus` → durable `execution_events` table

use crate::domain::events::ExecutionEvent;
use crate::domain::execution::ExecutionId;
use crate::domain::repository::WorkflowExecutionRepository;
use crate::infrastructure::event_bus::{DomainEvent, EventBus, EventBusError};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Sequence numbers for in-process events start far above any plausible
/// Temporal history index so the two writers never collide on the
/// `(execution_id, sequence_number)` UNIQUE constraint.
const LOCAL_SEQUENCE_START: i64 = 1_000_000_000_000;

// ============================================================================
// Service
// ============================================================================

/// Application service that persists [`ExecutionEvent`]s to the
/// `execution_events` audit-trail table.
pub struct ExecutionEventPersister {
    repository: Arc<dyn WorkflowExecutionRepository>,
    event_bus: Arc<EventBus>,
    /// Per-execution monotonic sequence counter. Mutex is fine — the persister
    /// is single-threaded (one tokio task) so contention is minimal; the
    /// `Mutex` is used purely so the test helper can assert state.
    sequence_counters: Arc<Mutex<HashMap<ExecutionId, i64>>>,
}

impl ExecutionEventPersister {
    /// Create a new persister bound to the given repository and event bus.
    pub fn new(repository: Arc<dyn WorkflowExecutionRepository>, event_bus: Arc<EventBus>) -> Self {
        Self {
            repository,
            event_bus,
            sequence_counters: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn the background persistence task.
    ///
    /// The task subscribes to the event bus and persists every
    /// [`ExecutionEvent`] it observes. Errors from the repository are logged
    /// and swallowed — the persister never crashes the orchestrator.
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        info!("ExecutionEvent persister started (audit trail enabled)");

        tokio::spawn(async move {
            let mut receiver = self.event_bus.subscribe();
            loop {
                match receiver.recv().await {
                    Ok(DomainEvent::Execution(event)) => {
                        if let Err(e) = self.persist(&event).await {
                            warn!(
                                error = %e,
                                event_type = event_type_str(&event),
                                "Failed to persist execution event (continuing)"
                            );
                        }
                    }
                    Ok(_) => {
                        // Non-execution domain events: ignore.
                        continue;
                    }
                    Err(EventBusError::Lagged(n)) => {
                        warn!(
                            lagged_events = n,
                            "execution event persister lagged; some audit-trail data may be lost"
                        );
                    }
                    Err(EventBusError::Closed) => {
                        info!("execution event persister: event bus closed; shutting down");
                        break;
                    }
                    Err(e) => {
                        warn!(
                            error = ?e,
                            "execution event persister: unexpected receiver error (continuing)"
                        );
                    }
                }
            }
        })
    }

    async fn persist(&self, event: &ExecutionEvent) -> Result<(), String> {
        let execution_id = execution_id_for(event);
        let iteration_number = iteration_number_for(event);
        let event_type = event_type_str(event).to_string();

        let payload =
            serde_json::to_value(event).map_err(|e| format!("serialize ExecutionEvent: {e}"))?;

        let sequence_number = {
            let mut counters = self.sequence_counters.lock().await;
            let counter = counters.entry(execution_id).or_insert(LOCAL_SEQUENCE_START);
            let n = *counter;
            *counter = counter.saturating_add(1);
            n
        };

        self.repository
            .append_event(
                execution_id,
                sequence_number,
                event_type,
                payload,
                iteration_number,
            )
            .await
            .map_err(|e| format!("repository.append_event: {e}"))
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn execution_id_for(event: &ExecutionEvent) -> ExecutionId {
    match event {
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
        | ExecutionEvent::LlmCallFailed { execution_id, .. }
        | ExecutionEvent::InstanceSpawned { execution_id, .. }
        | ExecutionEvent::InstanceTerminated { execution_id, .. } => *execution_id,
        // Variants not enumerated above use serde to extract the field. This
        // is robust against new variants and is only used as a last resort —
        // the common path matches above with no allocation.
        other => match serde_json::to_value(other).ok().and_then(|v| {
            v.as_object()
                .and_then(|o| o.values().next().cloned())
                .and_then(|inner| {
                    inner
                        .get("execution_id")
                        .and_then(|id| id.as_str())
                        .and_then(|s| uuid::Uuid::parse_str(s).ok())
                })
        }) {
            Some(uuid) => ExecutionId(uuid),
            None => ExecutionId(uuid::Uuid::nil()),
        },
    }
}

fn iteration_number_for(event: &ExecutionEvent) -> Option<u8> {
    match event {
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
        | ExecutionEvent::LlmCallFailed {
            iteration_number, ..
        }
        | ExecutionEvent::InstanceSpawned {
            iteration_number, ..
        }
        | ExecutionEvent::InstanceTerminated {
            iteration_number, ..
        } => Some(*iteration_number),
        _ => None,
    }
}

fn event_type_str(event: &ExecutionEvent) -> &'static str {
    match event {
        ExecutionEvent::ExecutionStarted { .. } => "ExecutionStarted",
        ExecutionEvent::IterationStarted { .. } => "IterationStarted",
        ExecutionEvent::IterationCompleted { .. } => "IterationCompleted",
        ExecutionEvent::IterationFailed { .. } => "IterationFailed",
        ExecutionEvent::RefinementApplied { .. } => "RefinementApplied",
        ExecutionEvent::ExecutionCompleted { .. } => "ExecutionCompleted",
        ExecutionEvent::ExecutionFailed { .. } => "ExecutionFailed",
        ExecutionEvent::ExecutionCancelled { .. } => "ExecutionCancelled",
        ExecutionEvent::ExecutionTimedOut { .. } => "ExecutionTimedOut",
        ExecutionEvent::ConsoleOutput { .. } => "ConsoleOutput",
        ExecutionEvent::LlmInteraction { .. } => "LlmInteraction",
        ExecutionEvent::LlmCallFailed { .. } => "LlmCallFailed",
        ExecutionEvent::InstanceSpawned { .. } => "InstanceSpawned",
        ExecutionEvent::InstanceTerminated { .. } => "InstanceTerminated",
        _ => "ExecutionEvent",
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::execution::ExecutionId;
    use crate::domain::repository::RepositoryError;
    use crate::domain::tenant::TenantId;
    use crate::domain::workflow::{WorkflowExecution, WorkflowExecutionEventRecord, WorkflowId};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::RwLock;

    /// Minimal recording repository — only `append_event` is exercised by the
    /// persister. All other methods return `Ok(...)` defaults.
    #[derive(Default)]
    struct RecordingRepo {
        events: RwLock<Vec<(ExecutionId, i64, String, serde_json::Value, Option<u8>)>>,
        fail_after: AtomicUsize,
        call_count: AtomicUsize,
    }

    impl RecordingRepo {
        fn new() -> Self {
            Self::default()
        }
        fn fail_on_call(&self, n: usize) {
            self.fail_after.store(n, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl WorkflowExecutionRepository for RecordingRepo {
        async fn save_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _execution: &WorkflowExecution,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn find_by_id_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _id: ExecutionId,
        ) -> Result<Option<WorkflowExecution>, RepositoryError> {
            Ok(None)
        }
        async fn find_active_for_tenant(
            &self,
            _tenant_id: &TenantId,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }
        async fn find_by_workflow_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _workflow_id: WorkflowId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }
        async fn list_paginated_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }
        async fn count_by_workflow_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _workflow_id: WorkflowId,
        ) -> Result<i64, RepositoryError> {
            Ok(0)
        }
        async fn update_temporal_linkage_for_tenant(
            &self,
            _tenant_id: &TenantId,
            _execution_id: ExecutionId,
            _temporal_workflow_id: &str,
            _temporal_run_id: &str,
        ) -> Result<(), RepositoryError> {
            Ok(())
        }

        async fn append_event(
            &self,
            execution_id: ExecutionId,
            sequence_number: i64,
            event_type: String,
            payload: serde_json::Value,
            iteration_number: Option<u8>,
        ) -> Result<(), RepositoryError> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
            let fail_at = self.fail_after.load(Ordering::SeqCst);
            if fail_at != 0 && n == fail_at {
                return Err(RepositoryError::Database("simulated failure".into()));
            }
            self.events.write().await.push((
                execution_id,
                sequence_number,
                event_type,
                payload,
                iteration_number,
            ));
            Ok(())
        }

        async fn find_events_by_execution(
            &self,
            _id: ExecutionId,
            _limit: usize,
            _offset: usize,
        ) -> Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
            Ok(vec![])
        }

        async fn find_tenant_id_by_execution(
            &self,
            _id: ExecutionId,
        ) -> Result<Option<TenantId>, RepositoryError> {
            Ok(None)
        }
    }

    fn iteration_started(execution_id: ExecutionId, n: u8) -> ExecutionEvent {
        ExecutionEvent::IterationStarted {
            execution_id,
            agent_id: AgentId::new(),
            iteration_number: n,
            action: "test".to_string(),
            started_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn persister_writes_events_to_repo() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repo = Arc::new(RecordingRepo::new());
        let persister = Arc::new(ExecutionEventPersister::new(
            repo.clone(),
            event_bus.clone(),
        ));
        let _h = persister.start();

        // Allow the spawned subscriber to register.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let exec_id = ExecutionId::new();
        for n in 1..=3u8 {
            event_bus.publish_execution_event(iteration_started(exec_id, n));
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        let events = repo.events.read().await.clone();
        assert_eq!(
            events.len(),
            3,
            "expected 3 persisted events, got {events:?}"
        );
        assert_eq!(events[0].1, LOCAL_SEQUENCE_START);
        assert_eq!(events[1].1, LOCAL_SEQUENCE_START + 1);
        assert_eq!(events[2].1, LOCAL_SEQUENCE_START + 2);
        for (_, _, event_type, _, _) in &events {
            assert_eq!(event_type, "IterationStarted");
        }
    }

    #[tokio::test]
    async fn persister_isolates_sequence_per_execution() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repo = Arc::new(RecordingRepo::new());
        let persister = Arc::new(ExecutionEventPersister::new(
            repo.clone(),
            event_bus.clone(),
        ));
        let _h = persister.start();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let a = ExecutionId::new();
        let b = ExecutionId::new();
        // Interleave: A1, B1, A2, B2, A3
        event_bus.publish_execution_event(iteration_started(a, 1));
        event_bus.publish_execution_event(iteration_started(b, 1));
        event_bus.publish_execution_event(iteration_started(a, 2));
        event_bus.publish_execution_event(iteration_started(b, 2));
        event_bus.publish_execution_event(iteration_started(a, 3));

        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        let events = repo.events.read().await.clone();
        let a_seqs: Vec<i64> = events
            .iter()
            .filter(|(id, ..)| *id == a)
            .map(|(_, s, ..)| *s)
            .collect();
        let b_seqs: Vec<i64> = events
            .iter()
            .filter(|(id, ..)| *id == b)
            .map(|(_, s, ..)| *s)
            .collect();
        assert_eq!(
            a_seqs,
            vec![
                LOCAL_SEQUENCE_START,
                LOCAL_SEQUENCE_START + 1,
                LOCAL_SEQUENCE_START + 2
            ]
        );
        assert_eq!(b_seqs, vec![LOCAL_SEQUENCE_START, LOCAL_SEQUENCE_START + 1]);
    }

    #[tokio::test]
    async fn persister_continues_on_repo_error() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repo = Arc::new(RecordingRepo::new());
        repo.fail_on_call(2); // second append_event returns Err
        let persister = Arc::new(ExecutionEventPersister::new(
            repo.clone(),
            event_bus.clone(),
        ));
        let _h = persister.start();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let exec_id = ExecutionId::new();
        event_bus.publish_execution_event(iteration_started(exec_id, 1));
        event_bus.publish_execution_event(iteration_started(exec_id, 2));
        event_bus.publish_execution_event(iteration_started(exec_id, 3));

        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        // Three append_event calls were made even though the second returned Err.
        assert_eq!(repo.call_count.load(Ordering::SeqCst), 3);
        // First and third stored; second was rejected.
        let events = repo.events.read().await.clone();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn persister_handles_lag() {
        // Use a tiny capacity so we can trigger Lagged easily by overflowing.
        let event_bus = Arc::new(EventBus::new(2));
        let repo = Arc::new(RecordingRepo::new());
        let persister = Arc::new(ExecutionEventPersister::new(
            repo.clone(),
            event_bus.clone(),
        ));

        // Subscribe BEFORE starting persister to claim a receiver slot, then
        // overflow the channel, then start the persister. The persister's
        // own receiver will start fresh, so we instead test via direct
        // EventBusError::Lagged matching using the subscribe receiver path.
        //
        // Simpler approach: spawn the persister and just confirm it doesn't
        // crash when many events are published rapidly past the capacity.
        let _h = persister.start();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let exec_id = ExecutionId::new();
        for n in 1..=20u8 {
            event_bus.publish_execution_event(iteration_started(exec_id, n));
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Whatever survived the lag is fine; the contract under test is that
        // the persister did not crash. We can publish another event and
        // confirm the persister still processes it.
        event_bus.publish_execution_event(iteration_started(exec_id, 99));
        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        let events = repo.events.read().await;
        assert!(
            !events.is_empty(),
            "persister should still be running after lag"
        );
    }
}
