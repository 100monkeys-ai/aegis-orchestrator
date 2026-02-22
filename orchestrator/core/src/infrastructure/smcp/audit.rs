// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Audit Logger (BC-12, ADR-035 §5)
//!
//! Emits structured audit records for every SMCP policy violation.
//!
//! ## Non-Repudiation
//!
//! Every blocked tool call produces a `warn!` tracing event with the full
//! `PolicyViolation` payload. Because SMCP envelopes carry Ed25519 or RS256
//! signatures, these log records constitute cryptographically non-repudiable
//! evidence (AGENTS.md §Non-Repudiation).
//!
//! ## Phase Note
//!
//! ⚠️ Phase 1 — Violations are written only to the structured log. Phase 2 will
//! publish them as [`crate::domain::events::SmcpEvent::PolicyViolationBlocked`]
//! domain events to the [`crate::infrastructure::event_bus::EventBus`], enabling
//! Cortex pattern learning and SOC 2 audit trail export.
//!
//! See ADR-035 §5.2 (Security Audit), AGENTS.md §Non-Repudiation.

use crate::domain::mcp::PolicyViolation;
use tracing::{info, warn};

/// Writes SMCP security events to the structured tracing log.
///
/// ⚠️ Phase 1 stub — Phase 2 will inject an `Arc<EventBus>` here so violations
/// are also published as domain events.
pub struct SmcpAuditLogger {}

impl SmcpAuditLogger {
    /// Create a new audit logger (stateless in Phase 1).
    pub fn new() -> Self {
        Self {}
    }

    /// Record a policy violation at `WARN` level.
    ///
    /// Emits the full `PolicyViolation` in structured form so it can be captured
    /// by any tracing subscriber (stdout JSON, Loki, etc.).
    ///
    /// > Phase 2: will also publish a `SmcpEvent::PolicyViolationBlocked` domain event.
    pub async fn log_violation(&self, violation: &PolicyViolation) {
        warn!("SMCP Policy Violation Detected: {:?}", violation);
        // Phase 2: publish to EventBus for Cortex pattern learning.
        info!("[Phase 2] policy violation events will be forwarded to the event bus for auditing");
    }
}
