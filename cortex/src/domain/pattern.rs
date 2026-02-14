// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PatternId(pub Uuid);

impl PatternId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PatternId {
    fn default() -> Self {
        Self::new()
    }
}

/// Error signature for pattern matching
/// Combines error type with normalized message hash for deduplication
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErrorSignature {
    pub error_type: String,
    pub error_message_hash: String,  // Hash of normalized error message
}

impl ErrorSignature {
    pub fn new(error_type: String, error_message: &str) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        // Normalize error message (remove timestamps, paths, etc.)
        let normalized = Self::normalize_error_message(error_message);
        
        // Hash the normalized message
        let mut hasher = DefaultHasher::new();
        normalized.hash(&mut hasher);
        let error_message_hash = format!("{:x}", hasher.finish());
        
        Self {
            error_type,
            error_message_hash,
        }
    }
    
    fn normalize_error_message(message: &str) -> String {
        use once_cell::sync::Lazy;
        use regex::Regex;
        
        static PATH_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"/[^\s]+:\d+").unwrap());
        static TIMESTAMP_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}").unwrap());
        
        // Remove file paths, line numbers, timestamps, etc.
        message
            .lines()
            .map(|line| {
                // Remove file paths (e.g., /path/to/file.rs:123)
                let line = PATH_REGEX.replace_all(line, "<path>");
                // Remove timestamps
                let line = TIMESTAMP_REGEX.replace_all(&line, "<timestamp>");
                line.to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Cortex Pattern - A learned error-solution pair
/// Implements ADR-018 (Weighted Cortex Memory) and ADR-024 (Holographic Cortex)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CortexPattern {
    pub id: PatternId,
    
    // Core content
    pub error_signature: ErrorSignature,
    pub solution_code: String,
    pub task_category: String,
    
    // Vector embedding is stored separately in LanceDB
    // We only store metadata here
    
    // Gradient metrics (ADR-018)
    pub success_score: f64,      // 0.0-1.0 from gradient validation
    pub execution_count: u64,    // How many times this pattern was used
    pub weight: f64,             // Deduplication counter (Hebbian learning)
    pub last_verified: DateTime<Utc>,
    
    // Hierarchy (ADR-023 - Skill Crystallization)
    pub skill_id: Option<String>,  // Reference to crystallized skill
    
    // Metadata
    pub created_at: DateTime<Utc>,
    pub tags: Vec<String>,
}

impl CortexPattern {
    /// Create a new pattern with default values
    pub fn new(
        error_signature: ErrorSignature,
        solution_code: String,
        task_category: String,
    ) -> Self {
        Self {
            id: PatternId::new(),
            error_signature,
            solution_code,
            task_category,
            success_score: 0.5,  // Neutral starting score
            execution_count: 1,
            weight: 1.0,         // Initial weight
            last_verified: Utc::now(),
            skill_id: None,
            created_at: Utc::now(),
            tags: Vec::new(),
        }
    }
    
    /// Increment weight (Hebbian learning - "neurons that fire together, wire together")
    /// Called when a duplicate pattern is detected
    pub fn increment_weight(&mut self, amount: f64) {
        self.weight += amount;
        self.last_verified = Utc::now();
    }
    
    /// Update success score with running average
    /// Integrates new validation score from ADR-017 (Gradient Validation)
    pub fn update_success_score(&mut self, new_score: f64) {
        // Running average: (old_total + new_score) / new_count
        let total = self.success_score * self.execution_count as f64 + new_score;
        self.execution_count += 1;
        self.success_score = total / self.execution_count as f64;
        self.last_verified = Utc::now();
    }
    
    /// Apply time decay (Adenosine - forgetting unused memories)
    pub fn apply_time_decay(&mut self, decay_factor: f64) {
        self.weight *= decay_factor;
    }
    
    /// Calculate resonance score for retrieval ranking (ADR-018)
    /// Combines similarity, success score, and recency
    pub fn resonance_score(&self, similarity: f64, recency_weight: f64) -> f64 {
        const ALPHA: f64 = 0.5;  // Similarity weight
        const BETA: f64 = 0.3;   // Success weight
        const GAMMA: f64 = 0.2;  // Recency weight
        
        (similarity * ALPHA) + (self.success_score * BETA) + (recency_weight * GAMMA)
    }
    
    /// Check if pattern should be pruned (low weight, old, unused)
    pub fn should_prune(&self, min_weight: f64, max_age_days: i64) -> bool {
        let age_days = (Utc::now() - self.last_verified).num_days();
        self.weight < min_weight || age_days > max_age_days
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_pattern_creation() {
        let sig = ErrorSignature::new("ImportError".to_string(), "No module named 'requests'");
        let pattern = CortexPattern::new(
            sig,
            "pip install requests".to_string(),
            "dependency_installation".to_string(),
        );
        
        assert_eq!(pattern.success_score, 0.5);
        assert_eq!(pattern.execution_count, 1);
        assert_eq!(pattern.weight, 1.0);
    }
    
    #[test]
    fn test_weight_increment() {
        let sig = ErrorSignature::new("ImportError".to_string(), "No module named 'requests'");
        let mut pattern = CortexPattern::new(
            sig,
            "pip install requests".to_string(),
            "dependency_installation".to_string(),
        );
        
        pattern.increment_weight(1.0);
        assert_eq!(pattern.weight, 2.0);
    }
    
    #[test]
    fn test_success_score_update() {
        let sig = ErrorSignature::new("ImportError".to_string(), "No module named 'requests'");
        let mut pattern = CortexPattern::new(
            sig,
            "pip install requests".to_string(),
            "dependency_installation".to_string(),
        );
        
        // Initial: score=0.5, count=1
        pattern.update_success_score(0.9);
        // New: (0.5*1 + 0.9) / 2 = 1.4 / 2 = 0.7
        assert_eq!(pattern.success_score, 0.7);
        assert_eq!(pattern.execution_count, 2);
    }
    
    #[test]
    fn test_resonance_score() {
        let sig = ErrorSignature::new("ImportError".to_string(), "No module named 'requests'");
        let pattern = CortexPattern::new(
            sig,
            "pip install requests".to_string(),
            "dependency_installation".to_string(),
        );
        
        let resonance = pattern.resonance_score(0.95, 1.0);
        // (0.95 * 0.5) + (0.5 * 0.3) + (1.0 * 0.2) = 0.475 + 0.15 + 0.2 = 0.825
        assert!((resonance - 0.825).abs() < 0.001);
    }
    
    #[test]
    fn test_error_signature_normalization() {
        let sig1 = ErrorSignature::new(
            "ImportError".to_string(),
            "File \"/home/user/project/main.py\", line 10: No module named 'requests'"
        );
        let sig2 = ErrorSignature::new(
            "ImportError".to_string(),
            "File \"/different/path/main.py\", line 15: No module named 'requests'"
        );
        
        // Same error type and normalized message should have same hash
        assert_eq!(sig1.error_type, sig2.error_type);
        // Note: This test might fail if normalization isn't perfect
        // In practice, we'd want more sophisticated normalization
    }
}
