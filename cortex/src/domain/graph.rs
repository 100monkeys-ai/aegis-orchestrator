// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Knowledge Graph entities for the Cortex
//! Implements ADR-024 (Holographic Cortex Memory Architecture)

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use serde_json::Value;

/// Node identifier in the knowledge graph
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for NodeId {
    fn default() -> Self {
        Self::new()
    }
}

/// Edge identifier in the knowledge graph
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EdgeId(pub Uuid);

impl EdgeId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EdgeId {
    fn default() -> Self {
        Self::new()
    }
}

/// Node in the knowledge graph
/// Represents entities like Agents, Tools, Errors, Concepts, Skills, Users
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CortexNode {
    pub id: NodeId,
    pub node_type: NodeType,
    pub properties: HashMap<String, Value>,
    pub weight: f64,  // Holographic weight (Hebbian learning)
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
}

impl CortexNode {
    pub fn new(node_type: NodeType) -> Self {
        Self {
            id: NodeId::new(),
            node_type,
            properties: HashMap::new(),
            weight: 1.0,  // Initial weight
            created_at: Utc::now(),
            last_accessed: Utc::now(),
        }
    }
    
    /// Increment weight (Dopamine - success reinforcement)
    pub fn apply_dopamine(&mut self, amount: f64) {
        self.weight += amount;
        self.last_accessed = Utc::now();
    }
    
    /// Decrement weight (Cortisol - failure penalty)
    pub fn apply_cortisol(&mut self, penalty: f64) {
        self.weight = (self.weight - penalty).max(0.1);  // Minimum 0.1
        self.last_accessed = Utc::now();
    }
    
    /// Apply time decay (Adenosine - forgetting)
    pub fn apply_time_decay(&mut self, decay_factor: f64) {
        self.weight *= decay_factor;
    }
    
    /// Check if node should be pruned
    pub fn should_prune(&self, min_weight: f64, max_age_days: i64) -> bool {
        let age_days = (Utc::now() - self.last_accessed).num_days();
        self.weight < min_weight || age_days > max_age_days
    }
    
    /// Set a property
    pub fn set_property(&mut self, key: String, value: Value) {
        self.properties.insert(key, value);
    }
    
    /// Get a property
    pub fn get_property(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }
}

/// Type of node in the knowledge graph
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeType {
    /// An agent in the system
    Agent {
        agent_id: String,
        agent_name: String,
    },
    
    /// A tool or capability
    Tool {
        name: String,
        description: String,
    },
    
    /// An error type
    Error {
        signature: String,
        error_type: String,
    },
    
    /// An abstract concept
    Concept {
        name: String,
        description: String,
    },
    
    /// A crystallized skill (from ADR-023)
    Skill {
        skill_id: String,
        name: String,
        runtime: String,
    },
    
    /// A user
    User {
        user_id: String,
        username: String,
    },
}

/// Edge in the knowledge graph
/// Represents relationships between nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CortexEdge {
    pub id: EdgeId,
    pub from_node: NodeId,
    pub to_node: NodeId,
    pub edge_type: EdgeType,
    pub weight: f64,  // Reinforcement weight
    pub created_at: DateTime<Utc>,
    pub last_used: DateTime<Utc>,
    pub properties: HashMap<String, Value>,
}

impl CortexEdge {
    pub fn new(from_node: NodeId, to_node: NodeId, edge_type: EdgeType) -> Self {
        Self {
            id: EdgeId::new(),
            from_node,
            to_node,
            edge_type,
            weight: 1.0,  // Initial weight
            created_at: Utc::now(),
            last_used: Utc::now(),
            properties: HashMap::new(),
        }
    }
    
    /// Increment weight (reinforcement)
    pub fn reinforce(&mut self, amount: f64) {
        self.weight += amount;
        self.last_used = Utc::now();
    }
    
    /// Decrement weight (weakening)
    pub fn weaken(&mut self, amount: f64) {
        self.weight = (self.weight - amount).max(0.1);  // Minimum 0.1
        self.last_used = Utc::now();
    }
    
    /// Apply time decay
    pub fn apply_time_decay(&mut self, decay_factor: f64) {
        self.weight *= decay_factor;
    }
    
    /// Check if edge should be pruned
    pub fn should_prune(&self, min_weight: f64, max_age_days: i64) -> bool {
        let age_days = (Utc::now() - self.last_used).num_days();
        self.weight < min_weight || age_days > max_age_days
    }
    
    /// Set a property
    pub fn set_property(&mut self, key: String, value: Value) {
        self.properties.insert(key, value);
    }
    
    /// Get a property
    pub fn get_property(&self, key: &str) -> Option<&Value> {
        self.properties.get(key)
    }
}

/// Type of edge in the knowledge graph
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeType {
    /// Agent created by user
    CreatedBy,
    
    /// Execution failed with error
    FailedWith,
    
    /// Error solved by pattern/agent
    SolvedBy,
    
    /// Agent depends on tool
    DependsOn,
    
    /// Skill crystallized from pattern (ADR-023)
    CrystallizedFrom,
    
    /// Concept inhibits another concept (GABA - negative edge)
    Inhibits,
    
    /// Agent used tool
    UsedTool,
    
    /// Pattern belongs to category
    BelongsTo,
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_node_creation() {
        let node = CortexNode::new(NodeType::Agent {
            agent_id: "agent-123".to_string(),
            agent_name: "TestAgent".to_string(),
        });
        
        assert_eq!(node.weight, 1.0);
        assert!(node.properties.is_empty());
    }
    
    #[test]
    fn test_dopamine_application() {
        let mut node = CortexNode::new(NodeType::Concept {
            name: "test".to_string(),
            description: "test concept".to_string(),
        });
        
        node.apply_dopamine(0.5);
        assert_eq!(node.weight, 1.5);
    }
    
    #[test]
    fn test_cortisol_application() {
        let mut node = CortexNode::new(NodeType::Concept {
            name: "test".to_string(),
            description: "test concept".to_string(),
        });
        
        node.apply_cortisol(0.5);
        assert_eq!(node.weight, 0.5);
        
        // Test minimum weight
        node.apply_cortisol(1.0);
        assert_eq!(node.weight, 0.1);  // Should not go below 0.1
    }
    
    #[test]
    fn test_edge_creation() {
        let from = NodeId::new();
        let to = NodeId::new();
        let edge = CortexEdge::new(from, to, EdgeType::DependsOn);
        
        assert_eq!(edge.weight, 1.0);
        assert_eq!(edge.from_node, from);
        assert_eq!(edge.to_node, to);
    }
    
    #[test]
    fn test_edge_reinforcement() {
        let from = NodeId::new();
        let to = NodeId::new();
        let mut edge = CortexEdge::new(from, to, EdgeType::SolvedBy);
        
        edge.reinforce(0.5);
        assert_eq!(edge.weight, 1.5);
    }
    
    #[test]
    fn test_node_properties() {
        let mut node = CortexNode::new(NodeType::Tool {
            name: "test-tool".to_string(),
            description: "A test tool".to_string(),
        });
        
        node.set_property("version".to_string(), Value::String("1.0.0".to_string()));
        assert_eq!(
            node.get_property("version"),
            Some(&Value::String("1.0.0".to_string()))
        );
    }
}
