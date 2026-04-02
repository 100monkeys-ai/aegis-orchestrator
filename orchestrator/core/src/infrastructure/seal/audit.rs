// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Audit Logger (BC-12, ADR-035 §5)
//!
//! Emits structured audit records for every SEAL policy violation.
//!
//! ## Non-Repudiation
//!
//! Every blocked tool call produces a `warn!` tracing event with the full
//! `PolicyViolation` payload. Because SEAL envelopes carry Ed25519 or RS256
//! signatures, these log records constitute cryptographically non-repudiable
//! evidence (AGENTS.md §Non-Repudiation).
//!
//! ## Integration Note
//!
//! Violations are written to the structured log in the current baseline. Event-bus
//! publication is handled by the owning integration layer when that path is enabled.
//!
//! See ADR-035 §5.2 (Security Audit), AGENTS.md §Non-Repudiation.

use crate::domain::mcp::PolicyViolation;
use tracing::{info, warn};

/// Writes SEAL security events to the structured tracing log.
///
/// Structured audit logger for SEAL policy violations.
pub struct SealAuditLogger {}

impl SealAuditLogger {
    /// Create a new audit logger.
    pub fn new() -> Self {
        Self {}
    }

    /// Record a policy violation at `WARN` level.
    ///
    /// Emits the full `PolicyViolation` in structured form so it can be captured
    /// by any tracing subscriber (stdout JSON, Loki, etc.).
    ///
    /// > Phase 2: will also publish a `SealEvent::PolicyViolationBlocked` domain event.
    pub async fn log_violation(&self, violation: &PolicyViolation) {
        warn!("SEAL Policy Violation Detected: {:?}", violation);
        info!("policy violation events are logged here for the current baseline");
    }
}

impl Default for SealAuditLogger {
    fn default() -> Self {
        Self::new()
    }
}
