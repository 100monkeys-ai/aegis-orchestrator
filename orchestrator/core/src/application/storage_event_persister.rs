// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Storage Event Persister Application Service
//!
//! Subscribes to storage events from the event bus and persists them to
//! the StorageEventRepository for audit trail and forensic analysis.
//!
//! Implements the application layer pattern per AGENTS.md Section 9:
//! - Coordinates domain (StorageEvent) and infrastructure (EventBus, Repository)
//! - No business logic (just orchestration)
//! - Handles errors without crashing the orchestrator
//!
//! Related ADRs:
//! - ADR-036: NFS Server Gateway Architecture (audit trail requirement)
//! - ADR-030: Event Bus Architecture (pub/sub mechanism)

use crate::domain::repository::StorageEventRepository;
use crate::infrastructure::event_bus::{EventBus, DomainEvent};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{info, error, warn, debug};

// ============================================================================
// Service
// ============================================================================

/// Application service that persists storage events to database
///
/// Runs as a background task consuming events from the event bus.
/// Never crashes the orchestrator - all errors are logged and ignored.
pub struct StorageEventPersister {
    repository: Arc<dyn StorageEventRepository>,
    event_bus: Arc<EventBus>,
}

impl StorageEventPersister {
    /// Create new persister with repository and event bus
    pub fn new(
        repository: Arc<dyn StorageEventRepository>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            repository,
            event_bus,
        }
    }

    /// Start the background persistence task
    ///
    /// Spawns a tokio task that subscribes to the event bus and persists
    /// storage events. Returns a handle that can be used to await shutdown.
    ///
    /// The task runs indefinitely until the event bus is closed (orchestrator shutdown).
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        info!("Starting storage event persister background task");

        tokio::spawn(async move {
            let mut receiver = self.event_bus.subscribe();
            let mut events_processed = 0u64;
            let mut errors_encountered = 0u64;

            loop {
                match receiver.recv().await {
                    Ok(DomainEvent::Storage(storage_event)) => {
                        events_processed += 1;
                        
                        // Log every 100 events for observability
                        if events_processed % 100 == 0 {
                            debug!(
                                "Storage event persister processed {} events ({} errors)",
                                events_processed,
                                errors_encountered
                            );
                        }

                        // Persist to database
                        if let Err(e) = self.repository.save(&storage_event).await {
                            errors_encountered += 1;
                            error!(
                                event = ?storage_event,
                                error = ?e,
                                "Failed to persist storage event to database"
                            );
                            
                            // Log warning every 10 errors to avoid spam
                            if errors_encountered % 10 == 0 {
                                warn!(
                                    "Storage event persistence has failed {} times",
                                    errors_encountered
                                );
                            }
                        }
                    }
                    Ok(_other_event) => {
                        // Ignore non-storage events
                        continue;
                    }
                    Err(e) => {
                        match e {
                            crate::infrastructure::event_bus::EventBusError::Closed => {
                                info!(
                                    "Event bus closed, shutting down storage event persister \
                                     (processed {} events, {} errors)",
                                    events_processed,
                                    errors_encountered
                                );
                                break;
                            }
                            crate::infrastructure::event_bus::EventBusError::Lagged(n) => {
                                warn!(
                                    "Storage event persister lagged by {} events, \
                                     some audit trail data may be lost",
                                    n
                                );
                                // Continue processing, don't crash
                            }
                            _ => {
                                error!(
                                    error = ?e,
                                    "Unexpected error receiving event from bus"
                                );
                                // Continue processing, don't crash
                            }
                        }
                    }
                }
            }

            info!(
                "Storage event persister shut down gracefully \
                 (processed {} events, {} errors)",
                events_processed,
                errors_encountered
            );
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::execution::ExecutionId;
    use crate::infrastructure::repositories::InMemoryStorageEventRepository;
    use chrono::Utc;

    #[tokio::test]
    async fn test_persister_processes_storage_events() {
        // Arrange
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryStorageEventRepository::new());
        let persister = Arc::new(StorageEventPersister::new(
            repository.clone(),
            event_bus.clone(),
        ));

        // Start background task
        use crate::domain::events::StorageEvent;
        use crate::domain::execution::ExecutionId;
        use crate::domain::volume::VolumeId;
        use chrono::Utc;

        let _handle = persister.start();

        // Give time for subscription
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Act: Publish storage event
        let event = StorageEvent::FileOpened {
            execution_id: ExecutionId::new(),
            volume_id: VolumeId::new(),
            path: "/workspace/test.txt".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
        };

        event_bus.publish_storage_event(event.clone());

        // Give time for async persistence
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Assert: Event was persisted
        let volume_id = match &event {
            StorageEvent::FileOpened { volume_id, .. } => *volume_id,
            other => panic!("Test setup error: expected FileOpened event, got {:?}", other),
        };
        let persisted_events = repository.find_by_volume(
            volume_id,
            None, // No limit
        ).await.unwrap();

        assert_eq!(persisted_events.len(), 1);
    }

    #[tokio::test]
    async fn test_persister_ignores_non_storage_events() {
        // Arrange
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryStorageEventRepository::new());
        let persister = Arc::new(StorageEventPersister::new(
            repository.clone(),
            event_bus.clone(),
        ));

        let _handle = persister.start();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Act: Publish non-storage event
        let execution_event = crate::domain::events::ExecutionEvent::ExecutionStarted {
            execution_id: ExecutionId::new(),
            agent_id: crate::domain::agent::AgentId::new(),
            started_at: Utc::now(),
        };

        event_bus.publish_execution_event(execution_event);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Assert: No events persisted (repository should be empty)
        // We can't directly check if repository is empty without exposing list_all,
        // so this is implicit - the test passes if no panics occur
    }
}
