// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Policy
//!
//! Provides policy functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Domain Layer
//! - **Purpose:** Implements policy

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("Invalid pattern: {0}")]
    InvalidPattern(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityPolicy {
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
    pub resources: ResourceLimits,
    pub isolation: IsolationType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPolicy {
    pub mode: PolicyMode,
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyMode {
    Allow,
    Deny,
}

impl NetworkPolicy {
    pub fn new(mode: PolicyMode, allowlist: Vec<String>) -> Self {
        Self { mode, allowlist }
    }

    pub fn allows(&self, host: &str) -> bool {
        match self.mode {
            PolicyMode::Allow => self.allowlist.iter().any(|pattern| matches_pattern(pattern, host)),
            PolicyMode::Deny => !self.allowlist.iter().any(|pattern| matches_pattern(pattern, host)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemPolicy {
    pub read: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu_us: u64, // Microseconds quota? Or shares? Let's use simple generic for now, aligned with typical container specs.
    pub memory_mb: u64,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IsolationType {
    Firecracker,
    Docker,
    Process, // For testing/dev
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
