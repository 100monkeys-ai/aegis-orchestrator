// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Qdrant Repo
//!
//! Provides qdrant repo functionality for the system.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure Layer
//! - **Purpose:** Implements qdrant repo

// ============================================================================
// ADR-024: Holographic Cortex Memory Architecture
// ============================================================================
// This module implements the pattern storage layer using Qdrant vector database.
// Status: Phase 1 Core Implementation (in progress)
// 
// The Qdrant backend provides semantic similarity search for cortex patterns,
// enabling the learning system to find applicable solutions to novel errors.
// See: adrs/024-holographic-cortex-memory-architecture.md
// ============================================================================

//! Qdrant production implementation for pattern storage
//! 
//! This module provides a production-ready vector store using Qdrant
//! for semantic similarity search of cortex patterns.

use async_trait::async_trait;
use anyhow::{Result, Context as _};
use qdrant::{Connection, Table};
use arrow::array::{ArrayRef, Float32Array, StringArray, UInt64Array, RecordBatch};
use arrow::datatypes::{Schema, Field, DataType};
use std::sync::Arc;

use crate::domain::{CortexPattern, PatternId, ErrorSignature};
use crate::infrastructure::repository::PatternRepository;

pub struct QdrantPatternRepository {
    connection: Connection,
    table_name: String,
}

impl QdrantPatternRepository {
    /// Create a new Qdrant pattern repository
    pub async fn new(db_path: &str, table_name: &str) -> Result<Self> {
        let connection = qdrant::connect(db_path)
            .execute()
            .await
            .context("Failed to connect to Qdrant")?;
        
        Ok(Self {
            connection,
            table_name: table_name.to_string(),
        })
    }
    
    /// Ensure the patterns table exists with correct schema
    async fn ensure_table(&self) -> Result<Table> {
        // Define schema for pattern storage
        let schema = Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("embedding", DataType::List(Arc::new(Field::new("item", DataType::Float32, true))), false),
            Field::new("pattern_json", DataType::Utf8, false),
            Field::new("error_type", DataType::Utf8, false),
            Field::new("task_category", DataType::Utf8, false),
            Field::new("weight", DataType::Float64, false),
            Field::new("success_score", DataType::Float64, false),
            Field::new("execution_count", DataType::UInt64, false),
            Field::new("last_verified_ts", DataType::Int64, false),
        ]);
        
        // Try to open existing table, create if doesn't exist
        match self.connection.open_table(&self.table_name).execute().await {
            Ok(table) => Ok(table),
            Err(_) => {
                // Table doesn't exist, create it
                // For now, we'll create an empty table
                // In production, you'd want to handle initial data
                self.connection
                    .create_table(&self.table_name, RecordBatch::new_empty(Arc::new(schema)))
                    .execute()
                    .await
                    .context("Failed to create patterns table")
            }
        }
    }
    
    /// Convert CortexPattern to Arrow RecordBatch
    fn pattern_to_record_batch(pattern: &CortexPattern, embedding: &[f32]) -> Result<RecordBatch> {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("embedding", DataType::List(Arc::new(Field::new("item", DataType::Float32, true))), false),
            Field::new("pattern_json", DataType::Utf8, false),
            Field::new("error_type", DataType::Utf8, false),
            Field::new("task_category", DataType::Utf8, false),
            Field::new("weight", DataType::Float64, false),
            Field::new("success_score", DataType::Float64, false),
            Field::new("execution_count", DataType::UInt64, false),
            Field::new("last_verified_ts", DataType::Int64, false),
        ]);
        
        let pattern_json = serde_json::to_string(pattern)?;
        
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec![pattern.id.0.to_string()])),
            Arc::new(Float32Array::from(embedding.to_vec())),
            Arc::new(StringArray::from(vec![pattern_json])),
            Arc::new(StringArray::from(vec![pattern.error_signature.error_type.clone()])),
            Arc::new(StringArray::from(vec![pattern.task_category.clone()])),
            Arc::new(arrow::array::Float64Array::from(vec![pattern.weight])),
            Arc::new(arrow::array::Float64Array::from(vec![pattern.success_score])),
            Arc::new(UInt64Array::from(vec![pattern.execution_count as u64])),
            Arc::new(arrow::array::Int64Array::from(vec![pattern.last_verified.timestamp()])),
        ];
        
        RecordBatch::try_new(Arc::new(schema), arrays)
            .context("Failed to create record batch")
    }
}

#[async_trait]
impl PatternRepository for QdrantPatternRepository {
    async fn store_pattern(&self, pattern: &CortexPattern, embedding: Vec<f32>) -> Result<PatternId> {
        let table = self.ensure_table().await?;
        
        let batch = Self::pattern_to_record_batch(pattern, &embedding)?;
        
        table.add(vec![batch])
            .execute()
            .await
            .context("Failed to store pattern in Qdrant")?;
        
        Ok(pattern.id)
    }
    
    async fn search_similar(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<(CortexPattern, f64)>> {
        let table = self.ensure_table().await?;
        
        // Perform vector search
        let results = table
            .search(&query_embedding)
            .limit(limit)
            .execute()
            .await
            .context("Failed to search patterns")?;
        
        // Convert results to patterns
        let mut patterns = Vec::new();
        
        for batch in results {
            let pattern_json_array = batch
                .column_by_name("pattern_json")
                .context("Missing pattern_json column")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("Invalid pattern_json type")?;
            
            let distance_array = batch
                .column_by_name("_distance")
                .context("Missing _distance column")?
                .as_any()
                .downcast_ref::<arrow::array::Float32Array>()
                .context("Invalid _distance type")?;
            
            for i in 0..batch.num_rows() {
                let pattern_json = pattern_json_array.value(i);
                let distance = distance_array.value(i) as f64;
                
                let pattern: CortexPattern = serde_json::from_str(pattern_json)?;
                let similarity = 1.0 - distance; // Convert distance to similarity
                
                patterns.push((pattern, similarity));
            }
        }
        
        Ok(patterns)
    }
    
    async fn find_by_id(&self, id: PatternId) -> Result<Option<CortexPattern>> {
        let table = self.ensure_table().await?;
        
        // Query by ID
        let results = table
            .query()
            .filter(format!("id = '{}'", id.0))
            .execute()
            .await
            .context("Failed to query pattern by ID")?;
        
        for batch in results {
            if batch.num_rows() > 0 {
                let pattern_json_array = batch
                    .column_by_name("pattern_json")
                    .context("Missing pattern_json column")?
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .context("Invalid pattern_json type")?;
                
                let pattern_json = pattern_json_array.value(0);
                let pattern: CortexPattern = serde_json::from_str(pattern_json)?;
                return Ok(Some(pattern));
            }
        }
        
        Ok(None)
    }
    
    async fn update_pattern(&self, pattern: &CortexPattern) -> Result<()> {
        // Qdrant doesn't support in-place updates
        // We need to delete and re-insert
        // For production, consider using versioning or merge strategies
        
        // For now, we'll just note this limitation
        // A proper implementation would:
        // 1. Read the existing embedding
        // 2. Delete the old record
        // 3. Insert the updated pattern with same embedding
        
        anyhow::bail!("Update not yet implemented for Qdrant - use delete + store")
    }
    
    async fn delete_pattern(&self, id: PatternId) -> Result<()> {
        let table = self.ensure_table().await?;
        
        table
            .delete(&format!("id = '{}'", id.0))
            .await
            .context("Failed to delete pattern")?;
        
        Ok(())
    }
    
    async fn find_by_error_signature(&self, signature: &ErrorSignature) -> Result<Vec<CortexPattern>> {
        let table = self.ensure_table().await?;
        
        let results = table
            .query()
            .filter(format!("error_type = '{}'", signature.error_type))
            .execute()
            .await
            .context("Failed to query patterns by error signature")?;
        
        let mut patterns = Vec::new();
        
        for batch in results {
            let pattern_json_array = batch
                .column_by_name("pattern_json")
                .context("Missing pattern_json column")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("Invalid pattern_json type")?;
            
            for i in 0..batch.num_rows() {
                let pattern_json = pattern_json_array.value(i);
                let pattern: CortexPattern = serde_json::from_str(pattern_json)?;
                patterns.push(pattern);
            }
        }
        
        Ok(patterns)
    }
    
    async fn get_all_patterns(&self) -> Result<Vec<CortexPattern>> {
        let table = self.ensure_table().await?;
        
        let results = table
            .query()
            .execute()
            .await
            .context("Failed to query all patterns")?;
        
        let mut patterns = Vec::new();
        
        for batch in results {
            let pattern_json_array = batch
                .column_by_name("pattern_json")
                .context("Missing pattern_json column")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("Invalid pattern_json type")?;
            
            for i in 0..batch.num_rows() {
                let pattern_json = pattern_json_array.value(i);
                let pattern: CortexPattern = serde_json::from_str(pattern_json)?;
                patterns.push(pattern);
            }
        }
        
        Ok(patterns)
    }
}


