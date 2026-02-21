// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use crate::domain::mcp::PolicyViolation;

/// Rate Limit definition
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    pub calls: u32,
    pub per_seconds: u32,
}

/// Fine-grained permission with constraints
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    /// Tool name pattern (supports wildcards: "fs.*", "cmd.run")
    pub tool_pattern: String,
    
    /// Optional path constraints (for fs.* tools)
    pub path_allowlist: Option<Vec<PathBuf>>,
    
    /// Optional command allowlist (for cmd.run tool)
    pub command_allowlist: Option<Vec<String>>,
    
    /// Optional domain allowlist (for web.* tools)
    pub domain_allowlist: Option<Vec<String>>,
    
    /// Rate limit configuration
    pub rate_limit: Option<RateLimit>,
    
    /// Maximum response size (bytes)
    pub max_response_size: Option<u64>,
}

impl Capability {
    /// Check if tool call is allowed by this capability
    pub fn allows(&self, tool_name: &str, args: &Value) -> Result<(), PolicyViolation> {
        // Check tool name match
        if !self.matches_tool(tool_name) {
            return Err(PolicyViolation::ToolNotAllowed {
                tool_name: tool_name.to_string(),
                allowed_tools: vec![self.tool_pattern.clone()],
            });
        }
        
        // Check path constraints for filesystem tools
        if tool_name.starts_with("fs.") || tool_name.starts_with("filesystem.") {
            if let Some(ref allowlist) = self.path_allowlist {
                if let Some(path) = args.get("path").and_then(|p| p.as_str()) {
                    if !self.path_in_allowlist(path, allowlist) {
                        return Err(PolicyViolation::PathOutsideBoundary {
                            path: PathBuf::from(path),
                            allowed_paths: allowlist.clone(),
                        });
                    }
                }
            }
        }
        
        // Check command constraints for cmd.run
        if tool_name == "cmd.run" {
            if let Some(ref allowlist) = self.command_allowlist {
                if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
                    let cmd_base = cmd.split_whitespace().next().unwrap_or("");
                    if !allowlist.contains(&cmd_base.to_string()) {
                        return Err(PolicyViolation::ToolNotAllowed {
                            tool_name: format!("cmd.run (command: {})", cmd),
                            allowed_tools: allowlist.clone(),
                        });
                    }
                }
            }
        }
        
        // Check domain constraints for web search
        if tool_name.starts_with("web.*") || tool_name.starts_with("web-search.") {
            if let Some(ref allowlist) = self.domain_allowlist {
                if let Some(url) = args.get("url").and_then(|u| u.as_str()) {
                    let domain = Self::extract_domain(url);
                    if !allowlist.iter().any(|d| domain.ends_with(d)) {
                        return Err(PolicyViolation::DomainNotAllowed {
                            domain,
                            allowed_domains: allowlist.clone(),
                        });
                    }
                }
            }
        }
        
        Ok(())
    }
    
    fn matches_tool(&self, tool_name: &str) -> bool {
        if self.tool_pattern == "*" {
            return true;
        }
        if self.tool_pattern.ends_with(".*") {
            let prefix = self.tool_pattern.trim_end_matches(".*");
            return tool_name.starts_with(prefix);
        }
        tool_name == self.tool_pattern
    }
    
    fn path_in_allowlist(&self, path: &str, allowlist: &[PathBuf]) -> bool {
        let path = PathBuf::from(path);
        for allowed_path in allowlist {
            if path.starts_with(allowed_path) {
                return true;
            }
        }
        false
    }
    
    fn extract_domain(url: &str) -> String {
        if let Ok(parsed_url) = url::Url::parse(url) {
            parsed_url.host_str().unwrap_or("").to_string()
        } else {
            "".to_string()
        }
    }
}
