// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Gradient Validation Domain (BC-4, ADR-017)
//!
//! Implements the Gradient Validation System: instead of binary pass/fail,
//! agent outputs are scored on a continuous `0.0 – 1.0` scale allowing ranking,
//! threshold-based acceptance, and multi-judge consensus.
//!
//! ## Key Concepts
//!
//! | Type | Description |
//! |------|-------------|
//! | `ValidationScore` | `f64` between 0.0 (reject) and 1.0 (perfect) |
//! | `ValidationResult` | Score + confidence + optional judge breakdown |
//! | `ValidationConfig` | Threshold + validators declared in agent manifest |
//!
//! ## Judge Agent Integration
//!
//! Judge agents (ADR-016) produce `ValidationResult`s that feed into this
//! module. Multiple judges’ scores are aggregated via weighted average
//! (consensus) to produce the final gradient score for an iteration.
//!
//! See ADR-016 (Agent-as-Judge Pattern), ADR-017 (Gradient Validation System),
//! AGENTS.md §Gradient Validation Domain.

use serde::{Serialize, Deserialize};
use thiserror::Error;
use crate::domain::agent::AgentId;
use std::collections::HashMap;
use serde_json::Value;

/// Score between 0.0 and 1.0 representing confidence/quality
pub type ValidationScore = f64;

/// Result from a single judge's assessment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GradientResult {
    /// The score assigned by the judge (0.0 - 1.0)
    pub score: ValidationScore,
    
    /// How confident the judge is in their own assessment (0.0 - 1.0)
    pub confidence: f64,
    
    /// Textual explanation of why this score was given
    pub reasoning: String,
    
    /// Specific signals identified (e.g., "syntax_error", "security_risk")
    #[serde(default)]
    pub signals: Vec<ValidationSignal>,

    /// Extensible metadata for future enhancements
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationSignal {
    pub category: String,
    pub score: f64,
    pub message: String,
}

/// Aggregated result from multiple judges
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiJudgeConsensus {
    /// The final aggregated score
    pub final_score: ValidationScore,
    
    /// Agreement level among judges (0.0 - 1.0)
    /// High variance = low consensus confidence
    pub consensus_confidence: f64,
    
    /// Individual assessments from each judge
    pub individual_results: Vec<(AgentId, GradientResult)>,
    
    /// Strategy used to reach consensus (e.g., "average", "weighted", "strict")
    pub strategy: String,

    /// Extensible metadata for future enhancements
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Validation execution failed: {0}")]
    Execution(String),
    #[error("Consensus could not be reached: {0}")]
    NoConsensus(String),
    #[error("Invalid validation request: {0}")]
    InvalidRequest(String),
}

/// Request sent to a validator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRequest {
    pub content: String,
    pub criteria: String,
    pub context: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use std::collections::HashMap;

    fn make_gradient_result(score: f64, confidence: f64) -> GradientResult {
        GradientResult {
            score,
            confidence,
            reasoning: "test".to_string(),
            signals: vec![],
            metadata: HashMap::new(),
        }
    }

    // ── GradientResult ────────────────────────────────────────────────────────

    #[test]
    fn test_gradient_result_serialization() {
        let result = make_gradient_result(0.85, 0.9);
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: GradientResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.score, 0.85);
        assert_eq!(deserialized.confidence, 0.9);
    }

    #[test]
    fn test_gradient_result_with_signals() {
        let signal = ValidationSignal {
            category: "security".to_string(),
            score: 0.4,
            message: "potential SQL injection".to_string(),
        };
        let result = GradientResult {
            score: 0.4,
            confidence: 0.95,
            reasoning: "Found SQL injection risk".to_string(),
            signals: vec![signal],
            metadata: HashMap::new(),
        };
        assert_eq!(result.signals.len(), 1);
        assert_eq!(result.signals[0].category, "security");
    }

    #[test]
    fn test_gradient_result_with_metadata() {
        let mut result = make_gradient_result(0.7, 0.8);
        result.metadata.insert("judge_id".to_string(), serde_json::json!("judge-1"));
        assert_eq!(result.metadata.len(), 1);
    }

    // ── MultiJudgeConsensus ───────────────────────────────────────────────────

    #[test]
    fn test_multi_judge_consensus_serialization() {
        let consensus = MultiJudgeConsensus {
            final_score: 0.88,
            consensus_confidence: 0.92,
            individual_results: vec![
                (AgentId::new(), make_gradient_result(0.9, 0.95)),
                (AgentId::new(), make_gradient_result(0.85, 0.88)),
            ],
            strategy: "weighted_average".to_string(),
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string(&consensus).unwrap();
        let deserialized: MultiJudgeConsensus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.final_score, 0.88);
        assert_eq!(deserialized.individual_results.len(), 2);
        assert_eq!(deserialized.strategy, "weighted_average");
    }

    // ── ValidationError ───────────────────────────────────────────────────────

    #[test]
    fn test_validation_error_display() {
        let exec_err = ValidationError::Execution("timeout".to_string());
        assert!(exec_err.to_string().contains("timeout"));

        let consensus_err = ValidationError::NoConsensus("disagreement".to_string());
        assert!(consensus_err.to_string().contains("disagreement"));

        let req_err = ValidationError::InvalidRequest("missing content".to_string());
        assert!(req_err.to_string().contains("missing content"));
    }

    // ── ValidationRequest ─────────────────────────────────────────────────────

    #[test]
    fn test_validation_request_creation() {
        let req = ValidationRequest {
            content: "fn main() {}".to_string(),
            criteria: "valid rust".to_string(),
            context: Some(serde_json::json!({"language": "rust"})),
        };
        assert_eq!(req.content, "fn main() {}");
        assert!(req.context.is_some());
    }

    #[test]
    fn test_validation_request_serialization() {
        let req = ValidationRequest {
            content: "some code".to_string(),
            criteria: "quality check".to_string(),
            context: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: ValidationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.content, req.content);
        assert_eq!(deserialized.criteria, req.criteria);
        assert!(deserialized.context.is_none());
    }

    // ── ValidationSignal ──────────────────────────────────────────────────────

    #[test]
    fn test_validation_signal_serialization() {
        let signal = ValidationSignal {
            category: "performance".to_string(),
            score: 0.6,
            message: "O(n^2) loop detected".to_string(),
        };
        let json = serde_json::to_string(&signal).unwrap();
        let deserialized: ValidationSignal = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.category, "performance");
        assert_eq!(deserialized.score, 0.6);
    }
}
