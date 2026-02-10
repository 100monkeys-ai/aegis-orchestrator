//! Workflow Engine Application Service
//!
//! This module implements the FSM execution engine that drives workflow state transitions.
//!
//! # Architecture
//!
//! - **Layer:** Application Layer
//! - **Purpose:** Orchestrate workflow execution (FSM tick loop)
//! - **Dependencies:** Domain (Workflow), Infrastructure (Parser, Runtime, Repositories)
//!
//! # Design Pattern
//!
//! The WorkflowEngine is an **Application Service** that:
//! 1. Loads workflow definitions
//! 2. Executes states (delegates to agents/system/human handlers)
//! 3. Evaluates transitions
//! 4. Updates execution state
//! 5. Publishes domain events
//!
//! # FSM Tick Loop
//!
//! ```text
//! loop {
//!     current_state = workflow_execution.current_state
//!     
//!     // Execute state
//!     output = execute_state(current_state)
//!     
//!     // Record output
//!     workflow_execution.record_state_output(current_state, output)
//!     
//!     // Evaluate transitions
//!     next_state = evaluate_transitions(current_state, output)
//!     
//!     if next_state.is_terminal() {
//!         break
//!     }
//!     
//!     // Transition
//!     workflow_execution.transition_to(next_state)
//! }
//! ```

use crate::domain::workflow::*;
use crate::domain::execution::{ExecutionId, Execution, ExecutionInput};
use crate::domain::agent::AgentId;
// Note: JudgeVerdict and MAX_RECURSIVE_DEPTH will be used in Phase 3 for parallel judge evaluation
// use crate::domain::judge::{JudgeVerdict, MAX_RECURSIVE_DEPTH};
use crate::domain::events::ExecutionEvent;
use crate::infrastructure::workflow_parser::WorkflowParser;
use crate::infrastructure::event_bus::EventBus;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{debug, info};

// ============================================================================
// Application Service: WorkflowEngine
// ============================================================================

/// Workflow Engine (Application Service)
///
/// Orchestrates workflow execution using FSM pattern.
pub struct WorkflowEngine {
    /// Loaded workflow definitions (workflow_name -> workflow)
    workflows: Arc<tokio::sync::RwLock<HashMap<String, Workflow>>>,
    
    /// Active workflow executions (execution_id -> workflow_execution)
    executions: Arc<tokio::sync::RwLock<HashMap<ExecutionId, WorkflowExecution>>>,
    
    /// Execution tracking for recursive calls (execution_id -> Execution)
    /// Made public for testing purposes
    pub execution_contexts: Arc<tokio::sync::RwLock<HashMap<ExecutionId, Execution>>>,
    
    /// Event bus for publishing domain events
    event_bus: Arc<EventBus>,
    
    /// Template renderer (Handlebars)
    template_engine: Arc<handlebars::Handlebars<'static>>,
}

impl WorkflowEngine {
    /// Create a new WorkflowEngine
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            workflows: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            executions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            execution_contexts: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            event_bus,
            template_engine: Arc::new(handlebars::Handlebars::new()),
        }
    }

    // ========================================================================
    // Workflow Management
    // ========================================================================

    /// Load a workflow from YAML file
    pub async fn load_workflow_from_file<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<WorkflowId> {
        let workflow = WorkflowParser::parse_file(path)
            .context("Failed to parse workflow manifest")?;

        self.register_workflow(workflow).await
    }

    /// Load a workflow from YAML string
    pub async fn load_workflow_from_yaml(&self, yaml: &str) -> Result<WorkflowId> {
        let workflow = WorkflowParser::parse_yaml(yaml)
            .context("Failed to parse workflow YAML")?;

        self.register_workflow(workflow).await
    }

    /// Register a workflow definition
    pub async fn register_workflow(&self, workflow: Workflow) -> Result<WorkflowId> {
        let workflow_id = workflow.id;
        let workflow_name = workflow.metadata.name.clone();

        info!(
            workflow_id = %workflow_id,
            workflow_name = %workflow_name,
            "Registering workflow"
        );

        let mut workflows = self.workflows.write().await;
        workflows.insert(workflow_name.clone(), workflow);

        Ok(workflow_id)
    }

    /// Get a workflow by name
    pub async fn get_workflow(&self, name: &str) -> Option<Workflow> {
        let workflows = self.workflows.read().await;
        workflows.get(name).cloned()
    }

    /// List all registered workflows
    pub async fn list_workflows(&self) -> Vec<String> {
        let workflows = self.workflows.read().await;
        workflows.keys().cloned().collect()
    }

    // ========================================================================
    // Workflow Execution
    // ========================================================================

    /// Start a new workflow execution
    pub async fn start_execution(
        &self,
        workflow_name: &str,
        execution_id: ExecutionId,
        input: WorkflowInput,
    ) -> Result<()> {
        // Get workflow definition
        let workflow = self
            .get_workflow(workflow_name)
            .await
            .ok_or_else(|| anyhow::anyhow!("Workflow '{}' not found", workflow_name))?;

        info!(
            execution_id = %execution_id,
            workflow_name = %workflow_name,
            "Starting workflow execution"
        );

        // Initialize execution state
        let mut workflow_execution = WorkflowExecution::new(&workflow);

        // Populate blackboard with input parameters
        for (key, value) in input.parameters.clone() {
            workflow_execution.blackboard.set(key, value);
        }
        
        // Create root execution context for depth tracking
        let execution_context = Execution::new(
            AgentId::new(), // Placeholder - workflows are not agents yet
            ExecutionInput {
                intent: Some(workflow_name.to_string()),
                payload: serde_json::to_value(input.parameters)?,
            },
            10, // Max iterations placeholder
        );

        // Store execution and context
        let mut executions = self.executions.write().await;
        executions.insert(execution_id, workflow_execution);
        
        let mut contexts = self.execution_contexts.write().await;
        contexts.insert(execution_id, execution_context);

        // Publish event
        self.event_bus
            .publish_execution_event(ExecutionEvent::ExecutionStarted {
                execution_id,
                agent_id: crate::domain::agent::AgentId::new(), // TODO: Map to agent
                started_at: Utc::now(),
            });

        Ok(())
    }

    /// Execute one FSM tick (process current state and transition)
    ///
    /// Returns:
    /// - `Ok(true)` if execution continues (not terminal)
    /// - `Ok(false)` if execution completed (terminal state reached)
    /// - `Err(...)` if execution failed
    pub async fn tick(&self, execution_id: ExecutionId) -> Result<bool> {
        // Get execution state
        let mut executions = self.executions.write().await;
        let workflow_execution = executions
            .get_mut(&execution_id)
            .ok_or_else(|| anyhow::anyhow!("Execution {} not found", execution_id))?;

        let current_state_name = workflow_execution.current_state.clone();

        // Get workflow definition
        let workflow = self
            .get_workflow_by_id(workflow_execution.workflow_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Workflow not found"))?;

        let current_state = workflow
            .get_state(&current_state_name)
            .ok_or_else(|| anyhow::anyhow!("State {} not found", current_state_name))?;

        debug!(
            execution_id = %execution_id,
            state = %current_state_name,
            "Executing workflow state"
        );

        // Execute state (this is simplified - actual implementation would delegate to handlers)
        let state_output = self.execute_state(&workflow, current_state, workflow_execution).await?;

        // Record output
        workflow_execution.record_state_output(current_state_name.clone(), state_output.clone());

        // Check if terminal state
        if current_state.transitions.is_empty() {
            info!(
                execution_id = %execution_id,
                state = %current_state_name,
                "Reached terminal state"
            );
            return Ok(false); // Execution complete
        }

        // Evaluate transitions
        let next_state = self
            .evaluate_transitions(current_state, &state_output, workflow_execution)
            .await?;

        // Transition to next state
        workflow_execution.transition_to(next_state.clone());

        info!(
            execution_id = %execution_id,
            from = %current_state_name,
            to = %next_state,
            "State transition"
        );

        Ok(true) // Continue execution
    }

    /// Run workflow to completion (blocking tick loop)
    pub async fn run_to_completion(&self, execution_id: ExecutionId) -> Result<()> {
        loop {
            let should_continue = self.tick(execution_id).await?;
            if !should_continue {
                break;
            }
        }
        Ok(())
    }

    // ========================================================================
    // State Execution (Simplified - TODO: Implement handlers)
    // ========================================================================

    async fn execute_state(
        &self,
        _workflow: &Workflow,
        state: &WorkflowState,
        workflow_execution: &WorkflowExecution,
    ) -> Result<serde_json::Value> {
        match &state.kind {
            StateKind::Agent { agent, input, .. } => {
                // Render input template with blackboard context
                let rendered_input = self.render_template(input, workflow_execution)?;
                
                debug!(agent = %agent, "Executing agent state");
                
                // TODO: Implement actual agent execution
                // For now, return structured output that can be used in transitions
                
                // Check if this is a judge agent (contains "judge" in name)
                if agent.to_lowercase().contains("judge") {
                    // Return judge-like output for gradient validation
                    Ok(serde_json::json!({
                        "score": 0.85,
                        "confidence": 0.90,
                        "reasoning": "Output meets requirements with minor improvements needed",
                        "suggestions": ["Add error handling", "Improve documentation"],
                        "verdict": "pass"
                    }))
                } else {
                    // Return generic agent output
                    Ok(serde_json::json!({
                        "output": format!("Agent {} executed with input: {}", agent, rendered_input),
                        "code": "print('Hello, World!')",
                        "success": true
                    }))
                }
            }

            StateKind::System { command, .. } => {
                debug!(command = %command, "Executing system state");
                
                // TODO: Execute system command
                // For now, return placeholder
                Ok(serde_json::json!({
                    "stdout": "Command output",
                    "stderr": "",
                    "exit_code": 0
                }))
            }

            StateKind::Human { prompt: _, .. } => {
                debug!("Executing human state");
                
                // TODO: Wait for human input
                // For now, return placeholder
                Ok(serde_json::json!({
                    "response": "yes",
                    "feedback": ""
                }))
            }

            StateKind::ParallelAgents { agents, .. } => {
                debug!(count = agents.len(), "Executing parallel agents state");
                
                // TODO: Execute agents in parallel and aggregate
                // For now, return placeholder
                Ok(serde_json::json!({
                    "final_score": 0.95,
                    "confidence": 0.9,
                    "individual_scores": [0.9, 0.95, 0.98]
                }))
            }
        }
    }

    // ========================================================================
    // Transition Evaluation
    // ========================================================================

    async fn evaluate_transitions(
        &self,
        state: &WorkflowState,
        state_output: &serde_json::Value,
        workflow_execution: &WorkflowExecution,
    ) -> Result<StateName> {
        // Evaluate transitions in order (first match wins)
        for transition in &state.transitions {
            if self
                .evaluate_condition(&transition.condition, state_output, workflow_execution)
                .await?
            {
                return Ok(transition.target.clone());
            }
        }

        // No transition matched (should not happen if workflow is well-formed)
        Err(anyhow::anyhow!("No transition condition matched"))
    }

    async fn evaluate_condition(
        &self,
        condition: &TransitionCondition,
        state_output: &serde_json::Value,
        workflow_execution: &WorkflowExecution,
    ) -> Result<bool> {
        Ok(match condition {
            TransitionCondition::Always => true,

            TransitionCondition::OnSuccess => {
                state_output.get("success").and_then(|v| v.as_bool()).unwrap_or(false)
            }

            TransitionCondition::OnFailure => {
                !state_output.get("success").and_then(|v| v.as_bool()).unwrap_or(true)
            }

            TransitionCondition::ExitCode0 => {
                state_output.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(1) == 0
            }

            TransitionCondition::ExitCodeNonZero => {
                state_output.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(1) != 0
            }

            TransitionCondition::ExitCode { value } => {
                state_output.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1) == (*value as i64)
            }

            TransitionCondition::ScoreAbove { threshold } => {
                let score = state_output.get("score")
                    .or_else(|| state_output.get("final_score"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                score > *threshold
            }

            TransitionCondition::ScoreBelow { threshold } => {
                let score = state_output.get("score")
                    .or_else(|| state_output.get("final_score"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                score < *threshold
            }

            TransitionCondition::ScoreBetween { min, max } => {
                let score = state_output.get("score")
                    .or_else(|| state_output.get("final_score"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                score >= *min && score <= *max
            }

            TransitionCondition::ConfidenceAbove { threshold } => {
                let confidence = state_output.get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                confidence > *threshold
            }

            TransitionCondition::Consensus { threshold, agreement } => {
                let score = state_output.get("final_score")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let conf = state_output.get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                score > *threshold && conf > *agreement
            }

            TransitionCondition::AllApproved => {
                // TODO: Check all individual scores
                true
            }

            TransitionCondition::AnyRejected => {
                // TODO: Check if any individual score is below threshold
                false
            }

            TransitionCondition::InputEquals { value } => {
                state_output.get("response")
                    .and_then(|v| v.as_str())
                    .map(|s| s == value)
                    .unwrap_or(false)
            }

            TransitionCondition::InputEqualsYes => {
                state_output.get("response")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_lowercase() == "yes")
                    .unwrap_or(false)
            }

            TransitionCondition::InputEqualsNo => {
                state_output.get("response")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_lowercase() == "no")
                    .unwrap_or(false)
            }

            TransitionCondition::Custom { expression } => {
                // TODO: Evaluate Handlebars expression as boolean
                self.evaluate_expression(expression, workflow_execution)?
            }
        })
    }

    // ========================================================================
    // Template Rendering
    // ========================================================================

    fn render_template(
        &self,
        template: &str,
        workflow_execution: &WorkflowExecution,
    ) -> Result<String> {
        // Build template context
        let mut context = serde_json::json!({
            "workflow": {}, // TODO: Add workflow context
            "blackboard": workflow_execution.blackboard.data(),
            "state": {} // TODO: Add current state data
        });

        // Add state outputs
        for (state_name, output) in &workflow_execution.state_outputs {
            context[state_name.as_str()] = output.clone();
        }

        // Render template
        let rendered = self
            .template_engine
            .render_template(template, &context)
            .context("Failed to render template")?;

        Ok(rendered)
    }

    fn evaluate_expression(
        &self,
        expression: &str,
        workflow_execution: &WorkflowExecution,
    ) -> Result<bool> {
        // Simplified: render template and check if result is "true"
        let rendered = self.render_template(expression, workflow_execution)?;
        Ok(rendered.trim() == "true")
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    async fn get_workflow_by_id(&self, workflow_id: WorkflowId) -> Option<Workflow> {
        let workflows = self.workflows.read().await;
        workflows.values().find(|w| w.id == workflow_id).cloned()
    }

    /// Get execution context by ID (for recursive execution tracking)
    pub async fn get_execution_context(&self, execution_id: &ExecutionId) -> Option<Execution> {
        let contexts = self.execution_contexts.read().await;
        contexts.get(execution_id).cloned()
    }

    /// Check if execution can spawn a child (respects MAX_RECURSIVE_DEPTH)
    pub async fn can_spawn_child(&self, execution_id: &ExecutionId) -> bool {
        self.get_execution_context(execution_id)
            .await
            .map(|ctx| ctx.can_spawn_child())
            .unwrap_or(false)
    }

    /// Get current recursion depth for execution
    pub async fn get_execution_depth(&self, execution_id: &ExecutionId) -> Option<u8> {
        self.get_execution_context(execution_id)
            .await
            .map(|ctx| ctx.depth())
    }
}

// ============================================================================
// Value Objects
// ============================================================================

/// Input for starting a workflow execution
#[derive(Debug, Clone)]
pub struct WorkflowInput {
    /// Parameters passed to workflow context
    pub parameters: HashMap<String, serde_json::Value>,
}

impl WorkflowInput {
    pub fn new() -> Self {
        Self {
            parameters: HashMap::new(),
        }
    }

    pub fn with_parameter(
        mut self,
        key: impl Into<String>,
        value: impl serde::Serialize,
    ) -> Result<Self> {
        let value = serde_json::to_value(value)?;
        self.parameters.insert(key.into(), value);
        Ok(self)
    }
}

impl Default for WorkflowInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_workflow_engine_creation() {
        let event_bus = EventBus::with_default_capacity();
        let engine = WorkflowEngine::new(Arc::new(event_bus));
        
        let workflows = engine.list_workflows().await;
        assert_eq!(workflows.len(), 0);
    }

    #[tokio::test]
    async fn test_load_simple_workflow() {
        let event_bus = EventBus::with_default_capacity();
        let engine = WorkflowEngine::new(Arc::new(event_bus));

        let yaml = r#"
apiVersion: 100monkeys.ai/v1
kind: Workflow
metadata:
  name: test-workflow
spec:
  initial_state: START
  states:
    START:
      kind: System
      command: echo "hello"
      transitions: []
"#;

        let result = engine.load_workflow_from_yaml(yaml).await;
        assert!(result.is_ok());

        let workflows = engine.list_workflows().await;
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0], "test-workflow");
    }
}
