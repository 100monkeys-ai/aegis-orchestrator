// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Health check handlers.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;

use crate::daemon::state::AppState;

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct HealthView {
    pub(crate) status: String,
    pub(crate) mode: String,
    pub(crate) uptime_seconds: u64,
}

pub(crate) async fn health_handler(State(state): State<Arc<AppState>>) -> Json<HealthView> {
    Json(HealthView {
        status: "healthy".to_string(),
        mode: "live".to_string(),
        uptime_seconds: state.start_time.elapsed().as_secs(),
    })
}

pub(crate) async fn readiness_handler(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let temporal_ready = if state.config.spec.temporal.is_some() {
        state.temporal_client_container.read().await.is_some()
    } else {
        true
    };

    let database_ready = state.config.spec.database.is_none() || state.cluster_repo.is_some();

    Json(serde_json::json!({
        "status": if temporal_ready && database_ready { "ready" } else { "degraded" },
        "uptime_seconds": state.start_time.elapsed().as_secs(),
        "dependencies": {
            "database": database_ready,
            "temporal": temporal_ready,
            "cluster_repository": state.cluster_repo.is_some(),
        }
    }))
}
