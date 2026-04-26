// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! SEAL attestation, invocation, and tool listing handlers.

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::domain::iam::{RealmKind, UserIdentity};
use aegis_orchestrator_core::domain::shared_kernel::ExecutionId;

use crate::daemon::state::AppState;

#[derive(serde::Deserialize)]
pub struct HttpAttestationRequest {
    pub agent_id: Option<String>,
    pub execution_id: Option<String>,
    pub container_id: Option<String>,
    #[serde(alias = "public_key_pem", alias = "agent_public_key")]
    pub public_key: String,
    pub security_context: Option<String>,
    pub principal_subject: Option<String>,
    pub user_id: Option<String>,
    pub workload_id: Option<String>,
    pub zaru_tier: Option<String>,
    pub tenant_id: Option<String>,
    pub task_summary: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct HttpSealEnvelope {
    pub protocol: Option<String>,
    pub security_token: String,
    pub signature: String,
    pub payload: serde_json::Value,
    pub timestamp: Option<String>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct SealToolsQuery {
    security_context: Option<String>,
}

pub(crate) async fn attest_seal_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(request): Json<HttpAttestationRequest>,
) -> impl IntoResponse {
    let tenant_id =
        if let Some(explicit) = request.tenant_id.as_deref().and_then(|s| {
            aegis_orchestrator_core::domain::tenant::TenantId::from_realm_slug(s).ok()
        }) {
            explicit
        } else if let Some(exec_id) = request
            .execution_id
            .as_deref()
            .and_then(|s| ExecutionId::from_string(s).ok())
        {
            match state
                .execution_service
                .get_execution_unscoped(exec_id)
                .await
            {
                Ok(execution) => execution.tenant_id,
                Err(_) => aegis_orchestrator_core::domain::tenant::TenantId::consumer(),
            }
        } else {
            aegis_orchestrator_core::domain::tenant::TenantId::consumer()
        };

    // Derive the realm classification from the authenticated `UserIdentity`
    // (set by the auth middleware after JWT validation). When no identity is
    // available (unauthenticated path or test harness), fall back to Consumer
    // since the HTTP attest endpoint is the Zaru consumer entry point.
    let realm = identity
        .as_ref()
        .map(|Extension(id)| id.realm_kind())
        .unwrap_or(RealmKind::Consumer);

    let internal_req =
        aegis_orchestrator_core::infrastructure::seal::attestation::AttestationRequest {
            agent_id: request.agent_id.clone(),
            execution_id: request.execution_id.clone(),
            container_id: request.container_id.clone(),
            public_key_pem: request.public_key.clone(),
            security_context: request.security_context.clone(),
            principal_subject: request.principal_subject.clone(),
            user_id: request.user_id.clone(),
            workload_id: request.workload_id.clone(),
            zaru_tier: request.zaru_tier.clone(),
            tenant_id,
            realm,
            task_summary: request.task_summary.clone(),
        };

    match state.attestation_service.attest(internal_req).await {
        Ok(res) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": res.status,
                "security_token": res.security_token,
                "expires_at": res.expires_at,
                "session_id": res.session_id,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": e.to_string()
            })),
        )
            .into_response(),
    }
}

pub(crate) async fn invoke_seal_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<HttpSealEnvelope>,
) -> impl IntoResponse {
    let (protocol, timestamp) = match (request.protocol, request.timestamp) {
        (Some(p), Some(t)) => (p, t),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "SEAL envelope requires both 'protocol' and 'timestamp' fields"
                })),
            )
                .into_response();
        }
    };

    let envelope = aegis_orchestrator_core::infrastructure::seal::envelope::SealEnvelope {
        protocol,
        security_token: request.security_token,
        signature: request.signature,
        payload: request.payload,
        timestamp,
    };

    // The ToolInvocationService is responsible for validating the security_token
    // and extracting any required claims (such as agent_id) from it as appropriate.
    match state.tool_invocation_service.invoke_tool(&envelope).await {
        Ok(res) => (StatusCode::OK, Json(res)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e.to_string()
            })),
        )
            .into_response(),
    }
}

pub(crate) async fn list_seal_tools_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SealToolsQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let security_context = headers
        .get("X-Zaru-Security-Context")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or(query.security_context);

    let tools_result = if let Some(ref security_context) = security_context {
        state
            .tool_invocation_service
            .get_available_tools_for_context(security_context)
            .await
    } else {
        state.tool_invocation_service.get_available_tools().await
    };

    match tools_result {
        Ok(tools) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "protocol": "seal/v1",
                "attestation_endpoint": "/v1/seal/attest",
                "invoke_endpoint": "/v1/seal/invoke",
                "security_context": security_context,
                "tools": tools,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}
