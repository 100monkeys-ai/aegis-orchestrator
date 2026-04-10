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
            let command = args.get("command").and_then(|c| c.as_str()).ok_or(
                PolicyViolation::MissingRequiredArgument("command".to_string()),
            )?;

            if let Some(ref allowlist) = self.command_allowlist {
                let base_command = command.split_whitespace().next().unwrap_or(command);
                if !allowlist.contains(&base_command.to_string()) {
                    return Err(PolicyViolation::CommandNotAllowed {
                        command: command.to_string(),
                        allowed_commands: allowlist.clone(),
                    });
                }
            }

            if let Some(ref sub_map) = self.subcommand_allowlist {
                let allowed_subcmds =
                    sub_map
                        .get(command)
                        .ok_or_else(|| PolicyViolation::CommandNotAllowed {
                            command: command.to_string(),
                            allowed_commands: sub_map.keys().cloned().collect(),
                        })?;

                let cmd_args: Vec<&str> = args
                    .get("args")
                    .and_then(|a| a.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                if !allowed_subcmds.is_empty() {
                    if let Some(subcmd) = cmd_args.first() {
                        if !allowed_subcmds.contains(&subcmd.to_string()) {
                            return Err(PolicyViolation::SubcommandNotAllowed {
                                command: command.to_string(),
                                subcommand: subcmd.to_string(),
                                allowed_subcommands: allowed_subcmds.clone(),
                            });
                        }
                    } else {
                        return Err(PolicyViolation::SubcommandNotAllowed {
                            command: command.to_string(),
                            subcommand: String::new(),
                            allowed_subcommands: allowed_subcmds.clone(),
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
        let path_buf = PathBuf::from(path);
        for allowed_path in allowlist {
            // Direct match (absolute paths)
            if path_buf.starts_with(allowed_path) {
                return true;
            }
            // Relative path resolution: treat as relative to the allowed directory,
            // but only if no component is ".." (blocks traversal attacks)
            if !path_buf.has_root()
                && !path_buf
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                let resolved = allowed_path.join(&path_buf);
                if resolved.starts_with(allowed_path) {
                    return true;
                }
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
            .allows("cmd.run", &json!({"command": "cargo", "args": ["build"]}))
            .is_ok());

        // Allowed: cargo check
        assert!(cap
            .allows("cmd.run", &json!({"command": "cargo", "args": ["check"]}))
            .is_ok());

        // Denied: incorrect command base
        assert!(cap
            .allows("cmd.run", &json!({"command": "npm", "args": ["install"]}))
            .is_err());

        // Denied: incorrect subcommand
        assert!(cap
            .allows("cmd.run", &json!({"command": "cargo", "args": ["publish"]}))
            .is_err());

        // Denied: missing subcommand (no args key)
        assert!(cap.allows("cmd.run", &json!({"command": "cargo"})).is_err());

        // Denied: base command not in command_allowlist → CommandNotAllowed
        assert!(matches!(
            cap.allows("cmd.run", &json!({"command": "npm", "args": ["install"]})),
            Err(PolicyViolation::CommandNotAllowed { .. })
        ));
    }

    #[test]
    fn test_path_in_allowlist_relative_path_resolves_under_allowed_dir() {
        let cap = Capability {
            tool_pattern: "fs.*".to_string(),
            path_allowlist: Some(vec![PathBuf::from("/workspace")]),
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        };

        // Relative path resolves under allowed directory
        assert!(cap
            .allows("fs.read", &json!({"path": "solution.py"}))
            .is_ok());

        // Absolute path under allowed directory
        assert!(cap
            .allows("fs.read", &json!({"path": "/workspace/solution.py"}))
            .is_ok());

        // Traversal attack blocked
        assert!(cap
            .allows("fs.read", &json!({"path": "../etc/passwd"}))
            .is_err());

        // Relative path with empty allowlist fails
        let cap_empty = Capability {
            tool_pattern: "fs.*".to_string(),
            path_allowlist: Some(vec![]),
            command_allowlist: None,
            subcommand_allowlist: None,
            domain_allowlist: None,
            max_response_size: None,
            rate_limit: None,
            max_concurrent: None,
        };
        assert!(cap_empty
            .allows("fs.read", &json!({"path": "solution.py"}))
            .is_err());
    }
}
