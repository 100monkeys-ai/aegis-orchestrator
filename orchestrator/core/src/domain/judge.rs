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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
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
    /// 
    /// # Arguments
    /// * `output` - The stdout from the execution
    /// * `exit_code` - The process exit code
    /// * `stderr` - The stderr output
    /// * `verbose` - Whether to emit detailed logging
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str, verbose: bool) -> Result<ValidationResult, ValidationError>;
}

pub struct BasicJudge;

#[async_trait]
impl EvaluationEngine for BasicJudge {
    async fn evaluate(&self, _output: &str, exit_code: i64, stderr: &str, _verbose: bool) -> Result<ValidationResult, ValidationError> {
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
            metadata: None,
        })
    }
}

/// LLM-based judge that uses an LLM to semantically evaluate output quality
pub struct LlmJudge {
    llm_provider: Arc<dyn LLMProvider>,
    prompt_template: String,
    threshold: f64,
    fallback_on_unavailable: bool,
    task_instruction: Option<String>,
}

impl LlmJudge {
    pub fn new(
        llm_provider: Arc<dyn LLMProvider>,
        prompt_template: String,
        threshold: f64,
        fallback_on_unavailable: bool,
        task_instruction: Option<String>,
    ) -> Self {
        Self {
            llm_provider,
            prompt_template,
            threshold,
            fallback_on_unavailable,
            task_instruction,
        }
    }
    
    /// Default prompt template for semantic validation.
    /// 
    /// Available placeholders:
    /// - `{criteria}`: The task instruction from the manifest's `task.instruction` field.
    ///                 Falls back to generic text if not provided.
    /// - `{output}`: The agent's stdout from execution.
    /// - `{exit_code}`: The process exit code (0 for success, non-zero for failure).
    /// - `{stderr}`: The agent's stderr output.
    /// 
    /// The LLM must respond with valid JSON containing:
    /// - `success`: boolean indicating if output meets criteria
    /// - `confidence`: float 0.0-1.0 indicating judge's confidence
    /// - `feedback`: string with brief explanation
    pub fn default_prompt_template() -> String {
        r#"You are evaluating the output of an automated agent execution.

Task Requirements:
{criteria}

Agent Output:
```
{output}
```

Execution Details:
- Exit Code: {exit_code}
- Stderr: {stderr}

Evaluate whether the output successfully meets the task requirements above.

Respond in JSON format:
{{
  "success": true/false,
  "confidence": 0.0-1.0,
  "feedback": "Brief explanation of your evaluation"
}}

Be objective and precise. Focus on whether the output fulfills the stated task requirements."#.to_string()
    }
}

#[async_trait]
impl EvaluationEngine for LlmJudge {
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str, verbose: bool) -> Result<ValidationResult, ValidationError> {
        if verbose {
            tracing::info!("🧑‍⚖️ LLM Judge evaluating output");
        }
        
        // Construct the evaluation prompt with task instruction as criteria
        let criteria = self.task_instruction
            .as_deref()
            .unwrap_or("Output should be valid, complete, and error-free");
        
        if verbose {
            tracing::debug!("Judge criteria: {}", criteria);
        }
        
        let prompt = self.prompt_template
            .replace("{output}", output)
            .replace("{exit_code}", &exit_code.to_string())
            .replace("{stderr}", stderr)
            .replace("{criteria}", criteria);
        
        if verbose {
            tracing::info!("Judge Prompt:\n{}", prompt);
        }
        
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
                    return basic_judge.evaluate(output, exit_code, stderr, verbose).await;
                } else {
                    return Err(ValidationError::LlmError(e));
                }
            }
        };
        
        if verbose {
            tracing::info!("Judge Response:\n{}", response.text);
        }
        
        // Strip markdown code fences if present (LLMs often wrap JSON in ```json ... ```)
        let response_text = response.text.trim();
        let json_text = if response_text.starts_with("```") {
            // Extract content between code fences
            response_text
                .lines()
                .skip(1) // Skip opening fence
                .take_while(|line| !line.trim().starts_with("```")) // Stop at closing fence
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            response_text.to_string()
        };
        
        // Parse JSON response
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json_text);
        
        match parsed {
            Ok(json) => {
                let llm_success = json["success"].as_bool().unwrap_or(false);
                let confidence = json["confidence"].as_f64().unwrap_or(0.0);
                let feedback = json["feedback"].as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "No feedback provided".to_string());
                
                if verbose {
                    tracing::info!("Judge response: success={}, confidence={:.2}, feedback={}", 
                        llm_success, confidence, feedback);
                }
                
                // Check if LLM explicitly failed the output
                if !llm_success {
                    return Ok(ValidationResult {
                        success: false,
                        errors: vec![format!("Output did not meet task criteria: {}", feedback)],
                        feedback: Some(feedback.clone()),
                        metadata: Some(serde_json::json!({
                            "failure_type": "semantic_check_failed",
                            "confidence": confidence,
                        })),
                    });
                }
                
                // Check confidence threshold (even if LLM said success)
                if confidence < self.threshold {
                    return Ok(ValidationResult {
                        success: false,
                        errors: vec![format!("Judge confidence too low ({:.2} < {:.2}): {}", 
                            confidence, self.threshold, feedback)],
                        feedback: Some(feedback.clone()),
                        metadata: Some(serde_json::json!({
                            "failure_type": "low_confidence",
                            "actual_confidence": confidence,
                            "required_confidence": self.threshold,
                        })),
                    });
                }
                
                // Success: both LLM approved and confidence is high enough
                Ok(ValidationResult {
                    success: true,
                    errors: Vec::new(),
                    feedback: Some(feedback),
                    metadata: Some(serde_json::json!({
                        "confidence": confidence,
                    })),
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
    async fn evaluate(&self, output: &str, exit_code: i64, stderr: &str, verbose: bool) -> Result<ValidationResult, ValidationError> {
        // First run basic validation (exit code, stderr)
        let basic_result = self.basic.evaluate(output, exit_code, stderr, verbose).await?;
        
        // If basic validation fails, return immediately
        if !basic_result.success {
            if verbose {
                tracing::info!("Basic validation failed");
            }
            return Ok(basic_result);
        }
        
        // If LLM judge is configured, run semantic validation
        if let Some(ref llm_judge) = self.llm {
            // Handle LLM validation result or error
            let llm_result = llm_judge.evaluate(output, exit_code, stderr, verbose).await?;
            
            // LLM judge result takes precedence since basic passed
            if !llm_result.success {
                // Return LLM failure result
                return Ok(llm_result);
            }
            
            // Both passed - return LLM result with its confidence metadata
            Ok(llm_result)
        } else {
            // No LLM judge, return basic result
            Ok(basic_result)
        }
    }
}

/// Factory to build judge from manifest configuration
/// 
/// Semantic validation is enabled by default when task_instruction exists.
/// Users can disable by setting semantic.enabled: false
pub fn build_judge_from_manifest(
    semantic_config: Option<&crate::domain::agent::SemanticValidation>,
    task_instruction: Option<&str>,
    llm_provider: Arc<dyn LLMProvider>,
) -> Arc<dyn EvaluationEngine> {
    // Determine if we should enable semantic validation
    let should_enable_semantic = if let Some(semantic) = semantic_config {
        // User explicitly configured semantic validation
        // Check the enabled flag
        semantic.enabled
    } else {
        // No explicit config: enable by default if task instruction exists
        task_instruction.is_some()
    };
    
    if should_enable_semantic {
        // Use custom config if provided, otherwise use defaults
        let (prompt, threshold, fallback) = if let Some(semantic) = semantic_config {
            (
                semantic.prompt.clone(),
                semantic.threshold,
                semantic.fallback_on_unavailable == crate::domain::agent::FallbackBehavior::Skip,
            )
        } else {
            // Defaults when enabled implicitly
            (
                LlmJudge::default_prompt_template(),
                0.8, // Default threshold
                true, // Default to skip on LLM unavailable
            )
        };
        
        let llm_judge = LlmJudge::new(
            llm_provider,
            prompt,
            threshold,
            fallback,
            task_instruction.map(|s| s.to_string()),
        );
        Arc::new(CompositeJudge::new(Some(llm_judge)))
    } else {
        // Semantic validation explicitly disabled or no task instruction
        tracing::debug!("Semantic validation disabled - using basic judge only");
        Arc::new(CompositeJudge::new(None))
    }
}
