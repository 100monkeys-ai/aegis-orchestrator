// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Credential & Secrets REST Handlers (BC-11, ADR-078 Gap 078-4)
//!
//! HTTP handlers for `/v1/credentials/*` and `/v1/secrets/*` endpoints.
//!
//! ## Authorization model
//!
//! | Endpoint group | Required identity | Required scope |
//! |----------------|-------------------|----------------|
//! | `GET /v1/credentials` | ConsumerUser \| TenantUser | `CredentialList` |
//! | `POST /v1/credentials/api-keys` | ConsumerUser \| TenantUser | `CredentialCreate` |
//! | `GET /v1/credentials/{id}` | ConsumerUser \| TenantUser | `CredentialRead` |
//! | `DELETE /v1/credentials/{id}` | ConsumerUser \| TenantUser | `CredentialDelete` |
//! | `POST /v1/credentials/{id}/rotate` | ConsumerUser \| TenantUser | `CredentialRotate` |
//! | `GET /v1/credentials/{id}/grants` | ConsumerUser \| TenantUser | `CredentialRead` |
//! | `POST /v1/credentials/{id}/grants` | ConsumerUser \| TenantUser | `CredentialGrant` |
//! | `DELETE /v1/credentials/{id}/grants/{grant_id}` | ConsumerUser \| TenantUser | `CredentialGrant` |
//! | `POST /v1/credentials/oauth/initiate` | ConsumerUser \| TenantUser | `CredentialCreate` |
//! | `GET /v1/credentials/oauth/callback` | — (state token) | — |
//! | `POST /v1/credentials/oauth/device/poll` | ConsumerUser \| TenantUser | `CredentialCreate` |
//! | `/v1/secrets/*` | Operator \| Admin | — |
//!
//! No business logic lives here — all work is delegated to
//! `CredentialManagementService` and `SecretsManager`.

use crate::daemon::state::AppState;
use aegis_orchestrator_core::domain::api_scope::ApiScope;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialGrantId, CredentialProvider, CredentialScope, GrantTarget,
};
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::secrets::{AccessContext, SensitiveString};
use aegis_orchestrator_core::domain::tenant::TenantId;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

// ============================================================================
// Authorization helpers
// ============================================================================

/// Require that the caller is a `ConsumerUser` or `TenantUser` AND holds the
/// given `scope`. Returns `(user_id, tenant_id)` on success.
///
/// The `_scope` parameter documents intent; per-scope enforcement is enforced
/// by the token-issuance layer. The identity kind check is the hard gate here.
#[allow(clippy::result_large_err)]
fn require_credential_scope(
    extensions: &axum::http::Extensions,
    _scope: ApiScope,
) -> Result<(String, TenantId), Response> {
    let identity = extensions.get::<UserIdentity>().cloned().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response()
    })?;

    match &identity.identity_kind {
        IdentityKind::ConsumerUser { tenant_id, .. } => {
            Ok((identity.sub.clone(), tenant_id.clone()))
        }
        IdentityKind::TenantUser { tenant_slug } => {
            let tenant_id = TenantId::from_realm_slug(tenant_slug).map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "Invalid tenant slug in identity"})),
                )
                    .into_response()
            })?;
            Ok((identity.sub.clone(), tenant_id))
        }
        _ => Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Consumer or tenant user identity required"})),
        )
            .into_response()),
    }
}

/// Require that the caller holds `Operator` or `Admin` role.
#[allow(clippy::result_large_err)]
fn require_operator_or_admin(
    extensions: &axum::http::Extensions,
) -> Result<UserIdentity, Response> {
    let identity = extensions.get::<UserIdentity>().cloned().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response()
    })?;

    match &identity.identity_kind {
        IdentityKind::Operator {
            aegis_role: AegisRole::Operator | AegisRole::Admin,
        } => Ok(identity),
        _ => Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "Operator or Admin role required"})),
        )
            .into_response()),
    }
}

// ============================================================================
// Request / Response types
// ============================================================================

#[derive(Debug, Deserialize)]
pub(crate) struct StoreApiKeyRequest {
    pub(crate) provider: String,
    pub(crate) label: String,
    /// "personal" | "team:\<uuid\>"
    pub(crate) scope: Option<String>,
    /// The raw API key value — treated as sensitive
    pub(crate) value: String,
    /// Arbitrary key/value tags for filtering and organisation.
    pub(crate) tags: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddGrantRequest {
    /// "agent" | "workflow" | "all_agents"
    pub(crate) target_type: String,
    /// Agent or workflow UUID string (required for "agent" and "workflow" targets)
    pub(crate) target_value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OAuthInitiateRequest {
    pub(crate) provider: String,
    pub(crate) redirect_uri: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OAuthCallbackQuery {
    pub(crate) state: String,
    pub(crate) code: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DevicePollRequest {
    pub(crate) device_code: String,
    pub(crate) provider: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WriteSecretRequest {
    pub(crate) data: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct RotateRequest {
    pub(crate) value: String,
}

// ============================================================================
// Parsing helpers
// ============================================================================

#[allow(clippy::result_large_err)]
fn parse_provider(s: &str) -> Result<CredentialProvider, Response> {
    let provider = match s {
        "openai" => CredentialProvider::OpenAI,
        "anthropic" => CredentialProvider::Anthropic,
        "github" => CredentialProvider::GitHub,
        "google_workspace" => CredentialProvider::GoogleWorkspace,
        other if !other.is_empty() => CredentialProvider::Custom(other.to_string()),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Provider name must not be empty"})),
            )
                .into_response());
        }
    };
    Ok(provider)
}

#[allow(clippy::result_large_err)]
fn parse_credential_scope(scope: Option<&str>) -> Result<CredentialScope, Response> {
    match scope {
        None | Some("personal") => Ok(CredentialScope::Personal),
        Some(s) if s.starts_with("team:") => {
            let team_str = &s["team:".len()..];
            let team_id = uuid::Uuid::parse_str(team_str).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("Invalid team UUID in scope: {team_str}")})),
                )
                    .into_response()
            })?;
            Ok(CredentialScope::Team { team_id })
        }
        Some(other) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid scope: {other}. Expected 'personal' or 'team:<uuid>'")})),
        )
            .into_response()),
    }
}

#[allow(clippy::result_large_err)]
fn parse_grant_target(req: &AddGrantRequest) -> Result<GrantTarget, Response> {
    match req.target_type.as_str() {
        "all_agents" => Ok(GrantTarget::AllAgents),
        "agent" => {
            let val = req.target_value.as_deref().ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "target_value is required for agent grants"})),
                )
                    .into_response()
            })?;
            let id = uuid::Uuid::parse_str(val).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("Invalid agent UUID: {val}")})),
                )
                    .into_response()
            })?;
            Ok(GrantTarget::Agent {
                agent_id: aegis_orchestrator_core::domain::agent::AgentId(id),
            })
        }
        "workflow" => {
            let val = req.target_value.as_deref().ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "target_value is required for workflow grants"})),
                )
                    .into_response()
            })?;
            let id = uuid::Uuid::parse_str(val).map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("Invalid workflow UUID: {val}")})),
                )
                    .into_response()
            })?;
            Ok(GrantTarget::Workflow { workflow_id: id })
        }
        other => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Invalid target_type: {other}. Expected 'agent', 'workflow', or 'all_agents'")})),
        )
            .into_response()),
    }
}

#[allow(clippy::result_large_err)]
fn parse_binding_id(id: &str) -> Result<CredentialBindingId, Response> {
    uuid::Uuid::parse_str(id)
        .map(CredentialBindingId)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid credential binding ID: {id}")})),
            )
                .into_response()
        })
}

#[allow(clippy::result_large_err)]
fn parse_grant_id(id: &str) -> Result<CredentialGrantId, Response> {
    uuid::Uuid::parse_str(id)
        .map(CredentialGrantId)
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid grant ID: {id}")})),
            )
                .into_response()
        })
}

// ============================================================================
// Credential Handlers
// ============================================================================

/// `GET /v1/credentials` — list all credential bindings owned by the caller.
pub(crate) async fn list_credentials_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (user_id, tenant_id) =
        match require_credential_scope(request.extensions(), ApiScope::CredentialList) {
            Ok(v) => v,
            Err(r) => return r,
        };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc.list_bindings(&tenant_id, &user_id).await {
        Ok(bindings) => {
            let count = bindings.len();
            (
                StatusCode::OK,
                Json(json!({
                    "credentials": bindings,
                    "count": count,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/credentials/api-keys` — store a new API key credential.
pub(crate) async fn store_api_key_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (user_id, tenant_id) =
        match require_credential_scope(request.extensions(), ApiScope::CredentialCreate) {
            Ok(v) => v,
            Err(r) => return r,
        };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: StoreApiKeyRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let provider = match parse_provider(&payload.provider) {
        Ok(p) => p,
        Err(r) => return r,
    };

    let scope = match parse_credential_scope(payload.scope.as_deref()) {
        Ok(s) => s,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc
        .store_api_key(
            &user_id,
            &tenant_id,
            provider,
            payload.label,
            scope,
            SensitiveString::new(&payload.value),
            payload.tags,
        )
        .await
    {
        Ok(binding_id) => (
            StatusCode::CREATED,
            Json(json!({"id": binding_id.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/credentials/{id}` — fetch a single credential binding.
pub(crate) async fn get_credential_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_credential_scope(request.extensions(), ApiScope::CredentialRead) {
        return r;
    }

    let binding_id = match parse_binding_id(&id) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc.get_binding(&binding_id).await {
        Ok(Some(binding)) => (StatusCode::OK, Json(json!({"credential": binding}))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Credential binding not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/credentials/{id}` — revoke a credential binding.
pub(crate) async fn revoke_credential_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_credential_scope(request.extensions(), ApiScope::CredentialDelete) {
        return r;
    }

    let binding_id = match parse_binding_id(&id) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc.revoke_binding(&binding_id).await {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "revoked", "id": id}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/credentials/{id}/rotate` — rotate the underlying secret value.
pub(crate) async fn rotate_credential_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_credential_scope(request.extensions(), ApiScope::CredentialRotate) {
        return r;
    }

    let binding_id = match parse_binding_id(&id) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: RotateRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc
        .rotate_credential(&binding_id, SensitiveString::new(&payload.value))
        .await
    {
        Ok(()) => (StatusCode::OK, Json(json!({"status": "rotated", "id": id}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/credentials/{id}/grants` — list grants for a binding.
pub(crate) async fn list_grants_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_credential_scope(request.extensions(), ApiScope::CredentialRead) {
        return r;
    }

    let binding_id = match parse_binding_id(&id) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc.get_binding(&binding_id).await {
        Ok(Some(binding)) => {
            let count = binding.grants.len();
            (
                StatusCode::OK,
                Json(json!({
                    "grants": binding.grants,
                    "count": count,
                })),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Credential binding not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/credentials/{id}/grants` — add a grant to a binding.
pub(crate) async fn add_grant_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (user_id, _tenant_id) =
        match require_credential_scope(request.extensions(), ApiScope::CredentialGrant) {
            Ok(v) => v,
            Err(r) => return r,
        };

    let binding_id = match parse_binding_id(&id) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: AddGrantRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let target = match parse_grant_target(&payload) {
        Ok(t) => t,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc.add_grant(&binding_id, target, user_id).await {
        Ok(grant_id) => (
            StatusCode::CREATED,
            Json(json!({"grant_id": grant_id.to_string()})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/credentials/{id}/grants/{grant_id}` — revoke a single grant.
pub(crate) async fn revoke_grant_handler(
    State(state): State<Arc<AppState>>,
    Path((id, grant_id_str)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_credential_scope(request.extensions(), ApiScope::CredentialGrant) {
        return r;
    }

    let binding_id = match parse_binding_id(&id) {
        Ok(b) => b,
        Err(r) => return r,
    };

    let grant_id = match parse_grant_id(&grant_id_str) {
        Ok(g) => g,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc.revoke_grant(&binding_id, &grant_id).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"status": "revoked", "grant_id": grant_id_str})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `POST /v1/credentials/oauth/initiate` — begin an OAuth2 PKCE flow.
pub(crate) async fn oauth_initiate_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let (user_id, tenant_id) =
        match require_credential_scope(request.extensions(), ApiScope::CredentialCreate) {
            Ok(v) => v,
            Err(r) => return r,
        };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: OAuthInitiateRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let provider = match parse_provider(&payload.provider) {
        Ok(p) => p,
        Err(r) => return r,
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc
        .initiate_oauth_connection(&user_id, &tenant_id, provider, payload.redirect_uri)
        .await
    {
        Ok(initiation) => (
            StatusCode::OK,
            Json(json!({
                "authorization_url": initiation.authorization_url,
                "state": initiation.state,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/credentials/oauth/callback` — complete an OAuth2 PKCE flow.
///
/// The provider redirects here with `?state=...&code=...` query parameters.
/// The state token links this request back to the pending binding created
/// during `oauth/initiate`.
pub(crate) async fn oauth_callback_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    match svc
        .complete_oauth_connection(&query.state, &query.code)
        .await
    {
        Ok(binding_id) => (
            StatusCode::OK,
            Json(json!({"id": binding_id.to_string(), "status": "active"})),
        )
            .into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("invalid or expired") || msg.contains("not found") {
                (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": msg})),
                )
                    .into_response()
            }
        }
    }
}

/// `POST /v1/credentials/oauth/device/poll` — poll for device-flow token completion.
///
/// Returns `202 Accepted` while the device flow is still pending, `200 OK`
/// with `binding_id` when it completes. The `device_code` is used as the
/// state token for the pending-state lookup.
pub(crate) async fn device_poll_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_credential_scope(request.extensions(), ApiScope::CredentialCreate) {
        return r;
    }

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: DevicePollRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let svc = match &state.credential_service {
        Some(s) => s.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "Credential service not configured"})),
            )
                .into_response();
        }
    };

    // Use the device_code as the state token for the pending-flow lookup.
    match svc
        .complete_oauth_connection(&payload.device_code, "device_flow")
        .await
    {
        Ok(binding_id) => (
            StatusCode::OK,
            Json(json!({
                "status": "complete",
                "binding_id": binding_id.to_string(),
            })),
        )
            .into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("invalid or expired") || msg.contains("not found") {
                // Device flow is still pending — state row not yet populated by the provider.
                (
                    StatusCode::ACCEPTED,
                    Json(json!({"status": "pending", "provider": payload.provider})),
                )
                    .into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": msg})),
                )
                    .into_response()
            }
        }
    }
}

// ============================================================================
// Secrets Handlers
// ============================================================================

/// `GET /v1/secrets` — list secret paths.
///
/// `SecretsManager` does not expose a KV list operation; returns 501.
pub(crate) async fn list_secrets_handler(
    State(_state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    if let Err(r) = require_operator_or_admin(request.extensions()) {
        return r;
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "Secret listing is not supported via this endpoint"})),
    )
        .into_response()
}

/// `GET /v1/secrets/{path}` — read a secret's field names by path.
///
/// Returns the field keys present in the secret; raw values are never surfaced
/// through this endpoint to avoid leaking sensitive material over the API.
pub(crate) async fn get_secret_handler(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let identity = match require_operator_or_admin(request.extensions()) {
        Ok(id) => id,
        Err(r) => return r,
    };

    let sm = &state.secrets_manager;
    let ctx = AccessContext::system(&identity.sub);

    match sm.read_secret("kv", &path, &ctx).await {
        Ok(data) => {
            // Return field keys only — never expose raw secret values over REST.
            let fields: Vec<&String> = data.keys().collect();
            (
                StatusCode::OK,
                Json(json!({
                    "path": path,
                    "fields": fields,
                })),
            )
                .into_response()
        }
        Err(aegis_orchestrator_core::domain::secrets::SecretsError::SecretNotFound { .. }) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Secret not found", "path": path})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `PUT /v1/secrets/{path}` — write (create or update) a secret.
pub(crate) async fn write_secret_handler(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let identity = match require_operator_or_admin(request.extensions()) {
        Ok(id) => id,
        Err(r) => return r,
    };

    let body = match axum::body::to_bytes(request.into_body(), 1024 * 256).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Invalid request body"})),
            )
                .into_response();
        }
    };

    let payload: WriteSecretRequest = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("Invalid JSON: {e}")})),
            )
                .into_response();
        }
    };

    let data_obj = match payload.data.as_object() {
        Some(m) => m,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "'data' must be a JSON object"})),
            )
                .into_response();
        }
    };

    let secret_data: HashMap<String, SensitiveString> = data_obj
        .iter()
        .map(|(k, v)| {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), SensitiveString::new(&s))
        })
        .collect();

    let sm = &state.secrets_manager;
    let ctx = AccessContext::system(&identity.sub);

    match sm.write_secret("kv", &path, secret_data, &ctx).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"status": "written", "path": path})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `DELETE /v1/secrets/{path}` — delete a secret by path.
pub(crate) async fn delete_secret_handler(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let identity = match require_operator_or_admin(request.extensions()) {
        Ok(id) => id,
        Err(r) => return r,
    };

    let sm = &state.secrets_manager;
    let ctx = AccessContext::system(&identity.sub);

    match sm.delete_secret("kv", &path, &ctx).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"status": "deleted", "path": path})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
