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
    /// ADR-117: when set to `"edge"`, the EdgeRouter step-3 implicit branch
    /// dispatches this tool through the edge fleet when the caller's tenant
    /// has exactly one connected edge daemon. Defaults to `None` (i.e. local
    /// builtin / MCP / SEAL chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<String>,
    /// ADR-117: when `true`, the tool is eligible for fleet (multi-target) fan
    /// out via `aegis.edge.fleet.invoke`. Surfaced so clients (e.g. Zaru
    /// fleet-launcher picker) can filter discovery results. Defaults to
    /// `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fleet_capable: bool,
}

/// Query for listing tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolListQuery {
    pub offset: Option<u32>,
    pub limit: Option<u32>,
    pub source: Option<ToolSource>,
    pub category: Option<ToolCategory>,
    /// ADR-117: when `Some(true)`, restrict results to tools with
    /// `fleet_capable == true` (used by the Zaru fleet-launcher picker).
    /// `Some(false)` excludes fleet-capable tools; `None` is unfiltered.
    #[serde(default)]
    pub fleet_capable: Option<bool>,
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
    /// ADR-117: when `Some(true)`, restrict results to tools with
    /// `fleet_capable == true`. `Some(false)` excludes them; `None` is
    /// unfiltered.
    #[serde(default)]
    pub fleet_capable: Option<bool>,
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
                // Deny-by-default: empty `permitted_tools` means no tools are visible,
                // matching the SecurityContext invariant (security_context.rs: "An empty
                // `capabilities` vec means **no tools are allowed** (most restrictive)").
                !permitted_tools.is_empty()
                    && permitted_tools
                        .iter()
                        .any(|p| Self::pattern_matches(p, &e.name))
            })
            .filter(|e| query.source.is_none_or(|s| e.source == s))
            .filter(|e| query.category.is_none_or(|c| e.category == c))
            .filter(|e| query.fleet_capable.is_none_or(|f| e.fleet_capable == f))
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
    ///
    /// `permitted_tools` follows the same deny-by-default invariant as
    /// [`list_tools`](Self::list_tools): an empty slice means *no tools are visible*.
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
                // Deny-by-default: empty `permitted_tools` means no tools are visible,
                // matching the SecurityContext invariant (security_context.rs: "An empty
                // `capabilities` vec means **no tools are allowed** (most restrictive)").
                !permitted_tools.is_empty()
                    && permitted_tools
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
            .filter(|e| query.fleet_capable.is_none_or(|f| e.fleet_capable == f))
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
            executor: tool.executor,
            fleet_capable: tool.fleet_capable,
        }
    }

    /// ADR-117: look up a single entry by exact tool name. Used by
    /// `tool_advertises_edge_executor` in the EdgeRouter step-3 decision.
    /// Returns `None` if the catalog has not been refreshed yet or the tool
    /// is not registered.
    pub async fn lookup(&self, tool_name: &str) -> Option<CatalogEntry> {
        let cache = self.cache.read().await;
        cache.iter().find(|e| e.name == tool_name).cloned()
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
        if name.starts_with("aegis.runtime.") {
            return ToolCategory::AgentManagement;
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
                    ..Default::default()
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "web.fetch".to_string(),
                    description: "Fetch URL".to_string(),
                    input_schema: json!({"type": "object"}),
                    ..Default::default()
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
                    ..Default::default()
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.workflow.list".to_string(),
                    description: "List all workflows".to_string(),
                    input_schema: json!({"type": "object"}),
                    ..Default::default()
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

    /// ADR-117 audit SEV-2-E (T1) regression: `CatalogEntry` must preserve the
    /// `executor` and `fleet_capable` fields plumbed up from `ToolMetadata`,
    /// and `lookup()` must return them so `tool_advertises_edge_executor` can
    /// implement EdgeRouter step 3 instead of always returning false.
    #[tokio::test]
    async fn lookup_surfaces_edge_executor_flag() {
        let catalog = StandardToolCatalog::new();
        catalog
            .refresh_from(vec![
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.edge.fleet.invoke".to_string(),
                    description: "Fleet invoke".to_string(),
                    input_schema: json!({"type": "object"}),
                    executor: Some("edge".to_string()),
                    fleet_capable: true,
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.agent.list".to_string(),
                    description: "List agents".to_string(),
                    input_schema: json!({"type": "object"}),
                    executor: None,
                    fleet_capable: false,
                },
            ])
            .await;

        // Edge tool: executor == "edge"
        let edge = catalog.lookup("aegis.edge.fleet.invoke").await.unwrap();
        assert_eq!(edge.executor.as_deref(), Some("edge"));
        assert!(edge.fleet_capable);

        // Local tool: executor == None
        let local = catalog.lookup("aegis.agent.list").await.unwrap();
        assert_eq!(local.executor, None);
        assert!(!local.fleet_capable);

        // Unknown tool: lookup returns None (caller defaults to false)
        assert!(catalog.lookup("does.not.exist").await.is_none());

        // Mirror the `tool_advertises_edge_executor` decision shape so the
        // test fails if the lookup ever stops surfacing the executor field.
        let advertises = |name: &str| {
            let catalog = &catalog;
            async move {
                catalog
                    .lookup(name)
                    .await
                    .map(|e| e.executor.as_deref() == Some("edge"))
                    .unwrap_or(false)
            }
        };
        assert!(advertises("aegis.edge.fleet.invoke").await);
        assert!(!advertises("aegis.agent.list").await);
        assert!(!advertises("does.not.exist").await);
    }

    /// ADR-117: the `?fleet_capable=true` filter on the discovery surface
    /// must restrict results to tools tagged fleet-capable. Used by zaru-client
    /// fleet-launcher picker.
    #[tokio::test]
    async fn list_tools_filters_by_fleet_capable() {
        let catalog = StandardToolCatalog::new();
        catalog
            .refresh_from(vec![
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.edge.fleet.invoke".to_string(),
                    description: "Fleet invoke".to_string(),
                    input_schema: json!({"type": "object"}),
                    executor: Some("edge".to_string()),
                    fleet_capable: true,
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.agent.list".to_string(),
                    description: "List agents".to_string(),
                    input_schema: json!({"type": "object"}),
                    executor: None,
                    fleet_capable: false,
                },
            ])
            .await;

        let permitted = vec!["aegis.*".to_string()];
        let response = catalog
            .list_tools(
                &permitted,
                ToolListQuery {
                    fleet_capable: Some(true),
                    ..Default::default()
                },
            )
            .await;

        assert_eq!(response.total, 1);
        assert_eq!(response.tools[0].name, "aegis.edge.fleet.invoke");
        assert!(response.tools[0].fleet_capable);
    }

    #[tokio::test]
    async fn list_tools_respects_pagination() {
        let catalog = StandardToolCatalog::new();
        let tools: Vec<_> = (0..10)
            .map(|i| crate::infrastructure::tool_router::ToolMetadata {
                name: format!("aegis.tool.{i}"),
                description: format!("Tool {i}"),
                input_schema: json!({"type": "object"}),
                ..Default::default()
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

    // ---------------------------------------------------------------------
    // Security regression tests — deny-by-default for `permitted_tools`.
    //
    // Prior to the fix, `list_tools` and `search_tools` filtered with
    // `permitted_tools.is_empty() || any(matches)`, which inverted the
    // SecurityContext invariant ("empty capabilities means no tools allowed")
    // into a "default allow" allowlist — granting universal tool access to
    // any caller whose permitted-tool slice happened to be empty. These tests
    // pin the deny-by-default semantic.
    // ---------------------------------------------------------------------

    fn two_tool_catalog_metadata() -> Vec<crate::infrastructure::tool_router::ToolMetadata> {
        vec![
            crate::infrastructure::tool_router::ToolMetadata {
                name: "tool.allowed".to_string(),
                description: "Allowed tool".to_string(),
                input_schema: json!({"type": "object"}),
                ..Default::default()
            },
            crate::infrastructure::tool_router::ToolMetadata {
                name: "tool.other".to_string(),
                description: "Other tool".to_string(),
                input_schema: json!({"type": "object"}),
                ..Default::default()
            },
        ]
    }

    #[tokio::test]
    async fn list_tools_empty_permitted_denies_all() {
        let catalog = StandardToolCatalog::new();
        catalog.refresh_from(two_tool_catalog_metadata()).await;

        let permitted: Vec<String> = vec![];
        let response = catalog
            .list_tools(&permitted, ToolListQuery::default())
            .await;

        assert_eq!(
            response.total, 0,
            "empty permitted_tools must hide all tools (deny-by-default)"
        );
        assert!(
            response.tools.is_empty(),
            "empty permitted_tools must yield zero visible tools"
        );
    }

    #[tokio::test]
    async fn list_tools_only_returns_permitted_match() {
        let catalog = StandardToolCatalog::new();
        catalog.refresh_from(two_tool_catalog_metadata()).await;

        let permitted = vec!["tool.allowed".to_string()];
        let response = catalog
            .list_tools(&permitted, ToolListQuery::default())
            .await;

        assert_eq!(response.total, 1);
        assert_eq!(response.tools.len(), 1);
        assert_eq!(response.tools[0].name, "tool.allowed");
    }

    #[tokio::test]
    async fn search_tools_empty_permitted_denies_all() {
        let catalog = StandardToolCatalog::new();
        catalog.refresh_from(two_tool_catalog_metadata()).await;

        let permitted: Vec<String> = vec![];
        let response = catalog
            .search_tools(&permitted, ToolSearchQuery::default())
            .await;

        assert_eq!(
            response.total_matches, 0,
            "empty permitted_tools must hide all tools from search (deny-by-default)"
        );
        assert!(
            response.tools.is_empty(),
            "empty permitted_tools must yield zero search results"
        );
    }

    #[tokio::test]
    async fn search_tools_only_returns_permitted_match() {
        let catalog = StandardToolCatalog::new();
        catalog.refresh_from(two_tool_catalog_metadata()).await;

        let permitted = vec!["tool.allowed".to_string()];
        let response = catalog
            .search_tools(&permitted, ToolSearchQuery::default())
            .await;

        assert_eq!(response.total_matches, 1);
        assert_eq!(response.tools.len(), 1);
        assert_eq!(response.tools[0].name, "tool.allowed");
    }

    #[tokio::test]
    async fn permitted_tools_glob_pattern_still_works_post_fix() {
        let catalog = StandardToolCatalog::new();
        catalog
            .refresh_from(vec![
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.agent.list".to_string(),
                    description: "List agents".to_string(),
                    input_schema: json!({"type": "object"}),
                    ..Default::default()
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.agent.create".to_string(),
                    description: "Create agent".to_string(),
                    input_schema: json!({"type": "object"}),
                    ..Default::default()
                },
                crate::infrastructure::tool_router::ToolMetadata {
                    name: "aegis.workflow.list".to_string(),
                    description: "List workflows".to_string(),
                    input_schema: json!({"type": "object"}),
                    ..Default::default()
                },
            ])
            .await;

        let permitted = vec!["aegis.agent.*".to_string()];

        let list_resp = catalog
            .list_tools(&permitted, ToolListQuery::default())
            .await;
        assert_eq!(list_resp.total, 2);
        let mut names: Vec<&str> = list_resp.tools.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["aegis.agent.create", "aegis.agent.list"]);

        let search_resp = catalog
            .search_tools(&permitted, ToolSearchQuery::default())
            .await;
        assert_eq!(search_resp.total_matches, 2);
        let mut search_names: Vec<&str> =
            search_resp.tools.iter().map(|t| t.name.as_str()).collect();
        search_names.sort();
        assert_eq!(search_names, vec!["aegis.agent.create", "aegis.agent.list"]);
    }
}
