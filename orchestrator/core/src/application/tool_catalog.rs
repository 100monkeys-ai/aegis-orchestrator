// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Tool Catalog Application Service
//!
//! Provides enriched tool discovery for agents and the Zaru client.
//! Wraps the raw [`ToolMetadata`](crate::infrastructure::tool_router::ToolMetadata)
//! from the infrastructure layer with source classification, category grouping,
//! and tag inference so consumers can filter, search, and paginate the tool
//! surface without coupling to the underlying router internals.
//!
//! ## Architecture
//!
//! - **Layer:** Application Layer
//! - **Bounded Context:** BC-14 SEAL Tooling Gateway (discovery surface)
//! - **Refresh model:** The catalog maintains an in-memory cache that is
//!   populated externally via [`StandardToolCatalog::refresh_from`]. A
//!   periodic refresh loop (owned by the host process) calls
//!   `ToolInvocationService::get_available_tools()` and feeds the result here.
//!
//! ## Related ADRs
//!
//! - ADR-033: Orchestrator-Mediated MCP Tool Routing
//! - ADR-053: SEAL Tooling Gateway

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Source of a tool in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSource {
    Builtin,
    McpServer,
    SealGateway,
}

/// Category for grouping tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    AgentManagement,
    WorkflowManagement,
    TaskManagement,
    SystemManagement,
    SchemaValidation,
    Filesystem,
    Execution,
    WebNetwork,
    ToolDiscovery,
    External,
}

/// Enriched tool metadata for discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub source: ToolSource,
    pub category: ToolCategory,
    pub tags: Vec<String>,
}

/// Query for listing tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolListQuery {
    pub offset: Option<u32>,
    pub limit: Option<u32>,
    pub source: Option<ToolSource>,
    pub category: Option<ToolCategory>,
}

/// Query for searching tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchQuery {
    pub keyword: Option<String>,
    pub name_pattern: Option<String>,
    pub source: Option<ToolSource>,
    pub category: Option<ToolCategory>,
    pub tags: Option<Vec<String>>,
}

/// Paginated response from [`StandardToolCatalog::list_tools`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolListResponse {
    pub tools: Vec<CatalogEntry>,
    pub total: u32,
    pub offset: u32,
    pub limit: u32,
}

/// Response from [`StandardToolCatalog::search_tools`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchResponse {
    pub tools: Vec<CatalogEntry>,
    pub total_matches: u32,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// In-memory tool catalog with periodic refresh.
///
/// The catalog does **not** own the refresh schedule — the host process calls
/// [`refresh_from`](Self::refresh_from) on a timer, feeding it the latest
/// `Vec<ToolMetadata>` from the tool router + SEAL gateway.
pub struct StandardToolCatalog {
    cache: Arc<RwLock<Vec<CatalogEntry>>>,
}

impl Default for StandardToolCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl StandardToolCatalog {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Replace the catalog contents with freshly-fetched tool metadata.
    ///
    /// Each [`ToolMetadata`](crate::infrastructure::tool_router::ToolMetadata) is
    /// enriched with source, category, and tags before being stored.
    pub async fn refresh_from(&self, tools: Vec<crate::infrastructure::tool_router::ToolMetadata>) {
        let entries: Vec<CatalogEntry> = tools.into_iter().map(Self::enrich).collect();
        let mut cache = self.cache.write().await;
        *cache = entries;
    }

    /// List tools with optional filters, scoped to the caller's permitted tool names.
    ///
    /// `permitted_tools` contains the glob patterns from the caller's
    /// [`SecurityContext`](crate::domain::security_context::SecurityContext)
    /// capabilities. An empty slice means *no tools are visible* (most-restrictive
    /// default matching the SecurityContext invariant).
    pub async fn list_tools(
        &self,
        permitted_tools: &[String],
        query: ToolListQuery,
    ) -> ToolListResponse {
        let cache = self.cache.read().await;
        let filtered: Vec<&CatalogEntry> = cache
            .iter()
            .filter(|e| {
                permitted_tools.is_empty()
                    || permitted_tools
                        .iter()
                        .any(|p| Self::pattern_matches(p, &e.name))
            })
            .filter(|e| query.source.is_none_or(|s| e.source == s))
            .filter(|e| query.category.is_none_or(|c| e.category == c))
            .collect();

        let total = filtered.len() as u32;
        let offset = query.offset.unwrap_or(0);
        let limit = query.limit.unwrap_or(50).min(200);

        let tools: Vec<CatalogEntry> = filtered
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .cloned()
            .collect();

        ToolListResponse {
            tools,
            total,
            offset,
            limit,
        }
    }

    /// Search tools with keyword/filter criteria, scoped to the caller's permitted
    /// tool names.
    pub async fn search_tools(
        &self,
        permitted_tools: &[String],
        query: ToolSearchQuery,
    ) -> ToolSearchResponse {
        let cache = self.cache.read().await;
        let keyword_lower = query.keyword.as_ref().map(|k| k.to_lowercase());

        let results: Vec<CatalogEntry> = cache
            .iter()
            .filter(|e| {
                permitted_tools.is_empty()
                    || permitted_tools
                        .iter()
                        .any(|p| Self::pattern_matches(p, &e.name))
            })
            .filter(|e| {
                if let Some(ref kw) = keyword_lower {
                    e.name.to_lowercase().contains(kw) || e.description.to_lowercase().contains(kw)
                } else {
                    true
                }
            })
            .filter(|e| {
                if let Some(ref pattern) = query.name_pattern {
                    Self::glob_matches(pattern, &e.name)
                } else {
                    true
                }
            })
            .filter(|e| query.source.is_none_or(|s| e.source == s))
            .filter(|e| query.category.is_none_or(|c| e.category == c))
            .filter(|e| {
                if let Some(ref required_tags) = query.tags {
                    required_tags.iter().all(|t| e.tags.contains(t))
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        let total_matches = results.len() as u32;
        ToolSearchResponse {
            tools: results,
            total_matches,
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn enrich(tool: crate::infrastructure::tool_router::ToolMetadata) -> CatalogEntry {
        let source = Self::classify_source(&tool.name);
        let category = Self::classify_category(&tool.name);
        let tags = Self::infer_tags(&tool.name);

        CatalogEntry {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
            source,
            category,
            tags,
        }
    }

    fn classify_source(name: &str) -> ToolSource {
        if name.starts_with("aegis.") || name.starts_with("fs.") || name.starts_with("cmd.") {
            ToolSource::Builtin
        } else if name.starts_with("web.") {
            ToolSource::McpServer
        } else {
            // Default: tools from unknown prefixes are likely from the SEAL Gateway
            // (ToolWorkflows and EphemeralCliTools have arbitrary names).
            ToolSource::SealGateway
        }
    }

    fn classify_category(name: &str) -> ToolCategory {
        if name.starts_with("aegis.agent.") {
            return ToolCategory::AgentManagement;
        }
        if name.starts_with("aegis.workflow.") {
            return ToolCategory::WorkflowManagement;
        }
        if name.starts_with("aegis.task.") {
            return ToolCategory::TaskManagement;
        }
        if name.starts_with("aegis.system.") {
            return ToolCategory::SystemManagement;
        }
        if name.starts_with("aegis.schema.") {
            return ToolCategory::SchemaValidation;
        }
        if name.starts_with("aegis.tools.") {
            return ToolCategory::ToolDiscovery;
        }
        if name.starts_with("fs.") {
            return ToolCategory::Filesystem;
        }
        if name.starts_with("cmd.") {
            return ToolCategory::Execution;
        }
        if name.starts_with("web.") {
            return ToolCategory::WebNetwork;
        }
        ToolCategory::External
    }

    fn infer_tags(name: &str) -> Vec<String> {
        let mut tags = Vec::new();

        // Read-only tools
        let read_only_suffixes = [
            ".list", ".search", ".get", ".export", ".status", ".logs", ".info", ".config",
        ];
        let read_only_names = [
            "fs.read",
            "fs.list",
            "fs.grep",
            "fs.glob",
            "web.search",
            "web.fetch",
        ];
        if read_only_suffixes.iter().any(|s| name.ends_with(s)) || read_only_names.contains(&name) {
            tags.push("read-only".to_string());
        }

        // Destructive tools
        let destructive_suffixes = [
            ".create", ".update", ".delete", ".remove", ".cancel", ".signal",
        ];
        let destructive_names = [
            "fs.write",
            "fs.edit",
            "fs.multi_edit",
            "fs.create_dir",
            "fs.create.dir",
            "fs.delete",
            "cmd.run",
        ];
        if destructive_suffixes.iter().any(|s| name.ends_with(s))
            || destructive_names.contains(&name)
        {
            tags.push("destructive".to_string());
        }

        // Network tools
        if name.starts_with("web.") {
            tags.push("network".to_string());
        }

        tags
    }

    /// Simple glob matching: supports `*` as wildcard for any sequence of characters.
    /// Handles patterns like `"aegis.agent.*"` or `"fs.*"`.
    fn glob_matches(pattern: &str, name: &str) -> bool {
        if !pattern.contains('*') {
            return pattern == name;
        }
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            let (prefix, suffix) = (parts[0], parts[1]);
            name.starts_with(prefix) && name.ends_with(suffix)
        } else {
            // Fallback: treat as prefix match if ends with *
            if let Some(prefix) = pattern.strip_suffix('*') {
                name.starts_with(prefix)
            } else {
                pattern == name
            }
        }
    }

    /// Check if a security-context tool pattern permits a tool name.
    /// Supports exact match and glob patterns (e.g. `"aegis.agent.*"`).
    fn pattern_matches(pattern: &str, tool_name: &str) -> bool {
        if pattern.contains('*') {
            Self::glob_matches(pattern, tool_name)
        } else {
            pattern == tool_name
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_metadata() -> crate::infrastructure::tool_router::ToolMetadata {
        crate::infrastructure::tool_router::ToolMetadata {
            name: "aegis.agent.list".to_string(),
            description: "List all agents".to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn enrich_classifies_builtin_agent_tool() {
        let entry = StandardToolCatalog::enrich(sample_metadata());
        assert_eq!(entry.source, ToolSource::Builtin);
        assert_eq!(entry.category, ToolCategory::AgentManagement);
        assert!(entry.tags.contains(&"read-only".to_string()));
    }

    #[test]
    fn enrich_classifies_fs_tool() {
        let meta = crate::infrastructure::tool_router::ToolMetadata {
            name: "fs.write".to_string(),
            description: "Write a file".to_string(),
            input_schema: json!({"type": "object"}),
        };
        let entry = StandardToolCatalog::enrich(meta);
        assert_eq!(entry.source, ToolSource::Builtin);
        assert_eq!(entry.category, ToolCategory::Filesystem);
        assert!(entry.tags.contains(&"destructive".to_string()));
    }

    #[test]
    fn enrich_classifies_web_tool() {
        let meta = crate::infrastructure::tool_router::ToolMetadata {
            name: "web.fetch".to_string(),
            description: "Fetch a URL".to_string(),
            input_schema: json!({"type": "object"}),
        };
        let entry = StandardToolCatalog::enrich(meta);
        assert_eq!(entry.source, ToolSource::McpServer);
        assert_eq!(entry.category, ToolCategory::WebNetwork);
        assert!(entry.tags.contains(&"read-only".to_string()));
        assert!(entry.tags.contains(&"network".to_string()));
    }

    #[test]
    fn enrich_classifies_unknown_as_seal_gateway() {
        let meta = crate::infrastructure::tool_router::ToolMetadata {
            name: "custom-workflow-tool".to_string(),
            description: "Some external tool".to_string(),
            input_schema: json!({"type": "object"}),
        };
        let entry = StandardToolCatalog::enrich(meta);
        assert_eq!(entry.source, ToolSource::SealGateway);
        assert_eq!(entry.category, ToolCategory::External);
    }

    #[test]
    fn glob_matches_exact() {
        assert!(StandardToolCatalog::glob_matches("fs.read", "fs.read"));
        assert!(!StandardToolCatalog::glob_matches("fs.read", "fs.write"));
    }

    #[test]
    fn glob_matches_wildcard_suffix() {
        assert!(StandardToolCatalog::glob_matches(
            "aegis.agent.*",
            "aegis.agent.list"
        ));
        assert!(StandardToolCatalog::glob_matches(
            "aegis.agent.*",
            "aegis.agent.create"
        ));
        assert!(!StandardToolCatalog::glob_matches(
            "aegis.agent.*",
            "aegis.workflow.list"
        ));
    }

    #[test]
    fn glob_matches_wildcard_prefix() {
        assert!(StandardToolCatalog::glob_matches(
            "*.list",
            "aegis.agent.list"
        ));
        assert!(!StandardToolCatalog::glob_matches(
            "*.list",
            "aegis.agent.create"
        ));
    }

    #[test]
    fn pattern_matches_exact_and_glob() {
        assert!(StandardToolCatalog::pattern_matches("fs.read", "fs.read"));
        assert!(!StandardToolCatalog::pattern_matches("fs.read", "fs.write"));
        assert!(StandardToolCatalog::pattern_matches("fs.*", "fs.read"));
        assert!(StandardToolCatalog::pattern_matches("fs.*", "fs.write"));
    }

    #[tokio::test]
    async fn list_tools_filters_by_source() {
        let catalog = StandardToolCatalog::new();
        catalog
            .refresh_from(vec![
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.agent.list".to_string(),
                    description: "List agents".to_string(),
                    input_schema: json!({"type": "object"}),
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "web.fetch".to_string(),
                    description: "Fetch URL".to_string(),
                    input_schema: json!({"type": "object"}),
                },
            ])
            .await;

        let permitted = vec!["aegis.*".to_string(), "web.*".to_string()];
        let response = catalog
            .list_tools(
                &permitted,
                ToolListQuery {
                    source: Some(ToolSource::McpServer),
                    ..Default::default()
                },
            )
            .await;

        assert_eq!(response.total, 1);
        assert_eq!(response.tools[0].name, "web.fetch");
    }

    #[tokio::test]
    async fn search_tools_keyword_match() {
        let catalog = StandardToolCatalog::new();
        catalog
            .refresh_from(vec![
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.agent.list".to_string(),
                    description: "List all agents".to_string(),
                    input_schema: json!({"type": "object"}),
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.workflow.list".to_string(),
                    description: "List all workflows".to_string(),
                    input_schema: json!({"type": "object"}),
                },
            ])
            .await;

        let permitted = vec!["aegis.*".to_string()];
        let response = catalog
            .search_tools(
                &permitted,
                ToolSearchQuery {
                    keyword: Some("workflow".to_string()),
                    ..Default::default()
                },
            )
            .await;

        assert_eq!(response.total_matches, 1);
        assert_eq!(response.tools[0].name, "aegis.workflow.list");
    }

    #[tokio::test]
    async fn list_tools_respects_pagination() {
        let catalog = StandardToolCatalog::new();
        let tools: Vec<_> = (0..10)
            .map(|i| crate::infrastructure::tool_router::ToolMetadata {
                name: format!("aegis.tool.{i}"),
                description: format!("Tool {i}"),
                input_schema: json!({"type": "object"}),
            })
            .collect();
        catalog.refresh_from(tools).await;

        let permitted = vec!["aegis.*".to_string()];
        let response = catalog
            .list_tools(
                &permitted,
                ToolListQuery {
                    offset: Some(3),
                    limit: Some(2),
                    ..Default::default()
                },
            )
            .await;

        assert_eq!(response.total, 10);
        assert_eq!(response.offset, 3);
        assert_eq!(response.limit, 2);
        assert_eq!(response.tools.len(), 2);
        assert_eq!(response.tools[0].name, "aegis.tool.3");
        assert_eq!(response.tools[1].name, "aegis.tool.4");
    }
}
