// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

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
