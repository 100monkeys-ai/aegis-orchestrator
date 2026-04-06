// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Capability Value Object (BC-12, ADR-035)
//!
//! A `Capability` is a fine-grained permission entry within a [`super::SecurityContext`].
//! Each capability grants access to tools matching a pattern, optionally constrained
//! by filesystem path, shell command, network domain, and response size.
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

use super::PolicyViolation;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// Per-capability rate limit configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Maximum number of calls allowed within the window.
    pub calls: u32,
    /// Window duration in seconds.
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
/// ADR-035 §2 (Capability Model), AGENTS.md §SEAL Protocol Domain `Capability`.
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
    /// If set, `cmd.run` tool calls must have command arguments matching these subcommands.
    /// Keys are base commands; values are the permitted subcommands for each base command.
    pub subcommand_allowlist: Option<HashMap<String, Vec<String>>>,
    /// If set, `web.*` tool calls must target a URL whose domain suffix matches
    /// one of these entries.
    pub domain_allowlist: Option<Vec<String>>,
    /// Maximum allowed response body size in bytes. `None` means unlimited.
    pub max_response_size: Option<u64>,
    /// Optional per-capability rate limit. Enforcement is deferred to a later phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Maximum number of concurrent `cmd.run` dispatches permitted for this
    /// capability. `None` means no limit enforced at the capability layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

impl Capability {
    /// Check whether a tool name matches this capability's tool pattern.
    pub fn matches_tool_name(&self, tool_name: &str) -> bool {
        self.matches_tool(tool_name)
    }

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
            if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
                let cmd_parts: Vec<&str> = cmd.split_whitespace().collect();
                let cmd_base = cmd_parts.first().unwrap_or(&"");

                if let Some(ref allowlist) = self.command_allowlist {
                    if !allowlist.contains(&cmd_base.to_string()) {
                        return Err(PolicyViolation::CommandNotAllowed {
                            command: cmd_base.to_string(),
                            allowed_commands: allowlist.clone(),
                        });
                    }
                }

                if let Some(ref sub_map) = self.subcommand_allowlist {
                    if !sub_map.contains_key(*cmd_base) {
                        return Err(PolicyViolation::CommandNotAllowed {
                            command: cmd_base.to_string(),
                            allowed_commands: sub_map.keys().cloned().collect(),
                        });
                    }
                    let permitted_subs = &sub_map[*cmd_base];
                    if !permitted_subs.is_empty() {
                        if cmd_parts.len() > 1 {
                            let subcommand = cmd_parts[1];
                            if !permitted_subs.contains(&subcommand.to_string()) {
                                return Err(PolicyViolation::SubcommandNotAllowed {
                                    base_command: cmd_base.to_string(),
                                    subcommand: subcommand.to_string(),
                                    allowed_subcommands: permitted_subs.clone(),
                                });
                            }
                        } else {
                            return Err(PolicyViolation::SubcommandNotAllowed {
                                base_command: cmd_base.to_string(),
                                subcommand: String::new(),
                                allowed_subcommands: permitted_subs.clone(),
                            });
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_cmd_run_subcommand_allowlist() {
        let cap = Capability {
            tool_pattern: "cmd.run".to_string(),
            path_allowlist: None,
            command_allowlist: Some(vec!["cargo".to_string()]),
            subcommand_allowlist: Some(HashMap::from([(
                "cargo".to_string(),
                vec!["build".to_string(), "check".to_string()],
            )])),
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        };

        // Allowed: cargo build
        assert!(cap
            .allows("cmd.run", &json!({"command": "cargo build"}))
            .is_ok());

        // Allowed: cargo check
        assert!(cap
            .allows("cmd.run", &json!({"command": "cargo check"}))
            .is_ok());

        // Denied: incorrect command base
        assert!(cap
            .allows("cmd.run", &json!({"command": "npm install"}))
            .is_err());

        // Denied: incorrect subcommand
        assert!(cap
            .allows("cmd.run", &json!({"command": "cargo publish"}))
            .is_err());

        // Denied: missing subcommand
        assert!(cap.allows("cmd.run", &json!({"command": "cargo"})).is_err());

        // Denied: base command not in command_allowlist
        assert!(matches!(
            cap.allows("cmd.run", &json!({"command": "npm install"})),
            Err(PolicyViolation::CommandNotAllowed { .. })
        ));
    }
}
