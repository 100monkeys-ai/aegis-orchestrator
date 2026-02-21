// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use std::sync::Arc;
use tracing::{warn, info};
use crate::infrastructure::event_bus::EventBus;
use crate::domain::mcp::PolicyViolation;

pub struct SmcpAuditLogger {
    event_bus: Arc<EventBus>,
}

impl SmcpAuditLogger {
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self { event_bus }
    }
    
    pub async fn log_violation(&self, violation: &PolicyViolation) {
        warn!("SMCP Policy Violation Detected: {:?}", violation);
        // We could also publish a DomainEvent to the EventBus here.
        info!("In a full implementation, SMCP policy violation events are forwarded to the event bus for auditing");
    }
}
