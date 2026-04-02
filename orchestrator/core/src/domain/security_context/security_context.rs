// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SecurityContext Aggregate (BC-4/BC-12, ADR-035)
//!
//! Defines the named permission boundary used by the SEAL protocol layer.
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
//! See ADR-035 (SEAL Implementation), AGENTS.md §SEAL Protocol Domain.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

use super::capability::Capability;
use crate::domain::tenant::TenantId;

/// Describes why a tool invocation was rejected by security policy evaluation.
///
/// Part of the Security Context bounded context (BC-4). Returned by
/// [`SecurityContext::evaluate`] and [`Capability::allows`] when a tool call
/// violates the configured security constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyViolation {
    ToolNotAllowed {
        tool_name: String,
        allowed_tools: Vec<String>,
    },
    ToolExplicitlyDenied {
        tool_name: String,
    },
    RateLimitExceeded {
        resource_type: String,
        bucket: String,
        limit: u64,
        current: u64,
        retry_after_seconds: u64,
    },
    PathOutsideBoundary {
        path: PathBuf,
        allowed_paths: Vec<PathBuf>,
    },
    PathTraversalAttempt {
        path: PathBuf,
    },
    DomainNotAllowed {
        domain: String,
        allowed_domains: Vec<String>,
    },
    MissingRequiredArgument(String),
    TimeoutExceeded {
        tool_name: String,
        max_duration: Duration,
    },
}

/// Audit metadata for a [`SecurityContext`] record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityContextMetadata {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Monotonically incrementing version number. Incremented on every `update_agent` call.
    pub version: u32,
}

/// Named permission boundary for agent MCP tool access (BC-12 SEAL, ADR-035).
///
/// **Aggregate root** for the Security Context bounded context. Owned by the
/// orchestrator; referenced in `SealSession` by value (cloned at attestation time
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
    /// Human-readable description shown in the Zaru client admin console.
    pub description: String,
    /// Permitted tool capabilities. Evaluated in order; first match wins.
    pub capabilities: Vec<Capability>,
    /// Tool names explicitly denied regardless of any matching `capabilities` entry.
    pub deny_list: Vec<String>,
    /// Audit metadata (timestamps, version).
    pub metadata: SecurityContextMetadata,
}

impl SecurityContext {
    /// Determine whether this context may invoke a tool by name, ignoring runtime-only
    /// constraints such as path, command, or URL arguments.
    pub fn permits_tool_name(&self, tool_name: &str) -> bool {
        if self.deny_list.contains(&tool_name.to_string()) {
            return false;
        }

        self.capabilities
            .iter()
            .any(|capability| capability.matches_tool_name(tool_name))
    }

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
    /// This method is called by [`crate::domain::seal_session::SealSession::evaluate_call`]
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
            allowed_tools: self
                .capabilities
                .iter()
                .map(|c| c.tool_pattern.clone())
                .collect(),
        })
    }

    /// Validate that the given tenant is allowed to use this SecurityContext (ADR-056).
    ///
    /// SecurityContext names must follow tenant-namespaced conventions:
    /// - `zaru-*` — only for consumer realm principals (`zaru-consumer` tenant)
    /// - `tenant-{slug}-*` — only for the matching enterprise tenant
    /// - `aegis-system-*` — only for system realm principals
    /// - Legacy bare names (no recognised prefix) are rejected outright.
    ///
    /// # Errors
    ///
    /// Returns a human-readable error string describing the ownership violation.
    pub fn validate_tenant_ownership(&self, tenant_id: &TenantId) -> Result<(), String> {
        validate_context_ownership(&self.name, tenant_id)
    }
}

/// Validate that a SecurityContext name is owned by the given tenant (ADR-056).
///
/// This is a free function so it can be called before the `SecurityContext` is
/// loaded from the repository (e.g. to fail fast on obviously-wrong names).
///
/// # Rules
///
/// 1. `zaru-*` → requires `tenant_id.is_consumer()`
/// 2. `tenant-{slug}-*` → requires context name starts with `tenant-{tenant_id}-`
/// 3. `aegis-system-*` → requires `tenant_id.is_system()`
/// 4. Any other prefix → rejected as a legacy bare name
pub fn validate_context_ownership(context_name: &str, tenant_id: &TenantId) -> Result<(), String> {
    if context_name.starts_with("zaru-") {
        if !tenant_id.is_consumer() {
            return Err(format!(
                "SecurityContext '{}' uses the 'zaru-' prefix which is reserved for consumer \
                 realm principals, but the requesting tenant is '{}'",
                context_name,
                tenant_id.as_str()
            ));
        }
        return Ok(());
    }

    if context_name.starts_with("tenant-") {
        let expected_prefix = format!("tenant-{}-", tenant_id.as_str());
        if !context_name.starts_with(&expected_prefix) {
            return Err(format!(
                "SecurityContext '{}' uses the 'tenant-' prefix but does not match the \
                 requesting tenant '{}'; expected prefix '{}'",
                context_name,
                tenant_id.as_str(),
                expected_prefix,
            ));
        }
        return Ok(());
    }

    if context_name.starts_with("aegis-system-") {
        if !tenant_id.is_system() {
            return Err(format!(
                "SecurityContext '{}' uses the 'aegis-system-' prefix which is reserved for \
                 system realm principals, but the requesting tenant is '{}'",
                context_name,
                tenant_id.as_str()
            ));
        }
        return Ok(());
    }

    Err(format!(
        "SecurityContext '{}' does not follow the required tenant-namespaced naming convention. \
         Context names must start with 'zaru-', 'tenant-{{slug}}-', or 'aegis-system-'",
        context_name
    ))
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
            capabilities: vec![Capability {
                tool_pattern: "fs.read".to_string(),
                path_allowlist: Some(vec!["/workspace".into()]),
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
            }],
            deny_list: vec![],
            metadata: test_metadata(),
        };

        // Allowed
        assert!(ctx
            .evaluate("fs.read", &json!({"path": "/workspace/test.txt"}))
            .is_ok());

        // Denied by capability limits (path)
        assert!(ctx
            .evaluate("fs.read", &json!({"path": "/etc/passwd"}))
            .is_err());

        // Denied implicitly (not in capabilities)
        assert!(ctx
            .evaluate("fs.write", &json!({"path": "/workspace/test.txt"}))
            .is_err());
    }

    #[test]
    fn test_security_context_evaluate_denylist_override() {
        let ctx = SecurityContext {
            name: "test-ctx".to_string(),
            description: "Testing context".to_string(),
            capabilities: vec![Capability {
                tool_pattern: "fs.*".to_string(),
                path_allowlist: Some(vec!["/workspace".into()]),
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
            }],
            // Although fs.* is allowed, fs.delete is explicitly denied
            deny_list: vec!["fs.delete".to_string()],
            metadata: test_metadata(),
        };

        // Allowed due to wildcard
        assert!(ctx
            .evaluate("fs.read", &json!({"path": "/workspace/test.txt"}))
            .is_ok());

        // Denied due to explicit deny list
        assert!(matches!(
            ctx.evaluate("fs.delete", &json!({"path": "/workspace/test.txt"})),
            Err(PolicyViolation::ToolExplicitlyDenied { .. })
        ));
    }

    #[test]
    fn test_validate_context_ownership_zaru_consumer() {
        let tenant = TenantId::consumer();
        assert!(validate_context_ownership("zaru-free", &tenant).is_ok());
        assert!(validate_context_ownership("zaru-pro", &tenant).is_ok());
    }

    #[test]
    fn test_validate_context_ownership_zaru_rejects_non_consumer() {
        let tenant = TenantId::system();
        assert!(validate_context_ownership("zaru-free", &tenant).is_err());
        let enterprise = TenantId::from_realm_slug("acme").unwrap();
        assert!(validate_context_ownership("zaru-pro", &enterprise).is_err());
    }

    #[test]
    fn test_validate_context_ownership_tenant_prefix_matching() {
        let tenant = TenantId::from_realm_slug("acme-corp").unwrap();
        assert!(validate_context_ownership("tenant-acme-corp-research", &tenant).is_ok());
        assert!(validate_context_ownership("tenant-acme-corp-deploy", &tenant).is_ok());
        // Wrong tenant
        let other = TenantId::from_realm_slug("other-corp").unwrap();
        assert!(validate_context_ownership("tenant-acme-corp-research", &other).is_err());
    }

    #[test]
    fn test_validate_context_ownership_aegis_system() {
        let system = TenantId::system();
        assert!(validate_context_ownership("aegis-system-internal", &system).is_ok());
        let consumer = TenantId::consumer();
        assert!(validate_context_ownership("aegis-system-internal", &consumer).is_err());
    }

    #[test]
    fn test_validate_context_ownership_rejects_legacy_bare_names() {
        let consumer = TenantId::consumer();
        assert!(validate_context_ownership("research-safe", &consumer).is_err());
        assert!(validate_context_ownership("coder-unrestricted", &consumer).is_err());
    }

    #[test]
    fn test_security_context_permits_tool_name_uses_patterns_and_denylist() {
        let ctx = SecurityContext {
            name: "test-ctx".to_string(),
            description: "Testing context".to_string(),
            capabilities: vec![Capability {
                tool_pattern: "fs.*".to_string(),
                path_allowlist: Some(vec!["/workspace".into()]),
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                max_response_size: None,
            }],
            deny_list: vec!["fs.delete".to_string()],
            metadata: test_metadata(),
        };

        assert!(ctx.permits_tool_name("fs.read"));
        assert!(!ctx.permits_tool_name("fs.delete"));
        assert!(!ctx.permits_tool_name("cmd.run"));
    }
}
