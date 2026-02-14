// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

// Event Bus Implementation - Pub/Sub for Domain Events
//
// Provides in-memory event streaming using tokio broadcast channels.
// Enables real-time event streaming to CLI, SSE endpoints, and observers.
//
// For MVP: In-memory only (events lost on restart)
// Phase 2: Add persistent event store for replay capability

use crate::domain::events::{AgentLifecycleEvent, ExecutionEvent, LearningEvent, ValidationEvent};
use aegis_cortex::domain::events::CortexEvent;
use aegis_cortex::application::EventBus as CortexEventBus;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Unified domain event type for the event bus
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    AgentLifecycle(AgentLifecycleEvent),
    Execution(ExecutionEvent),
    Learning(LearningEvent),
    Cortex(CortexEvent),
    Policy(crate::domain::events::PolicyEvent),
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

    /// Publish a learning event
    pub fn publish_learning_event(&self, event: LearningEvent) {
        self.publish(DomainEvent::Learning(event));
    }

    /// Publish a domain event to all subscribers
    fn publish(&self, event: DomainEvent) {
        debug!("Publishing event: {:?}", event);
        
        // Send to all subscribers
        // Note: send() returns the number of receivers that received the message
        let receiver_count = self.sender.send(event.clone()).unwrap_or(0);
        
        if receiver_count == 0 {
            debug!("No subscribers listening to event");
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
    pub fn subscribe_execution(&self, execution_id: crate::domain::execution::ExecutionId) -> ExecutionEventReceiver {
        let receiver = self.sender.subscribe();
        ExecutionEventReceiver {
            receiver,
            execution_id,
        }
    }

    /// Subscribe and filter for specific agent ID
    pub fn subscribe_agent(&self, agent_id: crate::domain::agent::AgentId) -> AgentEventReceiver {
        let receiver = self.sender.subscribe();
        AgentEventReceiver {
            receiver,
            agent_id,
        }
    }

    /// Get the number of active subscribers
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

#[async_trait::async_trait]
impl CortexEventBus for EventBus {
    async fn publish(&self, event: CortexEvent) -> anyhow::Result<()> {
        let _ = self.sender.send(DomainEvent::Cortex(event));
        Ok(())
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

impl ExecutionEventReceiver {
    /// Receive the next execution event for the specified execution ID
    /// Filters out events from other executions
    pub async fn recv(&mut self) -> Result<ExecutionEvent, EventBusError> {
        loop {
            let event = self.receiver.recv().await.map_err(|e| match e {
                broadcast::error::RecvError::Closed => EventBusError::Closed,
                broadcast::error::RecvError::Lagged(n) => {
                    warn!("Event receiver lagged by {} events", n);
                    EventBusError::Lagged(n)
                }
            })?;

            // Filter for execution events matching our ID
            match event {
                DomainEvent::Execution(exec_event) => {
                    if self.matches_execution(&exec_event) {
                        return Ok(exec_event);
                    }
                },
                DomainEvent::Cortex(cortex_event) => {
                    if self.matches_cortex(&cortex_event) {
                        return Ok(ExecutionEvent::Cortex(cortex_event));
                    }
                },
                _ => {}
            }
            // Continue loop if event doesn't match
        }
    }

    fn matches_execution(&self, event: &ExecutionEvent) -> bool {
        match event {
            ExecutionEvent::ExecutionStarted { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::IterationStarted { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::IterationCompleted { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::IterationFailed { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::RefinementApplied { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::ExecutionCompleted { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::ExecutionFailed { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::ExecutionCancelled { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::ConsoleOutput { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::LlmInteraction { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::InstanceSpawned { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::InstanceTerminated { execution_id, .. } => execution_id == &self.execution_id,
            ExecutionEvent::Validation(e) => match e {
                ValidationEvent::GradientValidationPerformed { execution_id, .. } => execution_id == &self.execution_id,
                ValidationEvent::MultiJudgeConsensus { execution_id, .. } => execution_id == &self.execution_id,
            },
            ExecutionEvent::Cortex(e) => self.matches_cortex(e),
        }
    }
    
    fn matches_cortex(&self, event: &CortexEvent) -> bool {
        match event {
            CortexEvent::PatternDiscovered { execution_id, .. } => *execution_id == Some(self.execution_id.0),
            CortexEvent::PatternWeightIncreased { execution_id, .. } => *execution_id == Some(self.execution_id.0),
            CortexEvent::PatternSuccessUpdated { execution_id, .. } => *execution_id == Some(self.execution_id.0),
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
                ExecutionEvent::Validation(_) => false, // TODO: Add agent_id to ValidationEvent for filtering
                ExecutionEvent::Cortex(_) => false,
            },
            DomainEvent::Learning(_) => false, // TODO: Link learning to agent
            DomainEvent::Cortex(_) => false,
            DomainEvent::Policy(_) => false, // TODO: Link policy to agent
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
                version: "1.1".to_string(),
                execution_targets: vec![],
                agent: crate::domain::agent::AgentIdentity {
                    name: "test".to_string(),
                    runtime: "python:3.11".to_string(),
                    autopull: true,
                    memory: false,
                    description: None,
                    version: None,
                    timeout_seconds: 300,
                },
                schedule: None,
                task: None,
                context: vec![],
                execution: None,
                permissions: None,
                tools: vec![],
                env: std::collections::HashMap::new(),
                advanced: None,
                metadata: None,
            },
            deployed_at: Utc::now(),
        };

        event_bus.publish_agent_event(event.clone());

        let received = receiver.recv().await.unwrap();
        match received {
            DomainEvent::AgentLifecycle(AgentLifecycleEvent::AgentDeployed { agent_id: id, .. }) => {
                assert_eq!(id, agent_id);
            }
            _ => panic!("Wrong event type received"),
        }
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
        match received {
            ExecutionEvent::ExecutionStarted { execution_id: id, .. } => {
                assert_eq!(id, execution_id);
            }
            _ => panic!("Wrong event type received"),
        }
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
}
