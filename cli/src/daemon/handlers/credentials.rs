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
//! | `GET /v1/credentials/{id}` | ConsumerUser \| TenantUser \| Operator | `CredentialRead` |
//! | `DELETE /v1/credentials/{id}` | ConsumerUser \| TenantUser \| Operator | `CredentialDelete` |
//! | `POST /v1/credentials/{id}/rotate` | ConsumerUser \| TenantUser \| Operator | `CredentialRotate` |
//! | `GET /v1/credentials/{id}/grants` | ConsumerUser \| TenantUser \| Operator | `CredentialRead` |
//! | `POST /v1/credentials/{id}/grants` | ConsumerUser \| TenantUser \| Operator | `CredentialGrant` |
//! | `DELETE /v1/credentials/{id}/grants/{grant_id}` | ConsumerUser \| TenantUser \| Operator | `CredentialGrant` |
//! | `POST /v1/credentials/oauth/initiate` | ConsumerUser \| TenantUser | `CredentialCreate` |
//! | `GET /v1/credentials/oauth/callback` | — (state token) | — |
//! | `POST /v1/credentials/oauth/device/poll` | ConsumerUser \| TenantUser | `CredentialCreate` |
//! | `/v1/secrets/*` | Operator \| Admin | — |
//!
//! The `{id}` routes additionally pass the caller to the service as a
//! [`CredentialActor`]; the service decides whether that caller may reach
//! the binding (owner, member of the team it is scoped to, operator) and
//! answers everyone else exactly as for a binding that does not exist.
//!
//! No business logic lives here — all work is delegated to
//! `CredentialManagementService` and `SecretsManager`.

use crate::daemon::state::AppState;
use aegis_orchestrator_core::application::credential_service::{
    CredentialActor, CredentialManagementService, StoreApiKeyCommand,
};
use aegis_orchestrator_core::domain::api_scope::ApiScope;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialGrantId, CredentialProvider, CredentialScope, CredentialType,
    GrantTarget,
};
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::secrets::{AccessContext, SensitiveString};
use aegis_orchestrator_core::domain::tenant::TenantId;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
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

/// Resolve the caller of a by-id credential route into the
/// [`CredentialActor`] the service authorises, with the caller's `sub`.
///
/// A missing identity is 401. Consumer and tenant users act as themselves
/// in their own tenant; operators act as operators, writing only when their
/// role is `aegis:admin` or `aegis:operator`; service accounts are refused
/// as before.
#[allow(clippy::result_large_err)]
fn credential_actor(
    extensions: &axum::http::Extensions,
) -> Result<(CredentialActor, String), Response> {
    let identity = extensions.get::<UserIdentity>().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response()
    })?;
    match &identity.identity_kind {
        IdentityKind::Operator { aegis_role } => Ok((
            CredentialActor::Operator {
                may_write: !matches!(aegis_role, AegisRole::Readonly),
            },
            identity.sub.clone(),
        )),
        _ => {
            let (user_id, tenant_id) =
                require_credential_scope(extensions, ApiScope::CredentialRead)?;
            Ok((
                CredentialActor::User {
                    user_id: user_id.clone(),
                    tenant_id,
                },
                user_id,
            ))
        }
    }
}

/// State of the by-id credential sub-router.
#[derive(Clone)]
pub(crate) struct CredentialsByIdState {
    pub(crate) credential_service: Option<Arc<dyn CredentialManagementService>>,
}

/// The `/v1/credentials/{id}*` routes (ADR-078). Merged into the daemon
/// router by `router::create_router`, beneath the same authentication
/// layers as every other route.
pub(crate) fn credentials_by_id_router(state: CredentialsByIdState) -> Router {
    Router::new()
        .route(
            "/v1/credentials/{id}",
            get(get_credential_handler).delete(revoke_credential_handler),
        )
        .route(
            "/v1/credentials/{id}/rotate",
            post(rotate_credential_handler),
        )
        .route(
            "/v1/credentials/{id}/grants",
            get(list_grants_handler).post(add_grant_handler),
        )
        .route(
            "/v1/credentials/{id}/grants/{grant_id}",
            axum::routing::delete(revoke_grant_handler),
        )
        .with_state(state)
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
    /// "secret" | "variable" | "service_account" | "oauth2"
    pub(crate) credential_type: String,
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
        "google" => CredentialProvider::Google,
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

#[allow(clippy::result_large_err)]
fn parse_credential_type(s: &str) -> Result<CredentialType, Response> {
    match s {
        "secret" => Ok(CredentialType::Secret),
        "variable" => Ok(CredentialType::Variable),
        "service_account" => Ok(CredentialType::ServiceAccount),
        "oauth2" => Ok(CredentialType::OAuth2),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("Unknown credential type: {}", s)})),
        )
            .into_response()),
    }
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

    let credential_type = match parse_credential_type(&payload.credential_type) {
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

    match svc
        .store_api_key(StoreApiKeyCommand {
            owner_user_id: user_id,
            tenant_id,
            provider,
            label: payload.label,
            scope,
            api_key_value: SensitiveString::new(&payload.value),
            credential_type,
        })
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
    State(state): State<CredentialsByIdState>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (actor, _sub) = match credential_actor(request.extensions()) {
        Ok(v) => v,
        Err(r) => return r,
    };

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

    match svc.get_binding(&actor, &binding_id).await {
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
    State(state): State<CredentialsByIdState>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (actor, _sub) = match credential_actor(request.extensions()) {
        Ok(v) => v,
        Err(r) => return r,
    };

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

    match svc.revoke_binding(&actor, &binding_id).await {
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
    State(state): State<CredentialsByIdState>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (actor, _sub) = match credential_actor(request.extensions()) {
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
        .rotate_credential(&actor, &binding_id, SensitiveString::new(&payload.value))
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
    State(state): State<CredentialsByIdState>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (actor, _sub) = match credential_actor(request.extensions()) {
        Ok(v) => v,
        Err(r) => return r,
    };

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

    match svc.get_binding(&actor, &binding_id).await {
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
    State(state): State<CredentialsByIdState>,
    Path(id): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let (actor, user_id) = match credential_actor(request.extensions()) {
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

    match svc.add_grant(&actor, &binding_id, target, user_id).await {
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
    State(state): State<CredentialsByIdState>,
    Path((id, grant_id_str)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (actor, _sub) = match credential_actor(request.extensions()) {
        Ok(v) => v,
        Err(r) => return r,
    };

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

    match svc.revoke_grant(&actor, &binding_id, &grant_id).await {
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

#[cfg(test)]
mod tests {
    //! Who may reach a credential binding by id (ADR-078; security audit
    //! 003 F-1): its owner; for a `team:<uuid>` binding, an active member of
    //! that team to read and an active owner or admin of it to manage; an
    //! operator, reading at every role and writing only as `aegis:admin` or
    //! `aegis:operator`. Everyone else is answered exactly as for a binding
    //! that does not exist. Driven through the daemon's real authentication
    //! stack against the real `StandardCredentialManagementService`, with
    //! in-memory binding and membership stores.

    use super::{credentials_by_id_router, CredentialsByIdState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, serve, tenant_user,
    };
    use aegis_orchestrator_core::application::credential_service::{
        CredentialActor, CredentialManagementService, OAuthProviderRegistry,
        StandardCredentialManagementService, StoreApiKeyCommand,
    };
    use aegis_orchestrator_core::domain::credential::{
        CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
        CredentialScope, CredentialType, GrantTarget, OAuthPendingState, UserCredentialBinding,
    };
    use aegis_orchestrator_core::domain::iam::{AegisRole, UserIdentity};
    use aegis_orchestrator_core::domain::repository::RepositoryError;
    use aegis_orchestrator_core::domain::secrets::SensitiveString;
    use aegis_orchestrator_core::domain::team::{
        Membership, MembershipRepository, MembershipRole, MembershipStatus, TeamId,
    };
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use aegis_orchestrator_core::infrastructure::secrets_manager::{
        SecretsManager, TestSecretStore,
    };
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[derive(Default)]
    struct InMemoryBindings {
        rows: RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>,
    }

    #[async_trait::async_trait]
    impl CredentialBindingRepository for InMemoryBindings {
        async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
            self.rows.write().await.insert(binding.id, binding.clone());
            Ok(())
        }
        async fn find_by_id(
            &self,
            id: &CredentialBindingId,
        ) -> anyhow::Result<Option<UserCredentialBinding>> {
            Ok(self.rows.read().await.get(id).cloned())
        }
        async fn find_by_owner(
            &self,
            tenant_id: &TenantId,
            owner_user_id: &str,
        ) -> anyhow::Result<Vec<UserCredentialBinding>> {
            Ok(self
                .rows
                .read()
                .await
                .values()
                .filter(|b| &b.tenant_id == tenant_id && b.owner_user_id == owner_user_id)
                .cloned()
                .collect())
        }
        async fn find_active_grants_for_target(
            &self,
            _tenant_id: &TenantId,
            _owner_user_id: &str,
            _provider: &CredentialProvider,
            _target: &GrantTarget,
        ) -> anyhow::Result<Vec<CredentialGrant>> {
            Ok(Vec::new())
        }
        async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
            self.rows.write().await.remove(id);
            Ok(())
        }
        async fn save_oauth_state(
            &self,
            _state: &str,
            _binding_id: &CredentialBindingId,
            _pkce_verifier: &str,
            _redirect_uri: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn find_oauth_state(
            &self,
            _state: &str,
        ) -> anyhow::Result<Option<OAuthPendingState>> {
            Ok(None)
        }
        async fn delete_oauth_state(&self, _state: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_expired_oauth_states(
            &self,
            _older_than: DateTime<Utc>,
        ) -> anyhow::Result<u64> {
            Ok(0)
        }
    }

    #[derive(Default)]
    struct InMemoryMemberships {
        rows: RwLock<Vec<Membership>>,
    }

    #[async_trait::async_trait]
    impl MembershipRepository for InMemoryMemberships {
        async fn save(&self, membership: &Membership) -> Result<(), RepositoryError> {
            self.rows.write().await.push(membership.clone());
            Ok(())
        }
        async fn find_by_team(&self, team_id: &TeamId) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .rows
                .read()
                .await
                .iter()
                .filter(|m| &m.team_id == team_id)
                .cloned()
                .collect())
        }
        async fn find_by_user(&self, user_id: &str) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .rows
                .read()
                .await
                .iter()
                .filter(|m| m.user_id == user_id)
                .cloned()
                .collect())
        }
        async fn find_active_for_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .find_by_user(user_id)
                .await?
                .into_iter()
                .filter(|m| m.status == MembershipStatus::Active)
                .collect())
        }
        async fn find_active_team_tenants_for_user(
            &self,
            _user_id: &str,
        ) -> Result<Vec<String>, RepositoryError> {
            Ok(Vec::new())
        }
        async fn is_active_member(
            &self,
            user_id: &str,
            team_id: &TeamId,
        ) -> Result<bool, RepositoryError> {
            Ok(self
                .find_by_team(team_id)
                .await?
                .iter()
                .any(|m| m.user_id == user_id && m.status == MembershipStatus::Active))
        }
        async fn count_active(&self, team_id: &TeamId) -> Result<u32, RepositoryError> {
            Ok(self
                .find_by_team(team_id)
                .await?
                .iter()
                .filter(|m| m.status == MembershipStatus::Active)
                .count() as u32)
        }
        async fn revoke(&self, _team_id: &TeamId, _user_id: &str) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    const OWNER: &str = "binding-owner-sub";
    const SCOPES: &str = "credential:read credential:delete credential:rotate credential:grant";

    struct Fixture {
        base: String,
        bindings: Arc<InMemoryBindings>,
        binding_id: CredentialBindingId,
        grant_id: String,
    }

    /// One binding owned by `OWNER`, scoped as `scope`, carrying one grant;
    /// the team (when there is one) has an active member and an active admin.
    async fn fixture(scope: CredentialScope, callers: &[(&str, UserIdentity)]) -> Fixture {
        let bindings = Arc::new(InMemoryBindings::default());
        let memberships = Arc::new(InMemoryMemberships::default());
        if let CredentialScope::Team { team_id } = scope {
            for (user, role) in [
                ("team-member-sub", MembershipRole::Member),
                ("team-admin-sub", MembershipRole::Admin),
            ] {
                memberships
                    .save(&Membership::new_active(TeamId(team_id), user.into(), role))
                    .await
                    .expect("seed membership");
            }
        }
        let event_bus = Arc::new(EventBus::new(16));
        let service = Arc::new(
            StandardCredentialManagementService::new(
                bindings.clone(),
                Arc::new(SecretsManager::from_store(
                    Arc::new(TestSecretStore::new()),
                    event_bus.clone(),
                )),
                event_bus,
                Arc::new(OAuthProviderRegistry::new()),
            )
            .with_membership_repo(memberships),
        );
        let owner_tenant = TenantId::for_consumer_user(OWNER).expect("owner tenant");
        let binding_id = service
            .store_api_key(StoreApiKeyCommand {
                owner_user_id: OWNER.into(),
                tenant_id: owner_tenant.clone(),
                provider: CredentialProvider::OpenAI,
                label: "owner's key".into(),
                scope,
                api_key_value: SensitiveString::new("sk-original"),
                credential_type: CredentialType::Secret,
            })
            .await
            .expect("seed binding");
        let grant_id = service
            .add_grant(
                &CredentialActor::User {
                    user_id: OWNER.into(),
                    tenant_id: owner_tenant,
                },
                &binding_id,
                GrantTarget::AllAgents,
                OWNER.into(),
            )
            .await
            .expect("seed grant")
            .to_string();
        let rows: Vec<(&str, UserIdentity, &str)> = callers
            .iter()
            .map(|(token, id)| (*token, id.clone(), SCOPES))
            .collect();
        let base = serve(
            credentials_by_id_router(CredentialsByIdState {
                credential_service: Some(service as Arc<dyn CredentialManagementService>),
            }),
            Some(identity_provider(&rows)),
            None,
        )
        .await;
        Fixture {
            base,
            bindings,
            binding_id,
            grant_id,
        }
    }

    /// The six by-id operations: (label, manages?, method, path suffix, body).
    fn operations() -> Vec<(
        &'static str,
        bool,
        reqwest::Method,
        String,
        Option<serde_json::Value>,
    )> {
        vec![
            (
                "read binding",
                false,
                reqwest::Method::GET,
                String::new(),
                None,
            ),
            (
                "list grants",
                false,
                reqwest::Method::GET,
                "/grants".into(),
                None,
            ),
            (
                "rotate",
                true,
                reqwest::Method::POST,
                "/rotate".into(),
                Some(serde_json::json!({ "value": "sk-attacker" })),
            ),
            (
                "add grant",
                true,
                reqwest::Method::POST,
                "/grants".into(),
                Some(serde_json::json!({ "target_type": "all_agents" })),
            ),
            (
                "revoke grant",
                true,
                reqwest::Method::DELETE,
                "/grants/{grant}".to_string(),
                None,
            ),
            (
                "revoke binding",
                true,
                reqwest::Method::DELETE,
                String::new(),
                None,
            ),
        ]
    }

    /// Run every operation as every caller on a fresh fixture and compare
    /// the answer with `expect(caller, manages)`: `true` means served,
    /// `false` means answered as for a binding that does not exist.
    async fn check(
        scope: CredentialScope,
        callers: &[(&str, UserIdentity, bool, bool)],
    ) -> Vec<String> {
        let mut failures = Vec::new();
        let tokens: Vec<(&str, UserIdentity)> = callers
            .iter()
            .map(|(t, id, _, _)| (*t, id.clone()))
            .collect();
        for (token, _, may_read, may_manage) in callers {
            for (label, manages, method, suffix, body) in operations() {
                let fx = fixture(scope.clone(), &tokens).await;
                let suffix = suffix.replace("{grant}", &fx.grant_id);
                let (status, answer) = send(
                    &fx.base,
                    &method,
                    &format!("/v1/credentials/{}{suffix}", fx.binding_id.0),
                    &body,
                    Some(token),
                )
                .await;
                let allowed = if manages { *may_manage } else { *may_read };
                let after = fx
                    .bindings
                    .find_by_id(&fx.binding_id)
                    .await
                    .expect("read store");
                if allowed {
                    if !(200..300).contains(&status) {
                        failures.push(format!(
                            "{label} by {token} answered {status} {answer}; expected success"
                        ));
                    }
                    continue;
                }
                // Refused: the same answer as for an id that was never issued,
                // and the binding untouched.
                let missing = uuid::Uuid::new_v4();
                let (m_status, m_answer) = send(
                    &fx.base,
                    &method,
                    &format!("/v1/credentials/{missing}{}", suffix),
                    &body,
                    Some(token),
                )
                .await;
                let normalise = |v: &serde_json::Value, id: &str| v.to_string().replace(id, "<id>");
                if status != m_status
                    || normalise(&answer, &fx.binding_id.0.to_string())
                        != normalise(&m_answer, &missing.to_string())
                {
                    failures.push(format!(
                        "{label} by {token} answered {status} {answer}, but a missing binding answers {m_status} {m_answer}"
                    ));
                }
                match after {
                    Some(b) if b.grants.len() == 1 => {}
                    other => failures.push(format!(
                        "{label} by {token} changed the binding: now {:?}",
                        other.map(|b| b.grants.len())
                    )),
                }
            }
        }
        failures
    }

    #[tokio::test]
    async fn credential_bindings_by_id_answer_outsiders_as_missing() {
        let personal = check(
            CredentialScope::Personal,
            &[
                ("another-consumer", consumer("other-sub"), false, false),
                (
                    "acme-tenant-user",
                    tenant_user("acme-sub", "acme"),
                    false,
                    false,
                ),
                (
                    "readonly-operator",
                    operator(AegisRole::Readonly),
                    true,
                    false,
                ),
            ],
        )
        .await;
        let team = check(
            CredentialScope::Team {
                team_id: uuid::Uuid::new_v4(),
            },
            &[
                ("non-member", consumer("outsider-sub"), false, false),
                ("team-member", consumer("team-member-sub"), true, false),
            ],
        )
        .await;
        let failures: Vec<String> = personal.into_iter().chain(team).collect();
        assert!(
            failures.is_empty(),
            "a caller reached a credential binding it does not own:\n{}",
            failures.join("\n")
        );
    }

    #[tokio::test]
    async fn credential_bindings_by_id_serve_owner_team_managers_and_writing_operators() {
        let personal = check(
            CredentialScope::Personal,
            &[
                ("owner", consumer(OWNER), true, true),
                ("admin-operator", operator(AegisRole::Admin), true, true),
                (
                    "operator-operator",
                    operator(AegisRole::Operator),
                    true,
                    true,
                ),
            ],
        )
        .await;
        let team = check(
            CredentialScope::Team {
                team_id: uuid::Uuid::new_v4(),
            },
            &[
                ("owner", consumer(OWNER), true, true),
                ("team-admin", consumer("team-admin-sub"), true, true),
            ],
        )
        .await;
        let failures: Vec<String> = personal.into_iter().chain(team).collect();
        assert!(
            failures.is_empty(),
            "a caller entitled to a credential binding was refused:\n{}",
            failures.join("\n")
        );
    }
}
