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
use crate::domain::repository::{WorkflowRepository, WorkflowExecutionRepository};
use crate::infrastructure::workflow_parser::WorkflowParser;
use crate::infrastructure::event_bus::EventBus;
use crate::application::validation_service::ValidationService;
use crate::application::execution::ExecutionService;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{debug, info};
use futures::StreamExt;
use crate::infrastructure::temporal_client::TemporalClient;

// Import Cortex service
use aegis_cortex::application::CortexService;
use aegis_cortex::infrastructure::EmbeddingClient;

// ============================================================================
// Application Service: WorkflowEngine
// ============================================================================

/// Workflow Engine (Application Service)
///
/// Orchestrates workflow execution using FSM pattern.
pub struct WorkflowEngine {
    /// Workflow repository for persistence
    repository: Arc<dyn WorkflowRepository>,

    /// Workflow execution repository for persistence
    workflow_execution_repository: Arc<dyn WorkflowExecutionRepository>,
    
    /// Active workflow executions (execution_id -> workflow_execution)
    executions: Arc<tokio::sync::RwLock<HashMap<ExecutionId, WorkflowExecution>>>,
    
    /// Execution tracking for recursive calls (execution_id -> Execution)
    /// Made public for testing purposes
    pub execution_contexts: Arc<tokio::sync::RwLock<HashMap<ExecutionId, Execution>>>,
    
    /// Event bus for publishing domain events
    event_bus: Arc<EventBus>,

    /// Validation service for multi-judge consensus
    validation_service: Arc<ValidationService>,

    /// Execution service for running agents
    execution_service: Arc<dyn ExecutionService>,
    
    /// Cortex service for pattern learning (Optional)
    cortex_service: Option<Arc<dyn CortexService>>,
    
    /// Embedding client for generating semantic embeddings (Optional)
    embedding_client: Option<Arc<EmbeddingClient>>,
    
    /// Template renderer (Handlebars)
    template_engine: Arc<handlebars::Handlebars<'static>>,
    
    /// Temporal client for starting workflows (Optional, Hot-swappable)
    /// Wrapped in RwLock to allow background connection/reconnection
    temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
}

impl WorkflowEngine {
    /// Create a new WorkflowEngine
    pub fn new(
        repository: Arc<dyn WorkflowRepository>,
        workflow_execution_repository: Arc<dyn WorkflowExecutionRepository>,
        event_bus: Arc<EventBus>,
        validation_service: Arc<ValidationService>,
        execution_service: Arc<dyn ExecutionService>,
        temporal_client: Arc<tokio::sync::RwLock<Option<Arc<TemporalClient>>>>,
        cortex_service: Option<Arc<dyn CortexService>>,
    ) -> Self {
        // Create embedding client if Cortex is enabled
        let embedding_client = cortex_service.as_ref().map(|_| Arc::new(EmbeddingClient::new()));
        
        Self {
            repository,
            workflow_execution_repository,
            executions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            execution_contexts: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            event_bus,
            validation_service,
            execution_service,
            cortex_service,
            embedding_client,
            template_engine: Arc::new(handlebars::Handlebars::new()),
            temporal_client,
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

        self.repository.save(&workflow).await?;

        Ok(workflow_id)
    }

    /// Get a workflow by name
    pub async fn get_workflow(&self, name: &str) -> Option<Workflow> {
        self.repository.find_by_name(name).await.ok().flatten()
    }

    /// List all registered workflows
    pub async fn list_workflows(&self) -> Vec<String> {
        match self.repository.list_all().await {
            Ok(workflows) => workflows.into_iter().map(|w| w.metadata.name).collect(),
            Err(_) => vec![],
        }
    }

    /// List agent executions (proxy to ExecutionService)
    pub async fn list_executions(
        &self,
        agent_id: Option<AgentId>,
        limit: usize,
    ) -> Result<Vec<crate::domain::execution::ExecutionInfo>> {
        let executions = self.execution_service.list_executions(agent_id, limit).await?;
        Ok(executions.into_iter().map(crate::domain::execution::ExecutionInfo::from).collect())
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
        let mut workflow_execution = WorkflowExecution::new(
            &workflow, 
            execution_id, 
            serde_json::to_value(&input.parameters)?
        );

        // Populate blackboard with input parameters
        for (key, value) in input.parameters.clone() {
            workflow_execution.blackboard.set(key, value);
        }
        
        // Create root execution context for depth tracking
        let execution_context = Execution::new(
            AgentId::new(), // Placeholder - workflows are not agents yet
            ExecutionInput {
                intent: Some(workflow_name.to_string()),
                payload: serde_json::to_value(&input.parameters)?,
            },
            10, // Max iterations placeholder
        );

        // Persist initial state (before moving into in-memory store)
        if let Err(e) = self.workflow_execution_repository.save(&workflow_execution).await {
            tracing::error!(execution_id = %execution_id, error = %e, "Failed to persist workflow execution");
        }

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

        // Start Temporal Workflow if client is available
        let client_opt = self.temporal_client.read().await;
        if let Some(client) = client_opt.as_ref() {
            info!("Triggering Temporal workflow: {}", workflow_name);
            match client.start_workflow(workflow_name, execution_id, input.parameters).await {
                Ok(run_id) => {
                    info!(execution_id = %execution_id, run_id = %run_id, "Temporal workflow started");
                }
                Err(e) => {
                    tracing::error!(execution_id = %execution_id, error = %e, "Failed to start Temporal workflow");
                    // We don't fail the whole request? Or should we?
                    // Ideally we should fail.
                    return Err(anyhow::anyhow!("Failed to start Temporal workflow: {}", e));
                }
            }
        } else {
            tracing::warn!("Temporal client not configured - workflow will not be executed in Temporal!");
        }

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
        let state_output = self.execute_state(&workflow, current_state, workflow_execution, execution_id).await?;

        // Record output
        workflow_execution.record_state_output(current_state_name.clone(), state_output.clone());

        // Check if terminal state
        if current_state.transitions.is_empty() {
            info!(
                state = %current_state_name,
                "Reached terminal state"
            );
            
            // Mark as completed in the local object (WorkflowExecution struct needs to update its status!)
            // Currently WorkflowExecution struct has status field I added.
            workflow_execution.status = crate::domain::execution::ExecutionStatus::Completed;
            
            // Persist final state
            if let Err(e) = self.workflow_execution_repository.save(workflow_execution).await {
                 tracing::error!(execution_id = %execution_id, error = %e, "Failed to persist completed workflow execution");
            }

            return Ok(false); // Execution complete
        }

        // Evaluate transitions
        let next_state = self
            .evaluate_transitions(current_state, &state_output, workflow_execution)
            .await?;

        // Transition to next state
        workflow_execution.transition_to(next_state.clone());

        // Persist state
        if let Err(e) = self.workflow_execution_repository.save(workflow_execution).await {
             tracing::error!(execution_id = %execution_id, error = %e, "Failed to persist workflow execution state");
        }

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
        workflow: &Workflow,
        state: &WorkflowState,
        workflow_execution: &WorkflowExecution,
        execution_id: ExecutionId,
    ) -> Result<serde_json::Value> {
        match &state.kind {
            StateKind::Agent { agent, input, .. } => {
                // Render input template with blackboard context
                let rendered_input = self.render_template(input, workflow_execution)?;
                
                debug!(agent = %agent, "Executing agent state");
                
                // Parse agent ID (assuming name is UUID for now)
                let agent_id = crate::domain::agent::AgentId::from_string(agent)
                    .map_err(|_| anyhow::anyhow!("Invalid agent ID: {}", agent))?;

                // Prepare execution input
                let execution_input = crate::domain::execution::ExecutionInput {
                    intent: Some(rendered_input.clone()),
                    payload: serde_json::json!({
                        "workflow_id": workflow.id,
                        "state": state.kind,
                        "context": workflow_execution.blackboard.data()
                    }),
                };

                // Start execution via ExecutionService
                let agent_execution_id = self.execution_service.start_execution(agent_id, execution_input).await?;
                
                info!(agent_execution_id = %agent_execution_id, "Started agent execution");

                // Wait for completion via event stream
                let mut stream = self.execution_service.stream_execution(agent_execution_id).await?;
                
                while let Some(event_result) = stream.next().await {
                    let event = event_result?;
                    match event {

                        ExecutionEvent::ExecutionCompleted { final_output, .. } => {
                            // Cortex Integration: Capture pattern on success
                            // Use workflow execution_id (not agent_execution_id) for workflow-level correlation
                            if let Some(cortex) = &self.cortex_service {
                                self.capture_execution_pattern(cortex, workflow, state, &final_output, execution_id).await
                                    .unwrap_or_else(|e| tracing::warn!("Failed to capture pattern: {}", e));
                            }

                            return Ok(serde_json::json!({
                                "output": final_output,
                                "agent_execution_id": agent_execution_id, // agent execution ID for tracking
                                "success": true
                            }));
                        },
                        ExecutionEvent::ExecutionFailed { reason, .. } => {
                            return Err(anyhow::anyhow!("Agent execution failed: {}", reason));
                        },
                        ExecutionEvent::ExecutionCancelled { reason, .. } => {
                            let reason = reason.unwrap_or_else(|| "Cancelled".to_string());
                            return Err(anyhow::anyhow!("Agent execution cancelled: {}", reason));
                        },
                        _ => {} // Ignore intermediate events
                    }
                }

                // If stream ends without terminal state (should not happen)
                Err(anyhow::anyhow!("Execution stream ended unexpectedly"))
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
                
                // Collect agents to run
                let mut agent_ids = Vec::new();
                // TODO: Map string names to AgentIds. For now assuming they are UUID strings.
                // In a real implementation, we would look up AgentId by name.
                for config in agents {
                    if let Ok(id) = crate::domain::agent::AgentId::from_string(&config.agent) {
                        agent_ids.push(id);
                    } else {
                         // warning: invalid agent id
                    }
                }

                // Construct validation request (assuming this is a validation step)
                // We use the input from the first agent config as the content? 
                // Or maybe the blackboard has the content?
                // For ParallelAgents, usually they all get the same input or variations.
                // The `input` field in ParallelAgentConfig is a template.
                
                // Let's assume the first agent's input template renders to the content we want validated.
                // Or if there are no agents, we return empty.
                if agent_ids.is_empty() {
                     return Ok(serde_json::json!({
                        "error": "No valid agents found for parallel execution"
                     }));
                }

                // Render the input for the first agent (as a proxy for the request content)
                // This is a simplification. Ideally, ValidationRequest should be explicit.
                let content = if let Some(first_config) = agents.first() {
                    self.render_template(&first_config.input, workflow_execution)?
                } else {
                    "No content".to_string()
                };

                let request = crate::domain::validation::ValidationRequest {
                    content,
                    criteria: "Evaluate based on system instructions".to_string(), // TODO: Make configurable
                    context: Some(serde_json::to_value(workflow_execution.blackboard.data())?),
                };

                // Execute validation
                match self.validation_service.validate_with_judges(execution_id, request, agent_ids).await {
                    Ok(consensus) => Ok(serde_json::to_value(consensus)?),
                    Err(e) => Err(anyhow::anyhow!("Validation failed: {}", e)),
                }
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

            TransitionCondition::ExitCodeZero => {
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
        self.repository.find_by_id(workflow_id).await.ok().flatten()
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

    /// Get the temporal client if available
    pub async fn get_temporal_client(&self) -> Option<Arc<TemporalClient>> {
        self.temporal_client.read().await.clone()
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

    // ========================================================================
    // Cortex Pattern Capture
    // ========================================================================

impl WorkflowEngine {

    async fn capture_execution_pattern(
        &self,
        cortex: &Arc<dyn CortexService>,
        workflow: &Workflow,
        state: &WorkflowState,
        final_output: &str,
        execution_id: ExecutionId,
    ) -> Result<()> {
        // Only capture patterns for Agents for now
        let (_agent_name, input_template) = match &state.kind {
            StateKind::Agent { agent, input, .. } => (agent, input),
            _ => return Ok(()),
        };

        // 1. Generate Error Signature (Context/Intent)
        // For successful execution, the "error" is actually the "task" or "intent"
        // We use the input content as the signature of the problem being solved.
        let signature = aegis_cortex::domain::ErrorSignature::new(
            "task_execution".to_string(),
            input_template, // Using the raw template as the signature base for now
        );

        // 2. Generate Embedding using EmbeddingClient
        let embedding = if let Some(client) = &self.embedding_client {
            client.generate_embedding(&format!("{}{}", input_template, final_output)).await?
        } else {
            // Fallback to simple hash-based embedding if client not available
            vec![0.0; 384]
        };

        // 3. Store in Cortex
        // We assume success since we are in the success branch
        let pattern_id = cortex.store_pattern(
            Some(execution_id.0), // Pass underlying Uuid
            signature,
            final_output.to_string(),
            workflow.metadata.name.clone(), // Category
            embedding,
        ).await?;

        // 4. Reinforce (Success)
        cortex.apply_dopamine(pattern_id, Some(execution_id.0), 0.5).await?;

        debug!("Captured execution pattern: {}", pattern_id.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::execution::ExecutionService;
    use crate::domain::execution::{Execution, ExecutionInput, Iteration, LlmInteraction};
    use crate::infrastructure::event_bus::DomainEvent;
    use async_trait::async_trait;

    struct MockExecutionService;

    #[async_trait]
    impl ExecutionService for MockExecutionService {
        async fn start_execution(&self, _agent_id: AgentId, _input: ExecutionInput) -> Result<ExecutionId> {
            Ok(ExecutionId::new())
        }
        async fn get_execution(&self, _id: ExecutionId) -> Result<Execution> {
            Ok(Execution::new(AgentId::new(), ExecutionInput { intent: None, payload: serde_json::Value::Null }, 3))
        }
        async fn get_iterations(&self, _exec_id: ExecutionId) -> Result<Vec<Iteration>> { Ok(vec![]) }
        async fn cancel_execution(&self, _id: ExecutionId) -> Result<()> { Ok(()) }
        async fn stream_execution(&self, id: ExecutionId) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<ExecutionEvent>> + Send>>> {
             let event = ExecutionEvent::ExecutionCompleted {
                 execution_id: id,
                 agent_id: AgentId::new(),
                 final_output: "mock output".to_string(),
                 total_iterations: 1,
                 completed_at: chrono::Utc::now(),
             };
             Ok(Box::pin(futures::stream::iter(vec![Ok(event)])))
        }
        async fn stream_agent_events(&self, _id: AgentId) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<DomainEvent>> + Send>>> {
             Ok(Box::pin(futures::stream::empty()))
        }
        async fn list_executions(&self, _agent_id: Option<AgentId>, _limit: usize) -> Result<Vec<Execution>> { Ok(vec![]) }
        async fn delete_execution(&self, _id: ExecutionId) -> Result<()> { Ok(()) }
        async fn record_llm_interaction(&self, _execution_id: ExecutionId, _iteration: u8, _interaction: LlmInteraction) -> Result<()> { Ok(()) }
    }

    #[tokio::test]
    async fn test_workflow_engine_creation() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let exec_service = Arc::new(MockExecutionService);
        let val_service = Arc::new(ValidationService::new(event_bus.clone(), exec_service.clone(), None));
        let repository = Arc::new(crate::infrastructure::repositories::InMemoryWorkflowRepository::new());
        let workflow_execution_repo = Arc::new(crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository::new());
        
        // Note: Cortex service is None here
        let engine = WorkflowEngine::new(repository, workflow_execution_repo, event_bus, val_service, exec_service, Arc::new(tokio::sync::RwLock::new(None)), None);
        
        let workflows = engine.list_workflows().await;
        assert_eq!(workflows.len(), 0);
    }

    #[tokio::test]
    async fn test_load_simple_workflow() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let exec_service = Arc::new(MockExecutionService);
        let val_service = Arc::new(ValidationService::new(event_bus.clone(), exec_service.clone(), None));
        let repository = Arc::new(crate::infrastructure::repositories::InMemoryWorkflowRepository::new());
        let workflow_execution_repo = Arc::new(crate::infrastructure::repositories::InMemoryWorkflowExecutionRepository::new());
        
        let engine = WorkflowEngine::new(repository, workflow_execution_repo, event_bus, val_service, exec_service, Arc::new(tokio::sync::RwLock::new(None)), None);

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
