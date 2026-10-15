// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Workflow gates
//!
//! | Route | Caller | Scope |
//! |-------|--------|-------|
//! | `GET /v1/workflow-gates?status=pending\|answered\|any` | a person (the runs they started) | `workflow:read` |
//!
//! A workflow run waits on a Human gate from the moment the worker posts
//! `HumanInputRequested` until its next event.
//!
//! `status=pending` (the default) lists the gates the caller's running
//! workflow executions wait on: a run is listed when its latest workflow
//! event is `HumanInputRequested`, once, with that event's sequence, so a
//! gate posted again after a refused word is listed with the latest post. A
//! run that went on past its gate, or ended, is not listed.
//!
//! A run's workflow events are the worker's, the rows below
//! `LOCAL_SEQUENCE_START`. The rows the orchestrator stores itself in the
//! same trail (a workflow agent state's repository rows), from
//! `LOCAL_SEQUENCE_START` on, stay in the run's history and its logs route
//! and neither open, hide nor close a gate.
//!
//! `status=answered` lists the gates of the caller's runs, running or ended,
//! that closed: the worker leaves a Human state with `WorkflowStateExited`,
//! whose `output` carries the person's `response`, their optional
//! `feedback` and `timeout` (true when the gate took its default response on
//! its timeout); a gate with no default response that times out fails the
//! run with `WorkflowExecutionFailed` and the error `Human input timeout`.
//! Each closed gate is listed with the word chosen, the feedback, when it
//! closed, its latest post's prompt and sequence, and `outcome`: `went_on`
//! when the run entered another state after the answer, `ended` when the run
//! ended on it, `timed_out` (with `response: null`) when it closed on its
//! timeout. `answered_by` is who answered (a person's sub, a service
//! account's client id), read from the exit's `output.answered_by`, and
//! `null` when the exit names nobody. A gate the run
//! left without an answer recorded (it went on, ended or was cancelled with
//! the gate open) is not listed. Newest first by `answered_at`, paged by
//! `limit` (default 50, at most 200) and `offset`.
//!
//! `status=any` lists both, each in its own listing's shape (an answered
//! gate carries `outcome`), newest first by the gate's latest time (its
//! answer, else its post), paged as `answered` is.
//!
//! Tenant-scoped as `POST /v1/workflows/executions/{id}/signal` is: the
//! caller's tenant from its identity. Within the tenant, only runs the caller
//! started are listed (the execution's `initiating_user_sub`): in a tenant
//! several people share, another member's run is not listed, nor a run that
//! records no starter. A service account is refused (403).
//!
//! The answer to a gate is `POST /v1/workflows/executions/{id}/signal`,
//! unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::events::WorkflowEvent;
use aegis_orchestrator_core::domain::execution::ExecutionStatus;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::repository::{
    WorkflowExecutionRepository, WorkflowRepository, LOCAL_SEQUENCE_START,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::workflow::{
    WorkflowExecution, WorkflowExecutionEventRecord, WorkflowId,
};
use aegis_orchestrator_core::infrastructure::{TemporalEventMapper, TemporalEventPayload};
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::daemon::handlers::tenant_id_from_identity;

type Refusal = (StatusCode, Json<Value>);

/// The event type the worker posts a Human gate with.
const GATE_EVENT_TYPE: &str = "HumanInputRequested";

/// The event type the worker leaves a state with; a Human state's output
/// carries the answer.
const STATE_EXITED_EVENT_TYPE: &str = "WorkflowStateExited";

/// The event type the worker enters a state with.
const STATE_ENTERED_EVENT_TYPE: &str = "WorkflowStateEntered";

/// The event type the worker fails a run with.
const RUN_FAILED_EVENT_TYPE: &str = "WorkflowExecutionFailed";

/// The event types that end a run.
const RUN_ENDED_EVENT_TYPES: [&str; 4] = [
    "WorkflowExecutionCompleted",
    RUN_FAILED_EVENT_TYPE,
    "WorkflowExecutionCancelled",
    "WorkflowStateVisitLimitExceeded",
];

/// The error the worker fails a run with when a gate that has no default
/// response reaches its timeout.
const GATE_TIMEOUT_ERROR: &str = "Human input timeout";

/// The page size of the answered listing when the caller names none.
const DEFAULT_LIMIT: usize = 50;

/// The largest page of the answered listing.
const MAX_LIMIT: usize = 200;

/// State of the workflow gates sub-router.
#[derive(Clone)]
pub(crate) struct WorkflowGatesState {
    /// The executions and their event trails.
    pub(crate) executions: Arc<dyn WorkflowExecutionRepository>,
    /// The workflows, for each gate's workflow name.
    pub(crate) workflows: Arc<dyn WorkflowRepository>,
}

/// The `/v1/workflow-gates` route, merged into the daemon router by
/// `router::create_router` beneath the same authentication layers as every
/// other route.
pub(crate) fn workflow_gates_router(state: WorkflowGatesState) -> Router {
    Router::new()
        .route("/v1/workflow-gates", get(list_workflow_gates_handler))
        .with_state(state)
}

fn refusal(status: StatusCode, error: &str) -> Refusal {
    (status, Json(json!({ "error": error })))
}

fn store_failed(error: impl std::fmt::Display) -> Refusal {
    tracing::error!(error = %error, "Workflow execution store failed listing gates");
    refusal(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Workflow execution store failed",
    )
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct GatesQuery {
    status: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Which gates the caller asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Listing {
    Pending,
    Answered,
    Any,
}

/// One listed gate, with the time it is ordered by: a pending gate's post,
/// an answered gate's answer.
struct ListedGate {
    at: DateTime<Utc>,
    view: Value,
}

/// A gate as the worker posted it.
struct PostedGate {
    sequence: i64,
    state_name: Option<String>,
    prompt: String,
    default_response: Option<String>,
    responses: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    requested_at: DateTime<Utc>,
}

/// A stored event read back through the listener's mapper, or `None` with a
/// warning when it does not read back.
fn read_event(
    execution: &WorkflowExecution,
    record: &WorkflowExecutionEventRecord,
) -> Option<WorkflowEvent> {
    let mapped = serde_json::from_value::<TemporalEventPayload>(record.payload.clone())
        .map_err(anyhow::Error::from)
        .and_then(|payload| TemporalEventMapper::to_domain_event(&payload));
    match mapped {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                sequence = record.sequence,
                event_type = %record.event_type,
                error = %error,
                "A stored workflow event does not read back; its gate is not listed"
            );
            None
        }
    }
}

/// The gate a stored `HumanInputRequested` posted.
fn posted_gate(
    execution: &WorkflowExecution,
    record: &WorkflowExecutionEventRecord,
) -> Option<PostedGate> {
    let WorkflowEvent::WorkflowHumanInputRequested {
        state_name,
        prompt,
        default_response,
        responses,
        expires_at,
        requested_at,
        ..
    } = read_event(execution, record)?
    else {
        return None;
    };
    Some(PostedGate {
        sequence: record.sequence,
        state_name,
        prompt,
        default_response,
        responses,
        expires_at,
        requested_at,
    })
}

/// Whether a stored row is the worker's: below `LOCAL_SEQUENCE_START`,
/// where the orchestrator's own rows start.
fn is_workers(record: &WorkflowExecutionEventRecord) -> bool {
    record.sequence < LOCAL_SEQUENCE_START
}

/// The gate a run waits on: `None` unless `latest`, the worker's latest
/// event of the run, is its `HumanInputRequested`. The store answers the
/// worker's latest event, passing over the orchestrator's own rows.
fn pending_gate(
    execution: &WorkflowExecution,
    latest: Option<WorkflowExecutionEventRecord>,
    workflow_name: Option<String>,
) -> Option<ListedGate> {
    let latest = latest.filter(|event| is_workers(event) && event.event_type == GATE_EVENT_TYPE)?;
    let gate = posted_gate(execution, &latest)?;
    // Until the worker names the gate's state, the run's current state is
    // it: the listener moves `current_state` on every state entered, and the
    // gate is the run's latest event.
    let state_name = gate
        .state_name
        .unwrap_or_else(|| execution.current_state.as_str().to_string());
    Some(ListedGate {
        at: gate.requested_at,
        view: json!({
            "execution_id": execution.id.to_string(),
            "workflow_name": workflow_name,
            "state_name": state_name,
            "prompt": gate.prompt,
            "responses": gate.responses,
            "default_response": gate.default_response,
            "expires_at": gate.expires_at.map(|time| time.to_rfc3339()),
            "sequence": gate.sequence,
            "requested_at": gate.requested_at.to_rfc3339(),
        }),
    })
}

/// How a closed gate closed.
struct Closing<'a> {
    response: Option<&'a str>,
    feedback: Option<&'a str>,
    answered_by: Option<&'a str>,
    state_name: Option<&'a str>,
    answered_at: DateTime<Utc>,
    outcome: &'static str,
}

fn answered_view(
    execution: &WorkflowExecution,
    workflow_name: &Option<String>,
    gate: PostedGate,
    closing: Closing<'_>,
) -> ListedGate {
    let state_name = gate
        .state_name
        .or_else(|| closing.state_name.map(str::to_string))
        .unwrap_or_else(|| execution.current_state.as_str().to_string());
    ListedGate {
        at: closing.answered_at,
        view: json!({
            "execution_id": execution.id.to_string(),
            "workflow_name": workflow_name,
            "state_name": state_name,
            "prompt": gate.prompt,
            "responses": gate.responses,
            "response": closing.response,
            "feedback": closing.feedback,
            "answered_at": closing.answered_at.to_rfc3339(),
            "answered_by": closing.answered_by,
            "sequence": gate.sequence,
            "requested_at": gate.requested_at.to_rfc3339(),
            "outcome": closing.outcome,
        }),
    }
}

fn run_has_ended(execution: &WorkflowExecution) -> bool {
    matches!(
        execution.status,
        ExecutionStatus::Completed | ExecutionStatus::Failed | ExecutionStatus::Cancelled
    )
}

/// The gates of one run that closed, read from the worker's events of its
/// gate trail in sequence order.
fn answered_gates(
    execution: &WorkflowExecution,
    trail: &[WorkflowExecutionEventRecord],
    workflow_name: &Option<String>,
) -> Vec<ListedGate> {
    let trail: Vec<&WorkflowExecutionEventRecord> =
        trail.iter().filter(|record| is_workers(record)).collect();
    let mut listed = Vec::new();
    let mut open: Option<PostedGate> = None;
    for (index, &record) in trail.iter().enumerate() {
        let event_type = record.event_type.as_str();
        if event_type == GATE_EVENT_TYPE {
            // A post again after a refused word replaces the earlier one.
            open = posted_gate(execution, record);
            continue;
        }
        let Some(gate) = open.take() else {
            continue;
        };
        if event_type == STATE_EXITED_EVENT_TYPE {
            let Some(WorkflowEvent::WorkflowStateExited {
                state_name,
                output,
                answered_by,
                exited_at,
                ..
            }) = read_event(execution, record)
            else {
                continue;
            };
            let timed_out = output.get("timeout").and_then(Value::as_bool) == Some(true);
            let response = output.get("response").and_then(Value::as_str);
            let closing = if timed_out {
                Closing {
                    response: None,
                    feedback: None,
                    answered_by: None,
                    state_name: Some(&state_name),
                    answered_at: exited_at,
                    outcome: "timed_out",
                }
            } else if let Some(response) = response {
                let outcome = match trail.get(index + 1) {
                    Some(next) if next.event_type == STATE_ENTERED_EVENT_TYPE => "went_on",
                    Some(next) if RUN_ENDED_EVENT_TYPES.contains(&next.event_type.as_str()) => {
                        "ended"
                    }
                    Some(_) => "went_on",
                    None if run_has_ended(execution) => "ended",
                    None => "went_on",
                };
                Closing {
                    response: Some(response),
                    feedback: output.get("feedback").and_then(Value::as_str),
                    answered_by: answered_by.as_deref(),
                    state_name: Some(&state_name),
                    answered_at: exited_at,
                    outcome,
                }
            } else {
                tracing::warn!(
                    execution_id = %execution.id,
                    sequence = record.sequence,
                    "A Human state's exit carries no response; its gate is not listed"
                );
                continue;
            };
            listed.push(answered_view(execution, workflow_name, gate, closing));
        } else if event_type == RUN_FAILED_EVENT_TYPE {
            if let Some(WorkflowEvent::WorkflowExecutionFailed {
                reason, failed_at, ..
            }) = read_event(execution, record)
            {
                if reason == GATE_TIMEOUT_ERROR {
                    let closing = Closing {
                        response: None,
                        feedback: None,
                        answered_by: None,
                        state_name: None,
                        answered_at: failed_at,
                        outcome: "timed_out",
                    };
                    listed.push(answered_view(execution, workflow_name, gate, closing));
                }
            }
        }
        // Any other event closes the gate with no answer recorded.
    }
    listed
}

/// Workflow names by id, each looked up once.
struct WorkflowNames<'a> {
    workflows: &'a dyn WorkflowRepository,
    tenant_id: &'a TenantId,
    names: HashMap<WorkflowId, Option<String>>,
}

impl WorkflowNames<'_> {
    async fn name(&mut self, workflow_id: WorkflowId) -> Option<String> {
        if let Some(name) = self.names.get(&workflow_id) {
            return name.clone();
        }
        let name = match self
            .workflows
            .find_by_id_visible(self.tenant_id, workflow_id)
            .await
        {
            Ok(Some(workflow)) => Some(workflow.metadata.name),
            _ => None,
        };
        self.names.insert(workflow_id, name.clone());
        name
    }
}

/// `GET /v1/workflow-gates`: the gates of the caller's workflow executions,
/// pending, answered or both, newest first.
pub(crate) async fn list_workflow_gates_handler(
    State(state): State<WorkflowGatesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Query(query): Query<GatesQuery>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("workflow:read")?;
    let identity = identity
        .as_deref()
        .ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    if matches!(identity.identity_kind, IdentityKind::ServiceAccount { .. }) {
        return Err(refusal(
            StatusCode::FORBIDDEN,
            "A person's identity is required to list workflow gates",
        ));
    }
    let listing = match query.status.as_deref() {
        None | Some("") | Some("pending") => Listing::Pending,
        Some("answered") => Listing::Answered,
        Some("any") => Listing::Any,
        Some(other) => {
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                &format!("Unknown status '{other}': the status is pending, answered or any"),
            ));
        }
    };
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if listing != Listing::Pending && !(1..=MAX_LIMIT).contains(&limit) {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            &format!("limit must be from 1 to {MAX_LIMIT}"),
        ));
    }

    let tenant_id = tenant_id_from_identity(Some(identity));
    let mut names = WorkflowNames {
        workflows: state.workflows.as_ref(),
        tenant_id: &tenant_id,
        names: HashMap::new(),
    };
    let mut gates = Vec::new();

    if listing != Listing::Answered {
        let running = state
            .executions
            .find_running_with_latest_event_for_tenant(&tenant_id)
            .await
            .map_err(store_failed)?;
        for (execution, latest) in running {
            if execution.initiating_user_sub.as_deref() != Some(identity.sub.as_str()) {
                continue;
            }
            if !latest
                .as_ref()
                .is_some_and(|event| is_workers(event) && event.event_type == GATE_EVENT_TYPE)
            {
                continue;
            }
            let workflow_name = names.name(execution.workflow_id).await;
            if let Some(gate) = pending_gate(&execution, latest, workflow_name) {
                gates.push(gate);
            }
        }
    }

    if listing != Listing::Pending {
        let trails = state
            .executions
            .find_gate_trails_for_initiator(&tenant_id, &identity.sub)
            .await
            .map_err(store_failed)?;
        for (execution, trail) in trails {
            // The repository answers the caller's own runs; the check holds
            // whatever a repository answers.
            if execution.initiating_user_sub.as_deref() != Some(identity.sub.as_str()) {
                continue;
            }
            let workflow_name = names.name(execution.workflow_id).await;
            gates.extend(answered_gates(&execution, &trail, &workflow_name));
        }
    }

    gates.sort_by_key(|gate| std::cmp::Reverse(gate.at));
    let page: Vec<Value> = if listing == Listing::Pending {
        gates.into_iter().map(|gate| gate.view).collect()
    } else {
        gates
            .into_iter()
            .skip(query.offset.unwrap_or(0))
            .take(limit)
            .map(|gate| gate.view)
            .collect()
    };

    Ok(Json(json!({
        "count": page.len(),
        "gates": page,
    })))
}

#[cfg(test)]
#[path = "workflow_gates_tests.rs"]
mod tests;
