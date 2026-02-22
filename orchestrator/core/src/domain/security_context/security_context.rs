// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0


use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::mcp::PolicyViolation;
use super::capability::Capability;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityContextMetadata {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: u32,
}

/// Defines a named permission boundary for agent executions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityContext {
    /// Unique name (e.g., "research-safe", "coder-unrestricted")
    pub name: String,
    
    /// Human-readable description
    pub description: String,
    
    /// Allowed capabilities (tool patterns with constraints)
    pub capabilities: Vec<Capability>,
    
    /// Explicitly denied tools (takes precedence over capabilities)
    pub deny_list: Vec<String>,
    
    /// Metadata (created_at, updated_at, version)
    pub metadata: SecurityContextMetadata,
}

impl SecurityContext {
    /// Evaluate whether a tool call is allowed
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
