// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use crate::domain::llm::{LLMProvider, GenerationOptions, LLMError};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub success: bool,
    pub errors: Vec<String>,
    pub feedback: Option<String>,
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Validation execution failed: {0}")]
    ExecutionError(String),
    
    #[error("LLM validation failed: {0}")]
    LlmError(#[from] LLMError),
}

#[async_trait]
pub trait EvaluationEngine: Send + Sync {
    /// Validates the output of an execution iteration
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str) -> Result<ValidationResult, ValidationError>;
}

pub struct BasicJudge;

#[async_trait]
impl EvaluationEngine for BasicJudge {
    async fn evaluate(&self, _output: &str, exit_code: i64, stderr: &str) -> Result<ValidationResult, ValidationError> {
        let mut errors = Vec::new();
        let mut feedback = None;
        let mut success = true;
        
        if exit_code != 0 {
            success = false;
            errors.push(format!("Process exited with code {}", exit_code));
            if !stderr.is_empty() {
                errors.push(format!("Stderr: {}", stderr));
            }
            feedback = Some("Execution failed. Please fix the errors listed.".to_string());
        } else if !stderr.is_empty() {
            // If exit code is 0, we treat stderr as warnings/logs, not failure.
            // But we can include it in the "errors" list if we want it visible, or just ignore it.
            // For now, let's NOT fail on stderr if exit code is 0.
            // The supervisor already logs stderr.
        }

        Ok(ValidationResult {
            success,
            errors,
            feedback,
        })
    }
}

/// LLM-based judge that uses an LLM to semantically evaluate output quality
pub struct LlmJudge {
    llm_provider: Arc<dyn LLMProvider>,
    prompt_template: String,
    threshold: f64,
    fallback_on_unavailable: bool,
}

impl LlmJudge {
    pub fn new(
        llm_provider: Arc<dyn LLMProvider>,
        prompt_template: String,
        threshold: f64,
        fallback_on_unavailable: bool,
    ) -> Self {
        Self {
            llm_provider,
            prompt_template,
            threshold,
            fallback_on_unavailable,
        }
    }
    
    /// Default prompt template for semantic validation
    pub fn default_prompt_template() -> String {
        r#"You are evaluating the output of an automated agent task.

Task Output:
```
{output}
```

Exit Code: {exit_code}

Stderr:
```
{stderr}
```

Evaluation Criteria:
{criteria}

Please evaluate whether this output successfully completes the task. Respond in JSON format:
{{
  "success": true/false,
  "confidence": 0.0-1.0,
  "feedback": "Brief explanation of your evaluation"
}}

Be objective and focus on whether the output meets the stated criteria."#.to_string()
    }
}

#[async_trait]
impl EvaluationEngine for LlmJudge {
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str) -> Result<ValidationResult, ValidationError> {
        // Construct the evaluation prompt
        let prompt = self.prompt_template
            .replace("{output}", output)
            .replace("{exit_code}", &exit_code.to_string())
            .replace("{stderr}", stderr)
            .replace("{criteria}", "Output should be valid, complete, and error-free");
        
        // Call LLM
        let options = GenerationOptions {
            temperature: Some(0.1), // Low temperature for consistent evaluation
            max_tokens: Some(500),
            ..Default::default()
        };
        
        let response = match self.llm_provider.generate(&prompt, &options).await {
            Ok(r) => r,
            Err(e) => {
                if self.fallback_on_unavailable {
                    // Fallback to basic validation
                    tracing::warn!("LLM validation unavailable, using basic validation: {}", e);
                    let basic_judge = BasicJudge;
                    return basic_judge.evaluate(output, exit_code, stderr).await;
                } else {
                    return Err(ValidationError::LlmError(e));
                }
            }
        };
        
        // Parse JSON response
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&response.text);
        
        match parsed {
            Ok(json) => {
                let success = json["success"].as_bool().unwrap_or(false);
                let confidence = json["confidence"].as_f64().unwrap_or(0.0);
                let feedback = json["feedback"].as_str().map(|s| s.to_string());
                
                // Apply confidence threshold
                let meets_threshold = confidence >= self.threshold;
                
                let mut errors = Vec::new();
                if !success || !meets_threshold {
                    if !meets_threshold {
                        errors.push(format!("Confidence {} below threshold {}", confidence, self.threshold));
                    }
                    if let Some(ref fb) = feedback {
                        errors.push(fb.clone());
                    }
                }
                
                Ok(ValidationResult {
                    success: success && meets_threshold,
                    errors,
                    feedback,
                })
            }
            Err(e) => {
                // If LLM response is not valid JSON, treat as failure
                Err(ValidationError::ExecutionError(
                    format!("Failed to parse LLM judge response as JSON: {}. Response: {}", e, response.text)
                ))
            }
        }
    }
}

/// Composite judge that chains multiple validation strategies
pub struct CompositeJudge {
    basic: BasicJudge,
    llm: Option<LlmJudge>,
}

impl CompositeJudge {
    pub fn new(llm: Option<LlmJudge>) -> Self {
        Self {
            basic: BasicJudge,
            llm,
        }
    }
}

#[async_trait]
impl EvaluationEngine for CompositeJudge {
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str) -> Result<ValidationResult, ValidationError> {
        // First run basic validation (exit code, stderr)
        let basic_result = self.basic.evaluate(output, exit_code, stderr).await?;
        
        // If basic validation fails, return immediately
        if !basic_result.success {
            return Ok(basic_result);
        }
        
        // If LLM judge is configured, run semantic validation
        if let Some(ref llm_judge) = self.llm {
            let llm_result = llm_judge.evaluate(output, exit_code, stderr).await?;
            
            // Combine results: both must pass
            let combined_success = basic_result.success && llm_result.success;
            let mut combined_errors = basic_result.errors.clone();
            combined_errors.extend(llm_result.errors);
            
            // Prefer LLM feedback if available
            let combined_feedback = llm_result.feedback.or(basic_result.feedback);
            
            Ok(ValidationResult {
                success: combined_success,
                errors: combined_errors,
                feedback: combined_feedback,
            })
        } else {
            // No LLM judge, return basic result
            Ok(basic_result)
        }
    }
}

/// Factory to build judge from manifest configuration
pub fn build_judge_from_manifest(
    semantic_config: Option<&crate::domain::agent::SemanticValidation>,
    llm_provider: Arc<dyn LLMProvider>,
) -> Arc<dyn EvaluationEngine> {
    if let Some(semantic) = semantic_config {
        let llm_judge = LlmJudge::new(
            llm_provider,
            semantic.prompt.clone(),
            semantic.threshold,
            semantic.fallback_on_unavailable == crate::domain::agent::FallbackBehavior::Skip,
        );
        Arc::new(CompositeJudge::new(Some(llm_judge)))
    } else {
        // No semantic validation configured, use basic judge only
        Arc::new(CompositeJudge::new(None))
    }
}
