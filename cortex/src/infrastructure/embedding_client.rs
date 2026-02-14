// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Embedding client for generating semantic embeddings
//! 
//! This module provides a client for the embedding service (sentence-transformers).
//! For now, uses a simple hash-based approach until the gRPC service is integrated.

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Client for generating embeddings
pub struct EmbeddingClient {
    // Future: gRPC client to embedding service
}

impl EmbeddingClient {
    /// Create a new embedding client
    pub fn new() -> Self {
        Self {}
    }
    
    /// Generate embedding for text
    /// 
    /// Currently uses hash-based approach for simplicity.
    /// TODO: Replace with gRPC call to sentence-transformers service
    pub async fn generate_embedding(&self, text: &str) -> Result<Vec<f32>> {
        // Simple hash-based embedding (384-dim to match all-MiniLM-L6-v2)
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        
        // Generate 384-dimensional vector from hash
        let embedding: Vec<f32> = (0..384)
            .map(|i| {
                let bit = (hash >> (i % 64)) & 1;
                bit as f32
            })
            .collect();
        
        Ok(embedding)
    }
}

impl Default for EmbeddingClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_generate_embedding() {
        let client = EmbeddingClient::new();
        let embedding = client.generate_embedding("test error message").await.unwrap();
        
        assert_eq!(embedding.len(), 384);
    }
    
    #[tokio::test]
    async fn test_consistent_embeddings() {
        let client = EmbeddingClient::new();
        let emb1 = client.generate_embedding("same text").await.unwrap();
        let emb2 = client.generate_embedding("same text").await.unwrap();
        
        assert_eq!(emb1, emb2, "Same text should produce same embedding");
    }
}
