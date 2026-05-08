// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Observability handlers: stimuli, security incidents, storage violations, dashboard.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::domain::iam::UserIdentity;

use crate::daemon::cluster_helpers::cluster_status_view;
use crate::daemon::handlers::{bounded_limit, is_operator, LimitQuery};
use crate::daemon::state::AppState;

use super::super::operator_read_models::{
    storage_violation_event_view, SecurityIncidentView, StimulusView, StorageViolationView,
};
use crate::daemon::cluster_helpers::ClusterStatusView;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct DashboardSummaryView {
    pub(crate) generated_at: chrono::DateTime<chrono::Utc>,
    pub(crate) uptime_seconds: u64,
    pub(crate) cluster: ClusterStatusView,
    pub(crate) swarm_count: usize,
    pub(crate) stimulus_count: usize,
    pub(crate) security_incident_count: usize,
    pub(crate) storage_violation_count: usize,
    pub(crate) recent_execution_count: usize,
    pub(crate) recent_workflow_execution_count: usize,
}

/// Reusable 403 body for non-operator callers hitting an operator-only
/// observability endpoint. Each of these handlers aggregates platform-wide
/// data and so MUST never be reachable by tenant or consumer users
/// (ADR-097 §observability).
fn operator_required_response(resource: &str) -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "operator_required",
            "message": format!(
                "{resource} aggregates across all tenants and is operator-restricted (ADR-097)."
            ),
        })),
    )
        .into_response()
}

pub(crate) async fn list_stimuli_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Query(query): Query<LimitQuery>,
) -> axum::response::Response {
    if !is_operator(identity.as_ref().map(|e| &e.0)) {
        return operator_required_response("Stimuli list");
    }
    let stimuli = state.operator_read_model.list_stimuli().await;
    let limit = bounded_limit(query.limit, stimuli.len().max(1), 500);
    Json(serde_json::json!({
        "items": stimuli.into_iter().take(limit).collect::<Vec<StimulusView>>(),
    }))
    .into_response()
}

pub(crate) async fn get_stimulus_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(stimulus_id): Path<Uuid>,
) -> axum::response::Response {
    if !is_operator(identity.as_ref().map(|e| &e.0)) {
        return operator_required_response("Stimulus detail");
    }
    match state
        .operator_read_model
        .get_stimulus(aegis_orchestrator_core::domain::stimulus::StimulusId(
            stimulus_id,
        ))
        .await
    {
        Some(stimulus) => Json(serde_json::json!({ "stimulus": stimulus })).into_response(),
        None => Json(serde_json::json!({"error": "stimulus not found"})).into_response(),
    }
}

pub(crate) async fn list_security_incidents_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Query(query): Query<LimitQuery>,
) -> axum::response::Response {
    if !is_operator(identity.as_ref().map(|e| &e.0)) {
        return operator_required_response("Security incidents");
    }
    let incidents = state.operator_read_model.list_security_incidents().await;
    let limit = bounded_limit(query.limit, incidents.len().max(1), 500);
    Json(serde_json::json!({
        "items": incidents.into_iter().take(limit).collect::<Vec<SecurityIncidentView>>(),
    }))
    .into_response()
}

pub(crate) async fn list_storage_violations_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Query(query): Query<LimitQuery>,
) -> axum::response::Response {
    if !is_operator(identity.as_ref().map(|e| &e.0)) {
        return operator_required_response("Storage violations");
    }
    let violations = match state.storage_event_repo.find_violations(None).await {
        Ok(events) => events
            .into_iter()
            .map(|event| storage_violation_event_view(&event))
            .collect::<Vec<StorageViolationView>>(),
        Err(e) => {
            return Json(serde_json::json!({
                "error": e.to_string(),
                "items": Vec::<StorageViolationView>::new(),
            }))
            .into_response();
        }
    };
    let limit = bounded_limit(query.limit, violations.len().max(1), 500);
    Json(serde_json::json!({
        "items": violations.into_iter().take(limit).collect::<Vec<StorageViolationView>>(),
    }))
    .into_response()
}

/// Operator-only cross-tenant observability dashboard.
///
/// SCOPE (ADR-097 §dashboard): this handler aggregates data across ALL
/// tenants — recent executions, workflow executions, swarms, stimuli,
/// security incidents — via the unscoped repository methods
/// (`list_recent_all_paginated`, `list_paginated_all`,
/// `list_swarms_unscoped`, `find_violations(None)`). It does NOT use
/// `TenantId::system()` — that is a misleading sentinel that returns
/// only the system tenant's rows, not aggregated cross-tenant data.
///
/// SECURITY: this MUST only be reachable by operators. We gate on the
/// authenticated identity's `IdentityKind::Operator` discriminant via
/// [`is_operator`]; any other identity (consumer user, tenant user,
/// service account) gets a 403. Tenant context middleware does not
/// enforce this on its own — it only resolves the caller's home tenant;
/// it doesn't restrict who can invoke a cross-tenant query.
pub(crate) async fn dashboard_summary_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    // Operator gate.
    let identity_ref = identity.as_ref().map(|e| &e.0);
    if !is_operator(identity_ref) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "operator_required",
                "message": "Dashboard summary aggregates data across all tenants and is restricted to operators (ADR-097)."
            })),
        )
            .into_response();
    }

    let cluster = cluster_status_view(&state).await;
    // Operator dashboard aggregates across all tenants — gated above by
    // the operator check, mirroring the system-tenant usage below.
    let swarms = state.swarm_service.list_swarms_unscoped().await;
    let stimuli = state.operator_read_model.list_stimuli().await;
    let security_incidents = state.operator_read_model.list_security_incidents().await;

    let storage_violation_count = match state.storage_event_repo.find_violations(None).await {
        Ok(events) => events.len(),
        Err(_) => 0,
    };
    // Cross-tenant aggregation via the unscoped repo methods (ADR-097).
    // The previous `find_recent_for_tenant(&TenantId::system())` /
    // `list_paginated_for_tenant(&TenantId::system())` only returned the
    // system tenant's rows — never aggregated platform-wide data — and
    // therefore reported false counts to operator dashboards.
    let recent_execution_count = state
        .execution_repo
        .list_recent_all_paginated(25, 0)
        .await
        .map(|items| items.len())
        .unwrap_or_default();
    let recent_workflow_execution_count = state
        .workflow_execution_repo
        .list_paginated_all(25, 0)
        .await
        .map(|items| items.len())
        .unwrap_or_default();

    Json(DashboardSummaryView {
        generated_at: chrono::Utc::now(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
        cluster,
        swarm_count: swarms.len(),
        stimulus_count: stimuli.len(),
        security_incident_count: security_incidents.len(),
        storage_violation_count,
        recent_execution_count,
        recent_workflow_execution_count,
    })
    .into_response()
}
