// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Qdrant production implementation for pattern storage
//! 
//! This module provides a production-ready vector store using Qdrant
//! for semantic similarity search of cortex patterns.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements internal responsibilities for qdrant repository

use async_trait::async_trait;
use anyhow::{Result, Context as _};
use qdrant_client::{
    Qdrant,
    qdrant::{
        CreateCollectionBuilder, Distance, VectorParamsBuilder,
        PointStruct, SearchPointsBuilder, Filter, Condition,
        ScrollPointsBuilder, PointId, GetPointsBuilder,
        UpsertPointsBuilder, SetPayloadPointsBuilder, DeletePointsBuilder,
        Value,
    },
};
use serde_json;
use std::collections::HashMap;
use uuid::Uuid;

use crate::domain::{CortexPattern, PatternId, ErrorSignature};
use crate::infrastructure::repository::PatternRepository;

const COLLECTION_NAME: &str = "aegis_patterns";
const VECTOR_DIM: u64 = 384; // EmbeddingClient vector dimension (all-MiniLM-L6-v2)

pub struct QdrantPatternRepository {
    client: Qdrant,
}

impl QdrantPatternRepository {
    /// Create a new Qdrant pattern repository
    pub async fn new(url: &str) -> Result<Self> {
        let client = Qdrant::from_url(url)
            .build()
            .context("Failed to create Qdrant client")?;
        
        Ok(Self { client })
    }
    
    /// Initialize the patterns collection with proper schema
    pub async fn initialize(&self) -> Result<()> {
        // Check if collection exists
        let collection_exists = self.client
            .collection_exists(COLLECTION_NAME)
            .await
            .context("Failed to check collection existence")?;
        
        if !collection_exists {
            // Create collection with cosine similarity
            self.client.create_collection(
                CreateCollectionBuilder::new(COLLECTION_NAME)
                    .vectors_config(VectorParamsBuilder::new(VECTOR_DIM, Distance::Cosine))
            ).await.context("Failed to create Qdrant collection")?;
        }
        
        Ok(())
    }
    
    /// Convert CortexPattern to Qdrant payload (HashMap<String, Value>)
    fn pattern_to_payload(pattern: &CortexPattern) -> HashMap<String, Value> {
        let mut payload = HashMap::new();
        
        payload.insert("id".to_string(), pattern.id.0.to_string().into());
        payload.insert("error_type".to_string(), pattern.error_signature.error_type.clone().into());
        payload.insert("error_message_hash".to_string(), pattern.error_signature.error_message_hash.clone().into());
        payload.insert("solution_code".to_string(), pattern.solution_code.clone().into());
        payload.insert("task_category".to_string(), pattern.task_category.clone().into());
        payload.insert("success_score".to_string(), pattern.success_score.into());
        payload.insert("execution_count".to_string(), (pattern.execution_count as i64).into());
        payload.insert("weight".to_string(), pattern.weight.into());
        payload.insert("last_verified".to_string(), pattern.last_verified.timestamp().into());
        payload.insert("created_at".to_string(), pattern.created_at.timestamp().into());
        
        if let Some(ref skill_id) = pattern.skill_id {
            payload.insert("skill_id".to_string(), skill_id.clone().into());
        }
        
        // Store tags as JSON string
        let tags_json = serde_json::to_string(&pattern.tags).unwrap_or_else(|_| "[]".to_string());
        payload.insert("tags".to_string(), tags_json.into());
        
        payload
    }
    
    /// Convert Qdrant payload to CortexPattern
    fn payload_to_pattern(payload: &HashMap<String, Value>) -> Result<CortexPattern> {
        let id_str = payload.get("id")
            .context("Missing id field")?
            .kind.as_ref().context("Missing id kind")?;
        let id = match id_str {
            qdrant_client::qdrant::value::Kind::StringValue(s) => Uuid::parse_str(s)?,
            _ => anyhow::bail!("Invalid id type"),
        };
        
        let error_type = Self::get_string_value(payload, "error_type")?;
        let error_message_hash = Self::get_string_value(payload, "error_message_hash")?;
        let solution_code = Self::get_string_value(payload, "solution_code")?;
        let task_category = Self::get_string_value(payload, "task_category")?;
        let success_score = Self::get_double_value(payload, "success_score")?;
        let execution_count = Self::get_integer_value(payload, "execution_count")? as u64;
        let weight = Self::get_double_value(payload, "weight")?;
        let last_verified_ts = Self::get_integer_value(payload, "last_verified")?;
        let created_at_ts = Self::get_integer_value(payload, "created_at")?;
        
        let skill_id = Self::get_string_value(payload, "skill_id").ok().filter(|s| !s.is_empty());
        
        let tags_json = Self::get_string_value(payload, "tags").unwrap_or_else(|_| "[]".to_string());
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        
        Ok(CortexPattern {
            id: PatternId(id),
            error_signature: ErrorSignature {
                error_type,
                error_message_hash,
            },
            solution_code,
            task_category,
            success_score,
            execution_count,
            weight,
            last_verified: chrono::DateTime::from_timestamp(last_verified_ts, 0)
                .context("Invalid last_verified timestamp")?,
            skill_id,
            created_at: chrono::DateTime::from_timestamp(created_at_ts, 0)
                .context("Invalid created_at timestamp")?,
            tags,
        })
    }
    
    fn get_string_value(payload: &HashMap<String, Value>, key: &str) -> Result<String> {
        let value = payload.get(key).with_context(|| format!("Missing field: {}", key))?;
        match &value.kind {
            Some(qdrant_client::qdrant::value::Kind::StringValue(s)) => Ok(s.clone()),
            _ => anyhow::bail!("Invalid type for field: {}", key),
        }
    }
    
    fn get_double_value(payload: &HashMap<String, Value>, key: &str) -> Result<f64> {
        let value = payload.get(key).with_context(|| format!("Missing field: {}", key))?;
        match &value.kind {
            Some(qdrant_client::qdrant::value::Kind::DoubleValue(d)) => Ok(*d),
            Some(qdrant_client::qdrant::value::Kind::IntegerValue(i)) => Ok(*i as f64),
            _ => anyhow::bail!("Invalid type for field: {}", key),
        }
    }
    
    fn get_integer_value(payload: &HashMap<String, Value>, key: &str) -> Result<i64> {
        let value = payload.get(key).with_context(|| format!("Missing field: {}", key))?;
        match &value.kind {
            Some(qdrant_client::qdrant::value::Kind::IntegerValue(i)) => Ok(*i),
            _ => anyhow::bail!("Invalid type for field: {}", key),
        }
    }
}

#[async_trait]
impl PatternRepository for QdrantPatternRepository {
    async fn store_pattern(&self, pattern: &CortexPattern, embedding: Vec<f32>) -> Result<PatternId> {
        // Validate embedding dimension
        if embedding.len() != VECTOR_DIM as usize {
            anyhow::bail!(
                "Invalid embedding dimension: expected {}, got {}",
                VECTOR_DIM,
                embedding.len()
            );
        }

        let payload = Self::pattern_to_payload(pattern);
        
        let point = PointStruct::new(
            pattern.id.0.to_string(),
            embedding,
            payload,
        );
        
        self.client
            .upsert_points(
                UpsertPointsBuilder::new(COLLECTION_NAME, vec![point])
            )
            .await
            .context("Failed to store pattern in Qdrant")?;
        
        Ok(pattern.id)
    }
    
    async fn search_similar(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<(CortexPattern, f64)>> {
        let search_result = self.client
            .search_points(
                SearchPointsBuilder::new(COLLECTION_NAME, query_embedding, limit as u64)
                    .with_payload(true)
            )
            .await
            .context("Failed to search patterns in Qdrant")?;
        
        let mut patterns = Vec::new();
        
        for scored_point in search_result.result {
            let pattern = Self::payload_to_pattern(&scored_point.payload)?;
            let similarity = scored_point.score as f64;
            patterns.push((pattern, similarity));
        }
        
        Ok(patterns)
    }
    
    async fn find_by_id(&self, id: PatternId) -> Result<Option<CortexPattern>> {
        let points = self.client
            .get_points(
                GetPointsBuilder::new(COLLECTION_NAME, vec![PointId::from(id.0.to_string())])
                    .with_payload(true)
            )
            .await
            .context("Failed to query pattern by ID")?;
        
        if let Some(point) = points.result.first() {
            let pattern = Self::payload_to_pattern(&point.payload)?;
            return Ok(Some(pattern));
        }
        
        Ok(None)
    }
    
    async fn update_pattern(&self, pattern: &CortexPattern) -> Result<()> {
        // Qdrant supports partial updates via set_payload
        let mut payload = HashMap::new();
        payload.insert("success_score".to_string(), pattern.success_score.into());
        payload.insert("execution_count".to_string(), (pattern.execution_count as i64).into());
        payload.insert("weight".to_string(), pattern.weight.into());
        payload.insert("last_verified".to_string(), pattern.last_verified.timestamp().into());
        
        self.client
            .set_payload(
                SetPayloadPointsBuilder::new(
                    COLLECTION_NAME,
                    payload,
                ).points_selector(vec![PointId::from(pattern.id.0.to_string())])
            )
            .await
            .context("Failed to update pattern in Qdrant")?;
        
        Ok(())
    }
    
    async fn delete_pattern(&self, id: PatternId) -> Result<()> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(COLLECTION_NAME)
                    .points(vec![PointId::from(id.0.to_string())])
            )
            .await
            .context("Failed to delete pattern from Qdrant")?;
        
        Ok(())
    }
    
    async fn find_by_error_signature(&self, signature: &ErrorSignature) -> Result<Vec<CortexPattern>> {
        let filter = Filter::must([
            Condition::matches("error_type", signature.error_type.clone()),
        ]);
        
        let scroll_result = self.client
            .scroll(
                ScrollPointsBuilder::new(COLLECTION_NAME)
                    .filter(filter)
                    .with_payload(true)
                    .limit(100)
            )
            .await
            .context("Failed to query patterns by error signature")?;
        
        let mut patterns = Vec::new();
        
        for point in scroll_result.result {
            let pattern = Self::payload_to_pattern(&point.payload)?;
            patterns.push(pattern);
        }
        
        Ok(patterns)
    }
    
    async fn get_all_patterns(&self) -> Result<Vec<CortexPattern>> {
        let mut patterns = Vec::new();
        let mut offset: Option<PointId> = None;
        let mut iteration_count = 0;
        const MAX_ITERATIONS: usize = 1000; // Safety limit to prevent infinite loops
        
        loop {
            if iteration_count >= MAX_ITERATIONS {
                tracing::warn!("Reached maximum iteration limit while scrolling patterns");
                break;
            }
            
            let mut builder = ScrollPointsBuilder::new(COLLECTION_NAME)
                .with_payload(true)
                .limit(100);
            
            if let Some(ref offset_id) = offset {
                builder = builder.offset(offset_id.clone());
            }
            
            let scroll_result = self.client
                .scroll(builder)
                .await
                .context("Failed to query all patterns")?;
            
            if scroll_result.result.is_empty() {
                break;
            }
            
            for point in &scroll_result.result {
                let pattern = Self::payload_to_pattern(&point.payload)?;
                patterns.push(pattern);
            }
            
            offset = scroll_result.next_page_offset;
            if offset.is_none() {
                break;
            }
            
            iteration_count += 1;
        }
        
        Ok(patterns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    
    fn create_test_pattern() -> CortexPattern {
        CortexPattern {
            id: PatternId::new(),
            error_signature: ErrorSignature {
                error_type: "NullPointerException".to_string(),
                error_message_hash: "abc123".to_string(),
            },
            solution_code: "if (obj != null) { obj.method(); }".to_string(),
            task_category: "error_handling".to_string(),
            success_score: 0.85,
            execution_count: 5,
            weight: 1.5,
            last_verified: Utc::now(),
            skill_id: None,
            created_at: Utc::now(),
            tags: vec!["java".to_string(), "null-check".to_string()],
        }
    }
    
    #[tokio::test]
    #[ignore] // Requires running Qdrant instance
    async fn test_store_and_retrieve_pattern() {
        let repo = QdrantPatternRepository::new("http://localhost:6334")
            .await
            .expect("Failed to create repository");
        
        repo.initialize().await.expect("Failed to initialize");
        
        let pattern = create_test_pattern();
        let embedding = vec![0.1; VECTOR_DIM as usize];
        
        let stored_id = repo.store_pattern(&pattern, embedding.clone())
            .await
            .expect("Failed to store pattern");
        
        assert_eq!(stored_id, pattern.id);
        
        let retrieved = repo.find_by_id(pattern.id)
            .await
            .expect("Failed to retrieve pattern");
        
        assert!(retrieved.is_some());
        let retrieved_pattern = retrieved.unwrap();
        assert_eq!(retrieved_pattern.id, pattern.id);
        assert_eq!(retrieved_pattern.error_signature.error_type, pattern.error_signature.error_type);
    }
    
    #[tokio::test]
    #[ignore] // Requires running Qdrant instance
    async fn test_search_similar() {
        let repo = QdrantPatternRepository::new("http://localhost:6334")
            .await
            .expect("Failed to create repository");
        
        repo.initialize().await.expect("Failed to initialize");
        
        let pattern = create_test_pattern();
        let embedding = vec![0.1; VECTOR_DIM as usize];
        
        repo.store_pattern(&pattern, embedding.clone())
            .await
            .expect("Failed to store pattern");
        
        let results = repo.search_similar(embedding, 5)
            .await
            .expect("Failed to search patterns");
        
        assert!(!results.is_empty());
        assert_eq!(results[0].0.id, pattern.id);
        assert!(results[0].1 > 0.9); // High similarity for same vector
    }
    
    #[tokio::test]
    #[ignore] // Requires running Qdrant instance
    async fn test_update_pattern() {
        let repo = QdrantPatternRepository::new("http://localhost:6334")
            .await
            .expect("Failed to create repository");
        
        repo.initialize().await.expect("Failed to initialize");
        
        let mut pattern = create_test_pattern();
        let embedding = vec![0.1; VECTOR_DIM as usize];
        
        repo.store_pattern(&pattern, embedding)
            .await
            .expect("Failed to store pattern");
        
        pattern.weight = 2.5;
        pattern.execution_count = 10;
        
        repo.update_pattern(&pattern)
            .await
            .expect("Failed to update pattern");
        
        let updated = repo.find_by_id(pattern.id)
            .await
            .expect("Failed to retrieve updated pattern")
            .expect("Pattern not found");
        
        assert_eq!(updated.weight, 2.5);
        assert_eq!(updated.execution_count, 10);
    }
    
    #[tokio::test]
    #[ignore] // Requires running Qdrant instance
    async fn test_delete_pattern() {
        let repo = QdrantPatternRepository::new("http://localhost:6334")
            .await
            .expect("Failed to create repository");
        
        repo.initialize().await.expect("Failed to initialize");
        
        let pattern = create_test_pattern();
        let embedding = vec![0.1; VECTOR_DIM as usize];
        
        repo.store_pattern(&pattern, embedding)
            .await
            .expect("Failed to store pattern");
        
        repo.delete_pattern(pattern.id)
            .await
            .expect("Failed to delete pattern");
        
        let retrieved = repo.find_by_id(pattern.id)
            .await
            .expect("Failed to query pattern");
        
        assert!(retrieved.is_none());
    }
}
