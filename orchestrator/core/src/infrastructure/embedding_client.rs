// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Embedding Client (ADR-075)
//!
//! Port and adapters for generating 384-dimensional embedding vectors via the
//! `aegis-embedding-service`. Mirrors the same pattern from `aegis-cortex`.
//!
//! ## Graceful Degradation
//!
//! If the embedding service is unavailable at startup, the orchestrator runs
//! without discovery. The `HashEmbeddingClient` is a deterministic fallback
//! for development and testing only.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use tokio::sync::Mutex;
use tonic::transport::Channel;

use aegis_orchestrator_proto::embedding::embedding_service_client::EmbeddingServiceClient;
use aegis_orchestrator_proto::embedding::EmbeddingRequest;

// ---------------------------------------------------------------------------
// EmbeddingPort — trait abstracting embedding generation
// ---------------------------------------------------------------------------

/// Port for generating semantic embeddings from text.
///
/// Implementations may call an external gRPC service or use a local fallback.
#[async_trait]
pub trait EmbeddingPort: Send + Sync {
    /// Generate a 384-dimensional embedding vector for the given text.
    async fn generate_embedding(&self, text: &str) -> Result<Vec<f32>>;
}

// ---------------------------------------------------------------------------
// HashEmbeddingClient — deterministic hash-based fallback
// ---------------------------------------------------------------------------

/// Hash-based embedding client for testing and local development.
///
/// Produces a deterministic 384-dimensional vector derived from the input
/// text's hash. This does **not** capture semantic similarity — it is a
/// placeholder that lets the rest of the pipeline run without an external
/// embedding service.
pub struct HashEmbeddingClient;

impl HashEmbeddingClient {
    /// Create a new hash-based embedding client.
    pub fn new() -> Self {
        Self
    }
}

impl Default for HashEmbeddingClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EmbeddingPort for HashEmbeddingClient {
    async fn generate_embedding(&self, text: &str) -> Result<Vec<f32>> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();

        let embedding: Vec<f32> = (0..384)
            .map(|i| {
                let bit = (hash >> (i % 64)) & 1;
                bit as f32
            })
            .collect();

        Ok(embedding)
    }
}

// ---------------------------------------------------------------------------
// GrpcEmbeddingClient — real gRPC embedding service client
// ---------------------------------------------------------------------------

/// gRPC client that calls an external embedding service to generate semantic
/// vectors.
///
/// Wraps the tonic-generated `EmbeddingServiceClient` behind a `Mutex` so
/// the struct can satisfy `Send + Sync` without requiring `&mut self`.
pub struct GrpcEmbeddingClient {
    client: Mutex<EmbeddingServiceClient<Channel>>,
}

impl GrpcEmbeddingClient {
    /// Connect to the embedding service at the given URL.
    ///
    /// # Arguments
    ///
    /// * `url` — gRPC endpoint, e.g. `"http://localhost:50053"`.
    pub async fn connect(url: &str) -> Result<Self> {
        let client = EmbeddingServiceClient::connect(url.to_string()).await?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }
}

#[async_trait]
impl EmbeddingPort for GrpcEmbeddingClient {
    async fn generate_embedding(&self, text: &str) -> Result<Vec<f32>> {
        let request = tonic::Request::new(EmbeddingRequest {
            text: text.to_string(),
        });

        let response = self.client.lock().await.generate_embedding(request).await?;

        Ok(response.into_inner().embedding)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_generate_embedding() {
        let client = HashEmbeddingClient::new();
        let embedding = client
            .generate_embedding("test error message")
            .await
            .unwrap();

        assert_eq!(embedding.len(), 384);
    }

    #[tokio::test]
    async fn test_consistent_embeddings() {
        let client = HashEmbeddingClient::new();
        let emb1 = client.generate_embedding("same text").await.unwrap();
        let emb2 = client.generate_embedding("same text").await.unwrap();

        assert_eq!(emb1, emb2, "Same text should produce same embedding");
    }

    #[tokio::test]
    async fn test_trait_object_usage() {
        let client: Box<dyn EmbeddingPort> = Box::new(HashEmbeddingClient::new());
        let embedding = client
            .generate_embedding("trait object test")
            .await
            .unwrap();

        assert_eq!(embedding.len(), 384);
    }
}
