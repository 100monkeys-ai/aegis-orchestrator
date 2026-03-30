// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Admin handlers: rate-limit override management (ADR-072).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::daemon::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ListOverridesQuery {
    tenant_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UsageQuery {
    scope_type: String,
    scope_id: String,
}

pub(crate) async fn list_rate_limit_overrides_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListOverridesQuery>,
) -> axum::response::Response {
    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    match repo
        .list(params.tenant_id.as_deref(), params.user_id.as_deref())
        .await
    {
        Ok(overrides) => {
            let count = overrides.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "overrides": overrides,
                    "count": count,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn upsert_rate_limit_override_handler(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<
        aegis_orchestrator_core::infrastructure::rate_limit::override_repository::CreateOverrideRequest,
    >,
) -> axum::response::Response {
    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    // Validate: exactly one of tenant_id or user_id must be set (matches DB constraint)
    if payload.tenant_id.is_some() == payload.user_id.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "Exactly one of tenant_id or user_id must be provided"}),
            ),
        )
            .into_response();
    }

    match repo.upsert(&payload).await {
        Ok(row) => (
            StatusCode::OK,
            Json(serde_json::to_value(&row).unwrap_or(serde_json::json!({"status": "upserted"}))),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn delete_rate_limit_override_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<uuid::Uuid>,
) -> axum::response::Response {
    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.delete(id).await {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "deleted", "id": id.to_string()})),
        )
            .into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Override not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn get_rate_limit_usage_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<UsageQuery>,
) -> axum::response::Response {
    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    match repo.get_usage(&params.scope_type, &params.scope_id).await {
        Ok(rows) => {
            let count = rows.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "usage": rows,
                    "count": count,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
