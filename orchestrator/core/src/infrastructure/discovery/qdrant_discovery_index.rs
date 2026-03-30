// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Qdrant Discovery Index (ADR-075)
//!
//! Implements the `DiscoveryIndex` port using the Qdrant vector database.
//! Manages two collections — one for agents, one for workflows — with
//! tenant-scoped filtered ANN search and idempotent collection bootstrapping.

use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use qdrant_client::qdrant::point_id::PointIdOptions;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder,
    Distance, FieldType, Filter, HnswConfigDiff, PointId, PointStruct, SearchPointsBuilder, Struct,
    UpsertPointsBuilder, Value, VectorParamsBuilder,
};
use qdrant_client::{Payload, Qdrant};
use tracing::{debug, info};

use crate::application::discovery_service::{AgentIndexEntry, DiscoveryIndex, WorkflowIndexEntry};
use crate::domain::discovery::{DiscoveryResourceKind, DiscoveryResult};
use crate::domain::shared_kernel::TenantId;

// ──────────────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────────────

const AGENT_COLLECTION: &str = "aegis_agent_discovery";
const WORKFLOW_COLLECTION: &str = "aegis_workflow_discovery";
const VECTOR_DIM: u64 = 384;

// ──────────────────────────────────────────────────────────────────────────────
// QdrantDiscoveryIndex
// ──────────────────────────────────────────────────────────────────────────────

/// Qdrant-backed implementation of the `DiscoveryIndex` port.
///
/// Stores agent and workflow embeddings in separate Qdrant collections with
/// cosine similarity search and tenant-scoped filtering.
pub struct QdrantDiscoveryIndex {
    client: Qdrant,
}

impl QdrantDiscoveryIndex {
    /// Connect to a Qdrant instance at the given gRPC URL (e.g. `http://localhost:6334`).
    pub async fn connect(url: &str) -> Result<Self> {
        let client = Qdrant::from_url(url)
            .build()
            .context("failed to build Qdrant client")?;

        // Verify connectivity
        client
            .health_check()
            .await
            .context("Qdrant health check failed")?;

        info!(url, "connected to Qdrant discovery index");
        Ok(Self { client })
    }

    /// Idempotent collection creation for both agent and workflow discovery.
    ///
    /// Creates collections with 384-dimensional cosine vectors, HNSW params
    /// (m=16, ef_construct=100), and payload field indexes for filtered search.
    pub async fn ensure_collections(&self) -> Result<()> {
        for collection in [AGENT_COLLECTION, WORKFLOW_COLLECTION] {
            self.ensure_single_collection(collection).await?;
        }
        Ok(())
    }

    async fn ensure_single_collection(&self, name: &str) -> Result<()> {
        let exists = self
            .client
            .collection_exists(name)
            .await
            .with_context(|| format!("failed to check existence of collection {name}"))?;

        if exists {
            debug!(collection = name, "discovery collection already exists");
            return Ok(());
        }

        let hnsw = HnswConfigDiff {
            m: Some(16),
            ef_construct: Some(100),
            ..Default::default()
        };

        let vector_params = VectorParamsBuilder::new(VECTOR_DIM, Distance::Cosine);

        self.client
            .create_collection(
                CreateCollectionBuilder::new(name)
                    .vectors_config(vector_params)
                    .hnsw_config(hnsw),
            )
            .await
            .with_context(|| format!("failed to create collection {name}"))?;

        info!(collection = name, "created discovery collection");

        // Create payload field indexes for filtered search
        self.create_field_index(name, "tenant_id", FieldType::Keyword)
            .await?;
        self.create_field_index(name, "name", FieldType::Keyword)
            .await?;
        self.create_field_index(name, "labels", FieldType::Keyword)
            .await?;
        self.create_field_index(name, "status", FieldType::Keyword)
            .await?;
        self.create_field_index(name, "is_platform_template", FieldType::Bool)
            .await?;
        self.create_field_index(name, "updated_at", FieldType::Integer)
            .await?;

        Ok(())
    }

    async fn create_field_index(
        &self,
        collection: &str,
        field: &str,
        field_type: FieldType,
    ) -> Result<()> {
        self.client
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                collection, field, field_type,
            ))
            .await
            .with_context(|| {
                format!("failed to create field index {field} on collection {collection}")
            })?;

        debug!(collection, field, "created payload field index");
        Ok(())
    }

    /// Build a tenant isolation filter that returns the tenant's own resources
    /// OR platform templates.
    fn tenant_filter(tenant_id: &TenantId) -> Filter {
        Filter::should(vec![
            Condition::matches("tenant_id", tenant_id.as_str().to_string()),
            Condition::matches("is_platform_template", true),
        ])
    }

    /// Merge optional status and label filters into the base filter.
    fn apply_query_filters(
        base: Filter,
        query: &crate::domain::discovery::DiscoveryQuery,
    ) -> Filter {
        let mut must_conditions: Vec<qdrant_client::qdrant::Condition> = vec![];

        if let Some(ref status) = query.status_filter {
            must_conditions.push(Condition::matches("status", status.clone()));
        }

        for (key, value) in &query.label_filters {
            // Labels are stored as "label_<key>" payload fields
            must_conditions.push(Condition::matches(format!("label_{key}"), value.clone()));
        }

        if must_conditions.is_empty() {
            base
        } else {
            // Combine: the base (should) filter for tenant isolation AND
            // the must conditions for status/label filters.
            Filter {
                must: must_conditions,
                should: base.should,
                must_not: vec![],
                min_should: None,
            }
        }
    }

    /// Build a Qdrant `Value` wrapping a `Struct` from a `HashMap<String, Value>`.
    fn struct_value(fields: HashMap<String, Value>) -> Value {
        Value {
            kind: Some(qdrant_client::qdrant::value::Kind::StructValue(Struct {
                fields,
            })),
        }
    }

    /// Build payload for an agent index entry.
    fn agent_payload(entry: &AgentIndexEntry) -> Payload {
        let mut payload = Payload::with_capacity(12);
        payload.insert("tenant_id", Value::from(entry.tenant_id.as_str()));
        payload.insert("name", Value::from(entry.name.as_str()));
        payload.insert("version", Value::from(entry.version.as_str()));
        payload.insert("description", Value::from(entry.description.as_str()));
        payload.insert("tools", Value::from(entry.tools.clone()));
        payload.insert(
            "task_description",
            Value::from(entry.task_description.as_str()),
        );
        payload.insert(
            "runtime_language",
            Value::from(entry.runtime_language.as_str()),
        );
        payload.insert("status", Value::from(entry.status.as_str()));
        payload.insert(
            "is_platform_template",
            Value::from(entry.is_platform_template),
        );
        payload.insert("updated_at", Value::from(entry.updated_at.timestamp()));

        // Store labels as individual "label_<key>" fields for filtered search
        for (key, value) in &entry.labels {
            payload.insert(format!("label_{key}"), Value::from(value.as_str()));
        }

        // Also store the full labels map as a JSON struct for retrieval
        let labels_fields: HashMap<String, Value> = entry
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect();
        payload.insert("labels_map", Self::struct_value(labels_fields));

        payload
    }

    /// Build payload for a workflow index entry.
    fn workflow_payload(entry: &WorkflowIndexEntry) -> Payload {
        let mut payload = Payload::with_capacity(10);
        payload.insert("tenant_id", Value::from(entry.tenant_id.as_str()));
        payload.insert("name", Value::from(entry.name.as_str()));
        payload.insert("version", Value::from(entry.version.as_str()));
        payload.insert("description", Value::from(entry.description.as_str()));
        payload.insert("state_names", Value::from(entry.state_names.clone()));
        payload.insert("agent_names", Value::from(entry.agent_names.clone()));
        payload.insert(
            "is_platform_template",
            Value::from(entry.is_platform_template),
        );
        payload.insert("updated_at", Value::from(entry.updated_at.timestamp()));

        for (key, value) in &entry.labels {
            payload.insert(format!("label_{key}"), Value::from(value.as_str()));
        }

        let labels_fields: HashMap<String, Value> = entry
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
            .collect();
        payload.insert("labels_map", Self::struct_value(labels_fields));

        payload
    }

    /// Extract a string from a Qdrant payload value.
    fn extract_string(payload: &HashMap<String, Value>, key: &str) -> String {
        payload
            .get(key)
            .and_then(|v| match &v.kind {
                Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Extract a boolean from a Qdrant payload value.
    fn extract_bool(payload: &HashMap<String, Value>, key: &str) -> bool {
        payload
            .get(key)
            .and_then(|v| match &v.kind {
                Some(qdrant_client::qdrant::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Extract a timestamp (i64) from payload and convert to `DateTime<Utc>`.
    fn extract_timestamp(payload: &HashMap<String, Value>, key: &str) -> DateTime<Utc> {
        payload
            .get(key)
            .and_then(|v| match &v.kind {
                Some(qdrant_client::qdrant::value::Kind::IntegerValue(ts)) => {
                    Utc.timestamp_opt(*ts, 0).single()
                }
                _ => None,
            })
            .unwrap_or_else(Utc::now)
    }

    /// Extract the labels map from the `labels_map` struct payload field.
    fn extract_labels(payload: &HashMap<String, Value>) -> HashMap<String, String> {
        payload
            .get("labels_map")
            .and_then(|v| match &v.kind {
                Some(qdrant_client::qdrant::value::Kind::StructValue(s)) => {
                    let map = s
                        .fields
                        .iter()
                        .filter_map(|(k, v)| match &v.kind {
                            Some(qdrant_client::qdrant::value::Kind::StringValue(sv)) => {
                                Some((k.clone(), sv.clone()))
                            }
                            _ => None,
                        })
                        .collect();
                    Some(map)
                }
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Extract the UUID string from a Qdrant `PointId`.
    fn point_id_to_string(id: &PointId) -> String {
        match &id.point_id_options {
            Some(PointIdOptions::Uuid(uuid)) => uuid.clone(),
            Some(PointIdOptions::Num(num)) => num.to_string(),
            None => String::new(),
        }
    }

    /// Convert a Qdrant `ScoredPoint` into a `DiscoveryResult`.
    fn scored_point_to_result(
        point: &qdrant_client::qdrant::ScoredPoint,
        kind: DiscoveryResourceKind,
    ) -> DiscoveryResult {
        let payload = &point.payload;
        let resource_id = point
            .id
            .as_ref()
            .map(Self::point_id_to_string)
            .unwrap_or_default();

        DiscoveryResult {
            resource_id,
            kind,
            name: Self::extract_string(payload, "name"),
            version: Self::extract_string(payload, "version"),
            description: Self::extract_string(payload, "description"),
            labels: Self::extract_labels(payload),
            similarity_score: point.score as f64,
            relevance_score: 0.0, // Computed by the application layer
            tenant_id: Self::extract_string(payload, "tenant_id"),
            updated_at: Self::extract_timestamp(payload, "updated_at"),
            is_platform_template: Self::extract_bool(payload, "is_platform_template"),
        }
    }

    /// Perform a vector search on the specified collection with tenant isolation.
    async fn search_collection(
        &self,
        collection: &str,
        tenant_id: &TenantId,
        embedding: Vec<f32>,
        query: &crate::domain::discovery::DiscoveryQuery,
        kind: DiscoveryResourceKind,
    ) -> Result<Vec<DiscoveryResult>> {
        let base_filter = Self::tenant_filter(tenant_id);
        let filter = Self::apply_query_filters(base_filter, query);

        let search = SearchPointsBuilder::new(collection, embedding, query.limit as u64)
            .filter(filter)
            .score_threshold(query.min_score as f32)
            .with_payload(true);

        let response = self
            .client
            .search_points(search)
            .await
            .with_context(|| format!("search failed on collection {collection}"))?;

        let results = response
            .result
            .iter()
            .map(|p| Self::scored_point_to_result(p, kind))
            .collect();

        Ok(results)
    }

    /// Get the point count for a collection.
    async fn collection_point_count(&self, collection: &str) -> Result<u64> {
        let info = self
            .client
            .collection_info(collection)
            .await
            .with_context(|| format!("failed to get collection info for {collection}"))?;

        Ok(info.result.and_then(|r| r.points_count).unwrap_or(0))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// DiscoveryIndex trait implementation
// ──────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl DiscoveryIndex for QdrantDiscoveryIndex {
    async fn index_agent(&self, entry: AgentIndexEntry) -> Result<()> {
        let point_id = entry.agent_id.clone();
        let vector = entry.embedding.clone();
        let payload = Self::agent_payload(&entry);

        let point = PointStruct::new(point_id.clone(), vector, payload);

        self.client
            .upsert_points(UpsertPointsBuilder::new(AGENT_COLLECTION, vec![point]).wait(true))
            .await
            .context("failed to upsert agent into discovery index")?;

        debug!(agent_id = %entry.agent_id, "indexed agent in discovery");
        Ok(())
    }

    async fn index_workflow(&self, entry: WorkflowIndexEntry) -> Result<()> {
        let point_id = entry.workflow_id.clone();
        let vector = entry.embedding.clone();
        let payload = Self::workflow_payload(&entry);

        let point = PointStruct::new(point_id.clone(), vector, payload);

        self.client
            .upsert_points(UpsertPointsBuilder::new(WORKFLOW_COLLECTION, vec![point]).wait(true))
            .await
            .context("failed to upsert workflow into discovery index")?;

        debug!(workflow_id = %entry.workflow_id, "indexed workflow in discovery");
        Ok(())
    }

    async fn remove_agent(&self, agent_id: &str) -> Result<()> {
        let point_id: PointId = agent_id.into();
        self.client
            .delete_points(
                DeletePointsBuilder::new(AGENT_COLLECTION)
                    .points(vec![point_id])
                    .wait(true),
            )
            .await
            .context("failed to delete agent from discovery index")?;

        debug!(agent_id, "removed agent from discovery index");
        Ok(())
    }

    async fn remove_workflow(&self, workflow_id: &str) -> Result<()> {
        let point_id: PointId = workflow_id.into();
        self.client
            .delete_points(
                DeletePointsBuilder::new(WORKFLOW_COLLECTION)
                    .points(vec![point_id])
                    .wait(true),
            )
            .await
            .context("failed to delete workflow from discovery index")?;

        debug!(workflow_id, "removed workflow from discovery index");
        Ok(())
    }

    async fn search_agents(
        &self,
        tenant_id: &TenantId,
        embedding: Vec<f32>,
        query: &crate::domain::discovery::DiscoveryQuery,
    ) -> Result<Vec<DiscoveryResult>> {
        self.search_collection(
            AGENT_COLLECTION,
            tenant_id,
            embedding,
            query,
            DiscoveryResourceKind::Agent,
        )
        .await
    }

    async fn search_workflows(
        &self,
        tenant_id: &TenantId,
        embedding: Vec<f32>,
        query: &crate::domain::discovery::DiscoveryQuery,
    ) -> Result<Vec<DiscoveryResult>> {
        self.search_collection(
            WORKFLOW_COLLECTION,
            tenant_id,
            embedding,
            query,
            DiscoveryResourceKind::Workflow,
        )
        .await
    }

    async fn agent_count(&self) -> Result<u64> {
        self.collection_point_count(AGENT_COLLECTION).await
    }

    async fn workflow_count(&self) -> Result<u64> {
        self.collection_point_count(WORKFLOW_COLLECTION).await
    }
}
