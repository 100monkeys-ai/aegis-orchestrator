// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! API key management handlers (ADR-093).
//!
//! Routes:
//!   GET    /v1/api-keys           — list keys for authenticated user
//!   POST   /v1/api-keys           — create a new key (returns key value once)
//!   DELETE /v1/api-keys/:id       — revoke a key
//!   POST   /v1/api-keys/validate  — validate a key and return user identity (MCP server auth)

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;
use chrono::{DateTime, Utc};
use rand::Rng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use aegis_orchestrator_core::domain::iam::{resolve_effective_tenant, IdentityKind, UserIdentity};
use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::CreateApiKeyRow;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::state::AppState;

// ── Request / Response DTOs ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CreateApiKeyRequest {
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Generate a new API key: `aegis_` prefix + 40 random alphanumeric chars.
fn generate_api_key() -> String {
    let mut rng = rand::rng();
    let suffix: String = (0..40)
        .map(|_| {
            let idx = rng.random_range(0..62u8);
            if idx < 10 {
                (b'0' + idx) as char
            } else if idx < 36 {
                (b'a' + idx - 10) as char
            } else {
                (b'A' + idx - 36) as char
            }
        })
        .collect();
    format!("aegis_{suffix}")
}

/// SHA-256 hex digest of the raw key value.
fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /v1/api-keys` — list API keys for the authenticated user.
pub(crate) async fn list_api_keys_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:list")?;
    let repo = match &state.api_key_repo {
        Some(r) => r.clone(),
        None => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "API key repository not configured"})),
            )
                .into_response());
        }
    };

    match repo.list_for_user(&identity.sub).await {
        Ok(rows) => {
            let keys: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "name": r.name,
                        "scopes": r.scopes,
                        "expires_at": r.expires_at,
                        "last_used_at": r.last_used_at,
                        "created_at": r.created_at,
                        "status": r.status,
                    })
                })
                .collect();
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "api_keys": keys })),
            )
                .into_response())
        }
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

/// `POST /v1/api-keys` — create a new API key.  The raw key value is returned
/// exactly once and is never stored.
pub(crate) async fn create_api_key_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
    Json(payload): Json<CreateApiKeyRequest>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:create")?;
    let repo = match &state.api_key_repo {
        Some(r) => r.clone(),
        None => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "API key repository not configured"})),
            )
                .into_response());
        }
    };

    let name_len = payload.name.len();
    if name_len == 0 || name_len > 64 {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name must be between 1 and 64 characters"})),
        )
            .into_response());
    }

    let key_value = generate_api_key();
    let key_hash = hash_key(&key_value);

    // Capture identity context at creation time so the validate endpoint
    // can return it without hitting the IAM provider.
    let tenant_id = resolve_effective_tenant(Some(&identity), None);
    let (aegis_role, zaru_tier) = match &identity.identity_kind {
        IdentityKind::Operator { aegis_role } => {
            (Some(aegis_role.as_claim_str().to_string()), None)
        }
        IdentityKind::ConsumerUser { zaru_tier, .. } => {
            (None, Some(zaru_tier.to_security_context_name().to_string()))
        }
        _ => (None, None),
    };

    let row = CreateApiKeyRow {
        id: Uuid::new_v4(),
        user_id: identity.sub.clone(),
        name: payload.name,
        key_hash,
        scopes: payload.scopes,
        expires_at: payload.expires_at,
        tenant_id: tenant_id.as_str().to_string(),
        aegis_role,
        zaru_tier,
    };

    match repo.create(&row).await {
        Ok(created) => Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": created.id,
                "name": created.name,
                "key": key_value,
                "scopes": created.scopes,
                "expires_at": created.expires_at,
                "created_at": created.created_at,
            })),
        )
            .into_response()),
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

/// `DELETE /v1/api-keys/:id` — revoke an API key.
pub(crate) async fn revoke_api_key_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:revoke")?;
    let repo = match &state.api_key_repo {
        Some(r) => r.clone(),
        None => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "API key repository not configured"})),
            )
                .into_response());
        }
    };

    match repo.revoke(id, &identity.sub).await {
        Ok(true) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({"message": "API key revoked"})),
        )
            .into_response()),
        Ok(false) => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "API key not found or already revoked"})),
        )
            .into_response()),
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

/// `POST /v1/api-keys/validate` — Validate an API key and return the associated
/// user identity. Called by the MCP server to authenticate external clients.
///
/// The raw API key is passed in the `Authorization: Bearer aegis_xxx` header.
/// This endpoint is exempt from IAM/OIDC auth (it IS the auth mechanism for
/// API-key-based clients).
pub(crate) async fn validate_api_key_handler(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> axum::response::Response {
    // Extract Bearer token from Authorization header
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match token {
        Some(t) if t.starts_with("aegis_") => t,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Missing or invalid API key"})),
            )
                .into_response()
        }
    };

    let repo = match &state.api_key_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "API key repository not configured"})),
            )
                .into_response()
        }
    };

    let key_hash = hash_key(token);

    match repo.find_by_key_hash(&key_hash).await {
        Ok(Some(row)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "user_id": row.user_id,
                "tenant_id": row.tenant_id,
                "aegis_role": row.aegis_role,
                "scopes": row.scopes,
                "zaru_tier": row.zaru_tier,
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid or expired API key"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
