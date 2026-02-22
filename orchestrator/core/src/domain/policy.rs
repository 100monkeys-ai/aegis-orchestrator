// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Security Policy Domain (BC-4)
//!
//! Value objects that define *infrastructure-level* security constraints enforced at
//! runtime by the orchestrator. These types are distinct from the *protocol-level*
//! SMCP `SecurityContext` (see [`crate::domain::security_context`]) — policy governs
//! what the container runtime is allowed to do; `SecurityContext` governs what MCP
//! tool calls the agent is allowed to make.
//!
//! ## Policy Layers
//!
//! ```text
//! Manifest YAML (agent spec)
//!     ↓ parsed by agent_manifest_parser
//! SecurityPolicy  ← this module
//!   ├─ NetworkPolicy   – egress domain allowlist
//!   ├─ FilesystemPolicy – POSIX path read/write lists
//!   ├─ ResourceLimits  – CPU / memory / timeout caps
//!   └─ IsolationType   – Docker (Phase 1) vs Firecracker (Phase 2)
//! ```
//!
//! ## NetworkPolicy Pattern Matching
//!
//! [`NetworkPolicy::allows`] supports two patterns:
//! - Exact match: `"api.github.com"`
//! - Wildcard subdomain: `"*.github.com"` (matches `api.github.com`, NOT `github.com`)
//!
//! See Also: ADR-035 (SMCP, protocol-level policy)

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error returned when a policy value object fails construction-time validation.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// A glob/wildcard pattern in the allowlist is syntactically invalid.
    #[error("Invalid pattern: {0}")]
    InvalidPattern(String),
}

/// Complete security constraint specification for an agent runtime instance.
///
/// Parsed from the `spec.security` section of an [`crate::domain::agent::AgentManifest`].
/// The orchestrator validates this struct before spawning any container and enforces
/// each sub-policy at its respective enforcement point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityPolicy {
    /// Egress network access rules (enforced by iptables / Docker network policy).
    pub network: NetworkPolicy,
    /// Filesystem path access rules (enforced by the NFS Server Gateway FSAL layer, ADR-036).
    pub filesystem: FilesystemPolicy,
    /// CPU, memory, and execution timeout limits.
    pub resources: ResourceLimits,
    /// Isolation technology to use for spawning the agent container.
    pub isolation: IsolationType,
}

/// Egress network access control list for an agent container.
///
/// Controls which external hostnames the agent may reach. Applied as a network
/// allowlist/denylist at the container networking layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPolicy {
    /// Whether `allowlist` specifies permitted hosts (`Allow`) or blocked hosts (`Deny`).
    pub mode: PolicyMode,
    /// List of hostname patterns. Supports exact matches and wildcard subdomains (`*.example.com`).
    pub allowlist: Vec<String>,
}

/// Controls the semantics of the `allowlist` field in a [`NetworkPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyMode {
    /// Only connections to hostnames matching the allowlist are permitted.
    Allow,
    /// Connections to hostnames matching the allowlist are **blocked**; all others are permitted.
    Deny,
}

impl NetworkPolicy {
    /// Create a new policy with the given mode and allowlist.
    pub fn new(mode: PolicyMode, allowlist: Vec<String>) -> Self {
        Self { mode, allowlist }
    }

    /// Returns `true` if the given `host` is permitted by this policy.
    ///
    /// Pattern matching rules (applied per entry in `allowlist`):
    /// - `"example.com"` — exact hostname match
    /// - `"*.example.com"` — matches any immediate subdomain; does **not** match the bare domain
    ///
    /// # Examples
    ///
    /// ```
    /// # use aegis_core::domain::policy::{NetworkPolicy, PolicyMode};
    /// let p = NetworkPolicy::new(PolicyMode::Allow, vec!["api.github.com".into(), "*.openai.com".into()]);
    /// assert!(p.allows("api.github.com"));
    /// assert!(p.allows("api.openai.com"));
    /// assert!(!p.allows("evil.com"));
    /// ```
    pub fn allows(&self, host: &str) -> bool {
        match self.mode {
            PolicyMode::Allow => self.allowlist.iter().any(|pattern| matches_pattern(pattern, host)),
            PolicyMode::Deny => !self.allowlist.iter().any(|pattern| matches_pattern(pattern, host)),
        }
    }
}

/// Filesystem path access rules enforced by the NFS Server Gateway FSAL (ADR-036).
///
/// Read and write permissions are expressed as POSIX path prefixes or glob patterns.
/// The FSAL validates every file operation against these lists before proxying to
/// the SeaweedFS storage backend.
///
/// # Defaults
///
/// Both `read` and `write` default to `["/workspace/**"]`, giving the agent full
/// read/write access to its workspace volume and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemPolicy {
    /// Path patterns the agent may **read** from the mounted volumes.
    pub read: Vec<String>,
    /// Path patterns the agent may **write** to the mounted volumes.
    pub write: Vec<String>,
}

impl Default for FilesystemPolicy {
    fn default() -> Self {
        Self {
            read: vec!["/workspace/**".to_string()],
            write: vec!["/workspace/**".to_string()],
        }
    }
}

/// CPU, memory, and timeout constraints for an agent execution instance.
///
/// These values are enforced by the container runtime (Docker `--cpus`, `--memory`)
/// and by the [`crate::domain::supervisor::Supervisor`] timeout loop. All fields
/// are mandatory in production manifests — omitting them grants unlimited resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// CPU time quota in **microseconds per second** (cgroup `cpu.cfs_quota_us`).
    /// Example: `500_000` = 50% of one CPU core. Applied via Docker `--cpu-quota`.
    pub cpu_us: u64,
    /// Maximum RAM in **mebibytes** (`--memory` in Docker).
    pub memory_mb: u64,
    /// Hard wall-clock timeout for the entire execution in **seconds**.
    /// The Supervisor will terminate the instance if this elapses before completion.
    pub timeout_seconds: u64,
}

/// Kernel-level isolation technology used to run the agent container.
///
/// The orchestrator validates this field via [`crate::domain::runtime::RuntimeConfig::validate_isolation`]
/// before spawning; unsupported variants return a [`crate::domain::runtime::RuntimeError::SpawnFailed`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IsolationType {
    /// ⚠️ Phase 2 — Firecracker MicroVM isolation. Not yet implemented.
    /// Provides stronger kernel-level isolation than Docker but requires a
    /// Firecracker-enabled host (bare-metal KVM).
    Firecracker,
    /// Phase 1 — Docker container isolation via the `bollard` crate (ADR-027).
    /// Default for all Phase 1 deployments.
    Docker,
    /// ⚠️ Development/testing only — runs the agent as a child process with no
    /// container isolation. **Never use in production.**
    Process,
}

fn matches_pattern(pattern: &str, value: &str) -> bool {
    if pattern == value {
        return true;
    }
    if pattern.starts_with("*.") {
        return value.ends_with(&pattern[2..]);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── NetworkPolicy ─────────────────────────────────────────────────────────

    #[test]
    fn test_network_policy_allow_mode_exact_match() {
        let policy = NetworkPolicy::new(
            PolicyMode::Allow,
            vec!["api.example.com".to_string(), "data.example.com".to_string()],
        );
        assert!(policy.allows("api.example.com"));
        assert!(policy.allows("data.example.com"));
        assert!(!policy.allows("evil.com"));
    }

    #[test]
    fn test_network_policy_allow_mode_wildcard() {
        let policy = NetworkPolicy::new(
            PolicyMode::Allow,
            vec!["*.example.com".to_string()],
        );
        assert!(policy.allows("api.example.com"));
        assert!(policy.allows("db.example.com"));
        // The wildcard `ends_with` check also matches the bare domain
        assert!(policy.allows("example.com"));
        assert!(!policy.allows("evil.com"));
    }

    #[test]
    fn test_network_policy_deny_mode_blocks_listed() {
        let policy = NetworkPolicy::new(
            PolicyMode::Deny,
            vec!["evil.com".to_string(), "*.ads.net".to_string()],
        );
        assert!(!policy.allows("evil.com"));
        assert!(!policy.allows("tracker.ads.net"));
        assert!(policy.allows("good.com"));
    }

    #[test]
    fn test_network_policy_deny_mode_allows_unlisted() {
        let policy = NetworkPolicy::new(PolicyMode::Deny, vec![]);
        assert!(policy.allows("anything.com"));
    }

    #[test]
    fn test_network_policy_allow_mode_empty_list_denies_all() {
        let policy = NetworkPolicy::new(PolicyMode::Allow, vec![]);
        assert!(!policy.allows("example.com"));
    }

    // ── FilesystemPolicy ──────────────────────────────────────────────────────

    #[test]
    fn test_filesystem_policy_default() {
        let policy = FilesystemPolicy::default();
        assert_eq!(policy.read, vec!["/workspace/**".to_string()]);
        assert_eq!(policy.write, vec!["/workspace/**".to_string()]);
    }

    // ── PolicyMode ────────────────────────────────────────────────────────────

    #[test]
    fn test_policy_mode_serialization() {
        let allow = serde_json::to_string(&PolicyMode::Allow).unwrap();
        let deny = serde_json::to_string(&PolicyMode::Deny).unwrap();
        assert_eq!(allow, "\"Allow\"");
        assert_eq!(deny, "\"Deny\"");

        let deserialized: PolicyMode = serde_json::from_str(&allow).unwrap();
        assert_eq!(deserialized, PolicyMode::Allow);
    }

    // ── IsolationType ─────────────────────────────────────────────────────────

    #[test]
    fn test_isolation_type_serialization() {
        let docker = serde_json::to_string(&IsolationType::Docker).unwrap();
        let process = serde_json::to_string(&IsolationType::Process).unwrap();
        assert!(docker.contains("Docker"));
        assert!(process.contains("Process"));
    }

    // ── SecurityPolicy ────────────────────────────────────────────────────────

    #[test]
    fn test_security_policy_creation() {
        let policy = SecurityPolicy {
            network: NetworkPolicy::new(PolicyMode::Allow, vec!["*.example.com".to_string()]),
            filesystem: FilesystemPolicy::default(),
            resources: ResourceLimits {
                cpu_us: 1_000_000,
                memory_mb: 512,
                timeout_seconds: 300,
            },
            isolation: IsolationType::Docker,
        };
        assert!(policy.network.allows("api.example.com"));
        assert_eq!(policy.resources.memory_mb, 512);
    }
}
