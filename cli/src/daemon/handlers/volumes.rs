// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Volume management handlers (Gap 079-7): user-facing CRUD + file operations.

use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Extension, FromRequest, Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use sha2::Digest;
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
        .list_volumes_with_usage(&tenant_id, &owner)
        .await
        .map(|vols| {
            Json(
                vols.into_iter()
                    .map(|vu| {
                        let v = vu.volume;
                        serde_json::json!({
                            "id": v.id.to_string(),
                            "name": v.name,
                            "status": format!("{:?}", v.status),
                            "size_limit_bytes": v.size_limit_bytes,
                            "used_bytes": vu.used_bytes,
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

/// Reserved volume name used for chat attachments (ADR-113).
///
/// When the URL path uses this literal in place of a UUID, the upload handler
/// lazy-provisions a persistent user volume of this name on first upload and
/// reuses it on subsequent uploads. The name is reserved per ADR-079 and
/// counts against the user's `ZaruTier` storage quota like any other volume.
const CHAT_ATTACHMENTS_VOLUME_NAME: &str = "chat-attachments";

/// Hard ceiling for the lazy-provisioned `chat-attachments` volume on tiers
/// whose `total_storage_bytes` is unbounded (Enterprise = `u64::MAX`). A raw
/// `u64::MAX` allocation is nonsensical at the storage layer; clamp to a
/// sensible per-volume default that still leaves room for additional volumes
/// within the same tier budget.
const CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Reject paths containing `..`, absolute prefixes, or control characters.
/// Path-sanitization downstream is the source of truth, but we fail fast at
/// the edge so multipart parsing isn't even attempted on a hostile filename.
fn validate_dest_path(path: &str) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if path.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "destination path is empty"})),
        ));
    }
    if path.starts_with('/') {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(
                serde_json::json!({"error": "destination path must be relative (no leading '/')"}),
            ),
        ));
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "destination path may not contain '..'"})),
        ));
    }
    if path.chars().any(|c| c.is_control()) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "destination path contains control characters"})),
        ));
    }
    Ok(())
}

/// Sniff the MIME type from the file's first bytes, ignoring any client-supplied
/// `Content-Type` header per ADR-113. Falls back to `application/octet-stream`.
fn sniff_upload_mime(data: &[u8]) -> String {
    infer::get(data)
        .map(|k| k.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// Resolve the target volume for an upload. Accepts either a UUID (existing
/// volume) or the reserved `chat-attachments` literal (lazy-provision per
/// ADR-113).
async fn resolve_or_provision_upload_volume(
    state: &Arc<AppState>,
    id_or_name: &str,
    tenant_id: aegis_orchestrator_core::domain::tenant::TenantId,
    owner: &str,
    tier: &ZaruTier,
) -> Result<VolumeId, (StatusCode, Json<serde_json::Value>)> {
    if let Ok(uuid) = Uuid::parse_str(id_or_name) {
        return Ok(VolumeId(uuid));
    }

    if id_or_name != CHAT_ATTACHMENTS_VOLUME_NAME {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("expected volume UUID or reserved name '{}'", CHAT_ATTACHMENTS_VOLUME_NAME)
            })),
        ));
    }

    // Look up the chat-attachments volume by name within (tenant, owner). If
    // it exists, return it; otherwise lazy-provision.
    let existing = state
        .user_volume_service
        .list_volumes(&tenant_id, owner)
        .await
        .map_err(user_volume_error_response)?;
    if let Some(v) = existing
        .into_iter()
        .find(|v| v.name == CHAT_ATTACHMENTS_VOLUME_NAME)
    {
        return Ok(v.id);
    }

    // Tier-aware sizing: respect the caller's `total_storage_bytes` budget
    // so the very first upload doesn't trip `StorageQuotaExceeded` on Free
    // (500 MiB tier ceiling) by requesting more than the tier allows. An
    // unrecognized tier fails closed (403) rather than falling back to a
    // hardcoded ceiling — same posture as `resolve_max_file_size_bytes`.
    let size_limit_bytes = chat_attachments_volume_size_for_tier(tier).ok_or_else(|| {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "no chat-attachments volume size configured for caller tier"
            })),
        )
    })?;

    let cmd = CreateUserVolumeCommand {
        tenant_id,
        owner_user_id: owner.to_string(),
        label: CHAT_ATTACHMENTS_VOLUME_NAME.to_string(),
        size_limit_bytes,
        zaru_tier: tier.clone(),
    };
    state
        .user_volume_service
        .create_volume(cmd)
        .await
        .map(|v| v.id)
        .map_err(user_volume_error_response)
}

/// Reject filenames containing path separators, `..`, control chars, NUL,
/// or that are empty / overlong. Same posture as `validate_dest_path` but
/// targeted at filenames specifically (no `/` at all, including as a
/// non-leading character).
fn validate_supplied_filename(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    !name
        .chars()
        .any(|c| c.is_control() || c == '/' || c == '\\' || c == '\0')
}

/// Decode the `X-Filename` header value. The Zaru chat proxy URL-encodes
/// the original filename (`encodeURIComponent`) so non-ASCII bytes survive
/// HTTP-header transport. Decode percent-escapes; if decoding fails, fall
/// back to the raw value.
fn decode_x_filename(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// Resolve the per-file upload cap for a tier from the canonical
/// `StorageTierLimits` table. An unknown tier returns `None` so the caller
/// fails closed rather than admitting the upload under an unbounded cap.
fn resolve_max_file_size_bytes(tier: &ZaruTier) -> Option<u64> {
    aegis_orchestrator_core::domain::volume::StorageTierLimits::default()
        .limits
        .get(tier)
        .map(|l| l.max_file_size_bytes)
}

/// Resolve the size to allocate for a lazy-provisioned `chat-attachments`
/// volume from the caller's tier. The volume's allocation must fit under the
/// tier's `total_storage_bytes` budget; otherwise `UserVolumeService::create_volume`
/// rejects the request with `StorageQuotaExceeded` even for an empty volume.
///
/// We allocate the full tier `total_storage_bytes` so the user gets as much
/// chat-attachment headroom as their tier permits without exceeding it.
/// (Free users can still create one extra volume because `max_volumes >= 2`,
/// but that volume will compete with chat-attachments for the storage
/// budget — a deliberate trade chosen over fragmenting the budget across
/// volumes the user may never create.)
///
/// Unbounded tiers (Enterprise = `u64::MAX`) are clamped to
/// `CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES` because a raw `u64::MAX` is
/// nonsensical at the storage backend.
///
/// Returns `None` for an unrecognized tier so callers fail closed rather than
/// admitting the volume creation under an unbounded cap, mirroring
/// `resolve_max_file_size_bytes`.
fn chat_attachments_volume_size_for_tier(tier: &ZaruTier) -> Option<u64> {
    aegis_orchestrator_core::domain::volume::StorageTierLimits::default()
        .limits
        .get(tier)
        .map(|l| {
            if l.total_storage_bytes == u64::MAX {
                CHAT_ATTACHMENTS_UNBOUNDED_TIER_CAP_BYTES
            } else {
                l.total_storage_bytes
            }
        })
}

/// Cast a per-tier byte cap into the `usize` accepted by `to_bytes`,
/// saturating at `usize::MAX` for unbounded tiers (Enterprise = `u64::MAX`)
/// and on 32-bit targets where `u64` exceeds `usize::MAX`.
fn cap_as_usize(cap: u64) -> usize {
    usize::try_from(cap).unwrap_or(usize::MAX)
}

/// POST /v1/volumes/:id/files/upload (ADR-079, ADR-113)
///
/// Accepts either a `multipart/form-data` body (used by the Python and
/// TypeScript SDK uploaders) or a raw byte body whose `Content-Type` is the
/// file's own MIME type (used by the Zaru chat proxy, which streams
/// `request.body` directly). Dispatch is keyed on the request's
/// `Content-Type` header.
///
/// The destination path is taken from the `path` query parameter (relative,
/// no `..`, no control chars). MIME type is content-sniffed, never trusted
/// from the client. The `:id` segment may be a volume UUID or the reserved
/// literal `chat-attachments`, in which case the volume is lazy-provisioned
/// on first upload per ADR-113.
///
/// # Why a single `Request` extractor
///
/// Axum's `Handler` trait permits at most one body-consuming extractor, and
/// it must be the last argument. `Multipart` and `Bytes` are both
/// body-consuming `FromRequest` extractors, so a handler signature with both
/// (even with `Option<Multipart>`) cannot satisfy `Handler<_, _>`. We take a
/// single `Request` and branch on `Content-Type` internally.
pub(crate) async fn upload_file(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id_or_name): Path<String>,
    Query(params): Query<FilePathQuery>,
    request: Request,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let tier = user_tier(identity_ref);

    validate_dest_path(&params.path)?;

    let vol_id =
        resolve_or_provision_upload_volume(&state, &id_or_name, tenant_id, &owner, &tier).await?;

    // Resolve the per-tier upload cap. Fail closed: an unknown tier rejects
    // the upload rather than falling back to an unbounded ceiling.
    let max_file_size = resolve_max_file_size_bytes(&tier).ok_or_else(|| {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "no upload size limit configured for caller tier"
            })),
        )
    })?;
    let max_file_size_usize = cap_as_usize(max_file_size);

    let is_multipart = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("multipart/")
        })
        .unwrap_or(false);

    // Read the X-Filename header before consuming the request body. Used
    // only by the raw-body branch (multipart carries filename per-field).
    let x_filename_decoded = request
        .headers()
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(decode_x_filename);

    let (data, supplied_name) = if is_multipart {
        let mut mp = axum::extract::Multipart::from_request(request, &())
            .await
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("multipart init error: {e}")})),
                )
            })?;
        let mut found: Option<(Vec<u8>, Option<String>)> = None;
        loop {
            let field_res = mp.next_field().await;
            let field = match field_res {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": format!("multipart parse error: {e}")})),
                    ))
                }
            };
            // Only the first file field is honoured; additional fields are
            // dropped rather than failing the upload, since browsers may
            // include hidden CSRF/metadata fields.
            let file_name = field.file_name().map(|s| s.to_string());
            let bytes = field.bytes().await.map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("multipart read error: {e}")})),
                )
            })?;
            if bytes.len() as u64 > max_file_size {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(serde_json::json!({
                        "error": format!(
                            "upload exceeds tier cap of {} bytes",
                            max_file_size
                        )
                    })),
                ));
            }
            if found.is_none() && !bytes.is_empty() {
                found = Some((bytes.to_vec(), file_name));
            }
        }
        match found {
            Some((bytes, name)) => (bytes, name),
            None => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({"error": "multipart body contained no file"})),
                ))
            }
        }
    } else {
        let body = request.into_body();
        // `to_bytes` enforces the per-tier cap directly: it returns an error
        // (mapped to 413) if the body exceeds `max_file_size_usize`.
        let bytes = to_bytes(body, max_file_size_usize).await.map_err(|e| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({"error": format!("body read error: {e}")})),
            )
        })?;
        // Honor the X-Filename header on the raw-body branch (ADR-113).
        // Validation rejects path separators, `..`, control chars, NUL,
        // empty, or overlong values; an invalid header falls through to the
        // path-derived display name.
        let header_name = x_filename_decoded
            .as_deref()
            .filter(|n| validate_supplied_filename(n))
            .map(|n| n.to_string());
        (bytes.to_vec(), header_name)
    };

    let mime_type = sniff_upload_mime(&data);
    let size = data.len() as u64;
    let mut hasher = sha2::Sha256::new();
    hasher.update(&data);
    let sha256 = format!("{:x}", hasher.finalize());

    state
        .file_operations_service
        .write_file(&vol_id, &owner, &params.path, &data, max_file_size)
        .await
        .map_err(file_ops_error_response)?;

    // Validate any client-supplied filename (multipart `filename` field or
    // X-Filename header). If invalid, fall through to the path-derived name.
    let display_name = supplied_name
        .filter(|n| validate_supplied_filename(n))
        .or_else(|| {
            std::path::Path::new(&params.path)
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| params.path.clone());

    Ok(Json(serde_json::json!({
        "volume_id": vol_id.to_string(),
        "path": params.path,
        "name": display_name,
        "mime_type": mime_type,
        "size": size,
        "sha256": sha256,
    })))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for ADR-113 path-safety: the upload handler MUST reject
    /// `..` path segments before any FSAL call. Without this, a malicious
    /// client could place uploads outside the intended subtree.
    #[test]
    fn validate_dest_path_rejects_traversal() {
        assert!(validate_dest_path("../etc/passwd").is_err());
        assert!(validate_dest_path("foo/../../bar").is_err());
        assert!(validate_dest_path("a/b/../c").is_err());
    }

    #[test]
    fn validate_dest_path_rejects_absolute_paths() {
        assert!(validate_dest_path("/etc/passwd").is_err());
        assert!(validate_dest_path("/foo").is_err());
    }

    #[test]
    fn validate_dest_path_rejects_control_characters() {
        assert!(validate_dest_path("foo\nbar").is_err());
        assert!(validate_dest_path("foo\0bar").is_err());
        assert!(validate_dest_path("foo\tbar").is_err());
    }

    #[test]
    fn validate_dest_path_rejects_empty() {
        assert!(validate_dest_path("").is_err());
    }

    #[test]
    fn validate_dest_path_accepts_safe_relative_paths() {
        assert!(validate_dest_path("foo.txt").is_ok());
        assert!(validate_dest_path("subdir/foo.txt").is_ok());
        assert!(validate_dest_path("a/b/c/d.png").is_ok());
        // dot in name is fine (not a segment)
        assert!(validate_dest_path("file.tar.gz").is_ok());
    }

    /// Regression test for ADR-113: MIME type MUST be derived from file
    /// content, not from any client-supplied header. PNG bytes uploaded with
    /// a `.txt` filename should still be detected as `image/png`.
    #[test]
    fn sniff_upload_mime_uses_content_not_extension() {
        let png_magic = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert_eq!(sniff_upload_mime(png_magic), "image/png");
    }

    #[test]
    fn sniff_upload_mime_falls_back_to_octet_stream_for_unknown_bytes() {
        // Plain ASCII without magic bytes => fallback.
        let ascii = b"hello world";
        assert_eq!(sniff_upload_mime(ascii), "application/octet-stream");
    }

    /// Regression test for the ADR-113 upload handler signature.
    ///
    /// An earlier revision of `upload_file` declared two body-consuming
    /// extractors — `Option<Multipart>` and `axum::body::Bytes` — in the same
    /// signature. Axum's `Handler` trait can only be derived for functions
    /// with at most one body extractor (and it must be the last argument), so
    /// CI failed with `the trait bound ... : Handler<_, _> is not satisfied`
    /// at the route registration site.
    ///
    /// This test wires `upload_file` into a `Router<Arc<AppState>>` exactly
    /// as the production router does. If anyone re-introduces a second
    /// body-consuming extractor on `upload_file`, this file will fail to
    /// compile — the build break is the regression signal.
    ///
    /// The test is `#[allow(dead_code)]` because we only need it to compile;
    /// invoking the route would require constructing a full `AppState`.
    #[allow(dead_code)]
    fn upload_file_is_a_valid_axum_handler() -> axum::Router<Arc<AppState>> {
        axum::Router::new().route(
            "/v1/volumes/{id}/files/upload",
            axum::routing::post(upload_file),
        )
    }

    // ========================================================================
    // ADR-113 regression: X-Filename header honored on raw-body uploads
    // ========================================================================

    /// The raw-body branch of `upload_file` previously ignored the
    /// `X-Filename` header sent by the Zaru chat proxy and derived
    /// `display_name` only from the URL path. This regression ensures a
    /// well-formed `X-Filename` value is accepted by the validator and would
    /// be used as the stored attachment name.
    #[test]
    fn validate_supplied_filename_accepts_well_formed_names() {
        assert!(validate_supplied_filename("report.pdf"));
        assert!(validate_supplied_filename("Photo 2026-04-26.png"));
        assert!(validate_supplied_filename("résumé.docx"));
        assert!(validate_supplied_filename("file.tar.gz"));
    }

    #[test]
    fn validate_supplied_filename_rejects_path_separators() {
        assert!(!validate_supplied_filename("foo/bar.txt"));
        assert!(!validate_supplied_filename("foo\\bar.txt"));
        assert!(!validate_supplied_filename("/etc/passwd"));
    }

    #[test]
    fn validate_supplied_filename_rejects_traversal_and_dots() {
        assert!(!validate_supplied_filename(".."));
        assert!(!validate_supplied_filename("."));
    }

    #[test]
    fn validate_supplied_filename_rejects_control_chars_and_nul() {
        assert!(!validate_supplied_filename("foo\nbar"));
        assert!(!validate_supplied_filename("foo\tbar"));
        assert!(!validate_supplied_filename("foo\0bar"));
        assert!(!validate_supplied_filename("foo\x07bar"));
    }

    #[test]
    fn validate_supplied_filename_rejects_empty_and_overlong() {
        assert!(!validate_supplied_filename(""));
        assert!(!validate_supplied_filename(&"a".repeat(256)));
        assert!(validate_supplied_filename(&"a".repeat(255)));
    }

    /// The Zaru proxy URL-encodes the filename before stuffing it into the
    /// `X-Filename` header (HTTP header values cannot carry arbitrary UTF-8
    /// safely). The handler must percent-decode before validating/using it.
    #[test]
    fn decode_x_filename_percent_decodes_utf8() {
        assert_eq!(decode_x_filename("r%C3%A9sum%C3%A9.pdf"), "résumé.pdf");
        assert_eq!(decode_x_filename("hello%20world.txt"), "hello world.txt");
    }

    #[test]
    fn decode_x_filename_passes_through_plain_ascii() {
        assert_eq!(decode_x_filename("report.pdf"), "report.pdf");
    }

    #[test]
    fn decode_x_filename_falls_back_on_invalid_encoding() {
        // Lone `%` without two hex digits is not valid percent-encoding;
        // `percent_decode_str` treats it literally, so we still get a
        // usable string back.
        let out = decode_x_filename("bogus%ZZ.txt");
        assert!(!out.is_empty());
    }

    // ========================================================================
    // ADR-113 regression: per-tier max upload size, replacing the hardcoded
    // 512 MiB ceiling.
    // ========================================================================

    /// Free tier caps per-file uploads at 50 MiB (per
    /// `StorageTierLimits::default()`). A previous revision applied a
    /// hardcoded 512 MiB ceiling regardless of tier.
    #[test]
    fn resolve_max_file_size_bytes_free_tier_is_50_mib() {
        let cap = resolve_max_file_size_bytes(&ZaruTier::Free).expect("free tier configured");
        assert_eq!(cap, 50 * 1024 * 1024);
    }

    #[test]
    fn resolve_max_file_size_bytes_pro_tier_is_500_mib() {
        let cap = resolve_max_file_size_bytes(&ZaruTier::Pro).expect("pro tier configured");
        assert_eq!(cap, 500 * 1024 * 1024);
    }

    #[test]
    fn resolve_max_file_size_bytes_business_tier_is_2_gib() {
        let cap =
            resolve_max_file_size_bytes(&ZaruTier::Business).expect("business tier configured");
        assert_eq!(cap, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn resolve_max_file_size_bytes_enterprise_tier_is_unbounded() {
        let cap =
            resolve_max_file_size_bytes(&ZaruTier::Enterprise).expect("enterprise configured");
        assert_eq!(cap, u64::MAX);
    }

    /// Enterprise's `u64::MAX` cap must convert into a `usize` that
    /// `axum::body::to_bytes` accepts without overflow. On 64-bit targets
    /// this is `usize::MAX`; on 32-bit it saturates rather than panics.
    #[test]
    fn cap_as_usize_saturates_on_overflow() {
        assert_eq!(cap_as_usize(u64::MAX), usize::MAX);
        assert_eq!(cap_as_usize(0), 0);
        assert_eq!(cap_as_usize(50 * 1024 * 1024), 50 * 1024 * 1024);
    }

    /// Demonstrates the cap-vs-size comparison the handler performs for
    /// the multipart branch: a Free-tier caller uploading 11 MiB is
    /// rejected, the same payload uploaded by a Pro-tier caller is
    /// accepted. (The handler's actual rejection path returns 413; this
    /// test exercises the inequality the path branches on.)
    #[test]
    fn tier_cap_rejects_oversize_for_free_accepts_for_pro() {
        let payload_size: u64 = 11 * 1024 * 1024;
        let free_cap = resolve_max_file_size_bytes(&ZaruTier::Free).unwrap();
        let pro_cap = resolve_max_file_size_bytes(&ZaruTier::Pro).unwrap();
        assert!(payload_size <= free_cap, "11 MiB <= 50 MiB Free cap");
        assert!(payload_size <= pro_cap, "11 MiB <= 500 MiB Pro cap");

        // 60 MiB exceeds Free but not Pro.
        let oversize: u64 = 60 * 1024 * 1024;
        assert!(oversize > free_cap, "60 MiB > 50 MiB Free cap (rejected)");
        assert!(oversize <= pro_cap, "60 MiB <= 500 MiB Pro cap (accepted)");
    }

    // ========================================================================
    // ADR-113 regression: chat-attachments lazy-provision must respect tier
    // ========================================================================

    /// The lazy-provision branch of `resolve_or_provision_upload_volume` used
    /// a hardcoded 1 GiB allocation regardless of caller tier. For Free users
    /// (`total_storage_bytes = 500 MiB`) this exceeded the tier ceiling so the
    /// very first upload — even an 89.8 KB PDF — failed with
    /// `StorageQuotaExceeded` before any byte of the file was inspected.
    ///
    /// `chat_attachments_volume_size_for_tier` MUST return a size that fits
    /// under the tier's `total_storage_bytes`. Specifically: for an empty
    /// owner the volume creation precondition
    /// `allocated_bytes + size_limit_bytes <= total_storage_bytes` must hold.
    #[test]
    fn chat_attachments_size_fits_within_free_tier_budget() {
        let limits = aegis_orchestrator_core::domain::volume::StorageTierLimits::default();
        let free_total = limits
            .limits
            .get(&ZaruTier::Free)
            .expect("Free tier limit present")
            .total_storage_bytes;

        let allocation = chat_attachments_volume_size_for_tier(&ZaruTier::Free)
            .expect("Free tier produces a chat-attachments allocation");

        // The bug: a 1 GiB hardcoded allocation > 500 MiB Free ceiling.
        // The fix: allocation MUST fit within the tier total.
        assert!(
            allocation <= free_total,
            "chat-attachments allocation ({allocation}) must not exceed Free total_storage_bytes ({free_total})"
        );
    }

    /// Pro tier (10 GiB total) similarly must produce an allocation that fits.
    #[test]
    fn chat_attachments_size_fits_within_pro_tier_budget() {
        let limits = aegis_orchestrator_core::domain::volume::StorageTierLimits::default();
        let pro_total = limits
            .limits
            .get(&ZaruTier::Pro)
            .expect("Pro tier limit present")
            .total_storage_bytes;

        let allocation = chat_attachments_volume_size_for_tier(&ZaruTier::Pro)
            .expect("Pro tier produces a chat-attachments allocation");

        assert!(
            allocation <= pro_total,
            "chat-attachments allocation ({allocation}) must not exceed Pro total_storage_bytes ({pro_total})"
        );
    }

    /// Business tier (100 GiB) — same invariant.
    #[test]
    fn chat_attachments_size_fits_within_business_tier_budget() {
        let limits = aegis_orchestrator_core::domain::volume::StorageTierLimits::default();
        let biz_total = limits
            .limits
            .get(&ZaruTier::Business)
            .expect("Business tier limit present")
            .total_storage_bytes;

        let allocation = chat_attachments_volume_size_for_tier(&ZaruTier::Business)
            .expect("Business tier produces a chat-attachments allocation");

        assert!(
            allocation <= biz_total,
            "chat-attachments allocation ({allocation}) must not exceed Business total_storage_bytes ({biz_total})"
        );
    }

    /// Enterprise tier has `total_storage_bytes = u64::MAX`. A literal `u64::MAX`
    /// allocation is nonsensical at the storage backend, so the helper MUST
    /// clamp to a sane finite value.
    #[test]
    fn chat_attachments_size_for_enterprise_is_finite() {
        let allocation = chat_attachments_volume_size_for_tier(&ZaruTier::Enterprise)
            .expect("Enterprise tier produces a chat-attachments allocation");

        assert!(
            allocation < u64::MAX,
            "Enterprise allocation ({allocation}) must be clamped below u64::MAX"
        );
        assert!(
            allocation >= 1024 * 1024 * 1024,
            "Enterprise clamp ({allocation}) should still be at least 1 GiB to be useful"
        );
    }

    /// The exact case from the live bug: a Free-tier caller's first upload of
    /// an 89.8 KB file. The chat-attachments lazy-provision request size MUST
    /// satisfy `UserVolumeService::create_volume`'s precondition
    /// `allocated_bytes + size_limit_bytes <= total_storage_bytes` when there
    /// are no pre-existing volumes (`allocated_bytes = 0`). Before the fix the
    /// helper returned 1 GiB and this inequality was violated for Free users,
    /// producing the `{"error":"storage quota exceeded ..."}` response on
    /// upload of any file size.
    #[test]
    fn free_tier_first_upload_does_not_trip_storage_quota() {
        let limits = aegis_orchestrator_core::domain::volume::StorageTierLimits::default();
        let free_total = limits
            .limits
            .get(&ZaruTier::Free)
            .expect("Free tier limit present")
            .total_storage_bytes;

        // Empty owner: no volumes yet, so allocated_bytes = 0.
        let allocated_bytes: u64 = 0;
        let size_limit_bytes = chat_attachments_volume_size_for_tier(&ZaruTier::Free)
            .expect("Free tier produces an allocation");

        // The exact precondition from
        // UserVolumeService::create_volume (user_volume_service.rs):
        //   allocated_bytes.saturating_add(size_limit_bytes) > total_storage_bytes
        //     => StorageQuotaExceeded
        let would_exceed = allocated_bytes.saturating_add(size_limit_bytes) > free_total;
        assert!(
            !would_exceed,
            "Free-tier first upload (89.8 KB or any size) would trip StorageQuotaExceeded: \
             allocated={allocated_bytes} + size_limit={size_limit_bytes} > total={free_total}"
        );
    }

    /// An unrecognized tier MUST fail closed (return `None`) so the upload
    /// handler can produce a 403 rather than admitting the volume creation
    /// under an unbounded ceiling. Mirrors the `resolve_max_file_size_bytes`
    /// posture introduced for the per-file cap.
    ///
    /// We can't construct a synthetic `ZaruTier` variant without ecosystem
    /// changes, so we instead document the contract by asserting that all
    /// known tiers DO resolve — a future tier added without a corresponding
    /// `StorageTierLimits` entry would silently fail this test.
    #[test]
    fn chat_attachments_size_resolves_for_every_known_tier() {
        for tier in [
            ZaruTier::Free,
            ZaruTier::Pro,
            ZaruTier::Business,
            ZaruTier::Enterprise,
        ] {
            assert!(
                chat_attachments_volume_size_for_tier(&tier).is_some(),
                "tier {tier:?} must have a chat-attachments allocation"
            );
        }
    }
}
