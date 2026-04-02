// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Admin handlers: rate-limit override management (ADR-072).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::daemon::state::AppState;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::rate_limit::{
    RateLimitBucket, RateLimitPolicyResolver, RateLimitResourceType,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::rate_limit::policy_resolver::HierarchicalPolicyResolver;
use axum::Extension;
use chrono::{DateTime, Utc};

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

#[derive(Debug, serde::Serialize)]
pub(crate) struct UserRateLimitUsageItem {
    pub resource_type: String,
    pub bucket: String,
    pub current_count: i64,
    pub limit_value: i64,
    pub window_seconds: u64,
    pub resets_at: DateTime<Utc>,
}

pub(crate) async fn get_user_rate_limit_usage_handler(
    State(state): State<Arc<AppState>>,
    Extension(identity): Extension<UserIdentity>,
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

    let tenant_id = match &identity.identity_kind {
        IdentityKind::ConsumerUser { .. } => TenantId::consumer(),
        IdentityKind::TenantUser { tenant_slug } => {
            TenantId::from_realm_slug(tenant_slug).unwrap_or_else(|_| TenantId::consumer())
        }
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => TenantId::system(),
    };

    let usage_rows = match repo.get_usage("user", &identity.sub).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let resolver = HierarchicalPolicyResolver::new(repo.pool().clone());

    let mut seen = std::collections::HashSet::new();
    let mut resource_types: Vec<(String, RateLimitResourceType)> = Vec::new();
    for row in &usage_rows {
        if seen.insert(row.resource_type.clone()) {
            resource_types.push((
                row.resource_type.clone(),
                parse_resource_type(&row.resource_type),
            ));
        }
    }

    let mut items: Vec<UserRateLimitUsageItem> = Vec::new();
    for (resource_type_str, resource_type) in &resource_types {
        let policy = match resolver
            .resolve_policy(&identity, &tenant_id, resource_type)
            .await
        {
            Ok(p) => p,
            Err(_) => continue,
        };
        for row in usage_rows
            .iter()
            .filter(|r| &r.resource_type == resource_type_str)
        {
            let Some(bucket) = bucket_from_str(&row.bucket) else {
                continue;
            };
            let Some(window) = policy.windows.get(&bucket) else {
                continue;
            };
            let resets_at =
                row.window_start + chrono::Duration::seconds(window.window_seconds as i64);
            items.push(UserRateLimitUsageItem {
                resource_type: row.resource_type.clone(),
                bucket: row.bucket.clone(),
                current_count: row.counter,
                limit_value: window.limit as i64,
                window_seconds: window.window_seconds,
                resets_at,
            });
        }
    }

    let count = items.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "usage": items, "count": count })),
    )
        .into_response()
}

fn bucket_from_str(s: &str) -> Option<RateLimitBucket> {
    match s {
        "per_minute" => Some(RateLimitBucket::PerMinute),
        "hourly" => Some(RateLimitBucket::Hourly),
        "daily" => Some(RateLimitBucket::Daily),
        "weekly" => Some(RateLimitBucket::Weekly),
        "monthly" => Some(RateLimitBucket::Monthly),
        _ => None,
    }
}

fn parse_resource_type(s: &str) -> RateLimitResourceType {
    match s {
        "agent_execution" => RateLimitResourceType::AgentExecution,
        "workflow_execution" => RateLimitResourceType::WorkflowExecution,
        "llm_call" => RateLimitResourceType::LlmCall,
        "llm_token" => RateLimitResourceType::LlmToken,
        other if other.starts_with("seal_tool:") => RateLimitResourceType::SealToolCall {
            tool_pattern: other["seal_tool:".len()..].to_string(),
        },
        other => RateLimitResourceType::SealToolCall {
            tool_pattern: other.to_string(),
        },
    }
}
