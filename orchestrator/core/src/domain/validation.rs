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

use crate::domain::agent::AgentId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use thiserror::Error;

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

// ── Validation Results (moved here from execution.rs — belong to validation domain) ──

/// All validation results collected for a single iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResults {
    pub system: Option<SystemValidationResult>,
    pub output: Option<OutputValidationResult>,
    pub semantic: Option<SemanticValidationResult>,
    pub gradient: Option<GradientResult>,
    pub consensus: Option<MultiJudgeConsensus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemValidationResult {
    pub success: bool,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputValidationResult {
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticValidationResult {
    pub success: bool,
    pub score: f64,
    pub reasoning: String,
}

// ── GradientValidator Trait & Concrete Validators (ADR-017) ──────────────────

/// Context passed to each gradient validator for a single iteration.
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// Original agent intent (for semantic evaluation context).
    pub task: String,
    /// Iteration stdout.
    pub output: String,
    /// Process exit code from the iteration.
    pub exit_code: i64,
    /// Iteration stderr (joined log lines).
    pub stderr: String,
    /// Mounted worker paths visible to judges (canonical `/workspace...` roots).
    pub worker_mounts: Vec<String>,
    /// Tool names that were blocked by policy during this iteration.
    pub policy_violations: Vec<String>,
    /// Live tool trajectory from the inner loop — threaded directly to avoid a
    /// DB fetch race where `store_iteration_trajectory` may not yet be visible
    /// when the judge reads the execution record.
    pub tool_trajectory: Vec<crate::domain::execution::TrajectoryStep>,
}

/// Extract the first JSON value from `text`, stripping markdown code fences.
///
/// Handles all common LLM fence patterns (case-insensitive language tags,
/// extra backticks, leading/trailing prose). Returns the trimmed interior,
/// or `None` if no fence block is found.
pub fn extract_json_from_text(text: &str) -> Option<String> {
    // Find the opening ``` (3+ backticks)
    let fence_start = text.find("```")?;
    let after_backticks = fence_start + 3;

    // Skip any additional backticks (e.g. ```` or `````)
    let rest = &text[after_backticks..];
    let extra_backticks = rest.len() - rest.trim_start_matches('`').len();
    let after_all_backticks = after_backticks + extra_backticks;

    // Skip optional language tag + whitespace up to (and including) the first newline.
    let content_start = if let Some(nl_offset) = text[after_all_backticks..].find('\n') {
        after_all_backticks + nl_offset + 1
    } else {
        return None;
    };

    // Find the closing ``` fence
    let closing_fence = text[content_start..].find("```")?;
    let content = &text[content_start..content_start + closing_fence];
    Some(content.trim().to_string())
}

/// Domain trait for gradient validators (ADR-017).
///
/// Each validator receives a [`ValidationContext`] and produces a [`GradientResult`].
/// Deterministic validators (`System`, `Output`) always have `confidence = 1.0`.
/// Semantic validators may return lower confidence when the LLM is uncertain.
#[async_trait::async_trait]
pub trait GradientValidator: Send + Sync {
    async fn validate(&self, ctx: &ValidationContext) -> anyhow::Result<GradientResult>;
}

/// Validates iteration output based on process exit code and stderr (ADR-017).
///
/// Score calculation:
/// - `exit_code == 0` → exit_score = 1.0; non-zero → 0.0
/// - `stderr` empty or `allow_stderr` → stderr_score = 1.0; else 0.5
/// - final = `exit_score * 0.7 + stderr_score * 0.3`
/// - If `must_succeed && exit_code != 0` → score forced to 0.0 regardless of weights
pub struct SystemGradientValidator {
    pub must_succeed: bool,
    pub allow_stderr: bool,
}

impl SystemGradientValidator {
    pub fn new(must_succeed: bool, allow_stderr: bool) -> Self {
        Self {
            must_succeed,
            allow_stderr,
        }
    }
}

#[async_trait::async_trait]
impl GradientValidator for SystemGradientValidator {
    async fn validate(&self, ctx: &ValidationContext) -> anyhow::Result<GradientResult> {
        const EXIT_CODE_WEIGHT: f64 = 0.7;
        const STDERR_WEIGHT: f64 = 0.3;

        let exit_score = if ctx.exit_code == 0 { 1.0 } else { 0.0 };
        let stderr_score = if ctx.stderr.is_empty() || self.allow_stderr {
            1.0
        } else {
            0.5
        };

        let weighted_score = exit_score * EXIT_CODE_WEIGHT + stderr_score * STDERR_WEIGHT;
        let must_succeed_violated = self.must_succeed && ctx.exit_code != 0;
        let score = if must_succeed_violated {
            0.0
        } else {
            weighted_score
        };

        let mut signals = Vec::new();
        if ctx.exit_code != 0 {
            signals.push(ValidationSignal {
                category: "exit_code".to_string(),
                score: 0.0,
                message: format!("Process exited with code {}", ctx.exit_code),
            });
        }
        if !ctx.stderr.is_empty() && !self.allow_stderr {
            signals.push(ValidationSignal {
                category: "stderr".to_string(),
                score: 0.5,
                message: format!("Stderr output present ({} bytes)", ctx.stderr.len()),
            });
        }

        let reasoning = format!(
            "Exit code: {} ({}), stderr: {} bytes{}",
            ctx.exit_code,
            if ctx.exit_code == 0 {
                "success"
            } else {
                "failure"
            },
            ctx.stderr.len(),
            if must_succeed_violated {
                " — must_succeed constraint violated"
            } else {
                ""
            },
        );

        Ok(GradientResult {
            score,
            confidence: 1.0,
            reasoning,
            signals,
            metadata: HashMap::new(),
        })
    }
}

/// Validates iteration output against a declared format, JSON schema, and/or regex (ADR-017).
///
/// All checks are deterministic (`confidence = 1.0`). Checks run in order:
/// format → schema → regex. The first failing check short-circuits and returns score 0.0.
pub struct OutputGradientValidator {
    pub format: String,
    pub schema: Option<serde_json::Value>,
    pub regex_pattern: Option<String>,
}

impl OutputGradientValidator {
    pub fn new(
        format: String,
        schema: Option<serde_json::Value>,
        regex_pattern: Option<String>,
    ) -> Self {
        Self {
            format,
            schema,
            regex_pattern,
        }
    }

    /// Strip the first markdown code fence from `text`, returning the interior content.
    /// Returns `None` when no fence is found; callers should fall back to the original text.
    ///
    /// Handles all common LLM fence patterns:
    /// - ` ```json\n{...}\n``` `  (standard, case-insensitive language tag)
    /// - ` ``` json\n{...}\n``` ` (space before language tag)
    /// - ` ```\n{...}\n``` `      (no language tag)
    /// - Leading/trailing text outside the fence is ignored.
    ///
    /// LLMs commonly wrap structured output in fences even when instructed to emit raw JSON.
    /// Stripping here keeps `OutputGradientValidator` compatible with both raw and fenced output.
    fn strip_code_fence(text: &str) -> Option<String> {
        // Find the opening ``` (3+ backticks)
        let fence_start = text.find("```")?;
        let after_backticks = fence_start + 3;

        // Skip any additional backticks (e.g. ```` or `````)
        let rest = &text[after_backticks..];
        let extra_backticks = rest.len() - rest.trim_start_matches('`').len();
        let after_all_backticks = after_backticks + extra_backticks;

        // Skip optional language tag + whitespace up to (and including) the first newline.
        // The language tag is everything between the backticks and the first newline.
        let content_start = if let Some(nl_offset) = text[after_all_backticks..].find('\n') {
            after_all_backticks + nl_offset + 1
        } else {
            // No newline after opening fence — no valid fence block
            return None;
        };

        // Find the closing ``` fence
        let closing_fence = text[content_start..].find("```")?;
        let content = &text[content_start..content_start + closing_fence];
        Some(content.trim().to_string())
    }
}

#[async_trait::async_trait]
impl GradientValidator for OutputGradientValidator {
    async fn validate(&self, ctx: &ValidationContext) -> anyhow::Result<GradientResult> {
        // Step 1: Format check
        let parsed_value: Option<serde_json::Value> = match self.format.to_lowercase().as_str() {
            "json" => {
                // Strip markdown fences before parsing — LLMs often emit ```json…``` wrappers.
                let stripped = Self::strip_code_fence(&ctx.output);
                let to_parse = stripped.as_deref().unwrap_or(&ctx.output);
                match serde_json::from_str::<serde_json::Value>(to_parse) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        return Ok(GradientResult {
                            score: 0.0,
                            confidence: 1.0,
                            reasoning: format!("Output is not valid JSON: {e}"),
                            signals: vec![ValidationSignal {
                                category: "format".to_string(),
                                score: 0.0,
                                message: format!("JSON parse error: {e}"),
                            }],
                            metadata: HashMap::new(),
                        });
                    }
                }
            }
            "yaml" => match serde_yaml::from_str::<serde_json::Value>(&ctx.output) {
                Ok(v) => Some(v),
                Err(e) => {
                    return Ok(GradientResult {
                        score: 0.0,
                        confidence: 1.0,
                        reasoning: format!("Output is not valid YAML: {e}"),
                        signals: vec![ValidationSignal {
                            category: "format".to_string(),
                            score: 0.0,
                            message: format!("YAML parse error: {e}"),
                        }],
                        metadata: HashMap::new(),
                    });
                }
            },
            _ => None, // Unknown format — skip format check
        };

        // Step 2: JSON Schema validation
        if let (Some(ref schema), Some(ref value)) = (&self.schema, &parsed_value) {
            let compiled = jsonschema::validator_for(schema)
                .map_err(|e| anyhow::anyhow!("Invalid JSON schema in manifest: {e}"))?;
            if !compiled.is_valid(value) {
                let errors: Vec<String> =
                    compiled.iter_errors(value).map(|e| e.to_string()).collect();
                return Ok(GradientResult {
                    score: 0.0,
                    confidence: 1.0,
                    reasoning: format!(
                        "Output does not conform to declared schema: {}",
                        errors.join("; ")
                    ),
                    signals: vec![ValidationSignal {
                        category: "schema".to_string(),
                        score: 0.0,
                        message: errors.join("; "),
                    }],
                    metadata: HashMap::new(),
                });
            }
        }

        // Step 3: Regex match
        if let Some(ref pattern) = self.regex_pattern {
            let re = regex::Regex::new(pattern)
                .map_err(|e| anyhow::anyhow!("Invalid regex in manifest: {e}"))?;
            if !re.is_match(&ctx.output) {
                return Ok(GradientResult {
                    score: 0.0,
                    confidence: 1.0,
                    reasoning: format!("Output does not match required pattern: {pattern}"),
                    signals: vec![ValidationSignal {
                        category: "regex".to_string(),
                        score: 0.0,
                        message: format!("Pattern '{pattern}' not matched in output"),
                    }],
                    metadata: HashMap::new(),
                });
            }
        }

        Ok(GradientResult {
            score: 1.0,
            confidence: 1.0,
            reasoning: "Output passed all format/schema/regex checks".to_string(),
            signals: vec![],
            metadata: HashMap::new(),
        })
    }
}

// ── ValidationPipeline (ADR-017) ──────────────────────────────────────────────

/// Tag for each entry in the pipeline, used to map `GradientResult`s to the
/// correct `ValidationResults` field.
pub enum ValidatorKind {
    System,
    Output,
    Semantic,
    MultiJudge,
}

/// One entry in the validation pipeline with its acceptance thresholds.
pub struct ValidatorEntry {
    pub kind: ValidatorKind,
    pub validator: Box<dyn GradientValidator>,
    /// Minimum score required for this entry to pass (0.0–1.0).
    pub min_score: f64,
    /// Minimum confidence required; scores with lower confidence are treated as fails.
    pub min_confidence: f64,
}

/// Result from running the full validation pipeline for one iteration.
#[derive(Debug)]
pub struct ValidationPipelineResult {
    /// Populated `ValidationResults` for persistence.
    pub results: ValidationResults,
    /// True when all enabled validators passed their thresholds.
    pub passed: bool,
    /// Human-readable reason for the first failure (if `!passed`).
    pub blocking_reason: Option<String>,
}

/// Ordered pipeline of gradient validators applied to each iteration output (ADR-017).
///
/// Constructed by the application layer factory from an agent's `ValidationConfig` and
/// passed to `Supervisor::run_loop`. Validators run sequentially in declaration order;
/// a failing entry short-circuits the rest.
///
/// If no `ValidationPipeline` is provided to the Supervisor, it returns on the
/// first runtime-success — the original behaviour.
pub struct ValidationPipeline {
    pub(crate) entries: Vec<ValidatorEntry>,
}

impl ValidationPipeline {
    /// Construct from already-built entries. Called by the application layer factory.
    pub fn new(entries: Vec<ValidatorEntry>) -> Self {
        Self { entries }
    }

    /// Run all validators in order. Short-circuits on the first blocking failure.
    ///
    /// Each entry's `min_score` and `min_confidence` are evaluated independently.
    /// A `MultiJudge` entry's individual results are stored in `GradientResult.metadata`
    /// and reconstructed into `ValidationResults.consensus`.
    pub async fn validate(
        &self,
        ctx: &ValidationContext,
    ) -> anyhow::Result<ValidationPipelineResult> {
        let mut system: Option<SystemValidationResult> = None;
        let mut output: Option<OutputValidationResult> = None;
        let mut semantic: Option<SemanticValidationResult> = None;
        let mut gradient: Option<GradientResult> = None;
        let mut consensus: Option<MultiJudgeConsensus> = None;

        for entry in &self.entries {
            let result = entry.validator.validate(ctx).await?;

            // Confidence gate: insufficient confidence is treated as a fail.
            let confidence_ok = result.confidence >= entry.min_confidence;
            let score_ok = result.score >= entry.min_score;
            let passed = score_ok && confidence_ok;

            let blocking_reason = if !confidence_ok {
                Some(format!(
                    "Validation confidence {:.2} below minimum {:.2}: {}",
                    result.confidence, entry.min_confidence, result.reasoning
                ))
            } else if !score_ok {
                Some(format!(
                    "Validation score {:.2} below minimum {:.2}: {}",
                    result.score, entry.min_score, result.reasoning
                ))
            } else {
                None
            };

            match entry.kind {
                ValidatorKind::System => {
                    system = Some(SystemValidationResult {
                        success: passed,
                        exit_code: ctx.exit_code as i32,
                        stdout: ctx.output.clone(),
                        stderr: ctx.stderr.clone(),
                    });
                    if !passed {
                        gradient = Some(result);
                        return Ok(ValidationPipelineResult {
                            results: ValidationResults {
                                system,
                                output,
                                semantic,
                                gradient,
                                consensus,
                            },
                            passed: false,
                            blocking_reason,
                        });
                    }
                    gradient = Some(result);
                }
                ValidatorKind::Output => {
                    output = Some(OutputValidationResult {
                        success: passed,
                        error: if passed {
                            None
                        } else {
                            Some(result.reasoning.clone())
                        },
                    });
                    if !passed {
                        gradient = Some(result);
                        return Ok(ValidationPipelineResult {
                            results: ValidationResults {
                                system,
                                output,
                                semantic,
                                gradient,
                                consensus,
                            },
                            passed: false,
                            blocking_reason,
                        });
                    }
                    gradient = Some(result);
                }
                ValidatorKind::Semantic | ValidatorKind::MultiJudge => {
                    // For MultiJudge, try to reconstruct the consensus struct from metadata.
                    if matches!(entry.kind, ValidatorKind::MultiJudge) {
                        if let Some(raw) = result.metadata.get("consensus") {
                            if let Ok(c) =
                                serde_json::from_value::<MultiJudgeConsensus>(raw.clone())
                            {
                                consensus = Some(c);
                            }
                        }
                    }
                    let sem = SemanticValidationResult {
                        success: passed,
                        score: result.score,
                        reasoning: result.reasoning.clone(),
                    };
                    if !passed {
                        semantic = Some(sem);
                        gradient = Some(result);
                        return Ok(ValidationPipelineResult {
                            results: ValidationResults {
                                system,
                                output,
                                semantic,
                                gradient,
                                consensus,
                            },
                            passed: false,
                            blocking_reason,
                        });
                    }
                    semantic = Some(sem);
                    gradient = Some(result);
                }
            }
        }

        Ok(ValidationPipelineResult {
            results: ValidationResults {
                system,
                output,
                semantic,
                gradient,
                consensus,
            },
            passed: true,
            blocking_reason: None,
        })
    }
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
        result
            .metadata
            .insert("judge_id".to_string(), serde_json::json!("judge-1"));
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

    // ── Code Fence Stripping (regression: markdown fences breaking JSON parsing) ──

    #[test]
    fn strip_code_fence_json_tag() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        let result = OutputGradientValidator::strip_code_fence(input);
        assert_eq!(result.unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_code_fence_no_language_tag() {
        let input = "```\n{\"key\": \"value\"}\n```";
        let result = OutputGradientValidator::strip_code_fence(input);
        assert_eq!(result.unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_code_fence_uppercase_json_tag() {
        // Regression: LLMs sometimes emit ```JSON instead of ```json
        let input = "```JSON\n{\"key\": \"value\"}\n```";
        let result = OutputGradientValidator::strip_code_fence(input);
        assert_eq!(result.unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_code_fence_with_leading_text() {
        let input = "Here is the result:\n```json\n{\"key\": \"value\"}\n```\n";
        let result = OutputGradientValidator::strip_code_fence(input);
        assert_eq!(result.unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn strip_code_fence_multiline_json() {
        // Regression test for the exact error scenario:
        // "Validation score 0.00 below minimum 1.00: Output is not valid JSON:
        //  expected ',' or '}' at line 18 column 71"
        let json_body = r#"{
  "analysis": {
    "score": 0.95,
    "reasoning": "The output meets all criteria",
    "signals": [
      {"category": "format", "score": 1.0, "message": "Valid JSON structure"},
      {"category": "content", "score": 0.9, "message": "Content is relevant"}
    ]
  },
  "metadata": {
    "judge_id": "validator-agent",
    "timestamp": "2026-04-09T12:00:00Z",
    "model": "claude-sonnet-4-20250514",
    "iteration": 1,
    "execution_id": "exec-12345",
    "additional_context": "This is a test with enough lines to reach line 18"
  }
}"#;
        let fenced = format!("```json\n{}\n```", json_body);
        let result = OutputGradientValidator::strip_code_fence(&fenced);
        let stripped = result.unwrap();
        // Must parse as valid JSON after stripping
        assert!(
            serde_json::from_str::<serde_json::Value>(&stripped).is_ok(),
            "Stripped content should be valid JSON but got: {stripped}"
        );
    }

    #[test]
    fn strip_code_fence_returns_none_without_fences() {
        let input = "{\"key\": \"value\"}";
        let result = OutputGradientValidator::strip_code_fence(input);
        assert!(result.is_none());
    }

    #[test]
    fn strip_code_fence_with_space_before_language_tag() {
        // Some LLMs emit ``` json instead of ```json
        let input = "``` json\n{\"key\": \"value\"}\n```";
        let result = OutputGradientValidator::strip_code_fence(input);
        assert_eq!(result.unwrap(), "{\"key\": \"value\"}");
    }

    #[test]
    fn extract_json_from_text_strips_fences() {
        let input = "Some preamble\n```json\n{\"result\": true}\n```\nSome postamble";
        let result = extract_json_from_text(input);
        assert_eq!(result.unwrap(), "{\"result\": true}");
    }

    #[test]
    fn extract_json_from_text_uppercase_tag() {
        let input = "```JSON\n{\"result\": true}\n```";
        let result = extract_json_from_text(input);
        assert_eq!(result.unwrap(), "{\"result\": true}");
    }

    #[tokio::test]
    async fn output_validator_passes_json_in_fences() {
        // Regression: OutputGradientValidator must strip markdown fences before
        // parsing and validating against JSON schema. Without stripping, the
        // fence markers cause serde_json to report "Output is not valid JSON".
        let schema = serde_json::json!({
            "type": "object",
            "required": ["result"],
            "properties": {
                "result": { "type": "boolean" }
            }
        });
        let validator = OutputGradientValidator::new("json".to_string(), Some(schema), None);
        let ctx = ValidationContext {
            task: "test".to_string(),
            output: "```json\n{\"result\": true}\n```".to_string(),
            exit_code: 0,
            stderr: String::new(),
            worker_mounts: vec![],
            policy_violations: vec![],
            tool_trajectory: vec![],
        };
        let result = validator.validate(&ctx).await.unwrap();
        assert_eq!(
            result.score, 1.0,
            "JSON in fences should pass validation but got: {}",
            result.reasoning
        );
    }

    #[tokio::test]
    async fn output_validator_passes_json_in_uppercase_fences() {
        // Regression: ```JSON tag (uppercase) was not stripped, causing parse failure.
        let validator = OutputGradientValidator::new("json".to_string(), None, None);
        let ctx = ValidationContext {
            task: "test".to_string(),
            output: "```JSON\n{\"key\": \"value\"}\n```".to_string(),
            exit_code: 0,
            stderr: String::new(),
            worker_mounts: vec![],
            policy_violations: vec![],
            tool_trajectory: vec![],
        };
        let result = validator.validate(&ctx).await.unwrap();
        assert_eq!(
            result.score, 1.0,
            "JSON in uppercase-tagged fences should pass validation but got: {}",
            result.reasoning
        );
    }
}
