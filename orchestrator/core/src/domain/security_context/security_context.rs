// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SecurityContext Aggregate (BC-4/BC-12, ADR-035)
//!
//! Defines the named permission boundary used by the SMCP protocol layer.
//! A `SecurityContext` is the **protocol-level** policy object — it controls which
//! MCP tools an agent may invoke, with what arguments, from which paths/domains,
//! and at what rate.
//!
//! This is distinct from the *infrastructure-level* `SecurityPolicy` in
//! [`crate::domain::policy`], which controls container networking, filesystem mounts,
//! and resource limits enforced by the OS.
//!
//! ## Evaluation Algorithm
//!
//! [`SecurityContext::evaluate`] applies rules in this exact order:
//! 1. **Deny list** — explicit denies always win
//! 2. **Capability scan** — find the first matching `Capability` that permits the call
//! 3. **Default deny** — no matching capability means the call is rejected
//!
//! ## Named Contexts
//!
//! Contexts are identified by name (e.g. `"research-safe"`, `"coder-unrestricted"`,
//! `"zaru-free"`). The agent manifest declares the context name; the orchestrator
//! looks it up via [`crate::domain::security_context::SecurityContextRepository`]
//! during attestation.
//!
//! See ADR-035 (SMCP Implementation), AGENTS.md §SMCP Protocol Domain.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::mcp::PolicyViolation;
use super::capability::Capability;

/// Audit metadata for a [`SecurityContext`] record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityContextMetadata {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Monotonically incrementing version number. Incremented on every `update_agent` call.
    pub version: u32,
}

/// Named permission boundary for agent MCP tool access (BC-12 SMCP, ADR-035).
///
/// **Aggregate root** for the Security Context bounded context. Owned by the
/// orchestrator; referenced in `SmcpSession` by value (cloned at attestation time
/// so per-execution policy snapshots are immune to runtime context updates).
///
/// # Invariants
///
/// - `name` is unique across all contexts in the registry.
/// - `deny_list` entries take precedence over any matching `capabilities` entry.
/// - An empty `capabilities` vec means **no tools are allowed** (most restrictive).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityContext {
    /// Unique name identifying this context (e.g. `"research-safe"`, `"zaru-free"`).
    pub name: String,
    /// Human-readable description shown in the Control Plane UI.
    pub description: String,
    /// Permitted tool capabilities. Evaluated in order; first match wins.
    pub capabilities: Vec<Capability>,
    /// Tool names explicitly denied regardless of any matching `capabilities` entry.
    pub deny_list: Vec<String>,
    /// Audit metadata (timestamps, version).
    pub metadata: SecurityContextMetadata,
}

impl SecurityContext {
    /// Evaluate whether a tool call is permitted by this `SecurityContext`.
    ///
    /// Applies the three-step policy algorithm:
    /// 1. Deny list check (explicit denies win over any capability)
    /// 2. Linear capability scan (first accepting capability returns `Ok(())`)
    /// 3. Default-deny (no capability matched → `ToolNotAllowed`)
    ///
    /// # Errors
    ///
    /// Returns a [`PolicyViolation`] describing *why* the call was denied:
    /// - `ToolExplicitlyDenied` — `tool_name` is in `deny_list`
    /// - `ToolNotAllowed` — no capability matches `tool_name`
    /// - `PathOutsideBoundary` / `DomainNotAllowed` — capability constraint violation
    ///
    /// # Security
    ///
    /// This method is called by [`crate::domain::smcp_session::SmcpSession::evaluate_call`]
    /// on **every** MCP tool invocation. It must be O(n) in capabilities and
    /// must **not** panic or silently permit on unexpected input.
    pub fn evaluate(&self, tool_name: &str, args: &Value) -> Result<(), PolicyViolation> {
        // 1. Check deny list first (explicit denies take precedence)
        if self.deny_list.contains(&tool_name.to_string()) {
            return Err(PolicyViolation::ToolExplicitlyDenied {
                tool_name: tool_name.to_string(),
            });
        }
        
        // 2. Check if any capability allows this call
        for capability in &self.capabilities {
            match capability.allows(tool_name, args) {
                Ok(()) => return Ok(()),
                Err(_) => continue, // Try next capability
            }
        }
        
        // 3. No capability matched — deny by default
        Err(PolicyViolation::ToolNotAllowed {
            tool_name: tool_name.to_string(),
            allowed_tools: self.capabilities.iter().map(|c| c.tool_pattern.clone()).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_metadata() -> SecurityContextMetadata {
        SecurityContextMetadata {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            version: 1,
        }
    }

    #[test]
    fn test_security_context_evaluate_allowlist() {
        let ctx = SecurityContext {
            name: "test-ctx".to_string(),
            description: "Testing context".to_string(),
            capabilities: vec![
                Capability {
                    tool_pattern: "fs.read".to_string(),
                    path_allowlist: Some(vec!["/workspace".into()]),
                    command_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                }
            ],
            deny_list: vec![],
            metadata: test_metadata(),
        };

        // Allowed
        assert!(ctx.evaluate("fs.read", &json!({"path": "/workspace/test.txt"})).is_ok());

        // Denied by capability limits (path)
        assert!(ctx.evaluate("fs.read", &json!({"path": "/etc/passwd"})).is_err());

        // Denied implicitly (not in capabilities)
        assert!(ctx.evaluate("fs.write", &json!({"path": "/workspace/test.txt"})).is_err());
    }

    #[test]
    fn test_security_context_evaluate_denylist_override() {
        let ctx = SecurityContext {
            name: "test-ctx".to_string(),
            description: "Testing context".to_string(),
            capabilities: vec![
                Capability {
                    tool_pattern: "fs.*".to_string(),
                    path_allowlist: Some(vec!["/workspace".into()]),
                    command_allowlist: None,
                    domain_allowlist: None,
                    rate_limit: None,
                    max_response_size: None,
                }
            ],
            // Although fs.* is allowed, fs.delete is explicitly denied
            deny_list: vec!["fs.delete".to_string()],
            metadata: test_metadata(),
        };

        // Allowed due to wildcard
        assert!(ctx.evaluate("fs.read", &json!({"path": "/workspace/test.txt"})).is_ok());

        // Denied due to explicit deny list
        assert!(matches!(
            ctx.evaluate("fs.delete", &json!({"path": "/workspace/test.txt"})),
            Err(PolicyViolation::ToolExplicitlyDenied { .. })
        ));
    }
}
