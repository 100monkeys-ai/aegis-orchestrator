// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Discovery Service (ADR-075)
//!
//! Application service orchestrating semantic search over agents and workflows.
//! Delegates all indexing and retrieval to the Cortex microservice via
//! `CortexGrpcClient`.
//!
//! ## Availability
//!
//! This service is only constructed when `spec.cortex` is configured.
//! When absent, all callers receive `None` and degrade gracefully.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use chrono::DateTime;

use crate::domain::discovery::{
    DiscoveryQuery, DiscoveryResourceKind, DiscoveryResponse, DiscoveryResult, SearchMode,
};
use crate::domain::iam::ZaruTier;
use crate::domain::shared_kernel::TenantId;
use crate::infrastructure::aegis_cortex_proto::{DiscoverAgentsRequest, DiscoverWorkflowsRequest};
use crate::infrastructure::cortex_client::CortexGrpcClient;

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryService trait
// ──────────────────────────────────────────────────────────────────────────────

/// Application service for semantic discovery of agents and workflows.
///
/// Delegates search to the Cortex service which owns indexing and retrieval.
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
// CortexDiscoveryService
// ──────────────────────────────────────────────────────────────────────────────

/// Production implementation of [`DiscoveryService`] backed by the Cortex service.
///
/// All embedding generation and vector search is delegated to Cortex via gRPC.
pub struct CortexDiscoveryService {
    cortex_client: Arc<CortexGrpcClient>,
}

impl CortexDiscoveryService {
    /// Create a new discovery service backed by the given Cortex client.
    pub fn new(cortex_client: Arc<CortexGrpcClient>) -> Self {
        Self { cortex_client }
    }
}

/// Convert a tier to the string expected by the Cortex proto API.
fn tier_str(tier: &ZaruTier) -> &'static str {
    match tier {
        ZaruTier::Free => "free",
        ZaruTier::Pro => "pro",
        ZaruTier::Business => "business",
        ZaruTier::Enterprise => "enterprise",
    }
}

/// Map a `DiscoveryResultItem` proto into a domain `DiscoveryResult`.
fn map_result_item(
    item: crate::infrastructure::aegis_cortex_proto::DiscoveryResultItem,
    kind: DiscoveryResourceKind,
) -> DiscoveryResult {
    let updated_at = DateTime::parse_from_rfc3339(&item.updated_at)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());

    DiscoveryResult {
        resource_id: item.resource_id,
        kind,
        name: item.name,
        version: item.version,
        description: item.description,
        labels: item.labels,
        similarity_score: item.similarity_score,
        relevance_score: item.relevance_score,
        tenant_id: item.tenant_id,
        updated_at,
        is_platform_template: item.is_platform_template,
        input_schema: item.input_schema,
    }
}

#[async_trait]
impl DiscoveryService for CortexDiscoveryService {
    async fn search_agents(
        &self,
        tenant_id: &TenantId,
        tier: &ZaruTier,
        query: DiscoveryQuery,
    ) -> Result<DiscoveryResponse> {
        let start = Instant::now();

        let req = DiscoverAgentsRequest {
            query: query.query,
            limit: query.limit,
            min_score: query.min_score,
            label_filters: query.label_filters,
            status_filter: query.status_filter,
            include_platform_templates: query.include_platform_templates,
            tenant_id: tenant_id.to_string(),
            tier: tier_str(tier).to_string(),
        };

        let resp = self
            .cortex_client
            .discover_agents(req)
            .await
            .map_err(|e| anyhow::anyhow!("Cortex DiscoverAgents RPC failed: {e}"))?;

        let results = resp
            .results
            .into_iter()
            .map(|item| map_result_item(item, DiscoveryResourceKind::Agent))
            .collect();

        Ok(DiscoveryResponse {
            results,
            total_indexed: resp.total_indexed,
            query_time_ms: start.elapsed().as_millis() as u64,
            search_mode: SearchMode::Semantic,
        })
    }

    async fn search_workflows(
        &self,
        tenant_id: &TenantId,
        tier: &ZaruTier,
        query: DiscoveryQuery,
    ) -> Result<DiscoveryResponse> {
        let start = Instant::now();

        let req = DiscoverWorkflowsRequest {
            query: query.query,
            limit: query.limit,
            min_score: query.min_score,
            label_filters: query.label_filters,
            include_platform_templates: query.include_platform_templates,
            tenant_id: tenant_id.to_string(),
            tier: tier_str(tier).to_string(),
        };

        let resp = self
            .cortex_client
            .discover_workflows(req)
            .await
            .map_err(|e| anyhow::anyhow!("Cortex DiscoverWorkflows RPC failed: {e}"))?;

        let results = resp
            .results
            .into_iter()
            .map(|item| map_result_item(item, DiscoveryResourceKind::Workflow))
            .collect();

        Ok(DiscoveryResponse {
            results,
            total_indexed: resp.total_indexed,
            query_time_ms: start.elapsed().as_millis() as u64,
            search_mode: SearchMode::Semantic,
        })
    }

    async fn find_similar_agents(
        &self,
        tenant_id: &TenantId,
        description: &str,
        threshold: f64,
    ) -> Result<Vec<DiscoveryResult>> {
        let query = DiscoveryQuery {
            query: description.to_string(),
            limit: 20,
            min_score: threshold,
            ..Default::default()
        };

        // Use Enterprise tier to get the highest result cap for similarity searches
        let resp = self
            .search_agents(tenant_id, &ZaruTier::Enterprise, query)
            .await?;

        Ok(resp.results)
    }

    async fn find_similar_workflows(
        &self,
        tenant_id: &TenantId,
        description: &str,
        threshold: f64,
    ) -> Result<Vec<DiscoveryResult>> {
        let query = DiscoveryQuery {
            query: description.to_string(),
            limit: 20,
            min_score: threshold,
            ..Default::default()
        };

        // Use Enterprise tier to get the highest result cap for similarity searches
        let resp = self
            .search_workflows(tenant_id, &ZaruTier::Enterprise, query)
            .await?;

        Ok(resp.results)
    }
}
