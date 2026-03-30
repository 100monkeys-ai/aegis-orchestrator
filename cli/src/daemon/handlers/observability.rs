// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Observability handlers: stimuli, security incidents, storage violations, dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::domain::tenant::TenantId;

use crate::daemon::cluster_helpers::cluster_status_view;
use crate::daemon::handlers::{bounded_limit, LimitQuery};
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

pub(crate) async fn list_stimuli_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<serde_json::Value> {
    let stimuli = state.operator_read_model.list_stimuli().await;
    let limit = bounded_limit(query.limit, stimuli.len().max(1), 500);
    Json(serde_json::json!({
        "items": stimuli.into_iter().take(limit).collect::<Vec<StimulusView>>(),
    }))
}

pub(crate) async fn get_stimulus_handler(
    State(state): State<Arc<AppState>>,
    Path(stimulus_id): Path<Uuid>,
) -> Json<serde_json::Value> {
    match state
        .operator_read_model
        .get_stimulus(aegis_orchestrator_core::domain::stimulus::StimulusId(
            stimulus_id,
        ))
        .await
    {
        Some(stimulus) => Json(serde_json::json!({ "stimulus": stimulus })),
        None => Json(serde_json::json!({"error": "stimulus not found"})),
    }
}

pub(crate) async fn list_security_incidents_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<serde_json::Value> {
    let incidents = state.operator_read_model.list_security_incidents().await;
    let limit = bounded_limit(query.limit, incidents.len().max(1), 500);
    Json(serde_json::json!({
        "items": incidents.into_iter().take(limit).collect::<Vec<SecurityIncidentView>>(),
    }))
}

pub(crate) async fn list_storage_violations_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<serde_json::Value> {
    let violations = match state.storage_event_repo.find_violations(None).await {
        Ok(events) => events
            .into_iter()
            .map(|event| storage_violation_event_view(&event))
            .collect::<Vec<StorageViolationView>>(),
        Err(e) => {
            return Json(serde_json::json!({
                "error": e.to_string(),
                "items": Vec::<StorageViolationView>::new(),
            }));
        }
    };
    let limit = bounded_limit(query.limit, violations.len().max(1), 500);
    Json(serde_json::json!({
        "items": violations.into_iter().take(limit).collect::<Vec<StorageViolationView>>(),
    }))
}

pub(crate) async fn dashboard_summary_handler(
    State(state): State<Arc<AppState>>,
) -> Json<DashboardSummaryView> {
    let cluster = cluster_status_view(&state).await;
    let swarms = state.swarm_service.list_swarms().await;
    let stimuli = state.operator_read_model.list_stimuli().await;
    let security_incidents = state.operator_read_model.list_security_incidents().await;

    let storage_violation_count = match state.storage_event_repo.find_violations(None).await {
        Ok(events) => events.len(),
        Err(_) => 0,
    };
    let recent_execution_count = state
        .execution_repo
        .find_recent_for_tenant(&TenantId::default(), 25)
        .await
        .map(|items| items.len())
        .unwrap_or_default();
    let recent_workflow_execution_count = state
        .workflow_execution_repo
        .list_paginated_for_tenant(&TenantId::default(), 25, 0)
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
}
