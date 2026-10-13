// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Pending workflow gates
//!
//! | Route | Caller | Scope |
//! |-------|--------|-------|
//! | `GET /v1/workflow-gates?status=pending` | a person (the runs they started) | `workflow:read` |
//!
//! A workflow run waits on a Human gate from the moment the worker posts
//! `HumanInputRequested` until its next event. The route lists the gates the
//! caller's running workflow executions wait on: a run is listed when its
//! latest workflow event is `HumanInputRequested`, once, with that event's
//! sequence, so a gate posted again after a refused word is listed with the
//! latest post. A run that went on past its gate, or ended, is not listed.
//!
//! Tenant-scoped as `POST /v1/workflows/executions/{id}/signal` is: the
//! caller's tenant from its identity. Within the tenant, only runs the caller
//! started are listed (the execution's `initiating_user_sub`): in a tenant
//! several people share, another member's run is not listed, nor a run that
//! records no starter. A service account is refused (403). Only `pending`
//! is listed; answered gates are read from a run's logs.
//!
//! The answer to a gate is `POST /v1/workflows/executions/{id}/signal`,
//! unchanged.

use std::sync::Arc;

use aegis_orchestrator_core::domain::events::WorkflowEvent;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::repository::{
    WorkflowExecutionRepository, WorkflowRepository,
};
use aegis_orchestrator_core::domain::workflow::{WorkflowExecution, WorkflowExecutionEventRecord};
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

/// State of the workflow gates sub-router.
#[derive(Clone)]
pub(crate) struct WorkflowGatesState {
    /// The running executions and their event trails.
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

#[derive(Debug, Deserialize, Default)]
pub(crate) struct GatesQuery {
    status: Option<String>,
}

/// One pending gate, as the worker posted it.
struct PendingGate {
    requested_at: DateTime<Utc>,
    view: Value,
}

/// The gate a run waits on: `None` unless the run's latest event is the
/// worker's `HumanInputRequested`.
fn pending_gate(
    execution: &WorkflowExecution,
    latest: Option<WorkflowExecutionEventRecord>,
    workflow_name: Option<String>,
) -> Option<PendingGate> {
    let latest = latest.filter(|event| event.event_type == GATE_EVENT_TYPE)?;
    let mapped = serde_json::from_value::<TemporalEventPayload>(latest.payload)
        .map_err(anyhow::Error::from)
        .and_then(|payload| TemporalEventMapper::to_domain_event(&payload));
    let event = match mapped {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(
                execution_id = %execution.id,
                sequence = latest.sequence,
                error = %error,
                "A stored HumanInputRequested event does not read back; its gate is not listed"
            );
            return None;
        }
    };
    let WorkflowEvent::WorkflowHumanInputRequested {
        state_name,
        prompt,
        default_response,
        responses,
        expires_at,
        requested_at,
        ..
    } = event
    else {
        return None;
    };
    // Until the worker names the gate's state, the run's current state is
    // it: the listener moves `current_state` on every state entered, and the
    // gate is the run's latest event.
    let state_name = state_name.unwrap_or_else(|| execution.current_state.as_str().to_string());
    Some(PendingGate {
        requested_at,
        view: json!({
            "execution_id": execution.id.to_string(),
            "workflow_name": workflow_name,
            "state_name": state_name,
            "prompt": prompt,
            "responses": responses,
            "default_response": default_response,
            "expires_at": expires_at.map(|time| time.to_rfc3339()),
            "sequence": latest.sequence,
            "requested_at": requested_at.to_rfc3339(),
        }),
    })
}

/// `GET /v1/workflow-gates?status=pending`: the gates the caller's running
/// workflow executions wait on, newest first.
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
    match query.status.as_deref() {
        None | Some("") | Some("pending") => {}
        Some(other) => {
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                &format!("Unknown status '{other}': only pending gates are listed"),
            ));
        }
    }

    let tenant_id = tenant_id_from_identity(Some(identity));
    let running = state
        .executions
        .find_running_with_latest_event_for_tenant(&tenant_id)
        .await
        .map_err(|error| {
            tracing::error!(error = %error, "Workflow execution store failed listing gates");
            refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Workflow execution store failed",
            )
        })?;

    let mut gates = Vec::new();
    for (execution, latest) in running {
        if execution.initiating_user_sub.as_deref() != Some(identity.sub.as_str()) {
            continue;
        }
        if !latest
            .as_ref()
            .is_some_and(|event| event.event_type == GATE_EVENT_TYPE)
        {
            continue;
        }
        let workflow_name = match state
            .workflows
            .find_by_id_visible(&tenant_id, execution.workflow_id)
            .await
        {
            Ok(Some(workflow)) => Some(workflow.metadata.name),
            _ => None,
        };
        if let Some(gate) = pending_gate(&execution, latest, workflow_name) {
            gates.push(gate);
        }
    }
    gates.sort_by_key(|gate| std::cmp::Reverse(gate.requested_at));

    Ok(Json(json!({
        "count": gates.len(),
        "gates": gates.into_iter().map(|gate| gate.view).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
#[path = "workflow_gates_tests.rs"]
mod tests;
