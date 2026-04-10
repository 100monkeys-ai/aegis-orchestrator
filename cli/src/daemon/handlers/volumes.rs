// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Volume management handlers (Gap 079-7): user-facing CRUD + file operations.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::application::file_operations_service::FileOperationsError;
use aegis_orchestrator_core::application::user_volume_service::UserVolumeError;
use aegis_orchestrator_core::application::volume_manager::CreateUserVolumeCommand;
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::volume::VolumeId;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

// ============================================================================
// Request / response types
// ============================================================================

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct CreateVolumeRequest {
    pub label: String,
    pub size_limit_bytes: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct RenameVolumeRequest {
    pub label: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct FilePathQuery {
    pub path: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct MovePathRequest {
    pub from: String,
    pub to: String,
}

// ============================================================================
// Error mapping helpers
// ============================================================================

fn user_volume_error_response(e: UserVolumeError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match &e {
        UserVolumeError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        UserVolumeError::Unauthorized => (StatusCode::FORBIDDEN, e.to_string()),
        UserVolumeError::VolumeCountQuotaExceeded | UserVolumeError::StorageQuotaExceeded => {
            (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        UserVolumeError::DuplicateName(_) => (StatusCode::CONFLICT, e.to_string()),
        UserVolumeError::VolumeAttached => (StatusCode::CONFLICT, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (status, Json(serde_json::json!({"error": message})))
}

fn file_ops_error_response(e: FileOperationsError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match &e {
        FileOperationsError::NotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        FileOperationsError::Unauthorized => (StatusCode::FORBIDDEN, e.to_string()),
        FileOperationsError::FileTooLarge => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        FileOperationsError::InvalidPath(_) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (status, Json(serde_json::json!({"error": message})))
}

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
// Handlers
// ============================================================================

/// POST /v1/volumes
pub(crate) async fn create_volume(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<CreateVolumeRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let tier = user_tier(identity_ref);

    let cmd = CreateUserVolumeCommand {
        tenant_id,
        owner_user_id: owner,
        label: body.label,
        size_limit_bytes: body.size_limit_bytes,
        zaru_tier: tier,
    };

    state
        .user_volume_service
        .create_volume(cmd)
        .await
        .map(|vol| {
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "id": vol.id.to_string(),
                    "name": vol.name,
                    "status": format!("{:?}", vol.status),
                    "size_limit_bytes": vol.size_limit_bytes,
                    "created_at": vol.created_at,
                })),
            )
        })
        .map_err(user_volume_error_response)
}

/// GET /v1/volumes
pub(crate) async fn list_volumes(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);

    state
        .user_volume_service
        .list_volumes(&tenant_id, &owner)
        .await
        .map(|vols| {
            Json(
                vols.into_iter()
                    .map(|v| {
                        serde_json::json!({
                            "id": v.id.to_string(),
                            "name": v.name,
                            "status": format!("{:?}", v.status),
                            "size_limit_bytes": v.size_limit_bytes,
                            "created_at": v.created_at,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .map_err(user_volume_error_response)
}

/// GET /v1/volumes/quota
pub(crate) async fn get_quota(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let tier = user_tier(identity_ref);

    state
        .user_volume_service
        .get_quota_usage(&tenant_id, &owner, &tier)
        .await
        .map(|usage| {
            Json(serde_json::json!({
                "volume_count": usage.volume_count,
                "total_bytes_used": usage.total_bytes_used,
                "max_volumes": usage.tier_limit.max_volumes,
                "total_storage_bytes": usage.tier_limit.total_storage_bytes,
                "max_file_size_bytes": usage.tier_limit.max_file_size_bytes,
            }))
        })
        .map_err(user_volume_error_response)
}

/// GET /v1/volumes/:id
pub(crate) async fn get_volume(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);

    let vols = state
        .user_volume_service
        .list_volumes(&tenant_id, &owner)
        .await
        .map_err(user_volume_error_response)?;

    let vol_id = VolumeId(id);
    vols.into_iter()
        .find(|v| v.id == vol_id)
        .map(|v| {
            Json(serde_json::json!({
                "id": v.id.to_string(),
                "name": v.name,
                "status": format!("{:?}", v.status),
                "size_limit_bytes": v.size_limit_bytes,
                "created_at": v.created_at,
            }))
        })
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "volume not found"})),
            )
        })
}

/// PATCH /v1/volumes/:id
pub(crate) async fn rename_volume(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RenameVolumeRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    state
        .user_volume_service
        .rename_volume(&vol_id, &owner, &body.label)
        .await
        .map(|_| Json(serde_json::json!({"success": true})))
        .map_err(user_volume_error_response)
}

/// DELETE /v1/volumes/:id
pub(crate) async fn delete_volume(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    state
        .user_volume_service
        .delete_volume(&vol_id, &owner)
        .await
        .map(|_| Json(serde_json::json!({"success": true})))
        .map_err(user_volume_error_response)
}

/// GET /v1/volumes/:id/files  (list directory)
pub(crate) async fn list_files(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Query(params): Query<FilePathQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    state
        .file_operations_service
        .list_directory(&vol_id, &owner, &params.path)
        .await
        .map(|entries| Json(serde_json::to_value(entries).unwrap_or(serde_json::json!([]))))
        .map_err(file_ops_error_response)
}

/// GET /v1/volumes/:id/files/download
pub(crate) async fn download_file(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Query(params): Query<FilePathQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    let content = state
        .file_operations_service
        .read_file(&vol_id, &owner, &params.path)
        .await
        .map_err(file_ops_error_response)?;

    Ok((
        [(axum::http::header::CONTENT_TYPE, content.content_type)],
        content.data,
    )
        .into_response())
}

/// POST /v1/volumes/:id/files/upload
pub(crate) async fn upload_file(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Query(params): Query<FilePathQuery>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let tier = user_tier(identity_ref);
    let vol_id = VolumeId(id);

    // Determine per-tier file size limit
    let tier_limits = aegis_orchestrator_core::domain::volume::StorageTierLimits::default();
    let max_file_size = tier_limits
        .limits
        .get(&tier)
        .map(|l| l.max_file_size_bytes)
        .unwrap_or(50 * 1024 * 1024);

    state
        .file_operations_service
        .write_file(&vol_id, &owner, &params.path, &body, max_file_size)
        .await
        .map(|_| Json(serde_json::json!({"success": true})))
        .map_err(file_ops_error_response)
}

/// DELETE /v1/volumes/:id/files
pub(crate) async fn delete_path(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Query(params): Query<FilePathQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    state
        .file_operations_service
        .delete_path(&vol_id, &owner, &params.path)
        .await
        .map(|_| Json(serde_json::json!({"success": true})))
        .map_err(file_ops_error_response)
}

/// POST /v1/volumes/:id/files/mkdir
pub(crate) async fn mkdir(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Query(params): Query<FilePathQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    state
        .file_operations_service
        .create_directory(&vol_id, &owner, &params.path)
        .await
        .map(|_| Json(serde_json::json!({"success": true})))
        .map_err(file_ops_error_response)
}

/// POST /v1/volumes/:id/files/move
pub(crate) async fn move_path(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<MovePathRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let owner = user_sub(identity_ref);
    let vol_id = VolumeId(id);

    state
        .file_operations_service
        .move_path(&vol_id, &owner, &body.from, &body.to)
        .await
        .map(|_| Json(serde_json::json!({"success": true})))
        .map_err(file_ops_error_response)
}
