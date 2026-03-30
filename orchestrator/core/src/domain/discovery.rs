// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Domain Types (ADR-075)
//!
//! Value objects for the Workflow & Agent Discovery Service. These types
//! define the query, result, and response shapes for semantic search over
//! agents and workflows. Pure domain types with no I/O dependencies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryResourceKind
// ──────────────────────────────────────────────────────────────────────────────

/// The kind of resource returned by a discovery query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoveryResourceKind {
    /// An agent definition.
    Agent,
    /// A workflow definition.
    Workflow,
}

// ──────────────────────────────────────────────────────────────────────────────
// SearchMode
// ──────────────────────────────────────────────────────────────────────────────

/// The search strategy used for discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    /// Embedding-based cosine similarity search.
    Semantic,
}

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryQuery
// ──────────────────────────────────────────────────────────────────────────────

/// A natural-language discovery query over agents and workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryQuery {
    /// Natural-language description of the desired resource.
    pub query: String,

    /// Maximum number of results to return.
    pub limit: u32,

    /// Minimum similarity threshold (0.0–1.0).
    pub min_score: f64,

    /// Optional label filters — all must match for a result to be included.
    pub label_filters: HashMap<String, String>,

    /// Optional status filter (e.g. "active", "draft").
    pub status_filter: Option<String>,

    /// Whether to include platform-provided templates in the results.
    pub include_platform_templates: bool,
}

impl Default for DiscoveryQuery {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 10,
            min_score: 0.3,
            label_filters: HashMap::new(),
            status_filter: None,
            include_platform_templates: true,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryResult
// ──────────────────────────────────────────────────────────────────────────────

/// A single result from a discovery query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    /// Unique identifier of the discovered resource.
    pub resource_id: String,

    /// Whether this is an agent or a workflow.
    pub kind: DiscoveryResourceKind,

    /// Human-readable name of the resource.
    pub name: String,

    /// Version string (e.g. "1.0.0").
    pub version: String,

    /// Description of the resource's purpose and capabilities.
    pub description: String,

    /// Labels attached to the resource (key-value metadata).
    pub labels: HashMap<String, String>,

    /// Raw cosine similarity score (0.0–1.0).
    pub similarity_score: f64,

    /// Composite relevance score combining similarity and freshness.
    pub relevance_score: f64,

    /// Tenant that owns this resource.
    pub tenant_id: String,

    /// When the resource was last updated.
    pub updated_at: DateTime<Utc>,

    /// Whether this resource is a platform-provided template.
    pub is_platform_template: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryResponse
// ──────────────────────────────────────────────────────────────────────────────

/// The response envelope for a discovery query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResponse {
    /// Matched results, ordered by relevance score descending.
    pub results: Vec<DiscoveryResult>,

    /// Total number of resources in the index (across all tenants visible to the caller).
    pub total_indexed: u64,

    /// Wall-clock time spent executing the query, in milliseconds.
    pub query_time_ms: u64,

    /// The search mode used for this query.
    pub search_mode: SearchMode,
}
