// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Service (ADR-075)
//!
//! Application service orchestrating semantic search over agents and workflows.
//! Composes the `DiscoveryIndex` port (Qdrant backend) with the `EmbeddingPort`
//! (embedding service) and domain repositories for backfill.
//!
//! ## Enterprise Feature
//!
//! This service is only constructed when `spec.discovery` is configured.
//! When absent, all callers receive `None` and degrade gracefully.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::discovery::{DiscoveryQuery, DiscoveryResponse, DiscoveryResult, SearchMode};
use crate::domain::iam::ZaruTier;
use crate::domain::shared_kernel::TenantId;
use crate::infrastructure::embedding_client::EmbeddingPort;

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryIndex — infrastructure port for vector storage
// ──────────────────────────────────────────────────────────────────────────────

/// Port for indexing and querying discovery resources in a vector store.
///
/// Implementations back onto Qdrant (or similar) and handle collection
/// management, upsert, deletion, and filtered ANN queries.
#[async_trait]
pub trait DiscoveryIndex: Send + Sync {
    /// Index (upsert) an agent into the vector store.
    async fn index_agent(&self, entry: AgentIndexEntry) -> Result<()>;

    /// Index (upsert) a workflow into the vector store.
    async fn index_workflow(&self, entry: WorkflowIndexEntry) -> Result<()>;

    /// Remove an agent from the vector store by ID.
    async fn remove_agent(&self, agent_id: &str) -> Result<()>;

    /// Remove a workflow from the vector store by ID.
    async fn remove_workflow(&self, workflow_id: &str) -> Result<()>;

    /// Search for agents matching the given embedding and query filters.
    async fn search_agents(
        &self,
        tenant_id: &TenantId,
        embedding: Vec<f32>,
        query: &DiscoveryQuery,
    ) -> Result<Vec<DiscoveryResult>>;

    /// Search for workflows matching the given embedding and query filters.
    async fn search_workflows(
        &self,
        tenant_id: &TenantId,
        embedding: Vec<f32>,
        query: &DiscoveryQuery,
    ) -> Result<Vec<DiscoveryResult>>;

    /// Return the total number of indexed agents across all tenants.
    async fn agent_count(&self) -> Result<u64>;

    /// Return the total number of indexed workflows across all tenants.
    async fn workflow_count(&self) -> Result<u64>;
}

// ──────────────────────────────────────────────────────────────────────────────
// Index entry structs
// ──────────────────────────────────────────────────────────────────────────────

/// Data required to index an agent in the vector store.
pub struct AgentIndexEntry {
    pub agent_id: String,
    pub tenant_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub labels: HashMap<String, String>,
    pub tools: Vec<String>,
    pub task_description: String,
    pub runtime_language: String,
    pub status: String,
    pub embedding: Vec<f32>,
    pub updated_at: DateTime<Utc>,
    pub is_platform_template: bool,
}

/// Data required to index a workflow in the vector store.
pub struct WorkflowIndexEntry {
    pub workflow_id: String,
    pub tenant_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub labels: HashMap<String, String>,
    pub state_names: Vec<String>,
    pub agent_names: Vec<String>,
    pub embedding: Vec<f32>,
    pub updated_at: DateTime<Utc>,
    pub is_platform_template: bool,
}

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryService trait
// ──────────────────────────────────────────────────────────────────────────────

/// Application service for semantic discovery of agents and workflows.
///
/// Composes the `DiscoveryIndex` port with the `EmbeddingPort` to perform
/// tier-aware, relevance-ranked semantic search.
#[async_trait]
pub trait DiscoveryService: Send + Sync {
    /// Search for agents matching a natural-language query, scoped to the
    /// caller's tenant and tier.
    async fn search_agents(
        &self,
        tenant_id: &TenantId,
        tier: &ZaruTier,
        query: DiscoveryQuery,
    ) -> Result<DiscoveryResponse>;

    /// Search for workflows matching a natural-language query, scoped to the
    /// caller's tenant and tier.
    async fn search_workflows(
        &self,
        tenant_id: &TenantId,
        tier: &ZaruTier,
        query: DiscoveryQuery,
    ) -> Result<DiscoveryResponse>;

    /// Find agents similar to the given description, filtering by similarity
    /// threshold.
    async fn find_similar_agents(
        &self,
        tenant_id: &TenantId,
        description: &str,
        threshold: f64,
    ) -> Result<Vec<DiscoveryResult>>;

    /// Find workflows similar to the given description, filtering by similarity
    /// threshold.
    async fn find_similar_workflows(
        &self,
        tenant_id: &TenantId,
        description: &str,
        threshold: f64,
    ) -> Result<Vec<DiscoveryResult>>;
}

// ──────────────────────────────────────────────────────────────────────────────
// StandardDiscoveryService
// ──────────────────────────────────────────────────────────────────────────────

/// Production implementation of [`DiscoveryService`].
///
/// Delegates embedding generation to `EmbeddingPort` and vector search to
/// `DiscoveryIndex`, applying tier-based result limits and composite
/// relevance scoring (similarity × 0.85 + freshness × 0.15).
pub struct StandardDiscoveryService {
    index: Arc<dyn DiscoveryIndex>,
    embedding: Arc<dyn EmbeddingPort>,
}

impl StandardDiscoveryService {
    /// Create a new discovery service backed by the given index and embedding ports.
    pub fn new(index: Arc<dyn DiscoveryIndex>, embedding: Arc<dyn EmbeddingPort>) -> Self {
        Self { index, embedding }
    }
}

/// Return the maximum number of results allowed for the given tier.
fn tier_result_limit(tier: &ZaruTier) -> u32 {
    match tier {
        ZaruTier::Free => 10,
        ZaruTier::Pro | ZaruTier::Business => 50,
        ZaruTier::Enterprise => 100,
    }
}

/// Compute the composite relevance score for a discovery result.
///
/// `relevance = 0.85 * similarity + 0.15 * freshness`
///
/// where `freshness = 1.0 / (1.0 + days_since_update * 0.01)`.
fn compute_relevance_score(similarity_score: f64, updated_at: DateTime<Utc>) -> f64 {
    let days_since_update = (Utc::now() - updated_at).num_seconds().max(0) as f64 / 86400.0;
    let freshness_factor = 1.0 / (1.0 + days_since_update * 0.01);
    0.85 * similarity_score + 0.15 * freshness_factor
}

#[async_trait]
impl DiscoveryService for StandardDiscoveryService {
    async fn search_agents(
        &self,
        tenant_id: &TenantId,
        tier: &ZaruTier,
        mut query: DiscoveryQuery,
    ) -> Result<DiscoveryResponse> {
        let start = Instant::now();

        // Clamp limit to tier maximum
        let max_limit = tier_result_limit(tier);
        query.limit = query.limit.min(max_limit);

        // Generate embedding from query text
        let embedding = self.embedding.generate_embedding(&query.query).await?;

        // Delegate to the index port
        let mut results = self
            .index
            .search_agents(tenant_id, embedding, &query)
            .await?;

        // Compute composite relevance scores and sort
        for result in &mut results {
            result.relevance_score =
                compute_relevance_score(result.similarity_score, result.updated_at);
        }
        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total_indexed = self.index.agent_count().await?;
        let query_time_ms = start.elapsed().as_millis() as u64;

        Ok(DiscoveryResponse {
            results,
            total_indexed,
            query_time_ms,
            search_mode: SearchMode::Semantic,
        })
    }

    async fn search_workflows(
        &self,
        tenant_id: &TenantId,
        tier: &ZaruTier,
        mut query: DiscoveryQuery,
    ) -> Result<DiscoveryResponse> {
        let start = Instant::now();

        let max_limit = tier_result_limit(tier);
        query.limit = query.limit.min(max_limit);

        let embedding = self.embedding.generate_embedding(&query.query).await?;

        let mut results = self
            .index
            .search_workflows(tenant_id, embedding, &query)
            .await?;

        for result in &mut results {
            result.relevance_score =
                compute_relevance_score(result.similarity_score, result.updated_at);
        }
        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total_indexed = self.index.workflow_count().await?;
        let query_time_ms = start.elapsed().as_millis() as u64;

        Ok(DiscoveryResponse {
            results,
            total_indexed,
            query_time_ms,
            search_mode: SearchMode::Semantic,
        })
    }

    async fn find_similar_agents(
        &self,
        tenant_id: &TenantId,
        description: &str,
        threshold: f64,
    ) -> Result<Vec<DiscoveryResult>> {
        let embedding = self.embedding.generate_embedding(description).await?;

        let query = DiscoveryQuery {
            query: description.to_string(),
            limit: 20,
            min_score: threshold,
            ..Default::default()
        };

        let mut results = self
            .index
            .search_agents(tenant_id, embedding, &query)
            .await?;

        for result in &mut results {
            result.relevance_score =
                compute_relevance_score(result.similarity_score, result.updated_at);
        }
        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    async fn find_similar_workflows(
        &self,
        tenant_id: &TenantId,
        description: &str,
        threshold: f64,
    ) -> Result<Vec<DiscoveryResult>> {
        let embedding = self.embedding.generate_embedding(description).await?;

        let query = DiscoveryQuery {
            query: description.to_string(),
            limit: 20,
            min_score: threshold,
            ..Default::default()
        };

        let mut results = self
            .index
            .search_workflows(tenant_id, embedding, &query)
            .await?;

        for result in &mut results {
            result.relevance_score =
                compute_relevance_score(result.similarity_score, result.updated_at);
        }
        results.sort_by(|a, b| {
            b.relevance_score
                .partial_cmp(&a.relevance_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Embedding text assembly helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Build the embedding text for an agent per ADR-075 §5.
///
/// Concatenates the agent's name, description, task description, label values,
/// tool names, and runtime language into a single string suitable for embedding
/// generation.
pub fn agent_embedding_text(
    name: &str,
    description: &str,
    task_description: &str,
    labels: &HashMap<String, String>,
    tools: &[String],
    runtime_language: &str,
) -> String {
    format!(
        "{name} | {description} | task: {task_description} | labels: {} | tools: {} | language: {runtime_language}",
        labels.values().cloned().collect::<Vec<_>>().join(", "),
        tools.join(", ")
    )
}

/// Build the embedding text for a workflow per ADR-075 §5.
///
/// Concatenates the workflow's name, description, state names, agent names,
/// and label values into a single string suitable for embedding generation.
pub fn workflow_embedding_text(
    name: &str,
    description: &str,
    state_names: &[String],
    agent_names: &[String],
    labels: &HashMap<String, String>,
) -> String {
    format!(
        "{name} | {description} | states: {} | agents: {} | labels: {}",
        state_names.join(", "),
        agent_names.join(", "),
        labels.values().cloned().collect::<Vec<_>>().join(", ")
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_limits_are_correct() {
        assert_eq!(tier_result_limit(&ZaruTier::Free), 10);
        assert_eq!(tier_result_limit(&ZaruTier::Pro), 50);
        assert_eq!(tier_result_limit(&ZaruTier::Business), 50);
        assert_eq!(tier_result_limit(&ZaruTier::Enterprise), 100);
    }

    #[test]
    fn relevance_score_fresh_document() {
        // A document updated right now should have freshness ≈ 1.0
        let score = compute_relevance_score(0.9, Utc::now());
        // 0.85 * 0.9 + 0.15 * ~1.0 ≈ 0.765 + 0.15 = 0.915
        assert!(score > 0.90, "Expected > 0.90, got {score}");
        assert!(score < 0.92, "Expected < 0.92, got {score}");
    }

    #[test]
    fn relevance_score_stale_document() {
        // A document updated 365 days ago
        let updated = Utc::now() - chrono::Duration::days(365);
        let score = compute_relevance_score(0.9, updated);
        // freshness = 1.0 / (1.0 + 365 * 0.01) = 1.0 / 4.65 ≈ 0.215
        // relevance = 0.85 * 0.9 + 0.15 * 0.215 ≈ 0.765 + 0.032 = 0.797
        assert!(score > 0.79, "Expected > 0.79, got {score}");
        assert!(score < 0.81, "Expected < 0.81, got {score}");
    }

    #[test]
    fn agent_embedding_text_format() {
        let mut labels = HashMap::new();
        labels.insert("domain".to_string(), "finance".to_string());
        let tools = vec!["calculator".to_string(), "ledger".to_string()];

        let text = agent_embedding_text(
            "FinanceBot",
            "Handles financial queries",
            "Process invoices",
            &labels,
            &tools,
            "python",
        );

        assert!(text.contains("FinanceBot"));
        assert!(text.contains("Handles financial queries"));
        assert!(text.contains("task: Process invoices"));
        assert!(text.contains("finance"));
        assert!(text.contains("calculator, ledger"));
        assert!(text.contains("language: python"));
    }

    #[test]
    fn workflow_embedding_text_format() {
        let mut labels = HashMap::new();
        labels.insert("team".to_string(), "ops".to_string());
        let states = vec!["init".to_string(), "running".to_string()];
        let agents = vec!["deployer".to_string()];

        let text =
            workflow_embedding_text("DeployFlow", "Deploys services", &states, &agents, &labels);

        assert!(text.contains("DeployFlow"));
        assert!(text.contains("Deploys services"));
        assert!(text.contains("states: init, running"));
        assert!(text.contains("agents: deployer"));
        assert!(text.contains("ops"));
    }
}
