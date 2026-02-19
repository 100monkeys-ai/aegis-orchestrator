// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

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
            read: vec

!["/workspace/**".to_string()],
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
