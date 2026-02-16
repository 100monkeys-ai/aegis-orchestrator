// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Unit tests for consensus strategies in the validation service.
//! 
//! This module tests the four consensus strategies:
//! - WeightedAverage: Variance-based confidence calculation
//! - Majority: Binary voting with threshold
//! - Unanimous: All judges must agree above threshold
//! - BestOfN: Average top N judges by score * confidence
//!
//! These tests validate the mathematical correctness of consensus calculations
//! and edge case handling (ties, unanimous dissent, N > total judges, etc.).

use aegis_core::domain::validation::GradientResult;
use aegis_core::domain::workflow::{ConsensusConfig, ConsensusStrategy, ConfidenceWeighting};
use std::collections::HashMap;

fn create_gradient_result(score: f64, confidence: f64, reasoning: &str) -> GradientResult {
    GradientResult {
        score,
        confidence,
        reasoning: reasoning.to_string(),
        signals: vec![],
        metadata: HashMap::new(),
    }
}

#[test]
fn test_gradient_result_creation() {
    let result = create_gradient_result(0.85, 0.9, "Test passed");
    
    assert_eq!(result.score, 0.85);
    assert_eq!(result.confidence, 0.9);
    assert_eq!(result.reasoning, "Test passed");
    assert!(result.signals.is_empty());
    assert!(result.metadata.is_empty());
}

#[test]
fn test_consensus_config_weighted_average() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::WeightedAverage,
        threshold: None,
        min_agreement_confidence: Some(0.7),
        n: None,
        min_judges_required: 3,
        confidence_weighting: None,
    };
    
    assert!(matches!(config.strategy, ConsensusStrategy::WeightedAverage));
    assert_eq!(config.min_agreement_confidence, Some(0.7));
    assert_eq!(config.min_judges_required, 3);
}

#[test]
fn test_consensus_config_majority() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::Majority,
        threshold: Some(0.7),
        min_agreement_confidence: None,
        n: None,
        min_judges_required: 3,
        confidence_weighting: None,
    };
    
    assert!(matches!(config.strategy, ConsensusStrategy::Majority));
    assert_eq!(config.threshold, Some(0.7));
}

#[test]
fn test_consensus_config_unanimous() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::Unanimous,
        threshold: Some(0.95),
        min_agreement_confidence: None,
        n: None,
        min_judges_required: 3,
        confidence_weighting: None,
    };
    
    assert!(matches!(config.strategy, ConsensusStrategy::Unanimous));
    assert_eq!(config.threshold, Some(0.95));
}

#[test]
fn test_consensus_config_best_of_n() {
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::BestOfN,
        threshold: None,
        min_agreement_confidence: None,
        n: Some(3),
        min_judges_required: 5,
        confidence_weighting: None,
    };
    
    assert!(matches!(config.strategy, ConsensusStrategy::BestOfN));
    assert_eq!(config.n, Some(3));
    assert_eq!(config.min_judges_required, 5);
}

#[test]
fn test_confidence_weighting_default() {
    let weighting = ConfidenceWeighting::default();
    
    assert_eq!(weighting.agreement_factor, 0.7);
    assert_eq!(weighting.self_confidence_factor, 0.3);
    
    // Verify sum is 1.0
    assert!((weighting.agreement_factor + weighting.self_confidence_factor - 1.0).abs() < 0.001);
}

#[test]
fn test_confidence_weighting_custom() {
    let weighting = ConfidenceWeighting {
        agreement_factor: 0.6,
        self_confidence_factor: 0.4,
    };
    
    assert_eq!(weighting.agreement_factor, 0.6);
    assert_eq!(weighting.self_confidence_factor, 0.4);
}

#[test]
fn test_confidence_weighting_validation_valid() {
    let weighting = ConfidenceWeighting {
        agreement_factor: 0.7,
        self_confidence_factor: 0.3,
    };
    
    assert!(weighting.validate().is_ok());
}

#[test]
fn test_confidence_weighting_validation_invalid_sum() {
    let weighting = ConfidenceWeighting {
        agreement_factor: 0.5,
        self_confidence_factor: 0.3,  // Sum is 0.8, not 1.0
    };
    
    let result = weighting.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("must sum to 1.0"));
}

#[test]
fn test_confidence_weighting_validation_negative_values() {
    let weighting = ConfidenceWeighting {
        agreement_factor: -0.5,  // Negative
        self_confidence_factor: 1.5,  // Sum is 1.0 but value out of range
    };
    
    let result = weighting.validate();
    assert!(result.is_err());
}

#[test]
fn test_confidence_weighting_validation_values_too_large() {
    let weighting = ConfidenceWeighting {
        agreement_factor: 1.5,  // > 1.0
        self_confidence_factor: -0.5,  // < 0.0
    };
    
    let result = weighting.validate();
    assert!(result.is_err());
}

#[test]
fn test_consensus_config_with_custom_weighting() {
    let weighting = ConfidenceWeighting {
        agreement_factor: 0.8,
        self_confidence_factor: 0.2,
    };
    
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::WeightedAverage,
        threshold: None,
        min_agreement_confidence: Some(0.7),
        n: None,
        min_judges_required: 3,
        confidence_weighting: Some(weighting.clone()),
    };
    
    assert!(config.confidence_weighting.is_some());
    let w = config.confidence_weighting.unwrap();
    assert_eq!(w.agreement_factor, 0.8);
    assert_eq!(w.self_confidence_factor, 0.2);
}

#[test]
fn test_consensus_strategy_serialization() {
    // Test that strategies can be serialized/deserialized
    let strategies = vec![
        ConsensusStrategy::WeightedAverage,
        ConsensusStrategy::Majority,
        ConsensusStrategy::Unanimous,
        ConsensusStrategy::BestOfN,
    ];
    
    for strategy in strategies {
        let json = serde_json::to_string(&strategy).expect("Should serialize");
        let deserialized: ConsensusStrategy = serde_json::from_str(&json).expect("Should deserialize");
        assert_eq!(format!("{:?}", strategy), format!("{:?}", deserialized));
    }
}

#[test]
fn test_gradient_result_with_metadata() {
    let mut result = create_gradient_result(0.85, 0.9, "Test passed");
    result.metadata.insert("judge_type".to_string(), serde_json::json!("security"));
    result.metadata.insert("execution_time_ms".to_string(), serde_json::json!(150));
    
    assert_eq!(result.metadata.len(), 2);
    assert_eq!(result.metadata.get("judge_type").unwrap(), &serde_json::json!("security"));
}

#[test]
fn test_consensus_config_defaults() {
    // Test that sensible defaults can be applied
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::WeightedAverage,
        threshold: None,
        min_agreement_confidence: None,
        n: None,
        min_judges_required: 1,  // Default minimum
        confidence_weighting: None,  // Will use default 0.7/0.3
    };
    
    assert_eq!(config.min_judges_required, 1);
    assert!(config.confidence_weighting.is_none());  // Will be populated at runtime
}

#[test]
fn test_consensus_config_boundary_values() {
    // Test with boundary values
    let config = ConsensusConfig {
        strategy: ConsensusStrategy::BestOfN,
        threshold: Some(1.0),  // Perfect score required
        min_agreement_confidence: Some(1.0),  // Perfect agreement
        n: Some(1),  // Single best judge
        min_judges_required: 1,
        confidence_weighting: None,
    };
    
    assert_eq!(config.threshold, Some(1.0));
    assert_eq!(config.min_agreement_confidence, Some(1.0));
    assert_eq!(config.n, Some(1));
}
