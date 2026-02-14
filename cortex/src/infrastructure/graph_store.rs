// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! In-memory graph repository implementation
//! TODO: Replace with Neo4j implementation for production

use async_trait::async_trait;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;

use crate::domain::graph::{CortexNode, CortexEdge, NodeId, EdgeId};
use crate::infrastructure::repository::GraphRepository;

/// In-memory implementation of GraphRepository for testing
pub struct InMemoryGraphRepository {
    nodes: Arc<RwLock<HashMap<NodeId, CortexNode>>>,
    edges: Arc<RwLock<HashMap<EdgeId, CortexEdge>>>,
}

impl InMemoryGraphRepository {
    pub fn new() -> Self {
        Self {
            nodes: Arc::new(RwLock::new(HashMap::new())),
            edges: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryGraphRepository {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GraphRepository for InMemoryGraphRepository {
    async fn create_node(&self, node: &CortexNode) -> Result<NodeId> {
        let mut nodes = self.nodes.write().await;
        let id = node.id;
        nodes.insert(id, node.clone());
        Ok(id)
    }
    
    async fn create_edge(&self, edge: &CortexEdge) -> Result<EdgeId> {
        let mut edges = self.edges.write().await;
        let id = edge.id;
        edges.insert(id, edge.clone());
        Ok(id)
    }
    
    async fn find_node(&self, id: NodeId) -> Result<Option<CortexNode>> {
        let nodes = self.nodes.read().await;
        Ok(nodes.get(&id).cloned())
    }
    
    async fn find_edge(&self, id: EdgeId) -> Result<Option<CortexEdge>> {
        let edges = self.edges.read().await;
        Ok(edges.get(&id).cloned())
    }
    
    async fn update_node(&self, node: &CortexNode) -> Result<()> {
        let mut nodes = self.nodes.write().await;
        
        if nodes.contains_key(&node.id) {
            nodes.insert(node.id, node.clone());
            Ok(())
        } else {
            anyhow::bail!("Node not found: {:?}", node.id)
        }
    }
    
    async fn update_edge(&self, edge: &CortexEdge) -> Result<()> {
        let mut edges = self.edges.write().await;
        
        if edges.contains_key(&edge.id) {
            edges.insert(edge.id, edge.clone());
            Ok(())
        } else {
            anyhow::bail!("Edge not found: {:?}", edge.id)
        }
    }
    
    async fn delete_node(&self, id: NodeId) -> Result<()> {
        let mut nodes = self.nodes.write().await;
        nodes.remove(&id)
            .ok_or_else(|| anyhow::anyhow!("Node not found: {:?}", id))?;
        Ok(())
    }
    
    async fn delete_edge(&self, id: EdgeId) -> Result<()> {
        let mut edges = self.edges.write().await;
        edges.remove(&id)
            .ok_or_else(|| anyhow::anyhow!("Edge not found: {:?}", id))?;
        Ok(())
    }
    
    async fn query_nodes(&self, _cypher: &str) -> Result<Vec<CortexNode>> {
        // Simple implementation: return all nodes
        // TODO: Implement actual Cypher query parsing
        let nodes = self.nodes.read().await;
        Ok(nodes.values().cloned().collect())
    }
    
    async fn find_edges_from(&self, node_id: NodeId) -> Result<Vec<CortexEdge>> {
        let edges = self.edges.read().await;
        Ok(edges
            .values()
            .filter(|edge| edge.from_node == node_id)
            .cloned()
            .collect())
    }
    
    async fn find_edges_to(&self, node_id: NodeId) -> Result<Vec<CortexEdge>> {
        let edges = self.edges.read().await;
        Ok(edges
            .values()
            .filter(|edge| edge.to_node == node_id)
            .cloned()
            .collect())
    }
    
    async fn traverse(
        &self,
        start_node: NodeId,
        max_depth: usize,
    ) -> Result<Vec<CortexNode>> {
        let nodes = self.nodes.read().await;
        let edges = self.edges.read().await;
        
        let mut visited = HashMap::new();
        let mut to_visit = vec![(start_node, 0)];
        let mut result = Vec::new();
        
        while let Some((current_id, depth)) = to_visit.pop() {
            if depth > max_depth {
                continue;
            }
            
            if visited.contains_key(&current_id) {
                continue;
            }
            
            if let Some(node) = nodes.get(&current_id) {
                visited.insert(current_id, true);
                result.push(node.clone());
                
                // Find all outgoing edges
                for edge in edges.values() {
                    if edge.from_node == current_id {
                        to_visit.push((edge.to_node, depth + 1));
                    }
                }
            }
        }
        
        Ok(result)
    }
}

// TODO: Implement Neo4j repository
/*
pub struct Neo4jGraphRepository {
    graph: neo4rs::Graph,
}

impl Neo4jGraphRepository {
    pub async fn new(uri: &str, user: &str, password: &str) -> Result<Self> {
        let graph = neo4rs::Graph::new(uri, user, password).await?;
        Ok(Self { graph })
    }
}

#[async_trait]
impl GraphRepository for Neo4jGraphRepository {
    async fn create_node(&self, node: &CortexNode) -> Result<NodeId> {
        // Cypher: CREATE (n:CortexNode {id: $id, ...}) RETURN n
        todo!("Implement Neo4j node creation")
    }
    
    async fn create_edge(&self, edge: &CortexEdge) -> Result<EdgeId> {
        // Cypher: MATCH (a:CortexNode {id: $from}), (b:CortexNode {id: $to})
        //         CREATE (a)-[r:EDGE_TYPE {id: $id, ...}]->(b)
        //         RETURN r
        todo!("Implement Neo4j edge creation")
    }
    
    async fn query_nodes(&self, cypher: &str) -> Result<Vec<CortexNode>> {
        // Execute Cypher query
        // let mut result = self.graph.execute(neo4rs::query(cypher)).await?;
        todo!("Implement Cypher query execution")
    }
    
    async fn traverse(
        &self,
        start_node: NodeId,
        max_depth: usize,
    ) -> Result<Vec<CortexNode>> {
        // Cypher: MATCH path = (start:CortexNode {id: $id})-[*1..${max_depth}]->(n)
        //         RETURN DISTINCT n
        todo!("Implement Neo4j traversal")
    }
    
    // ... implement other methods
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::graph::{NodeType, EdgeType};
    
    #[tokio::test]
    async fn test_create_and_find_node() {
        let repo = InMemoryGraphRepository::new();
        
        let node = CortexNode::new(NodeType::Agent {
            agent_id: "agent-123".to_string(),
            agent_name: "TestAgent".to_string(),
        });
        
        let id = repo.create_node(&node).await.unwrap();
        
        let found = repo.find_node(id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, node.id);
    }
    
    #[tokio::test]
    async fn test_create_and_find_edge() {
        let repo = InMemoryGraphRepository::new();
        
        let node1 = CortexNode::new(NodeType::Agent {
            agent_id: "agent-1".to_string(),
            agent_name: "Agent1".to_string(),
        });
        let node2 = CortexNode::new(NodeType::Tool {
            name: "tool-1".to_string(),
            description: "A tool".to_string(),
        });
        
        repo.create_node(&node1).await.unwrap();
        repo.create_node(&node2).await.unwrap();
        
        let edge = CortexEdge::new(node1.id, node2.id, EdgeType::DependsOn);
        let edge_id = repo.create_edge(&edge).await.unwrap();
        
        let found = repo.find_edge(edge_id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().from_node, node1.id);
    }
    
    #[tokio::test]
    async fn test_find_edges_from() {
        let repo = InMemoryGraphRepository::new();
        
        let node1 = CortexNode::new(NodeType::Agent {
            agent_id: "agent-1".to_string(),
            agent_name: "Agent1".to_string(),
        });
        let node2 = CortexNode::new(NodeType::Tool {
            name: "tool-1".to_string(),
            description: "Tool 1".to_string(),
        });
        let node3 = CortexNode::new(NodeType::Tool {
            name: "tool-2".to_string(),
            description: "Tool 2".to_string(),
        });
        
        repo.create_node(&node1).await.unwrap();
        repo.create_node(&node2).await.unwrap();
        repo.create_node(&node3).await.unwrap();
        
        let edge1 = CortexEdge::new(node1.id, node2.id, EdgeType::DependsOn);
        let edge2 = CortexEdge::new(node1.id, node3.id, EdgeType::DependsOn);
        
        repo.create_edge(&edge1).await.unwrap();
        repo.create_edge(&edge2).await.unwrap();
        
        let edges = repo.find_edges_from(node1.id).await.unwrap();
        assert_eq!(edges.len(), 2);
    }
    
    #[tokio::test]
    async fn test_traverse() {
        let repo = InMemoryGraphRepository::new();
        
        // Create a simple graph: A -> B -> C
        let node_a = CortexNode::new(NodeType::Concept {
            name: "A".to_string(),
            description: "Node A".to_string(),
        });
        let node_b = CortexNode::new(NodeType::Concept {
            name: "B".to_string(),
            description: "Node B".to_string(),
        });
        let node_c = CortexNode::new(NodeType::Concept {
            name: "C".to_string(),
            description: "Node C".to_string(),
        });
        
        repo.create_node(&node_a).await.unwrap();
        repo.create_node(&node_b).await.unwrap();
        repo.create_node(&node_c).await.unwrap();
        
        let edge_ab = CortexEdge::new(node_a.id, node_b.id, EdgeType::DependsOn);
        let edge_bc = CortexEdge::new(node_b.id, node_c.id, EdgeType::DependsOn);
        
        repo.create_edge(&edge_ab).await.unwrap();
        repo.create_edge(&edge_bc).await.unwrap();
        
        // Traverse from A with max depth 2
        let nodes = repo.traverse(node_a.id, 2).await.unwrap();
        assert_eq!(nodes.len(), 3); // Should find A, B, C
        
        // Traverse from A with max depth 1
        let nodes = repo.traverse(node_a.id, 1).await.unwrap();
        assert_eq!(nodes.len(), 2); // Should find A, B only
    }
    
    #[tokio::test]
    async fn test_update_node_weight() {
        let repo = InMemoryGraphRepository::new();
        
        let mut node = CortexNode::new(NodeType::Concept {
            name: "test".to_string(),
            description: "Test node".to_string(),
        });
        
        repo.create_node(&node).await.unwrap();
        
        // Apply dopamine
        node.apply_dopamine(0.5);
        repo.update_node(&node).await.unwrap();
        
        let updated = repo.find_node(node.id).await.unwrap().unwrap();
        assert_eq!(updated.weight, 1.5);
    }
}
