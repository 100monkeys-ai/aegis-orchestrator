// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::agent::AgentId;
use crate::domain::events::{
    AgentLifecycleEvent, CorrelatedActivityEvent, ExecutionEvent, StorageEvent, ValidationEvent,
    WorkflowEvent,
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
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        verbose: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<CorrelatedActivityEvent>> + Send>>> {
        let history = self
            .execution_history(tenant_id, execution_id, verbose)
            .await?;

        if verbose {
            // In verbose mode, subscribe to the global event bus and forward
            // ONLY events whose execution_id matches the (already tenant-scoped)
            // exec_id. System-level events with no execution_id are NOT forwarded
            // here — they belong to no tenant and therefore must not leak across
            // a tenant-scoped stream (audit 002 §4.4).
            let receiver = self.event_bus.subscribe();
            let live = futures::stream::unfold(
                (receiver, execution_id),
                |(mut receiver, exec_id)| async move {
                    loop {
                        match receiver.recv().await {
                            Ok(event) => {
                                let event_exec_id = event.execution_id();
                                if event_exec_id == Some(exec_id) {
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

    /// Follow an agent's activity for `tenant_id`: its history, then every
    /// live event that belongs on the stream (see `event_belongs_to_agent`).
    ///
    /// `verbose` adds nothing to the live part: the events it once added were
    /// those that carry neither an agent nor an execution, which cannot be
    /// tied to the caller's tenant and are never forwarded.
    pub async fn stream_agent_activity(
        &self,
        agent_id: AgentId,
        tenant_id: &TenantId,
        verbose: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<CorrelatedActivityEvent>> + Send>>> {
        let history = self.agent_history(agent_id, tenant_id, verbose).await?;
        let repository = Arc::clone(&self.execution_repository);
        let cache = Arc::new(RwLock::new(HashMap::<ExecutionId, bool>::new()));
        let tenant = tenant_id.clone();
        let receiver = self.event_bus.subscribe();

        let live = futures::stream::unfold(
            (receiver, repository, cache, tenant),
            move |(mut receiver, repository, cache, tenant)| async move {
                loop {
                    match receiver.recv().await {
                        Ok(event) => {
                            match event_belongs_to_agent(
                                &event,
                                agent_id,
                                &tenant,
                                &repository,
                                &cache,
                            )
                            .await
                            {
                                Ok(true) => {
                                    let normalized = normalize_domain_event(&event, Some(agent_id));
                                    return Some((
                                        Ok(normalized),
                                        (receiver, repository, cache, tenant),
                                    ));
                                }
                                Ok(false) => continue,
                                Err(error) => {
                                    return Some((
                                        Err(error),
                                        (receiver, repository, cache, tenant),
                                    ));
                                }
                            }
                        }
                        Err(EventBusError::Closed) => return None,
                        Err(error) => {
                            return Some((
                                Err(anyhow!("Event bus error: {error}")),
                                (receiver, repository, cache, tenant),
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
        tenant_id: &TenantId,
        execution_id: ExecutionId,
        _verbose: bool,
    ) -> Result<Vec<CorrelatedActivityEvent>> {
        // Tenant-scoped lookup: callers MUST supply the authenticated tenant.
        // A miss here returns an empty history (the SSE handler maps that to
        // a 404-equivalent — no execution data is leaked across tenants).
        // Audit 002 §4.3.
        let execution = self
            .execution_repository
            .find_by_id_for_tenant(tenant_id, execution_id)
            .await?
            .ok_or_else(|| {
                anyhow!("Execution {execution_id} not found for the requesting tenant")
            })?;

        let mut history = execution_to_history(&execution);

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

/// Does `event` belong on `agent_id`'s stream for `tenant_id`?
///
/// Only when it can be tied to both: it names an execution of `tenant_id`
/// run by `agent_id` (and, if it names an agent too, that agent), or it is a
/// lifecycle event of `agent_id` whose every tenant is `tenant_id`. An event
/// that names an execution not found for the tenant, or that names neither
/// an execution nor an agent, does not. The outcome for an execution is
/// cached once the execution is found under any tenant; one not stored yet
/// is looked up again next time.
async fn event_belongs_to_agent(
    event: &DomainEvent,
    agent_id: AgentId,
    tenant_id: &TenantId,
    repository: &Arc<dyn ExecutionRepository>,
    cache: &Arc<RwLock<HashMap<ExecutionId, bool>>>,
) -> Result<bool> {
    let Some(execution_id) = event.execution_id() else {
        return Ok(match event {
            DomainEvent::AgentLifecycle(lifecycle) => {
                lifecycle_belongs_to_agent(lifecycle, agent_id, tenant_id)
            }
            _ => false,
        });
    };
    if event.agent_id().is_some_and(|named| named != agent_id) {
        return Ok(false);
    }

    let cached = cache.read().await.get(&execution_id).copied();
    let belongs = match cached {
        Some(belongs) => belongs,
        None => match repository
            .find_by_id_for_tenant(tenant_id, execution_id)
            .await?
        {
            Some(execution) => {
                let belongs = execution.agent_id == agent_id;
                cache.write().await.insert(execution_id, belongs);
                belongs
            }
            None => {
                if repository
                    .find_by_id_unscoped(execution_id)
                    .await?
                    .is_some()
                {
                    // Another tenant's execution.
                    cache.write().await.insert(execution_id, false);
                }
                false
            }
        },
    };
    Ok(belongs)
}

/// A lifecycle event belongs on an agent's stream when it is that agent's
/// and every tenant it names is the caller's. One that names no tenant
/// cannot be tied to the caller.
fn lifecycle_belongs_to_agent(
    event: &AgentLifecycleEvent,
    agent_id: AgentId,
    tenant_id: &TenantId,
) -> bool {
    match event {
        AgentLifecycleEvent::AgentDeployed {
            agent_id: a,
            tenant_id: t,
            ..
        }
        | AgentLifecycleEvent::AgentUpdated {
            agent_id: a,
            tenant_id: t,
            ..
        }
        | AgentLifecycleEvent::AgentRemoved {
            agent_id: a,
            tenant_id: t,
            ..
        } => *a == agent_id && t == tenant_id,
        AgentLifecycleEvent::AgentScopeChanged {
            agent_id: a,
            previous_tenant_id,
            new_tenant_id,
            ..
        } => *a == agent_id && previous_tenant_id == tenant_id && new_tenant_id == tenant_id,
        AgentLifecycleEvent::AgentPaused { .. }
        | AgentLifecycleEvent::AgentResumed { .. }
        | AgentLifecycleEvent::AgentFailed { .. } => false,
    }
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
            iteration_number,
            code_diff,
            ..
        }) => {
            if code_diff.diff.is_empty() {
                format!("Applied refinement after iteration {iteration_number}")
            } else {
                format!(
                    "Applied refinement after iteration {iteration_number}: {}",
                    code_diff.diff
                )
            }
        }
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
        // AEGIS ADR-136 G13b: every tool call has a line naming its tool.
        DomainEvent::Execution(ExecutionEvent::ToolDispatched { tool, status, .. }) => {
            format!("Called {tool}: {status}")
        }
        // AEGIS ADR-136 G13a: the run's repository steps.
        DomainEvent::Execution(ExecutionEvent::RepositoryPrepared {
            label,
            branch,
            started_from,
            created,
            ..
        }) => {
            if *created {
                format!(
                    "Prepared repository {label} at /workspace/{label}: branch {branch} created from {started_from}"
                )
            } else {
                format!(
                    "Prepared repository {label} at /workspace/{label}: branch {branch} checked out at {started_from}"
                )
            }
        }
        DomainEvent::Execution(ExecutionEvent::RepositoryCommitted {
            label,
            branch,
            commit_sha,
            author,
            ..
        }) => format!("Committed {commit_sha} on branch {branch} of repository {label} as {author}"),
        DomainEvent::Execution(ExecutionEvent::RepositoryPushed {
            label,
            branch,
            branch_url,
            ..
        }) => format!("Pushed branch {branch} of repository {label} to {branch_url}"),
        // AEGIS ADR-141 F8: a workflow's landing on its repository's branch.
        DomainEvent::Execution(ExecutionEvent::RepositoryLanded {
            commit_sha,
            label,
            git_ref,
            ..
        }) => crate::domain::events::repository_landed_line(commit_sha, label, git_ref),
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

        async fn list_paginated_all(
            &self,
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
                attachments: Vec::new(),
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
            .stream_execution_activity(&TenantId::system(), execution_id, false)
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

    fn execution_for(agent_id: AgentId) -> Execution {
        Execution::new(
            agent_id,
            ExecutionInput {
                intent: Some("scoping".to_string()),
                input: Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            1,
            "aegis-system-operator".to_string(),
        )
    }

    /// Tenant A follows its own agent's verbose stream while tenant B works
    /// on the same bus: B creates a git binding whose URL holds a marker
    /// credential, B runs A's agent (as with a shared agent), B's billing
    /// state drifts, and a step runs under an execution nobody stored. A
    /// receives none of it, and still receives its own agent's event.
    #[tokio::test]
    async fn agent_stream_carries_nothing_of_another_tenant() {
        use crate::domain::git_repo::{CloneStrategy, GitRef, GitRepoBinding};
        use crate::domain::tenant::TenantId;

        let marker = "Kq-cross-tenant-marker-credential";
        let tenant_a = TenantId::from_string("tenant-a").unwrap();
        let tenant_b = TenantId::from_string("tenant-b").unwrap();
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryExecutionRepository::new());
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();

        let a_exec = execution_for(agent_a);
        let b_runs_a = execution_for(agent_a);
        let b_exec = execution_for(agent_b);
        repository
            .save_for_tenant(&tenant_a, &a_exec)
            .await
            .unwrap();
        repository
            .save_for_tenant(&tenant_b, &b_runs_a)
            .await
            .unwrap();
        repository
            .save_for_tenant(&tenant_b, &b_exec)
            .await
            .unwrap();

        let service = CorrelatedActivityStreamService::new(
            event_bus.clone(),
            repository,
            Some(Arc::new(EmptyWorkflowExecutionRepository)),
        );
        let mut stream = service
            .stream_agent_activity(agent_a, &tenant_a, true)
            .await
            .unwrap();

        let mut received: Vec<String> = Vec::new();
        let mut errors = 0usize;
        // History first: A's own execution.
        while let Ok(Some(item)) = timeout(Duration::from_millis(300), stream.next()).await {
            match item {
                Ok(activity) => received.push(serde_json::to_string(&activity).unwrap()),
                Err(_) => errors += 1,
            }
        }
        let history = received.len();

        // Tenant B's activity.
        let mut binding = GitRepoBinding::new(
            tenant_b.clone(),
            None,
            format!("https://x-access-token:{marker}@git.example.invalid/b/repo.git"),
            GitRef::Branch("main".to_string()),
            None,
            crate::domain::volume::VolumeId::new(),
            "b-repo".to_string(),
            CloneStrategy::Libgit2,
            false,
            None,
            None,
            None,
        );
        for event in binding.take_events() {
            event_bus.publish_git_repo_event(event);
        }
        event_bus.publish_execution_event(ExecutionEvent::ConsoleOutput {
            execution_id: b_runs_a.id,
            agent_id: agent_a,
            iteration_number: 1,
            stream: "stdout".to_string(),
            content: format!("tenant-b output {marker}"),
            timestamp: Utc::now(),
        });
        event_bus.publish_storage_event(StorageEvent::FileOpened {
            execution_id: Some(b_exec.id),
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            path: format!("/workspace/{marker}"),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });
        event_bus.publish_drift_event(crate::domain::events::DriftEvent::OrphanSubscription {
            tenant_id: tenant_b.clone(),
            stripe_customer_id: "cus_tenant_b".to_string(),
            detected_at: Utc::now(),
        });
        event_bus.publish_container_run_event(
            crate::domain::events::ContainerRunEvent::ContainerRunStarted {
                execution_id: ExecutionId::new(),
                state_name: "GIT_CLONE".to_string(),
                step_name: "git-clone".to_string(),
                image: "alpine/git:latest".to_string(),
                command: vec![],
                started_at: Utc::now(),
            },
        );
        // Then tenant A's own agent.
        event_bus.publish_storage_event(StorageEvent::FileOpened {
            execution_id: Some(a_exec.id),
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            path: "/workspace/a.txt".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });

        let mut own = false;
        while let Ok(Some(item)) = timeout(Duration::from_secs(2), stream.next()).await {
            match item {
                Ok(activity) => {
                    let is_own = activity.event_type == "file_opened"
                        && activity.execution_id == Some(a_exec.id);
                    received.push(serde_json::to_string(&activity).unwrap());
                    if is_own {
                        own = true;
                        break;
                    }
                }
                Err(_) => errors += 1,
            }
        }

        let live = received.len() - history - usize::from(own);
        assert!(
            live == 0 && errors == 0,
            "tenant A's stream on its own agent received {live} event(s) and {errors} error(s) \
             it cannot tie to tenant A"
        );
        assert!(
            !received.iter().any(|r| r.contains(marker)
                || r.contains("tenant-b")
                || r.contains("cus_tenant_b")),
            "tenant A's stream holds tenant B's data"
        );
        assert!(
            own,
            "tenant A's stream did not receive its own agent's event"
        );
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
                attachments: Vec::new(),
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

    /// Regression for audit 002 §4.3: a streamer authenticated as tenant-A
    /// requesting tenant-B's execution_id MUST get an error (handler maps it
    /// to a 404 — no execution data leaks across the tenant boundary).
    #[tokio::test]
    async fn stream_execution_activity_rejects_cross_tenant_lookup() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryExecutionRepository::new());

        let tenant_a = TenantId::from_string("tenant-a").unwrap();
        let tenant_b = TenantId::from_string("tenant-b").unwrap();

        let execution = Execution::new(
            AgentId::new(),
            ExecutionInput {
                intent: Some("victim".to_string()),
                input: Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            1,
            "aegis-system-operator".to_string(),
        );
        let exec_id = execution.id;
        repository
            .save_for_tenant(&tenant_b, &execution)
            .await
            .unwrap();

        let service = CorrelatedActivityStreamService::new(event_bus, repository, None);

        // Tenant-A asks for tenant-B's execution → must error (handler returns 404).
        let res = service
            .stream_execution_activity(&tenant_a, exec_id, false)
            .await;
        assert!(
            res.is_err(),
            "cross-tenant stream_execution_activity must fail; got Ok stream"
        );

        // History lookup must also error out for the same reason — no rows leak.
        let res = service.execution_history(&tenant_a, exec_id, false).await;
        assert!(
            res.is_err(),
            "cross-tenant execution_history must fail; got Ok rows"
        );
    }

    /// Regression for audit 002 §4.4: a verbose subscription scoped to
    /// tenant-A's execution MUST NOT receive events emitted for an execution
    /// owned by tenant-B, even though both flow through the same global
    /// event bus. The previous implementation matched events whose
    /// execution_id was `None` and additionally subscribed globally with no
    /// tenant filter — turning verbose mode into a cross-tenant read primitive.
    #[tokio::test]
    async fn verbose_stream_drops_events_from_other_tenants() {
        let event_bus = Arc::new(EventBus::with_default_capacity());
        let repository = Arc::new(InMemoryExecutionRepository::new());

        let tenant_a = TenantId::from_string("tenant-a").unwrap();
        let tenant_b = TenantId::from_string("tenant-b").unwrap();
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();

        let exec_a = Execution::new(
            agent_a,
            ExecutionInput {
                intent: Some("a".to_string()),
                input: Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            1,
            "aegis-system-operator".to_string(),
        );
        let exec_a_id = exec_a.id;
        repository
            .save_for_tenant(&tenant_a, &exec_a)
            .await
            .unwrap();

        let exec_b = Execution::new(
            agent_b,
            ExecutionInput {
                intent: Some("b".to_string()),
                input: Value::Null,
                workspace_volume_id: None,
                workspace_volume_mount_path: None,
                workspace_remote_path: None,
                workflow_execution_id: None,
                attachments: Vec::new(),
            },
            1,
            "aegis-system-operator".to_string(),
        );
        let exec_b_id = exec_b.id;
        repository
            .save_for_tenant(&tenant_b, &exec_b)
            .await
            .unwrap();

        let service =
            CorrelatedActivityStreamService::new(event_bus.clone(), repository.clone(), None);

        // Tenant-A subscribes (verbose=true) to its own execution.
        let mut stream = service
            .stream_execution_activity(&tenant_a, exec_a_id, true)
            .await
            .unwrap();

        // Drain history (a single ExecutionStarted for exec_a).
        let _ = stream.next().await.unwrap().unwrap();

        // Publish a tenant-B-owned event AND a system-level event with no
        // execution_id. Neither must reach tenant-A's stream.
        event_bus.publish_storage_event(StorageEvent::FilesystemPolicyViolation {
            execution_id: Some(exec_b_id),
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            operation: "write".to_string(),
            path: "/workspace/tenant-b-secret.txt".to_string(),
            policy_rule: "deny-write".to_string(),
            violated_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });
        event_bus.publish_storage_event(StorageEvent::FileOpened {
            execution_id: None,
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            path: "/system/global.log".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });

        // Then publish one event that DOES belong to tenant-A's execution; the
        // stream must skip past the cross-tenant + system-level events and
        // deliver only this one.
        event_bus.publish_storage_event(StorageEvent::FileOpened {
            execution_id: Some(exec_a_id),
            workflow_execution_id: None,
            volume_id: crate::domain::volume::VolumeId::new(),
            path: "/workspace/tenant-a-allowed.txt".to_string(),
            open_mode: "read".to_string(),
            opened_at: Utc::now(),
            caller_node_id: None,
            host_node_id: None,
        });

        let next = timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for tenant-A event")
            .expect("stream ended unexpectedly")
            .unwrap();

        assert_eq!(next.event_type, "file_opened");
        assert_eq!(
            next.execution_id,
            Some(exec_a_id),
            "stream must only deliver events for tenant-A's execution; got exec_id={:?} (tenant-B was {:?}, system was None)",
            next.execution_id,
            exec_b_id,
        );
        assert!(
            !next.message.contains("tenant-b-secret"),
            "tenant-B's event must not appear in tenant-A's stream"
        );
        assert!(
            !next.message.contains("/system/global.log"),
            "system-level (execution_id=None) event must not appear in a tenant-scoped stream"
        );
    }

    /// AEGIS ADR-136 G13a, G13b: the run's repository rows and every tool
    /// call each have their line, type name and stage.
    #[test]
    fn repository_rows_and_tool_calls_have_their_lines() {
        let execution_id = ExecutionId::new();
        let agent_id = AgentId::new();
        let sha = "0123456789abcdef0123456789abcdef01234567".to_string();
        let row = |event: ExecutionEvent| {
            let row = normalize_domain_event(&DomainEvent::Execution(event), None);
            (row.event_type, row.stage, row.message)
        };
        let prepared = |created| ExecutionEvent::RepositoryPrepared {
            execution_id,
            agent_id,
            label: "app".to_string(),
            branch: "aegis/1234abcd".to_string(),
            started_from: sha.clone(),
            created,
            prepared_at: Utc::now(),
        };
        let repository = Some("repository".to_string());
        let mut wrong = Vec::new();
        for (got, want) in [
            (
                row(prepared(true)),
                (
                    "repository_prepared".to_string(),
                    repository.clone(),
                    format!("Prepared repository app at /workspace/app: branch aegis/1234abcd created from {sha}"),
                ),
            ),
            (
                row(prepared(false)),
                (
                    "repository_prepared".to_string(),
                    repository.clone(),
                    format!("Prepared repository app at /workspace/app: branch aegis/1234abcd checked out at {sha}"),
                ),
            ),
            (
                row(ExecutionEvent::RepositoryCommitted {
                    execution_id,
                    agent_id,
                    label: "app".to_string(),
                    branch: "aegis/1234abcd".to_string(),
                    commit_sha: sha.clone(),
                    author: "Ada Lovelace".to_string(),
                    committed_at: Utc::now(),
                }),
                (
                    "repository_committed".to_string(),
                    repository.clone(),
                    format!("Committed {sha} on branch aegis/1234abcd of repository app as Ada Lovelace"),
                ),
            ),
            (
                row(ExecutionEvent::RepositoryPushed {
                    execution_id,
                    agent_id,
                    label: "app".to_string(),
                    branch: "aegis/1234abcd".to_string(),
                    remote_url: crate::domain::secrets::RedactedUrl::new("https://github.com/o/r.git"),
                    branch_url: crate::domain::secrets::RedactedUrl::new(
                        "https://github.com/o/r/tree/aegis/1234abcd",
                    ),
                    pushed_at: Utc::now(),
                }),
                (
                    "repository_pushed".to_string(),
                    repository.clone(),
                    "Pushed branch aegis/1234abcd of repository app to https://github.com/o/r/tree/aegis/1234abcd".to_string(),
                ),
            ),
            (
                row(ExecutionEvent::ToolDispatched {
                    execution_id,
                    agent_id,
                    iteration_number: 1,
                    call_index: 0,
                    tool_call_id: "call-1".to_string(),
                    tool: "github.create_pull_request".to_string(),
                    arguments: serde_json::json!({}),
                    stdin_given: None,
                    status: "succeeded".to_string(),
                    sentence: None,
                    refused_by: None,
                    dispatch_id: None,
                    dispatched_at: Utc::now(),
                }),
                (
                    "tool_dispatched".to_string(),
                    Some("tool".to_string()),
                    "Called github.create_pull_request: succeeded".to_string(),
                ),
            ),
        ] {
            if got != want {
                wrong.push(format!("got {got:?}, want {want:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
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

    /// The narrative's line for a refinement reads the refinement itself
    /// when it carries one, and as before when its diff is empty.
    #[test]
    fn refinement_line_carries_the_refinement() {
        let refinement = |diff: &str| {
            normalize_domain_event(
                &DomainEvent::Execution(ExecutionEvent::RefinementApplied {
                    execution_id: ExecutionId::new(),
                    agent_id: AgentId::new(),
                    iteration_number: 1,
                    code_diff: CodeDiff {
                        file_path: String::new(),
                        diff: diff.to_string(),
                    },
                    applied_at: Utc::now(),
                    cortex_pattern_id: None,
                    cortex_pattern_category: None,
                    cortex_success_score: None,
                    cortex_solution_approach: None,
                }),
                None,
            )
            .message
        };
        let sentence = "Your previous answer exceeded the model provider's time limit: the provider ended it after 120.5 s, before it finished, so none of it was kept. Answer this time with a tool call that runs a program to compute the result; do not write the computed result in your text.";
        assert_eq!(
            refinement(sentence),
            format!("Applied refinement after iteration 1: {sentence}"),
            "the narrative must name the refinement"
        );
        assert_eq!(
            refinement(""),
            "Applied refinement after iteration 1",
            "an empty refinement reads as before"
        );
    }
}
