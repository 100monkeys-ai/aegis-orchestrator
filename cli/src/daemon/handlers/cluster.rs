// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cluster status and node handlers.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;

use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::cluster_helpers::{cluster_status_view, load_cluster_nodes};
use crate::daemon::handlers::{bounded_limit, LimitQuery};
use crate::daemon::state::AppState;

pub(crate) async fn cluster_status_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("node:read")?;
    Ok(Json(cluster_status_view(&state).await))
}

pub(crate) async fn cluster_nodes_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Query(query): Query<LimitQuery>,
) -> Result<
    impl axum::response::IntoResponse,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    scope_guard.require("node:list")?;
    let nodes = load_cluster_nodes(&state).await;
    let limit = bounded_limit(query.limit, nodes.len().max(1), 500);
    Ok(Json(serde_json::json!({
        "source": if state.cluster_repo.is_some() { "cluster_repository" } else { "local_fallback" },
        "items": nodes.into_iter().take(limit).collect::<Vec<_>>(),
    })))
}

fn unavailable(msg: &str) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": msg })),
    )
}

/// ADR-152: members with their leases, load and edge streams, the duties
/// and their holders, and the last day's executions by status.
pub(crate) async fn cluster_overview_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
) -> Result<impl axum::response::IntoResponse, (axum::http::StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("node:read")?;
    let Some(overview) = state.cluster_overview.as_ref() else {
        return Err(unavailable(
            "the cluster view needs the orchestrator's database",
        ));
    };
    match overview.overview().await {
        Ok(view) => Ok(Json(view)),
        Err(e) => {
            tracing::warn!(error = %e, "cluster overview query failed");
            Err(unavailable("the cluster view could not be read"))
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct MetricsQuery {
    /// A named series, or `targets_up` for the status matrix.
    series: String,
    /// The range in minutes, for a series (default 60, at most 1440).
    range_mins: Option<u32>,
}

/// ADR-152: one named graph series, or the scrape-target status matrix, from
/// the cluster's Prometheus. Only the fixed expressions in
/// `overview::METRIC_SERIES` are ever evaluated.
pub(crate) async fn cluster_metrics_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Query(query): Query<MetricsQuery>,
) -> Result<impl axum::response::IntoResponse, (axum::http::StatusCode, Json<serde_json::Value>)> {
    use aegis_orchestrator_core::infrastructure::cluster::overview::{
        series_query, TARGETS_UP_QUERY,
    };
    scope_guard.require("node:read")?;
    let Some(base) = state
        .config
        .spec
        .observability
        .as_ref()
        .and_then(|o| o.metrics.as_ref())
        .and_then(|m| {
            aegis_orchestrator_core::domain::node_config::resolve_env_value_optional(&m.query_url)
        })
    else {
        return Err(unavailable("no metrics source is configured"));
    };
    let base = base.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| unavailable("metrics client"))?;
    let request = if query.series == "targets_up" {
        client
            .get(format!("{base}/api/v1/query"))
            .query(&[("query", TARGETS_UP_QUERY)])
    } else {
        let Some(promql) = series_query(&query.series) else {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "unknown series" })),
            ));
        };
        let range = i64::from(query.range_mins.unwrap_or(60).clamp(5, 1440)) * 60;
        let end = chrono::Utc::now().timestamp();
        let step = (range / 120).max(15);
        client.get(format!("{base}/api/v1/query_range")).query(&[
            ("query", promql.to_string()),
            ("start", (end - range).to_string()),
            ("end", end.to_string()),
            ("step", step.to_string()),
        ])
    };
    let answer = request
        .send()
        .await
        .map_err(|_| unavailable("the metrics source did not answer"))?;
    let body: serde_json::Value = answer
        .json()
        .await
        .map_err(|_| unavailable("the metrics source answered no JSON"))?;
    Ok(Json(serde_json::json!({
        "series": query.series,
        "data": body.get("data").cloned().unwrap_or(serde_json::Value::Null),
    })))
}
