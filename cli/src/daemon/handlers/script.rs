// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Script REST Handlers (BC-7, ADR-110 §D7)
//!
//! HTTP handlers for the `/v1/scripts/*` surface.
//!
//! | Endpoint | Scope | Notes |
//! |---|---|---|
//! | `POST   /v1/scripts` | `script:write` | Create script at version 1 |
//! | `GET    /v1/scripts` | `script:read`  | List caller's active scripts |
//! | `GET    /v1/scripts/:id` | `script:read` | Detail + version history (404 on non-owner) |
//! | `PUT    /v1/scripts/:id` | `script:write` | Update + bump version |
//! | `DELETE /v1/scripts/:id` | `script:write` | Soft-delete |
//!
//! All JSON endpoints require Keycloak JWT. Handlers themselves do no
//! business logic — they translate JSON into command structs and defer
//! to [`ScriptService`].

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use uuid::Uuid;

use aegis_orchestrator_core::application::script_service::{
    CreateScriptCommand, ScriptService, ScriptServiceError, UpdateScriptCommand,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::script::{Script, ScriptId};
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

// ============================================================================
// Request / response DTOs
// ============================================================================

/// Body shape for `POST /v1/scripts` and `PUT /v1/scripts/:id`.
///
/// A single DTO is used for both create + update because the shape is
/// identical. `visibility` is accepted but must be `"private"` — any
/// other value returns `400 Bad Request` (only Private is implemented in
/// this wave per ADR-110 §D7).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ScriptRequestBody {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) code: String,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    /// Optional. Defaults to `"private"` when absent. Any other value
    /// is rejected with `400 Bad Request` until the marketplace ADR
    /// defines wider semantics.
    #[serde(default)]
    pub(crate) visibility: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
pub(crate) struct ListScriptsQuery {
    /// Optional filter: only return scripts whose `tags` array contains
    /// this tag. Server-side filter to keep round-trips small when the
    /// client has many scripts.
    #[serde(default)]
    pub(crate) tag: Option<String>,
    /// Optional case-insensitive substring match over `name` and
    /// `description`.
    #[serde(default)]
    pub(crate) q: Option<String>,
}

// ============================================================================
// Error mapping
// ============================================================================

fn script_error_response(e: ScriptServiceError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match &e {
        ScriptServiceError::NotFound => (StatusCode::NOT_FOUND, e.to_string()),
        ScriptServiceError::DuplicateName => (StatusCode::CONFLICT, e.to_string()),
        ScriptServiceError::TierLimitExceeded { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        ScriptServiceError::Domain(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        ScriptServiceError::Repository(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (status, Json(json!({ "error": message })))
}

fn visibility_error(raw: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": format!(
                "script visibility '{raw}' is not supported yet; only 'private' is accepted"
            )
        })),
    )
}

// ============================================================================
// Identity helpers
// ============================================================================

fn user_tier(identity: Option<&UserIdentity>) -> ZaruTier {
    match identity.map(|i| &i.identity_kind) {
        Some(IdentityKind::ConsumerUser { zaru_tier, .. }) => zaru_tier.clone(),
        _ => ZaruTier::Enterprise,
    }
}

fn user_sub(identity: Option<&UserIdentity>) -> String {
    identity
        .map(|i| i.sub.clone())
        .unwrap_or_else(|| "anonymous".to_string())
}

// ============================================================================
// Serialization — clean DTO without `deleted_at` / `domain_events`.
// ============================================================================

fn script_dto(s: &Script) -> serde_json::Value {
    json!({
        "id": s.id.0,
        "tenant_id": s.tenant_id,
        "created_by": s.created_by,
        "name": s.name,
        "description": s.description,
        "code": s.code,
        "tags": s.tags,
        "visibility": s.visibility.as_str(),
        "version": s.version,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

// ============================================================================
// Service access
// ============================================================================

fn script_service(
    state: &AppState,
) -> Result<Arc<ScriptService>, (StatusCode, Json<serde_json::Value>)> {
    state.script_service.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "script service not configured"})),
    ))
}

fn require_private_visibility(
    raw: Option<&str>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match raw {
        None => Ok(()),
        Some(v) if v.eq_ignore_ascii_case("private") => Ok(()),
        Some(other) => Err(visibility_error(other)),
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// `POST /v1/scripts` — create a new script at version 1.
pub(crate) async fn create_script(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<ScriptRequestBody>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("script:write")?;
    require_private_visibility(body.visibility.as_deref())?;

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let created_by = user_sub(identity_ref);
    let tier = user_tier(identity_ref);
    let svc = script_service(&state)?;

    let cmd = CreateScriptCommand {
        tenant_id,
        created_by,
        zaru_tier: tier,
        name: body.name,
        description: body.description,
        code: body.code,
        tags: body.tags,
    };

    let script = svc.create(cmd).await.map_err(script_error_response)?;
    Ok((StatusCode::CREATED, Json(script_dto(&script))))
}

/// `GET /v1/scripts` — list caller's active scripts with optional
/// server-side filtering by `tag` and `q` (substring).
pub(crate) async fn list_scripts(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Query(query): Query<ListScriptsQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("script:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let created_by = user_sub(identity_ref);
    let svc = script_service(&state)?;

    let mut scripts = svc
        .list(&tenant_id, &created_by)
        .await
        .map_err(script_error_response)?;

    if let Some(tag) = query.tag.as_deref() {
        scripts.retain(|s| s.tags.iter().any(|t| t == tag));
    }
    if let Some(q) = query.q.as_deref() {
        let needle = q.to_ascii_lowercase();
        scripts.retain(|s| {
            s.name.to_ascii_lowercase().contains(&needle)
                || s.description.to_ascii_lowercase().contains(&needle)
        });
    }

    Ok(Json(scripts.iter().map(script_dto).collect::<Vec<_>>()))
}

/// `GET /v1/scripts/:id` — script detail plus version history.
///
/// Returns `404 Not Found` when the script exists but is owned by
/// another user (never `403`, to avoid leaking existence).
pub(crate) async fn get_script(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("script:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let created_by = user_sub(identity_ref);
    let svc = script_service(&state)?;

    let script_id = ScriptId(id);
    let script = svc
        .get(&script_id, &tenant_id, &created_by)
        .await
        .map_err(script_error_response)?;

    let versions = svc
        .list_versions(&script_id, &tenant_id, &created_by)
        .await
        .map_err(script_error_response)?;

    let mut body = script_dto(&script);
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "versions".to_string(),
            json!(versions
                .iter()
                .map(|v| json!({
                    "version": v.version,
                    "updated_at": v.updated_at,
                    "updated_by": v.updated_by,
                }))
                .collect::<Vec<_>>()),
        );
    }

    Ok(Json(body))
}

/// `PUT /v1/scripts/:id` — update + bump the version counter.
pub(crate) async fn update_script(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<ScriptRequestBody>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("script:write")?;
    require_private_visibility(body.visibility.as_deref())?;

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let created_by = user_sub(identity_ref);
    let svc = script_service(&state)?;

    let cmd = UpdateScriptCommand {
        name: body.name,
        description: body.description,
        code: body.code,
        tags: body.tags,
    };

    let script = svc
        .update(&ScriptId(id), &tenant_id, &created_by, cmd)
        .await
        .map_err(script_error_response)?;

    Ok(Json(script_dto(&script)))
}

/// `DELETE /v1/scripts/:id` — soft-delete.
pub(crate) async fn delete_script(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("script:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let created_by = user_sub(identity_ref);
    let svc = script_service(&state)?;

    svc.delete(&ScriptId(id), &tenant_id, &created_by)
        .await
        .map_err(script_error_response)?;

    Ok(StatusCode::NO_CONTENT)
}
