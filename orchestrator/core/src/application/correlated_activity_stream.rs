// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::agent::AgentId;
use crate::domain::events::{
    CorrelatedActivityEvent, ExecutionEvent, StorageEvent, ValidationEvent, WorkflowEvent,
};
use crate::domain::execution::{Execution, ExecutionId, ExecutionStatus, IterationStatus};
use crate::domain::repository::{ExecutionRepository, WorkflowExecutionRepository};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::DomainEvent;
use crate::infrastructure::event_bus::{EventBus, EventBusError};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct CorrelatedActivityStreamService {
    event_bus: Arc<EventBus>,
    execution_repository: Arc<dyn ExecutionRepository>,
    workflow_execution_repository: Option<Arc<dyn WorkflowExecutionRepository>>,
}

impl CorrelatedActivityStreamService {
    pub fn new(
        event_bus: Arc<EventBus>,
        execution_repository: Arc<dyn ExecutionRepository>,
        workflow_execution_repository: Option<Arc<dyn WorkflowExecutionRepository>>,
    ) -> Self {
        Self {
            event_bus,
            execution_repository,
            workflow_execution_repository,
        }
    }

    pub async fn stream_execution_activity(
        &self,
        execution_id: ExecutionId,
        verbose: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<CorrelatedActivityEvent>> + Send>>> {
        let history = self.execution_history(execution_id, verbose).await?;

        if verbose {
            // In verbose mode, subscribe to the global event bus so we also see
            // system-level events that have no execution_id (e.g. MCP server lifecycle).
            let receiver = self.event_bus.subscribe();
            let live = futures::stream::unfold(
                (receiver, execution_id),
                |(mut receiver, exec_id)| async move {
                    loop {
                        match receiver.recv().await {
                            Ok(event) => {
                                // Include events that match this execution OR have no execution_id
                                // (system-level events visible only in verbose mode).
                                let event_exec_id = event.execution_id();
                                if event_exec_id == Some(exec_id) || event_exec_id.is_none() {
                                    return Some((
                                        Ok(normalize_domain_event(&event, None)),
                                        (receiver, exec_id),
                                    ));
                                }
                                continue;
                            }
                            Err(EventBusError::Closed) => return None,
                            Err(e) => {
                                return Some((
                                    Err(anyhow!("Event bus error: {e}")),
                                    (receiver, exec_id),
                                ));
                            }
                        }
                    }
                },
            );

            let history_stream = futures::stream::iter(history.into_iter().map(Ok));
            Ok(Box::pin(history_stream.chain(live)))
        } else {
            let receiver = self.event_bus.subscribe_execution_domain(execution_id);
            let live = futures::stream::unfold(receiver, |mut receiver| async move {
                match receiver.recv().await {
                    Ok(event) => Some((Ok(normalize_domain_event(&event, None)), receiver)),
                    Err(EventBusError::Closed) => None,
                    Err(e) => Some((Err(anyhow!("Event bus error: {e}")), receiver)),
                }
            });

            let history_stream = futures::stream::iter(history.into_iter().map(Ok));
            Ok(Box::pin(history_stream.chain(live)))
        }
    }

    pub async fn stream_agent_activity(
        &self,
        agent_id: AgentId,
        tenant_id: &TenantId,
        verbose: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<CorrelatedActivityEvent>> + Send>>> {
        let history = self.agent_history(agent_id, tenant_id, verbose).await?;
        let repository = Arc::clone(&self.execution_repository);
        let cache = Arc::new(RwLock::new(HashMap::<ExecutionId, AgentId>::new()));
        let receiver = self.event_bus.subscribe();

        let live = futures::stream::unfold(
            (receiver, repository, cache, verbose),
            move |(mut receiver, repository, cache, verbose)| async move {
                loop {
                    match receiver.recv().await {
                        Ok(event) => {
                            match resolve_agent_for_event(&event, &repository, &cache).await {
                                Ok(Some(resolved_agent_id)) if resolved_agent_id == agent_id => {
                                    let normalized =
                                        normalize_domain_event(&event, Some(resolved_agent_id));
                                    return Some((
                                        Ok(normalized),
                                        (receiver, repository, cache, verbose),
                                    ));
                                }
                                Ok(None) if verbose => {
                                    // In verbose mode, emit unresolvable events (system-level
                                    // events with no agent_id) with agent_id: None so they are
                                    // visible to the caller.
                                    let normalized = normalize_domain_event(&event, None);
                                    return Some((
                                        Ok(normalized),
                                        (receiver, repository, cache, verbose),
                                    ));
                                }
                                Ok(_) => continue,
                                Err(error) => {
                                    return Some((
                                        Err(error),
                                        (receiver, repository, cache, verbose),
                                    ));
                                }
                            }
                        }
                        Err(EventBusError::Closed) => return None,
                        Err(error) => {
                            return Some((
                                Err(anyhow!("Event bus error: {error}")),
                                (receiver, repository, cache, verbose),
                            ));
                        }
                    }
                }
            },
        );

        let history_stream = futures::stream::iter(history.into_iter().map(Ok));
        Ok(Box::pin(history_stream.chain(live)))
    }

    pub async fn execution_history(
        &self,
        execution_id: ExecutionId,
        _verbose: bool,
    ) -> Result<Vec<CorrelatedActivityEvent>> {
        let mut history = Vec::new();

        if let Some(execution) = self
            .execution_repository
            .find_by_id_unscoped(execution_id)
            .await?
        {
            history.extend(execution_to_history(&execution));
        }

        if let Some(repo) = &self.workflow_execution_repository {
            let records = repo.find_events_by_execution(execution_id, 500, 0).await?;
            for record in records {
                if let Some(event) = workflow_record_to_activity(record.payload) {
                    history.push(event);
                }
            }
        }

        history.sort_by_key(|a| a.timestamp);
        Ok(history)
    }

    pub async fn agent_history(
        &self,
        agent_id: AgentId,
        tenant_id: &TenantId,
        _verbose: bool,
    ) -> Result<Vec<CorrelatedActivityEvent>> {
        let executions = self
            .execution_repository
            .find_by_agent_for_tenant(tenant_id, agent_id, 50)
            .await?;
        let mut history = Vec::new();

        for execution in executions {
            history.extend(execution_to_history(&execution));
            if let Some(repo) = &self.workflow_execution_repository {
                let records = repo.find_events_by_execution(execution.id, 500, 0).await?;
                for record in records {
                    if let Some(event) = workflow_record_to_activity(record.payload) {
                        history.push(event);
                    }
                }
            }
        }

        history.sort_by_key(|a| a.timestamp);
        Ok(history)
    }
}

async fn resolve_agent_for_event(
    event: &DomainEvent,
    repository: &Arc<dyn ExecutionRepository>,
    cache: &Arc<RwLock<HashMap<ExecutionId, AgentId>>>,
) -> Result<Option<AgentId>> {
    if let Some(agent_id) = event.agent_id() {
        return Ok(Some(agent_id));
    }

    let Some(execution_id) = event.execution_id() else {
        return Ok(None);
    };

    if let Some(agent_id) = cache.read().await.get(&execution_id).copied() {
        return Ok(Some(agent_id));
    }

    let execution = repository
        .find_by_id_unscoped(execution_id)
        .await?
        .ok_or_else(|| {
            anyhow!("Execution {execution_id} not found while correlating agent activity")
        })?;
    let agent_id = execution.agent_id;
    cache.write().await.insert(execution_id, agent_id);
    Ok(Some(agent_id))
}

fn execution_to_history(execution: &Execution) -> Vec<CorrelatedActivityEvent> {
    let mut history = Vec::new();

    history.push(normalize_domain_event(
        &DomainEvent::Execution(ExecutionEvent::ExecutionStarted {
            execution_id: execution.id,
            agent_id: execution.agent_id,
            started_at: execution.started_at,
        }),
        None,
    ));

    for iteration in execution.iterations() {
        history.push(normalize_domain_event(
            &DomainEvent::Execution(ExecutionEvent::IterationStarted {
                execution_id: execution.id,
                agent_id: execution.agent_id,
                iteration_number: iteration.number,
                action: iteration.action.clone(),
                started_at: iteration.started_at,
            }),
            None,
        ));

        for interaction in &iteration.llm_interactions {
            history.push(normalize_domain_event(
                &DomainEvent::Execution(ExecutionEvent::LlmInteraction {
                    execution_id: execution.id,
                    agent_id: execution.agent_id,
                    iteration_number: iteration.number,
                    provider: interaction.provider.clone(),
                    model: interaction.model.clone(),
                    input_tokens: None,
                    output_tokens: None,
                    prompt: interaction.prompt.clone(),
                    response: interaction.response.clone(),
                    timestamp: interaction.timestamp,
                }),
                None,
            ));
        }

        if let Some(results) = &iteration.validation_results {
            if let Some(gradient) = &results.gradient {
                history.push(normalize_domain_event(
                    &DomainEvent::Execution(ExecutionEvent::Validation(
                        ValidationEvent::GradientValidationPerformed {
                            execution_id: execution.id,
                            agent_id: execution.agent_id,
                            iteration_number: iteration.number,
                            score: gradient.score,
                            confidence: gradient.confidence,
                            validated_at: iteration.ended_at.unwrap_or(iteration.started_at),
                        },
                    )),
                    None,
                ));
            }

            if let Some(consensus) = &results.consensus {
                history.push(normalize_domain_event(
                    &DomainEvent::Execution(ExecutionEvent::Validation(
                        ValidationEvent::MultiJudgeConsensus {
                            execution_id: execution.id,
                            agent_id: execution.agent_id,
                            judge_scores: consensus
                                .individual_results
                                .iter()
                                .map(|(judge_id, result)| (*judge_id, result.score))
                                .collect(),
                            final_score: consensus.final_score,
                            confidence: consensus.consensus_confidence,
                            reached_at: iteration.ended_at.unwrap_or(iteration.started_at),
                        },
                    )),
                    None,
                ));
            }
        }

        if let Some(code_diff) = &iteration.code_changes {
            history.push(normalize_domain_event(
                &DomainEvent::Execution(ExecutionEvent::RefinementApplied {
                    execution_id: execution.id,
                    agent_id: execution.agent_id,
                    iteration_number: iteration.number,
                    code_diff: code_diff.clone(),
                    applied_at: iteration.ended_at.unwrap_or(iteration.started_at),
                    cortex_pattern_id: None,
                    cortex_pattern_category: None,
                    cortex_success_score: None,
                    cortex_solution_approach: None,
                }),
                None,
            ));
        }

        match iteration.status {
            IterationStatus::Success => {
                if let Some(output) = &iteration.output {
                    history.push(normalize_domain_event(
                        &DomainEvent::Execution(ExecutionEvent::IterationCompleted {
                            execution_id: execution.id,
                            agent_id: execution.agent_id,
                            iteration_number: iteration.number,
                            output: output.clone(),
                            completed_at: iteration.ended_at.unwrap_or(iteration.started_at),
                        }),
                        None,
                    ));
                }
            }
            IterationStatus::Failed => {
                if let Some(error) = &iteration.error {
                    history.push(normalize_domain_event(
                        &DomainEvent::Execution(ExecutionEvent::IterationFailed {
                            execution_id: execution.id,
                            agent_id: execution.agent_id,
                            iteration_number: iteration.number,
                            error: error.clone(),
                            failed_at: iteration.ended_at.unwrap_or(iteration.started_at),
                        }),
                        None,
                    ));
                }
            }
            IterationStatus::Running | IterationStatus::Refining => {}
        }
    }

    match execution.status {
        ExecutionStatus::Completed => {
            history.push(normalize_domain_event(
                &DomainEvent::Execution(ExecutionEvent::ExecutionCompleted {
                    execution_id: execution.id,
                    agent_id: execution.agent_id,
                    final_output: execution
                        .iterations()
                        .last()
                        .and_then(|iteration| iteration.output.clone())
                        .unwrap_or_default(),
                    total_iterations: execution.iterations().len() as u8,
                    completed_at: execution.ended_at.unwrap_or(execution.started_at),
                }),
                None,
            ));
        }
        ExecutionStatus::Failed => {
            history.push(normalize_domain_event(
                &DomainEvent::Execution(ExecutionEvent::ExecutionFailed {
                    execution_id: execution.id,
                    agent_id: execution.agent_id,
                    reason: execution
                        .error
                        .clone()
                        .unwrap_or_else(|| "Execution failed".to_string()),
                    total_iterations: execution.iterations().len() as u8,
                    failed_at: execution.ended_at.unwrap_or(execution.started_at),
                }),
                None,
            ));
        }
        ExecutionStatus::Cancelled => {
            history.push(normalize_domain_event(
                &DomainEvent::Execution(ExecutionEvent::ExecutionCancelled {
                    execution_id: execution.id,
                    agent_id: execution.agent_id,
                    reason: execution.error.clone(),
                    cancelled_at: execution.ended_at.unwrap_or(execution.started_at),
                }),
                None,
            ));
        }
        ExecutionStatus::Pending | ExecutionStatus::Running => {}
    }

    history
}

fn workflow_record_to_activity(payload: Value) -> Option<CorrelatedActivityEvent> {
    let event_type = payload.get("event_type")?.as_str()?.to_string();
    let execution_id = payload
        .get("execution_id")
        .and_then(Value::as_str)
        .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
        .map(ExecutionId);
    let timestamp = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let iteration = payload
        .get("iteration_number")
        .and_then(Value::as_u64)
        .map(|value| value as u8);
    let state_name = payload
        .get("state_name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let category = if event_type == "RefinementApplied" {
        "execution"
    } else {
        "workflow"
    };
    let message = if event_type == "RefinementApplied" {
        format!(
            "Applied refinement for iteration {}",
            iteration.unwrap_or_default()
        )
    } else if let Some(state_name) = &state_name {
        format!("{event_type} in state {state_name}")
    } else {
        event_type.clone()
    };

    Some(CorrelatedActivityEvent {
        event_type: to_snake_case(&event_type),
        category: category.to_string(),
        timestamp,
        execution_id,
        agent_id: None,
        iteration,
        stage: Some(if category == "workflow" {
            "workflow".to_string()
        } else {
            "iteration".to_string()
        }),
        message,
        details: payload,
    })
}

pub fn normalize_domain_event(
    event: &DomainEvent,
    resolved_agent_id: Option<AgentId>,
) -> CorrelatedActivityEvent {
    CorrelatedActivityEvent {
        event_type: event.event_type_name().to_string(),
        category: event.category().to_string(),
        timestamp: event.timestamp(),
        execution_id: event.execution_id(),
        agent_id: resolved_agent_id.or_else(|| event.agent_id()),
        iteration: event.iteration_number(),
        stage: event.stage().map(ToOwned::to_owned),
        message: event_message(event),
        details: serde_json::to_value(event).unwrap_or(Value::Null),
    }
}

fn event_message(event: &DomainEvent) -> String {
    match event {
        DomainEvent::Execution(ExecutionEvent::ExecutionStarted { execution_id, .. }) => {
            format!("Execution {execution_id} started")
        }
        DomainEvent::Execution(ExecutionEvent::IterationStarted {
            iteration_number,
            action,
            ..
        }) => format!("Iteration {iteration_number} started: {action}"),
        DomainEvent::Execution(ExecutionEvent::IterationCompleted {
            iteration_number, ..
        }) => format!("Iteration {iteration_number} completed"),
        DomainEvent::Execution(ExecutionEvent::IterationFailed {
            iteration_number,
            error,
            ..
        }) => format!("Iteration {iteration_number} failed: {}", error.message),
        DomainEvent::Execution(ExecutionEvent::RefinementApplied {
            iteration_number, ..
        }) => format!("Applied refinement after iteration {iteration_number}"),
        DomainEvent::Execution(ExecutionEvent::ExecutionCompleted {
            total_iterations, ..
        }) => format!("Execution completed after {total_iterations} iterations"),
        DomainEvent::Execution(ExecutionEvent::ExecutionFailed { reason, .. }) => {
            format!("Execution failed: {reason}")
        }
        DomainEvent::Execution(ExecutionEvent::ExecutionCancelled { reason, .. }) => match reason {
            Some(reason) => format!("Execution cancelled: {reason}"),
            None => "Execution cancelled".to_string(),
        },
        DomainEvent::Execution(ExecutionEvent::ExecutionTimedOut {
            timeout_seconds, ..
        }) => format!("Execution timed out after {timeout_seconds}s"),
        DomainEvent::Execution(ExecutionEvent::ConsoleOutput {
            iteration_number,
            stream,
            ..
        }) => format!("Console {stream} output on iteration {iteration_number}"),
        DomainEvent::Execution(ExecutionEvent::LlmInteraction {
            iteration_number,
            provider,
            model,
            ..
        }) => format!("LLM interaction on iteration {iteration_number} via {provider}/{model}"),
        DomainEvent::Execution(ExecutionEvent::LlmCallFailed {
            iteration_number,
            provider,
            model,
            error_class,
            message,
            ..
        }) => format!(
            "LLM call failed on iteration {iteration_number} via {provider}/{model}: {error_class:?} - {message}"
        ),
        DomainEvent::Execution(ExecutionEvent::InstanceSpawned {
            iteration_number,
            instance_id,
            ..
        }) => format!(
            "Spawned instance {:?} for iteration {iteration_number}",
            instance_id
        ),
        DomainEvent::Execution(ExecutionEvent::InstanceTerminated {
            iteration_number,
            instance_id,
            ..
        }) => format!(
            "Terminated instance {:?} for iteration {iteration_number}",
            instance_id
        ),
        DomainEvent::Execution(ExecutionEvent::Validation(
            ValidationEvent::GradientValidationPerformed {
                iteration_number,
                score,
                confidence,
                ..
            },
        )) => format!(
            "Gradient validation for iteration {iteration_number}: score={score:.2}, confidence={confidence:.2}"
        ),
        DomainEvent::Execution(ExecutionEvent::Validation(
            ValidationEvent::MultiJudgeConsensus {
                final_score,
                confidence,
                ..
            },
        )) => format!(
            "Multi-judge consensus reached: score={final_score:.2}, confidence={confidence:.2}"
        ),
        DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionStarted { execution_id, .. }) => {
            format!("Workflow execution {execution_id} started")
        }
        DomainEvent::Workflow(WorkflowEvent::WorkflowStateEntered { state_name, .. }) => {
            format!("Entered workflow state {state_name}")
        }
        DomainEvent::Workflow(WorkflowEvent::WorkflowStateExited { state_name, .. }) => {
            format!("Exited workflow state {state_name}")
        }
        DomainEvent::Workflow(WorkflowEvent::WorkflowIterationStarted {
            iteration_number, ..
        }) => format!("Workflow iteration {iteration_number} started"),
        DomainEvent::Workflow(WorkflowEvent::WorkflowIterationCompleted {
            iteration_number,
            ..
        }) => format!("Workflow iteration {iteration_number} completed"),
        DomainEvent::Workflow(WorkflowEvent::WorkflowIterationFailed {
            iteration_number,
            error,
            ..
        }) => format!("Workflow iteration {iteration_number} failed: {error}"),
        DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionCompleted {
            execution_id, ..
        }) => format!("Workflow execution {execution_id} completed"),
        DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionFailed {
            execution_id,
            reason,
            ..
        }) => format!("Workflow execution {execution_id} failed: {reason}"),
        DomainEvent::Workflow(WorkflowEvent::WorkflowExecutionCancelled {
            execution_id, ..
        }) => format!("Workflow execution {execution_id} cancelled"),
        DomainEvent::Workflow(WorkflowEvent::SubworkflowTriggered {
            child_execution_id,
            parent_state_name,
            mode,
            ..
        }) => format!(
            "Subworkflow {child_execution_id} triggered from state {parent_state_name} ({mode})"
        ),
        DomainEvent::Workflow(WorkflowEvent::SubworkflowCompleted {
            child_execution_id,
            result_key,
            ..
        }) => format!("Subworkflow {child_execution_id} completed, result at key '{result_key}'"),
        DomainEvent::Workflow(WorkflowEvent::SubworkflowFailed {
            child_execution_id,
            reason,
            ..
        }) => format!("Subworkflow {child_execution_id} failed: {reason}"),
        DomainEvent::Storage(StorageEvent::FileOpened {
            path, open_mode, ..
        }) => {
            format!("Opened {path} with mode {open_mode}")
        }
        DomainEvent::Storage(StorageEvent::FileWritten {
            path,
            bytes_written,
            ..
        }) => format!("Wrote {bytes_written} bytes to {path}"),
        DomainEvent::Storage(StorageEvent::FilesystemPolicyViolation {
            operation, path, ..
        }) => format!("Blocked filesystem {operation} on {path}"),
        DomainEvent::Storage(StorageEvent::PathTraversalBlocked { attempted_path, .. }) => {
            format!("Blocked path traversal attempt: {attempted_path}")
        }
        _ => event.event_type_name().replace('_', " "),
    }
}

fn to_snake_case(input: &str) -> String {
    let mut output = String::with_capacity(input.len() + 4);
    for (index, ch) in input.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index != 0 {
                output.push('_');
            }
            output.push(ch.to_ascii_lowercase());
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::execution::CodeDiff;
    use crate::domain::execution::{ExecutionInput, LlmInteraction};
    use crate::domain::repository::{RepositoryError, WorkflowExecutionRepository};
    use crate::domain::workflow::{WorkflowExecution, WorkflowExecutionEventRecord, WorkflowId};
    use crate::infrastructure::repositories::InMemoryExecutionRepository;
    use async_trait::async_trait;
    use futures::StreamExt;
    use std::time::Duration;
    use tokio::time::timeout;

    #[derive(Default)]
    struct EmptyWorkflowExecutionRepository;

    #[async_trait]
    impl WorkflowExecutionRepository for EmptyWorkflowExecutionRepository {
        async fn find_tenant_id_by_execution(
            &self,
            _id: crate::domain::execution::ExecutionId,
        ) -> std::result::Result<Option<crate::domain::tenant::TenantId>, RepositoryError> {
            Ok(None)
        }

        async fn save_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _execution: &WorkflowExecution,
        ) -> std::result::Result<(), RepositoryError> {
            Ok(())
        }

        async fn find_by_id_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _id: ExecutionId,
        ) -> std::result::Result<Option<WorkflowExecution>, RepositoryError> {
            Ok(None)
        }

        async fn find_active_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
        ) -> std::result::Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn find_by_workflow_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _workflow_id: WorkflowId,
            _limit: usize,
            _offset: usize,
        ) -> std::result::Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn list_paginated_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _limit: usize,
            _offset: usize,
        ) -> std::result::Result<Vec<WorkflowExecution>, RepositoryError> {
            Ok(vec![])
        }

        async fn update_temporal_linkage_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _execution_id: ExecutionId,
            _temporal_workflow_id: &str,
            _temporal_run_id: &str,
        ) -> std::result::Result<(), RepositoryError> {
            Ok(())
        }

        async fn append_event(
            &self,
            _execution_id: ExecutionId,
            _sequence_number: i64,
            _event_type: String,
            _payload: Value,
            _iteration_number: Option<u8>,
        ) -> std::result::Result<(), RepositoryError> {
            Ok(())
        }

        async fn count_by_workflow_for_tenant(
            &self,
            _tenant_id: &crate::domain::tenant::TenantId,
            _workflow_id: WorkflowId,
        ) -> std::result::Result<i64, RepositoryError> {
            Ok(0)
        }

        async fn find_events_by_execution(
            &self,
            _id: ExecutionId,
            _limit: usize,
            _offset: usize,
        ) -> std::result::Result<Vec<WorkflowExecutionEventRecord>, RepositoryError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn stream_execution_activity_replays_history_and_live_events() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryExecutionRepository::new());
        let agent_id = AgentId::new();

        let mut execution = Execution::new(
            agent_id,
            ExecutionInput {
                intent: Some("test".to_string()),
                input: Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
            },
            3,
            "aegis-system-operator".to_string(),
        );
        execution.start();
        execution.start_iteration("generate".to_string()).unwrap();
        execution
            .add_llm_interaction(
                1,
                LlmInteraction {
                    provider: "openai".to_string(),
                    model: "gpt-5".to_string(),
                    prompt: "hello".to_string(),
                    response: "world".to_string(),
                    timestamp: Utc::now(),
                },
            )
            .unwrap();
        execution.complete_iteration("ok".to_string());
        execution.complete();
        repository
            .save_for_tenant(&TenantId::system(), &execution)
            .await
            .unwrap();

        let service = CorrelatedActivityStreamService::new(event_bus.clone(), repository, None);
        let execution_id = execution.id;
        let mut stream = service
            .stream_execution_activity(execution_id, false)
            .await
            .unwrap();

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.event_type, "execution_started");
        assert_eq!(first.execution_id, Some(execution_id));

        event_bus.publish_storage_event(StorageEvent::FileOpened {
            execution_id: Some(execution_id),
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            path: "/workspace/file.rs".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });

        let mut saw_live_storage = false;
        for _ in 0..8 {
            let next = timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("timed out waiting for stream item")
                .expect("stream ended unexpectedly")
                .unwrap();
            if next.event_type == "file_opened" {
                saw_live_storage = true;
                assert_eq!(next.category, "storage");
                break;
            }
        }

        assert!(saw_live_storage, "expected live storage event in stream");
    }

    #[tokio::test]
    async fn stream_agent_activity_correlates_execution_only_events() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryExecutionRepository::new());
        let agent_id = AgentId::new();
        let execution = Execution::new(
            agent_id,
            ExecutionInput {
                intent: Some("agent stream".to_string()),
                input: Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
            },
            2,
            "aegis-system-operator".to_string(),
        );
        let execution_id = execution.id;
        repository
            .save_for_tenant(&TenantId::system(), &execution)
            .await
            .unwrap();

        let service = CorrelatedActivityStreamService::new(
            event_bus.clone(),
            repository,
            Some(Arc::new(EmptyWorkflowExecutionRepository)),
        );
        let mut stream = service
            .stream_agent_activity(agent_id, &TenantId::system(), false)
            .await
            .unwrap();

        let _ = stream.next().await.unwrap().unwrap();

        event_bus.publish_storage_event(StorageEvent::FilesystemPolicyViolation {
            execution_id: Some(execution_id),
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            operation: "write".to_string(),
            path: "/workspace/secret.txt".to_string(),
            policy_rule: "deny-write".to_string(),
            violated_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });

        let next = timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for agent-correlated event")
            .expect("stream ended unexpectedly")
            .unwrap();
        assert_eq!(next.event_type, "filesystem_policy_violation");
        assert_eq!(next.execution_id, Some(execution_id));
        assert_eq!(next.agent_id, Some(agent_id));
        assert_eq!(next.category, "storage");
    }

    #[test]
    fn normalize_domain_event_handles_timeout_and_refinement() {
        let execution_id = ExecutionId::new();
        let agent_id = AgentId::new();

        let timeout_event = normalize_domain_event(
            &DomainEvent::Execution(ExecutionEvent::ExecutionTimedOut {
                execution_id,
                agent_id,
                timeout_seconds: 30,
                total_iterations: 4,
                timed_out_at: Utc::now(),
            }),
            None,
        );
        assert_eq!(timeout_event.event_type, "execution_timed_out");
        assert_eq!(timeout_event.stage.as_deref(), Some("execution"));

        let refinement_event = normalize_domain_event(
            &DomainEvent::Execution(ExecutionEvent::RefinementApplied {
                execution_id,
                agent_id,
                iteration_number: 2,
                code_diff: CodeDiff {
                    file_path: "src/main.rs".to_string(),
                    diff: "+ fix".to_string(),
                },
                applied_at: Utc::now(),
                cortex_pattern_id: None,
                cortex_pattern_category: None,
                cortex_success_score: None,
                cortex_solution_approach: None,
            }),
            None,
        );
        assert_eq!(refinement_event.event_type, "refinement_applied");
        assert_eq!(refinement_event.iteration, Some(2));
        assert!(refinement_event.message.contains("refinement"));
    }
}
