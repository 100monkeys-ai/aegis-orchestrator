// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

//! Maps domain [`ExecutionEvent`] to proto [`ExecutionEvent`] for cluster forwarding.
//!
//! This mapper is used by the cluster gRPC server when forwarding execution
//! streams between nodes. Unlike the presentation-layer mapper in
//! `presentation::grpc::server`, this one does not require a separate
//! `execution_id` parameter since each domain variant already carries its own.

use crate::domain::events::ExecutionEvent as DomainEvent;
use crate::infrastructure::aegis_runtime_proto::{
    execution_event, ExecutionCompleted, ExecutionEvent, ExecutionFailed, ExecutionStarted,
    IterationCompleted, IterationError, IterationFailed, IterationOutput, IterationStarted,
    RefinementApplied,
};

/// Convert a domain `ExecutionEvent` into the protobuf `ExecutionEvent` used on
/// the cluster gRPC wire format.
///
/// Domain variants that have no direct proto equivalent (e.g. `ConsoleOutput`,
/// `LlmInteraction`, `Validation`) are mapped to the closest proto message.
/// Variants that truly cannot be represented are returned as `None` so the
/// caller can skip them.
pub fn domain_to_proto(event: DomainEvent) -> Option<ExecutionEvent> {
    match event {
        DomainEvent::ExecutionStarted {
            execution_id,
            agent_id,
            started_at,
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionStarted(ExecutionStarted {
                execution_id: execution_id.to_string(),
                agent_id: agent_id.0.to_string(),
                started_at: started_at.to_rfc3339(),
            })),
        }),

        DomainEvent::IterationStarted {
            execution_id,
            iteration_number,
            action,
            started_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationStarted(IterationStarted {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                action,
                started_at: started_at.to_rfc3339(),
            })),
        }),

        DomainEvent::IterationCompleted {
            execution_id,
            iteration_number,
            output,
            completed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationCompleted(
                IterationCompleted {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    output,
                    completed_at: completed_at.to_rfc3339(),
                },
            )),
        }),

        DomainEvent::IterationFailed {
            execution_id,
            iteration_number,
            error,
            failed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationFailed(IterationFailed {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                error: Some(IterationError {
                    error_type: "runtime_error".to_string(),
                    message: error.message.clone(),
                    stacktrace: error.details.clone(),
                }),
                failed_at: failed_at.to_rfc3339(),
            })),
        }),

        DomainEvent::RefinementApplied {
            execution_id,
            iteration_number,
            code_diff,
            applied_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::RefinementApplied(
                RefinementApplied {
                    execution_id: execution_id.to_string(),
                    iteration_number: iteration_number as u32,
                    code_diff: format!("{}:\n{}", code_diff.file_path, code_diff.diff),
                    applied_at: applied_at.to_rfc3339(),
                },
            )),
        }),

        DomainEvent::ExecutionCompleted {
            execution_id,
            final_output,
            total_iterations,
            completed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionCompleted(
                ExecutionCompleted {
                    execution_id: execution_id.to_string(),
                    final_output,
                    total_iterations: total_iterations as u32,
                    completed_at: completed_at.to_rfc3339(),
                },
            )),
        }),

        DomainEvent::ExecutionFailed {
            execution_id,
            reason,
            total_iterations,
            failed_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                execution_id: execution_id.to_string(),
                reason,
                total_iterations: total_iterations as u32,
                failed_at: failed_at.to_rfc3339(),
            })),
        }),

        // ExecutionCancelled has no direct proto variant; map to ExecutionFailed.
        DomainEvent::ExecutionCancelled {
            execution_id,
            reason,
            cancelled_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                execution_id: execution_id.to_string(),
                reason: reason.unwrap_or_else(|| "execution cancelled".to_string()),
                total_iterations: 0,
                failed_at: cancelled_at.to_rfc3339(),
            })),
        }),

        // ExecutionTimedOut has no direct proto variant; map to ExecutionFailed.
        DomainEvent::ExecutionTimedOut {
            execution_id,
            timeout_seconds,
            total_iterations,
            timed_out_at,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::ExecutionFailed(ExecutionFailed {
                execution_id: execution_id.to_string(),
                reason: format!("execution timed out after {timeout_seconds}s"),
                total_iterations: total_iterations as u32,
                failed_at: timed_out_at.to_rfc3339(),
            })),
        }),

        // ConsoleOutput maps to IterationOutput (closest proto equivalent).
        DomainEvent::ConsoleOutput {
            execution_id,
            iteration_number,
            content,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationOutput(IterationOutput {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                output: content,
            })),
        }),

        // LlmInteraction maps to IterationOutput with a summary string.
        DomainEvent::LlmInteraction {
            execution_id,
            iteration_number,
            provider,
            model,
            response,
            ..
        } => Some(ExecutionEvent {
            event: Some(execution_event::Event::IterationOutput(IterationOutput {
                execution_id: execution_id.to_string(),
                iteration_number: iteration_number as u32,
                output: format!("[LLM {provider}/{model}] {response}"),
            })),
        }),

        // InstanceSpawned/InstanceTerminated and Validation are not relevant
        // for the cluster forwarding stream — skip them.
        DomainEvent::InstanceSpawned { .. }
        | DomainEvent::InstanceTerminated { .. }
        | DomainEvent::Validation(_) => None,
    }
}
