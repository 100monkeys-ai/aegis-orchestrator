use crate::agent::Permissions;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Security policy enforcement result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Allow,
    Deny(String),
}

/// Errors during policy evaluation.
#[derive(Debug, Error)]
pub enum SecurityError {
    #[error("Policy violation: {0}")]
    PolicyViolation(String),
    
    #[error("Invalid permission configuration: {0}")]
    InvalidConfig(String),
}

/// A request for network access from an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkAccessRequest {
    pub protocol: String,
    pub host: String,
    pub port: u16,
}

/// A request for filesystem access from an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemAccessRequest {
    pub operation: FsOperation,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsOperation {
    Read,
    Write,
}

/// The security policy engine that enforces agent permissions.
/// 
/// All access requests are denied by default. Agents must explicitly
/// declare required permissions in their manifest.
pub struct PolicyEngine;

impl PolicyEngine {
    pub fn new() -> Self {
        Self
    }

    /// Evaluate a network access request against agent permissions.
    pub fn evaluate_network(
        &self,
        permissions: &Permissions,
        request: &NetworkAccessRequest,
    ) -> PolicyDecision {
        // Check if the host is in the allow list
        let host_allowed = permissions
            .network
            .allow
            .iter()
            .any(|allowed| self.matches_pattern(allowed, &request.host));

        if host_allowed {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny(format!(
                "Network access to {}:{} is not allowed",
                request.host, request.port
            ))
        }
    }

    /// Evaluate a filesystem access request against agent permissions.
    pub fn evaluate_filesystem(
        &self,
        permissions: &Permissions,
        request: &FilesystemAccessRequest,
    ) -> PolicyDecision {
        let allowed_paths = match request.operation {
            FsOperation::Read => &permissions.filesystem.read,
            FsOperation::Write => &permissions.filesystem.write,
        };

        let path_allowed = allowed_paths
            .iter()
            .any(|allowed| request.path.starts_with(allowed));

        if path_allowed {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Deny(format!(
                "Filesystem {:?} access to {} is not allowed",
                request.operation, request.path
            ))
        }
    }

    /// Check if a host matches a pattern (supports wildcards).
    fn matches_pattern(&self, pattern: &str, host: &str) -> bool {
        if pattern == host {
            return true;
        }

        // Simple wildcard support for subdomain matching
        if pattern.starts_with("*.") {
            let domain = &pattern[2..];
            return host.ends_with(domain);
        }

        false
    }
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{NetworkPermissions, FilesystemPermissions};

    #[test]
    fn test_network_policy_exact_match() {
        let engine = PolicyEngine::new();
        let permissions = Permissions {
            network: NetworkPermissions {
                allow: vec!["api.openai.com".to_string()],
            },
            filesystem: FilesystemPermissions {
                read: vec![],
                write: vec![],
            },
            execution_time_seconds: 300,
            memory_mb: 512,
        };

        let request = NetworkAccessRequest {
            protocol: "https".to_string(),
            host: "api.openai.com".to_string(),
            port: 443,
        };

        assert_eq!(engine.evaluate_network(&permissions, &request), PolicyDecision::Allow);
    }

    #[test]
    fn test_network_policy_wildcard() {
        let engine = PolicyEngine::new();
        let permissions = Permissions {
            network: NetworkPermissions {
                allow: vec!["*.openai.com".to_string()],
            },
            filesystem: FilesystemPermissions {
                read: vec![],
                write: vec![],
            },
            execution_time_seconds: 300,
            memory_mb: 512,
        };

        let request = NetworkAccessRequest {
            protocol: "https".to_string(),
            host: "api.openai.com".to_string(),
            port: 443,
        };

        assert_eq!(engine.evaluate_network(&permissions, &request), PolicyDecision::Allow);
    }

    #[test]
    fn test_network_policy_deny() {
        let engine = PolicyEngine::new();
        let permissions = Permissions {
            network: NetworkPermissions {
                allow: vec!["api.openai.com".to_string()],
            },
            filesystem: FilesystemPermissions {
                read: vec![],
                write: vec![],
            },
            execution_time_seconds: 300,
            memory_mb: 512,
        };

        let request = NetworkAccessRequest {
            protocol: "https".to_string(),
            host: "malicious.com".to_string(),
            port: 443,
        };

        match engine.evaluate_network(&permissions, &request) {
            PolicyDecision::Deny(_) => (),
            _ => panic!("Expected deny"),
        }
    }
}
