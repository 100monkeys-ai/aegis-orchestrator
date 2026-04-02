// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Admin handlers: rate-limit override management (ADR-072).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::daemon::state::AppState;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::rate_limit::{
    tier_defaults, RateLimitBucket, RateLimitPolicyResolver, RateLimitResourceType,
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

    let tier = match &identity.identity_kind {
        IdentityKind::ConsumerUser { zaru_tier } => zaru_tier.clone(),
        _ => ZaruTier::Enterprise,
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

    // Build lookup: (resource_type_str, bucket_str) -> (counter, window_start)
    let mut counter_map: std::collections::HashMap<
        (String, String),
        (i64, chrono::DateTime<chrono::Utc>),
    > = std::collections::HashMap::new();
    for row in &usage_rows {
        counter_map.insert(
            (row.resource_type.clone(), row.bucket.clone()),
            (row.counter, row.window_start),
        );
    }

    let resolver = HierarchicalPolicyResolver::new(repo.pool().clone());
    let now = chrono::Utc::now();

    // Seed from full tier policy so new users see all limits at zero
    let all_policies = tier_defaults(&tier);
    let mut items: Vec<UserRateLimitUsageItem> = Vec::new();

    for default_policy in &all_policies {
        let resource_type = &default_policy.resource_type;
        let resource_str = resource_type_to_db_str(resource_type);
        let policy = match resolver
            .resolve_policy(&identity, &tenant_id, resource_type)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    resource_type = %resource_str,
                    error = %e,
                    "Failed to resolve rate-limit policy; skipping resource type"
                );
                continue;
            }
        };
        for (bucket, window) in &policy.windows {
            let bucket_str = bucket_to_str(bucket);
            let (current_count, resets_at) =
                match counter_map.get(&(resource_str.clone(), bucket_str.clone())) {
                    Some((count, window_start)) => {
                        let resets_at =
                            *window_start + chrono::Duration::seconds(window.window_seconds as i64);
                        (*count, resets_at)
                    }
                    None => (
                        0i64,
                        now + chrono::Duration::seconds(window.window_seconds as i64),
                    ),
                };
            items.push(UserRateLimitUsageItem {
                resource_type: resource_str.clone(),
                bucket: bucket_str,
                current_count,
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

fn bucket_to_str(bucket: &RateLimitBucket) -> String {
    match bucket {
        RateLimitBucket::PerMinute => "per_minute".into(),
        RateLimitBucket::Hourly => "hourly".into(),
        RateLimitBucket::Daily => "daily".into(),
        RateLimitBucket::Weekly => "weekly".into(),
        RateLimitBucket::Monthly => "monthly".into(),
    }
}

fn resource_type_to_db_str(rt: &RateLimitResourceType) -> String {
    match rt {
        RateLimitResourceType::AgentExecution => "agent_execution".into(),
        RateLimitResourceType::WorkflowExecution => "workflow_execution".into(),
        RateLimitResourceType::LlmCall => "llm_call".into(),
        RateLimitResourceType::LlmToken => "llm_token".into(),
        RateLimitResourceType::SealToolCall { tool_pattern } => {
            format!("seal_tool:{tool_pattern}")
        }
    }
}
