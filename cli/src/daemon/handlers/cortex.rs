// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cortex pattern, skills, and metrics handlers.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::daemon::handlers::CortexQueryParams;
use crate::daemon::state::AppState;

pub(crate) async fn list_cortex_patterns_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CortexQueryParams>,
) -> impl IntoResponse {
    let Some(ref cortex_client) = state.cortex_client else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "cortex_not_configured",
                "message": "Cortex gRPC service is not configured; orchestrator running in memoryless mode"
            })),
        )
            .into_response();
    };

    let request =
        aegis_orchestrator_core::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: params.q.clone().unwrap_or_default(),
            error_type: None,
            limit: Some(params.limit.unwrap_or(100) as u32),
            min_success_score: None,
            tenant_id: String::new(),
        };

    match cortex_client.query_patterns(request).await {
        Ok(response) => {
            let patterns: Vec<serde_json::Value> = response
                .patterns
                .into_iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "error_signature_hash": p.error_signature_hash,
                        "error_type": p.error_type,
                        "error_message": p.error_message,
                        "solution_approach": p.solution_approach,
                        "solution_code": p.solution_code,
                        "frequency": p.frequency,
                        "success_count": p.success_count,
                        "total_count": p.total_count,
                        "success_score": p.success_score,
                        "created_at": p.created_at,
                        "last_used_at": p.last_used_at,
                    })
                })
                .collect();
            Json(serde_json::json!({ "items": patterns })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query cortex patterns");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("cortex_query_failed: {e}") })),
            )
                .into_response()
        }
    }
}

pub(crate) async fn get_cortex_skills_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let Some(ref cortex_client) = state.cortex_client else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "cortex_not_configured",
                "message": "Cortex gRPC service is not configured; orchestrator running in memoryless mode"
            })),
        )
            .into_response();
    };

    let request =
        aegis_orchestrator_core::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: String::new(),
            error_type: None,
            limit: Some(500),
            min_success_score: None,
            tenant_id: String::new(),
        };

    match cortex_client.query_patterns(request).await {
        Ok(response) => {
            let mut skill_map: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            for p in &response.patterns {
                let category = if p.error_type.is_empty() {
                    "general"
                } else {
                    &p.error_type
                };
                let entry = skill_map.entry(category.to_string()).or_insert_with(|| {
                    serde_json::json!({
                        "id": category,
                        "name": category,
                        "description": format!("Learned patterns for {category} errors"),
                        "category": category,
                        "level": "intermediate",
                        "patternCount": 0u64,
                        "usageCount": 0u64,
                        "successRate": 0.0f64,
                    })
                });
                if let Some(obj) = entry.as_object_mut() {
                    let count = obj
                        .get("patternCount")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        + 1;
                    obj.insert("patternCount".to_string(), serde_json::json!(count));
                    let usage = obj.get("usageCount").and_then(|v| v.as_u64()).unwrap_or(0)
                        + p.frequency as u64;
                    obj.insert("usageCount".to_string(), serde_json::json!(usage));
                    let total = p.total_count.max(1) as f64;
                    let rate = (p.success_count as f64 / total) * 100.0;
                    let prev_rate = obj
                        .get("successRate")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    obj.insert(
                        "successRate".to_string(),
                        serde_json::json!((prev_rate + rate) / 2.0),
                    );
                    if count >= 5 {
                        obj.insert("level".to_string(), serde_json::json!("advanced"));
                    }
                }
            }
            let skills: Vec<serde_json::Value> = skill_map.into_values().collect();
            Json(serde_json::json!({ "items": skills })).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query cortex skills");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("cortex_query_failed: {e}") })),
            )
                .into_response()
        }
    }
}

pub(crate) async fn get_cortex_metrics_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let Some(ref cortex_client) = state.cortex_client else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "cortex_not_configured",
                "message": "Cortex gRPC service is not configured; orchestrator running in memoryless mode"
            })),
        )
            .into_response();
    };

    let request =
        aegis_orchestrator_core::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: String::new(),
            error_type: None,
            limit: Some(1000),
            min_success_score: None,
            tenant_id: String::new(),
        };

    match cortex_client.query_patterns(request).await {
        Ok(response) => {
            let total_patterns = response.patterns.len();
            let total_frequency: u64 = response.patterns.iter().map(|p| p.frequency as u64).sum();
            let avg_success: f64 = if total_patterns > 0 {
                response
                    .patterns
                    .iter()
                    .map(|p| p.success_score as f64)
                    .sum::<f64>()
                    / total_patterns as f64
            } else {
                0.0
            };

            let mut error_types: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for p in &response.patterns {
                if !p.error_type.is_empty() {
                    error_types.insert(p.error_type.clone());
                }
            }

            Json(serde_json::json!({
                "totalPatterns": total_patterns,
                "totalSkills": error_types.len(),
                "avgSuccessRate": (avg_success * 100.0).round() / 100.0,
                "totalFrequency": total_frequency,
                "patternsThisWeek": 0,
                "growthRate": 0,
                "firstAttemptSuccessRate": 0,
                "averageIterations": 0,
                "learningVelocity": 0,
                "mostUsedSkills": error_types.into_iter().take(5).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to query cortex metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("cortex_query_failed: {e}") })),
            )
                .into_response()
        }
    }
}
