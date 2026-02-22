// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Capability Value Object (BC-12, ADR-035)
//!
//! A `Capability` is a fine-grained permission entry within a [`super::SecurityContext`].
//! Each capability grants access to tools matching a pattern, optionally constrained
//! by filesystem path, shell command, network domain, rate limit, and response size.
//!
//! ## Pattern Matching
//!
//! `tool_pattern` supports:
//! - `"*"` — matches any tool name
//! - `"fs.*"` — matches any tool starting with `"fs."`
//! - `"fs.read"` — exact match only
//!
//! ## Constraint Evaluation
//!
//! [`Capability::allows`] applies constraints in this order (first failure returns
//! a `PolicyViolation`):
//! 1. Tool name pattern match
//! 2. Path allowlist (for `fs.*` / `filesystem.*` tools)
//! 3. Command allowlist (for `cmd.run`)
//! 4. Domain allowlist (for `web.*` / `web-search.*` tools)

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use crate::domain::mcp::PolicyViolation;

/// Per-tool rate-limiting configuration within a [`Capability`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Maximum number of tool invocations allowed within the time window.
    pub calls: u32,
    /// Length of the sliding time window in seconds.
    pub per_seconds: u32,
}

/// Fine-grained MCP tool permission with optional constraints.
///
/// A value object within the [`super::SecurityContext`] aggregate. Multiple
/// capabilities can be defined per context; the first one that matches a tool
/// call (via [`Capability::allows`]) grants access.
///
/// # See Also
///
/// ADR-035 §2 (Capability Model), AGENTS.md §SMCP Protocol Domain `Capability`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    /// Tool name pattern. Supports exact match and prefix wildcard (`"fs.*"`).
    pub tool_pattern: String,
    /// If set, `fs.*` / `filesystem.*` tool calls with a `path` argument must have
    /// that path fall under one of these prefixes.
    pub path_allowlist: Option<Vec<PathBuf>>,
    /// If set, `cmd.run` tool calls must use a command whose base executable name
    /// is in this list.
    pub command_allowlist: Option<Vec<String>>,
    /// If set, `web.*` tool calls must target a URL whose domain suffix matches
    /// one of these entries.
    pub domain_allowlist: Option<Vec<String>>,
    /// Optional per-capability rate limit. `None` means unlimited. Note: rate limiting
    /// enforcement requires Phase 2 (in-memory rate counters not yet implemented).
    pub rate_limit: Option<RateLimit>,
    /// Maximum allowed response body size in bytes. `None` means unlimited.
    pub max_response_size: Option<u64>,
}

impl Capability {
    /// Evaluate whether `tool_name` with `args` is permitted by this capability.
    ///
    /// Returns `Ok(())` if the call is allowed. On any constraint violation, returns
    /// an `Err(PolicyViolation)` describing the specific failure so it can be logged
    /// and fed into audit events.
    ///
    /// # Errors
    ///
    /// - `ToolNotAllowed` — `tool_name` does not match `tool_pattern`
    /// - `PathOutsideBoundary` — `path` argument is outside `path_allowlist`
    /// - `ToolNotAllowed` (with cmd context) — `cmd.run` command not in `command_allowlist`
    /// - `DomainNotAllowed` — `url` argument domain not in `domain_allowlist`
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
        if tool_name.starts_with("web.") || tool_name.starts_with("web-search.") {
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
