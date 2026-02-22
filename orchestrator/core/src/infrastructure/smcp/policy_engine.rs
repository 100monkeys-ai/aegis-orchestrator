// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Policy Engine (ADR-035 §4.4)
//!
//! Thin application-layer shim that delegates all policy decisions to
//! [`crate::domain::security_context::SecurityContext::evaluate`].
//!
//! The `SecurityContext` contains the authoritative Cedar-style policy rules
//! (allow/deny capabilities with path/domain/rate constraints). The `PolicyEngine`
//! exists as a named infrastructure component to:
//! 1. Provide an explicit extension point for future Cedar integration
//! 2. Maintain the DDD layering boundary (infrastructure calls domain)
//! 3. Enable independent unit-testing of the evaluation pipeline

use crate::domain::mcp::PolicyViolation;
use crate::domain::security_context::SecurityContext;
use serde_json::Value;

/// Evaluates SMCP tool call requests against a `SecurityContext`'s capability rules.
///
/// All policy decisions are fully delegated to the domain: this struct does not
/// contain any policy logic itself. See [`SecurityContext::evaluate`] for the
/// enforcement algorithm (deny-list check → capability scan → default-deny).
pub struct PolicyEngine;

impl PolicyEngine {
    /// Create a new policy engine instance.
    pub fn new() -> Self {
        Self
    }

    /// Evaluate whether `tool_name` with `args` is allowed by `security_context`.
    ///
    /// # Errors
    ///
    /// Returns a [`PolicyViolation`] if the call is denied:
    /// - `ToolExplicitlyDenied` — tool is in the `deny_list`
    /// - `ToolNotAllowed` — no capability matches the tool name
    /// - `PathOutsideBoundary`, `DomainNotAllowed` etc. — capability constraint violation
    pub fn evaluate(
        &self,
        security_context: &SecurityContext,
        tool_name: &str,
        args: &Value,
    ) -> Result<(), PolicyViolation> {
        security_context.evaluate(tool_name, args)
    }
}
