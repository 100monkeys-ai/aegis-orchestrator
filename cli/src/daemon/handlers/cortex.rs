// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cortex pattern, skills, and metrics handlers.

use std::sync::Arc;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use crate::daemon::handlers::{tenant_id_from_identity, CortexQueryParams};
use crate::daemon::state::AppState;

/// Build the `QueryPatternsRequest.tenant_id` field from the caller's
/// authenticated identity. ADR-097: this MUST equal the JWT-derived per-user
/// tenant — never the empty string. Extracted as a free function so it can be
/// covered by a focused regression test that does not require spinning up a
/// Cortex gRPC client.
fn cortex_request_tenant_id(identity: Option<&UserIdentity>) -> String {
    tenant_id_from_identity(identity).as_str().to_string()
}

pub(crate) async fn list_cortex_patterns_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
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

    // ADR-097: scope all Cortex reads to the caller's authenticated tenant.
    // Previously the orchestrator passed an empty tenant_id, returning patterns
    // belonging to every tenant on the cluster.
    let caller_tenant = cortex_request_tenant_id(identity.as_ref().map(|e| &e.0));

    let request =
        aegis_orchestrator_core::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: params.q.clone().unwrap_or_default(),
            error_type: None,
            limit: Some(params.limit.unwrap_or(100) as u32),
            min_success_score: None,
            tenant_id: caller_tenant,
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
    identity: Option<Extension<UserIdentity>>,
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

    // ADR-097: scope skills aggregation to the caller's authenticated tenant.
    let caller_tenant = cortex_request_tenant_id(identity.as_ref().map(|e| &e.0));

    let request =
        aegis_orchestrator_core::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: String::new(),
            error_type: None,
            limit: Some(500),
            min_success_score: None,
            tenant_id: caller_tenant,
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
    identity: Option<Extension<UserIdentity>>,
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

    // ADR-097: scope metrics aggregation to the caller's authenticated tenant.
    let caller_tenant = cortex_request_tenant_id(identity.as_ref().map(|e| &e.0));

    let request =
        aegis_orchestrator_core::infrastructure::aegis_cortex_proto::QueryPatternsRequest {
            error_signature: String::new(),
            error_type: None,
            limit: Some(1000),
            min_success_score: None,
            tenant_id: caller_tenant,
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

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::iam::{IdentityKind, ZaruTier};
    use aegis_orchestrator_core::domain::tenant::TenantId;

    /// Regression: ADR-097. Outgoing `QueryPatternsRequest.tenant_id` MUST
    /// equal the JWT-derived per-user tenant — never the empty string. The
    /// previous implementation passed `String::new()`, returning every
    /// tenant's patterns to the caller.
    #[test]
    fn list_cortex_patterns_uses_caller_tenant() {
        let sub = "user-abc-123";
        let identity = UserIdentity {
            sub: sub.into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        };

        let derived = cortex_request_tenant_id(Some(&identity));
        let expected = TenantId::for_consumer_user(sub).unwrap();

        assert_eq!(
            derived,
            expected.as_str(),
            "outgoing Cortex tenant_id must be the caller's per-user tenant"
        );
        assert_ne!(derived, "", "tenant_id must never be the empty string");
    }

    #[test]
    fn cortex_tenant_id_is_never_empty_for_known_identity_kinds() {
        // Operator → aegis-system, ServiceAccount → aegis-system, ConsumerUser → u-{sub}.
        // None of these may collapse to "" (which would unscope the Cortex query).
        let cases = [
            UserIdentity {
                sub: "op-1".into(),
                realm_slug: "aegis-system".into(),
                email: None,
                name: None,
                identity_kind: IdentityKind::Operator {
                    aegis_role: aegis_orchestrator_core::domain::iam::AegisRole::Operator,
                },
            },
            UserIdentity {
                sub: "svc-1".into(),
                realm_slug: "aegis-system".into(),
                email: None,
                name: None,
                identity_kind: IdentityKind::ServiceAccount {
                    client_id: "aegis-temporal-worker".into(),
                },
            },
        ];
        for identity in &cases {
            let derived = cortex_request_tenant_id(Some(identity));
            assert!(!derived.is_empty(), "{identity:?} produced empty tenant_id");
        }
    }
}
