// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Prompt Template Engine
//!
//! This module provides template rendering functionality for agent prompts,
//! using Handlebars for template-variable substitution.
//!
//! # Architecture
//!
//! - **Layer:** Infrastructure
//! - **Purpose:** Transform templates with template variables into final prompts
//! - **Integration:** Agent task.prompt_template → LLM input
//!
//! # Supported Placeholders
//!
//! - `{{intent}}` - Caller-supplied free-text intent (ADR-092)
//! - `{{instruction}}` - Agent's task instruction
//! - `{{input}}` - User input (string or structured object supporting `{{input.KEY}}`)
//! - `{{iteration_number}}` - Current iteration count
//! - `{{previous_error}}` - Error from previous iteration
//! - `{{context}}` - Concatenated context attachments
//!
//! # Usage
//!
//! ```ignore
//! use prompt_template_engine::PromptTemplateEngine;
//!
//! let engine = PromptTemplateEngine::new();
//! let prompt = engine.render(
//!     "Task: {{instruction}}\n\nUser: {{input}}",
//!     &context,
//! ).await?;
//! ```

use anyhow::{Context, Result};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// Template Context
// ============================================================================

/// Context data for prompt template rendering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptContext {
    /// Caller-supplied intent (free-text steering for the LLM). Rendered as
    /// `{{intent}}` in the prompt template (ADR-092).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,

    /// Agent's task instruction
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,

    /// Structured user input for this execution.  When a `Value::Object`,
    /// Handlebars resolves `{{input.KEY}}` via dot-notation natively.  When a
    /// `Value::String`, `{{input}}` renders the string directly (ADR-092).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,

    /// Current iteration number (1-based)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration_number: Option<u32>,

    /// Error message from previous iteration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_error: Option<String>,

    /// Concatenated context attachments
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,

    /// Additional custom fields
    #[serde(flatten)]
    pub extras: HashMap<String, serde_json::Value>,
}

impl PromptContext {
    /// Create a new empty context
    pub fn new() -> Self {
        Self {
            intent: None,
            instruction: None,
            input: None,
            iteration_number: None,
            previous_error: None,
            context: None,
            extras: HashMap::new(),
        }
    }

    /// Builder-style setter for intent (ADR-092)
    pub fn intent(mut self, intent: impl Into<String>) -> Self {
        self.intent = Some(intent.into());
        self
    }

    /// Builder-style setter for instruction
    pub fn instruction(mut self, instruction: impl Into<String>) -> Self {
        self.instruction = Some(instruction.into());
        self
    }

    /// Builder-style setter for input.
    ///
    /// Accepts a `serde_json::Value`. When the value is an object, Handlebars
    /// resolves `{{input.KEY}}` via dot-notation. When the value is a string,
    /// `{{input}}` renders it directly.
    pub fn input(mut self, input: serde_json::Value) -> Self {
        self.input = Some(input);
        self
    }

    /// Builder-style setter for iteration number
    pub fn iteration_number(mut self, number: u32) -> Self {
        self.iteration_number = Some(number);
        self
    }

    /// Builder-style setter for previous error
    pub fn previous_error(mut self, error: impl Into<String>) -> Self {
        self.previous_error = Some(error.into());
        self
    }

    /// Builder-style setter for context
    pub fn context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Add extra field
    pub fn extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.extras.insert(key.into(), value);
        self
    }
}

impl Default for PromptContext {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Template Engine
// ============================================================================

pub struct PromptTemplateEngine {
    handlebars: Handlebars<'static>,
}

impl PromptTemplateEngine {
    /// Create a new template engine
    pub fn new() -> Self {
        let mut handlebars = Handlebars::new();

        // Configure Handlebars
        handlebars.set_strict_mode(false); // Don't fail on missing variables

        // LLM prompts are plain text — disable HTML escaping so that backticks,
        // quotes, and angle brackets in `{{input}}` / `{{instruction}}` reach the
        // model verbatim rather than as &quot;, &#x60;, &lt;, etc.
        handlebars.register_escape_fn(handlebars::no_escape);

        // Register custom helpers (if needed in future)
        // handlebars.register_helper("uppercase", Box::new(uppercase_helper));

        Self { handlebars }
    }

    /// Render a template with context
    ///
    /// # Example
    /// ```ignore
    /// let engine = PromptTemplateEngine::new();
    /// let context = PromptContext::new()
    ///     .instruction("Summarize emails")
    ///     .input(serde_json::Value::String("user@example.com".to_string()));
    ///
    /// let prompt = engine.render("Task: {{instruction}}\nUser: {{input}}", &context)?;
    /// ```
    pub fn render(&self, template: &str, context: &PromptContext) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .context("Failed to render prompt template")
    }

    /// Render template from agent manifest (with fallback)
    ///
    /// If no template is provided, uses a default format.
    pub fn render_with_fallback(
        &self,
        template: Option<&str>,
        context: &PromptContext,
    ) -> Result<String> {
        let template = template.unwrap_or(Self::default_template());
        self.render(template, context)
    }

    /// Get the default prompt template
    pub fn default_template() -> &'static str {
        "{{#if instruction}}Task: {{instruction}}\n\n{{/if}}{{#if intent}}Intent: {{intent}}\n\n{{/if}}{{#if input}}Input: {{input}}{{/if}}"
    }

    /// Validate template syntax without rendering
    pub fn validate_template(&self, template: &str) -> Result<()> {
        // Try to compile the template
        handlebars::template::Template::compile(template)
            .map(|_| ())
            .context("Invalid Handlebars template syntax")
    }
}

impl Default for PromptTemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Convenience Functions
// ============================================================================

/// Render a template string with a simple key-value map
pub fn render_simple(template: &str, vars: &HashMap<String, String>) -> Result<String> {
    let engine = PromptTemplateEngine::new();
    let mut context = PromptContext::new();

    for (key, value) in vars {
        context
            .extras
            .insert(key.clone(), serde_json::Value::String(value.clone()));
    }

    engine.render(template, &context)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_rendering() {
        let engine = PromptTemplateEngine::new();
        let context = PromptContext::new()
            .instruction("Write a function")
            .input(serde_json::Value::String("add two numbers".to_string()));

        let template = "Task: {{instruction}}\nInput: {{input}}";
        let result = engine.render(template, &context).unwrap();

        assert!(result.contains("Write a function"));
        assert!(result.contains("add two numbers"));
    }

    #[test]
    fn test_iteration_context() {
        let engine = PromptTemplateEngine::new();
        let context = PromptContext::new()
            .instruction("Fix the error")
            .iteration_number(3)
            .previous_error("SyntaxError: invalid syntax");

        let template =
            "Attempt {{iteration_number}}: {{instruction}}\nPrevious error: {{previous_error}}";
        let result = engine.render(template, &context).unwrap();

        assert!(result.contains("Attempt 3"));
        assert!(result.contains("SyntaxError"));
    }

    #[test]
    fn test_missing_variables() {
        let engine = PromptTemplateEngine::new();
        let context = PromptContext::new().instruction("Test");

        // Missing 'input' should not cause error (strict_mode = false)
        let template = "Task: {{instruction}}\nInput: {{input}}";
        let result = engine.render(template, &context).unwrap();

        assert!(result.contains("Test"));
    }

    #[test]
    fn test_conditional_rendering() {
        let engine = PromptTemplateEngine::new();

        // With instruction
        let context = PromptContext::new().instruction("Test instruction");
        let template = "{{#if instruction}}Has instruction: {{instruction}}{{/if}}";
        let result = engine.render(template, &context).unwrap();
        assert!(result.contains("Has instruction: Test instruction"));

        // Without instruction
        let context = PromptContext::new();
        let result = engine.render(template, &context).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_default_template() {
        let engine = PromptTemplateEngine::new();
        let context = PromptContext::new()
            .instruction("Test task")
            .input(serde_json::Value::String("Test input".to_string()));

        let result = engine
            .render(PromptTemplateEngine::default_template(), &context)
            .unwrap();

        assert!(result.contains("Test task"));
        assert!(result.contains("Test input"));
    }

    #[test]
    fn test_fallback_rendering() {
        let engine = PromptTemplateEngine::new();
        let context = PromptContext::new()
            .instruction("Test")
            .input(serde_json::Value::String("input".to_string()));

        // No template provided - should use default
        let result = engine.render_with_fallback(None, &context).unwrap();
        assert!(result.contains("Test"));

        // With template - should use provided
        let result = engine
            .render_with_fallback(Some("Custom: {{instruction}}"), &context)
            .unwrap();
        assert!(result.contains("Custom:"));
    }

    #[test]
    fn test_validate_template() {
        let engine = PromptTemplateEngine::new();

        // Valid template
        assert!(engine.validate_template("{{instruction}}").is_ok());

        // Invalid template (unclosed bracket)
        assert!(engine.validate_template("{{instruction").is_err());
    }

    #[test]
    fn test_extra_fields() {
        let engine = PromptTemplateEngine::new();
        let context = PromptContext::new().extra(
            "custom_field",
            serde_json::Value::String("custom value".to_string()),
        );

        let template = "Custom: {{custom_field}}";
        let result = engine.render(template, &context).unwrap();

        assert!(result.contains("custom value"));
    }

    #[test]
    fn test_render_simple() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "Alice".to_string());
        vars.insert("age".to_string(), "30".to_string());

        let template = "Name: {{name}}, Age: {{age}}";
        let result = render_simple(template, &vars).unwrap();

        assert!(result.contains("Alice"));
        assert!(result.contains("30"));
    }
}
