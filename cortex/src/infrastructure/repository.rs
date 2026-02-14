// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Repository interfaces for Cortex bounded context
//! Defines the contracts for pattern and graph storage

use async_trait::async_trait;
use anyhow::Result;
use crate::domain::{CortexPattern, PatternId, ErrorSignature};
use crate::domain::graph::{CortexNode, CortexEdge, NodeId, EdgeId};

/// Repository for storing and retrieving patterns from vector store
#[async_trait]
pub trait PatternRepository: Send + Sync {
    /// Store a new pattern with its embedding vector
    async fn store_pattern(&self, pattern: &CortexPattern, embedding: Vec<f32>) -> Result<PatternId>;
    
    /// Search for similar patterns using vector similarity
    /// Returns patterns with their similarity scores (0.0-1.0)
    async fn search_similar(
        &self,
        query_embedding: Vec<f32>,
        limit: usize,
    ) -> Result<Vec<(CortexPattern, f64)>>;
    
    /// Find a pattern by its ID
    async fn find_by_id(&self, id: PatternId) -> Result<Option<CortexPattern>>;
    
    /// Update an existing pattern (for weight/score updates)
    async fn update_pattern(&self, pattern: &CortexPattern) -> Result<()>;
    
    /// Delete a pattern (for pruning)
    async fn delete_pattern(&self, id: PatternId) -> Result<()>;
    
    /// Find patterns by error signature (for deduplication check)
    async fn find_by_error_signature(&self, signature: &ErrorSignature) -> Result<Vec<CortexPattern>>;
    
    /// Get all patterns (for batch processing/consolidation)
    async fn get_all_patterns(&self) -> Result<Vec<CortexPattern>>;
}

/// Repository for storing and retrieving nodes from knowledge graph
#[async_trait]
pub trait GraphRepository: Send + Sync {
    /// Create a new node in the graph
    async fn create_node(&self, node: &CortexNode) -> Result<NodeId>;
    
    /// Create a new edge in the graph
    async fn create_edge(&self, edge: &CortexEdge) -> Result<EdgeId>;
    
    /// Find a node by its ID
    async fn find_node(&self, id: NodeId) -> Result<Option<CortexNode>>;
    
    /// Find an edge by its ID
    async fn find_edge(&self, id: EdgeId) -> Result<Option<CortexEdge>>;
    
    /// Update a node (for weight updates)
    async fn update_node(&self, node: &CortexNode) -> Result<()>;
    
    /// Update an edge (for weight updates)
    async fn update_edge(&self, edge: &CortexEdge) -> Result<()>;
    
    /// Delete a node
    async fn delete_node(&self, id: NodeId) -> Result<()>;
    
    /// Delete an edge
    async fn delete_edge(&self, id: EdgeId) -> Result<()>;
    
    /// Execute a Cypher query and return matching nodes
    async fn query_nodes(&self, cypher: &str) -> Result<Vec<CortexNode>>;
    
    /// Find all edges from a node
    async fn find_edges_from(&self, node_id: NodeId) -> Result<Vec<CortexEdge>>;
    
    /// Find all edges to a node
    async fn find_edges_to(&self, node_id: NodeId) -> Result<Vec<CortexEdge>>;
    
    /// Multi-hop traversal: find nodes reachable from a starting node
    async fn traverse(
        &self,
        start_node: NodeId,
        max_depth: usize,
    ) -> Result<Vec<CortexNode>>;
}
