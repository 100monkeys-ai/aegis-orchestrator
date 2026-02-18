// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Qdrant vector store implementation for pattern storage
//! Implements ADR-018 (Weighted Cortex Memory)

use async_trait::async_trait;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;

use crate::domain::{CortexPattern, PatternId, ErrorSignature};
use crate::infrastructure::repository::PatternRepository;

/// In-memory implementation of PatternRepository for testing
/// TODO: Replace with actual Qdrant implementation
pub struct InMemoryPatternRepository {
    patterns: Arc<RwLock<HashMap<PatternId, (CortexPattern, Vec<f32>)>>>,
}

impl InMemoryPatternRepository {
    pub fn new() -> Self {
        Self {
            patterns: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Calculate cosine similarity between two vectors
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
        if a.len() != b.len() {
            return 0.0;
        }
        
        let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        
        if magnitude_a == 0.0 || magnitude_b == 0.0 {
            return 0.0;
        }
        
        (dot_product / (magnitude_a * magnitude_b)) as f64
    }
}

impl Default for InMemoryPatternRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PatternRepository for InMemoryPatternRepository {
    async fn store_pattern(&self, pattern: &CortexPattern, embedding: Vec<f32>) -> Result<PatternId> {
        let mut patterns = self.patterns.write().await;
        let id = pattern.id;
        patterns.insert(id, (pattern.clone(), embedding));
        Ok(id)
    }
    
    async fn search_similar(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<(CortexPattern, f64)>> {
        let patterns = self.patterns.read().await;
        
        let mut results: Vec<(CortexPattern, f64)> = patterns
            .values()
            .map(|(pattern, embedding)| {
                let similarity = Self::cosine_similarity(&query_embedding, embedding);
                (pattern.clone(), similarity)
            })
            .collect();
        
        // Sort by similarity descending
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        
        // Take top N
        results.truncate(limit);
        
        Ok(results)
    }
    
    async fn find_by_id(&self, id: PatternId) -> Result<Option<CortexPattern>> {
        let patterns = self.patterns.read().await;
        Ok(patterns.get(&id).map(|(pattern, _)| pattern.clone()))
    }
    
    async fn update_pattern(&self, pattern: &CortexPattern) -> Result<()> {
        let mut patterns = self.patterns.write().await;
        
        if let Some((_existing_pattern, embedding)) = patterns.get(&pattern.id) {
            let embedding = embedding.clone();
            patterns.insert(pattern.id, (pattern.clone(), embedding));
            Ok(())
        } else {
            anyhow::bail!("Pattern not found: {:?}", pattern.id)
        }
    }
    
    async fn delete_pattern(&self, id: PatternId) -> Result<()> {
        let mut patterns = self.patterns.write().await;
        patterns.remove(&id)
            .ok_or_else(|| anyhow::anyhow!("Pattern not found: {:?}", id))?;
        Ok(())
    }
    
    async fn find_by_error_signature(&self, signature: &ErrorSignature) -> Result<Vec<CortexPattern>> {
        let patterns = self.patterns.read().await;
        
        let results = patterns
            .values()
            .filter(|(pattern, _)| &pattern.error_signature == signature)
            .map(|(pattern, _)| pattern.clone())
            .collect();
        
        Ok(results)
    }
    
    async fn get_all_patterns(&self) -> Result<Vec<CortexPattern>> {
        let patterns = self.patterns.read().await;
        Ok(patterns.values().map(|(pattern, _)| pattern.clone()).collect())
    }
}

// TODO: Implement actual Qdrant repository
// This will require:
// 1. Add qdrant dependency to Cargo.toml
// 2. Create Qdrant connection and table
// 3. Implement vector search using Qdrant API
// 4. Handle schema migrations

/*
pub struct QdrantPatternRepository {
    connection: qdrant::Connection,
    table_name: String,
}

impl QdrantPatternRepository {
    pub async fn new(db_path: &str, table_name: &str) -> Result<Self> {
        let connection = qdrant::connect(db_path).execute().await?;
        Ok(Self {
            connection,
            table_name: table_name.to_string(),
        })
    }
    
    async fn ensure_table_exists(&self) -> Result<qdrant::Table> {
        // Create table if it doesn't exist
        // Schema: 
        // - id: UUID
        // - vector: [f32; 1536] (or configurable dimension)
        // - pattern_json: String (serialized CortexPattern)
        // - error_type: String (for filtering)
        // - weight: f64 (for sorting)
        // - success_score: f64 (for sorting)
        // - last_verified: DateTime (for time decay)
        todo!("Implement table creation")
    }
}

#[async_trait]
impl PatternRepository for QdrantPatternRepository {
    async fn store_pattern(&self, pattern: &CortexPattern, embedding: Vec<f32>) -> Result<PatternId> {
        let table = self.ensure_table_exists().await?;
        
        // Serialize pattern to JSON
        let pattern_json = serde_json::to_string(pattern)?;
        
        // Insert into Qdrant
        // table.add(...)
        
        todo!("Implement Qdrant storage")
    }
    
    async fn search_similar(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<(CortexPattern, f64)>> {
        let table = self.ensure_table_exists().await?;
        
        // Vector search using Qdrant
        // let results = table.search(&query_embedding).limit(limit).execute().await?;
        
        todo!("Implement Qdrant vector search")
    }
    
    // ... implement other methods
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pattern::ErrorSignature;
    
    #[tokio::test]
    async fn test_store_and_retrieve() {
        let repo = InMemoryPatternRepository::new();
        
        let signature = ErrorSignature::new("ImportError".to_string(), "No module named 'requests'");
        let pattern = CortexPattern::new(
            signature,
            "pip install requests".to_string(),
            "dependency".to_string(),
        );
        
        let embedding = vec![0.1, 0.2, 0.3, 0.4];
        
        let id = repo.store_pattern(&pattern, embedding).await.unwrap();
        
        let retrieved = repo.find_by_id(id).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, pattern.id);
    }
    
    #[tokio::test]
    async fn test_vector_search() {
        let repo = InMemoryPatternRepository::new();
        
        // Store some patterns
        let sig1 = ErrorSignature::new("ImportError".to_string(), "requests");
        let pattern1 = CortexPattern::new(sig1, "pip install requests".to_string(), "dep".to_string());
        repo.store_pattern(&pattern1, vec![1.0, 0.0, 0.0]).await.unwrap();
        
        let sig2 = ErrorSignature::new("ImportError".to_string(), "numpy");
        let pattern2 = CortexPattern::new(sig2, "pip install numpy".to_string(), "dep".to_string());
        repo.store_pattern(&pattern2, vec![0.0, 1.0, 0.0]).await.unwrap();
        
        // Search with query similar to pattern1
        let results = repo.search_similar(vec![0.9, 0.1, 0.0], 1).await.unwrap();
        
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.id, pattern1.id);
        assert!(results[0].1 > 0.9); // High similarity
    }
    
    #[tokio::test]
    async fn test_update_pattern() {
        let repo = InMemoryPatternRepository::new();
        
        let signature = ErrorSignature::new("ImportError".to_string(), "requests");
        let mut pattern = CortexPattern::new(
            signature,
            "pip install requests".to_string(),
            "dep".to_string(),
        );
        
        let embedding = vec![0.1, 0.2, 0.3];
        repo.store_pattern(&pattern, embedding).await.unwrap();
        
        // Update weight
        pattern.increment_weight(1.0);
        repo.update_pattern(&pattern).await.unwrap();
        
        let retrieved = repo.find_by_id(pattern.id).await.unwrap().unwrap();
        assert_eq!(retrieved.weight, 2.0);
    }
    
    #[tokio::test]
    async fn test_find_by_error_signature() {
        let repo = InMemoryPatternRepository::new();
        
        let signature = ErrorSignature::new("ImportError".to_string(), "requests");
        let pattern1 = CortexPattern::new(
            signature.clone(),
            "pip install requests".to_string(),
            "dep".to_string(),
        );
        let pattern2 = CortexPattern::new(
            signature.clone(),
            "conda install requests".to_string(),
            "dep".to_string(),
        );
        
        repo.store_pattern(&pattern1, vec![0.1]).await.unwrap();
        repo.store_pattern(&pattern2, vec![0.2]).await.unwrap();
        
        let results = repo.find_by_error_signature(&signature).await.unwrap();
        assert_eq!(results.len(), 2);
    }
}
