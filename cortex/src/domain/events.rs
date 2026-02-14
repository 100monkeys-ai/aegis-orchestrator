// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Domain events for the Cortex bounded context
//! Implements event sourcing for learning and memory operations

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use super::pattern::PatternId;

/// Cortex domain events
/// These events are published to the EventBus for observability and integration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CortexEvent {
    // Pattern events (ADR-018: Weighted Cortex Memory)
    
    /// A new pattern was discovered and stored
    PatternDiscovered {
        pattern_id: PatternId,
        execution_id: Option<uuid::Uuid>,
        error_signature: String,
        task_category: String,
        timestamp: DateTime<Utc>,
    },
    
    /// Pattern weight was increased (deduplication or dopamine)
    PatternWeightIncreased {
        pattern_id: PatternId,
        execution_id: Option<uuid::Uuid>,
        old_weight: f64,
        new_weight: f64,
        reason: WeightIncreaseReason,
        timestamp: DateTime<Utc>,
    },
    
    /// Pattern success score was updated with new validation result
    PatternSuccessUpdated {
        pattern_id: PatternId,
        execution_id: Option<uuid::Uuid>,
        old_score: f64,
        new_score: f64,
        execution_count: u64,
        timestamp: DateTime<Utc>,
    },
    
    /// Pattern was pruned (removed due to low weight or age)
    PatternPruned {
        pattern_id: PatternId,
        final_weight: f64,
        age_days: i64,
        reason: String,
        timestamp: DateTime<Utc>,
    },
    
    // Skill events (ADR-023: Evolutionary Skill Crystallization)
    
    /// A skill was crystallized from patterns
    SkillCrystallized {
        skill_id: String,
        pattern_ids: Vec<PatternId>,
        success_rate: f64,
        timestamp: DateTime<Utc>,
    },
    
    /// A skill was evolved (improved through dreaming/self-improvement)
    SkillEvolved {
        skill_id: String,
        old_success_rate: f64,
        new_success_rate: f64,
        timestamp: DateTime<Utc>,
    },
    
    // Graph events (ADR-024: Holographic Cortex Memory Architecture)
    
    /// A node was created in the knowledge graph
    NodeCreated {
        node_id: String,
        node_type: String,
        properties: serde_json::Value,
        timestamp: DateTime<Utc>,
    },
    
    /// An edge was created in the knowledge graph
    EdgeCreated {
        edge_id: String,
        from_node: String,
        to_node: String,
        edge_type: String,
        timestamp: DateTime<Utc>,
    },
    
    /// Node weight was increased (neurochemistry - dopamine)
    NodeWeightIncreased {
        node_id: String,
        old_weight: f64,
        new_weight: f64,
        reason: String,  // "dopamine" | "consolidation"
        timestamp: DateTime<Utc>,
    },
    
    /// Edge was pruned (neurochemistry - adenosine decay)
    EdgePruned {
        edge_id: String,
        reason: String,  // "adenosine_decay" | "low_weight"
        timestamp: DateTime<Utc>,
    },
    
    // Sleep cycle events (ADR-024: Consolidation & Dreaming)
    
    /// Consolidation job completed
    ConsolidationCompleted {
        merged_nodes: usize,
        pruned_edges: usize,
        duration_ms: u64,
        timestamp: DateTime<Utc>,
    },
    
    /// Dreaming job completed (self-improvement)
    DreamingCompleted {
        improved_patterns: usize,
        failed_attempts: usize,
        duration_ms: u64,
        timestamp: DateTime<Utc>,
    },
}

/// Reason for weight increase
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeightIncreaseReason {
    /// Duplicate pattern detected (Hebbian learning)
    Deduplication,
    /// Success reinforcement (Neurochemistry - Dopamine)
    Dopamine,
    /// Manual weight adjustment
    Manual,
}

impl CortexEvent {
    /// Get the timestamp of the event
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            CortexEvent::PatternDiscovered { timestamp, .. } => *timestamp,
            CortexEvent::PatternWeightIncreased { timestamp, .. } => *timestamp,
            CortexEvent::PatternSuccessUpdated { timestamp, .. } => *timestamp,
            CortexEvent::PatternPruned { timestamp, .. } => *timestamp,
            CortexEvent::SkillCrystallized { timestamp, .. } => *timestamp,
            CortexEvent::SkillEvolved { timestamp, .. } => *timestamp,
            CortexEvent::NodeCreated { timestamp, .. } => *timestamp,
            CortexEvent::EdgeCreated { timestamp, .. } => *timestamp,
            CortexEvent::NodeWeightIncreased { timestamp, .. } => *timestamp,
            CortexEvent::EdgePruned { timestamp, .. } => *timestamp,
            CortexEvent::ConsolidationCompleted { timestamp, .. } => *timestamp,
            CortexEvent::DreamingCompleted { timestamp, .. } => *timestamp,
        }
    }
    
    /// Get the event type as a string
    pub fn event_type(&self) -> &'static str {
        match self {
            CortexEvent::PatternDiscovered { .. } => "pattern_discovered",
            CortexEvent::PatternWeightIncreased { .. } => "pattern_weight_increased",
            CortexEvent::PatternSuccessUpdated { .. } => "pattern_success_updated",
            CortexEvent::PatternPruned { .. } => "pattern_pruned",
            CortexEvent::SkillCrystallized { .. } => "skill_crystallized",
            CortexEvent::SkillEvolved { .. } => "skill_evolved",
            CortexEvent::NodeCreated { .. } => "node_created",
            CortexEvent::EdgeCreated { .. } => "edge_created",
            CortexEvent::NodeWeightIncreased { .. } => "node_weight_increased",
            CortexEvent::EdgePruned { .. } => "edge_pruned",
            CortexEvent::ConsolidationCompleted { .. } => "consolidation_completed",
            CortexEvent::DreamingCompleted { .. } => "dreaming_completed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    
    #[test]
    fn test_event_serialization() {
        let event = CortexEvent::PatternDiscovered {
            pattern_id: PatternId(Uuid::new_v4()),
            execution_id: None,
            error_signature: "ImportError: requests".to_string(),
            task_category: "dependency".to_string(),
            timestamp: Utc::now(),
        };
        
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: CortexEvent = serde_json::from_str(&json).unwrap();
        
        assert_eq!(event.event_type(), deserialized.event_type());
    }
    
    #[test]
    fn test_weight_increase_reason() {
        let event = CortexEvent::PatternWeightIncreased {
            pattern_id: PatternId(Uuid::new_v4()),
            execution_id: None,
            old_weight: 1.0,
            new_weight: 2.0,
            reason: WeightIncreaseReason::Deduplication,
            timestamp: Utc::now(),
        };
        
        assert_eq!(event.event_type(), "pattern_weight_increased");
    }
}
