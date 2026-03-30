// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cluster status and node handlers.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;

use crate::daemon::cluster_helpers::{cluster_status_view, load_cluster_nodes, ClusterStatusView};
use crate::daemon::handlers::{bounded_limit, LimitQuery};
use crate::daemon::state::AppState;

pub(crate) async fn cluster_status_handler(
    State(state): State<Arc<AppState>>,
) -> Json<ClusterStatusView> {
    Json(cluster_status_view(&state).await)
}

pub(crate) async fn cluster_nodes_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LimitQuery>,
) -> Json<serde_json::Value> {
    let nodes = load_cluster_nodes(&state).await;
    let limit = bounded_limit(query.limit, nodes.len().max(1), 500);
    Json(serde_json::json!({
        "source": if state.cluster_repo.is_some() { "cluster_repository" } else { "local_fallback" },
        "items": nodes.into_iter().take(limit).collect::<Vec<_>>(),
    }))
}
