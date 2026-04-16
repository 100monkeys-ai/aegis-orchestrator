// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding REST Handlers (BC-7, ADR-081 Wave A2)
//!
//! HTTP handlers for the `/v1/storage/git/*` surface.
//!
//! | Endpoint | Scope | Notes |
//! |---|---|---|
//! | `POST   /v1/storage/git` | `volume:write` | Create binding + spawn background clone |
//! | `GET    /v1/storage/git` | `volume:read`  | List caller's bindings (redacted) |
//! | `GET    /v1/storage/git/:id` | `volume:read` | Binding detail (redacted, 404 on non-owner) |
//! | `DELETE /v1/storage/git/:id` | `volume:write` | Cascade delete binding + volume |
//! | `POST   /v1/storage/git/:id/refresh` | `volume:write` | A2 returns 501 (A3 implements) |
//!
//! All five endpoints require Keycloak JWT. The handlers themselves do
//! no business logic — they translate JSON into command structs and
//! defer to [`GitRepoService`].

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use uuid::Uuid;

use aegis_orchestrator_core::application::git_repo_service::{CreateGitRepoCommand, GitRepoError};
use aegis_orchestrator_core::domain::credential::CredentialBindingId;
use aegis_orchestrator_core::domain::git_repo::{GitRef, GitRepoBinding, GitRepoBindingId};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

// ============================================================================
// Request / response types
// ============================================================================

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CreateGitRepoRequest {
    pub(crate) repo_url: String,
    #[serde(default)]
    pub(crate) credential_binding_id: Option<Uuid>,
    #[serde(default)]
    pub(crate) git_ref: Option<GitRefDto>,
    #[serde(default)]
    pub(crate) sparse_paths: Option<Vec<String>>,
    pub(crate) label: String,
    #[serde(default)]
    pub(crate) auto_refresh: bool,
    #[serde(default = "default_shallow")]
    pub(crate) shallow: bool,
}

fn default_shallow() -> bool {
    true
}

/// DTO for [`GitRef`] — matches the shape `{ "type": "branch", "value": "main" }`.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum GitRefDto {
    Branch { value: String },
    Tag { value: String },
    Commit { value: String },
}

impl From<GitRefDto> for GitRef {
    fn from(dto: GitRefDto) -> Self {
        match dto {
            GitRefDto::Branch { value } => GitRef::Branch(value),
            GitRefDto::Tag { value } => GitRef::Tag(value),
            GitRefDto::Commit { value } => GitRef::Commit(value),
        }
    }
}

// ============================================================================
// Error mapping
// ============================================================================

fn git_repo_error_response(e: GitRepoError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match &e {
        GitRepoError::BindingNotFound | GitRepoError::NotOwned => {
            (StatusCode::NOT_FOUND, e.to_string())
        }
        GitRepoError::TierLimitExceeded { .. } => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        GitRepoError::UrlValidationFailed(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        GitRepoError::NotYetImplemented(_) => (StatusCode::NOT_IMPLEMENTED, e.to_string()),
        GitRepoError::CloneFailed(_) => (StatusCode::BAD_GATEWAY, e.to_string()),
        GitRepoError::VolumeProvisioningFailed(_) => {
            (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        GitRepoError::SecretResolutionFailed(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
        GitRepoError::Repository(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    (status, Json(json!({ "error": message })))
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
// Redaction
// ============================================================================

/// Serialize a [`GitRepoBinding`] for the REST surface.
///
/// **Redacts** the `webhook_secret` field to prevent leaking it through
/// GET responses (ADR-081 §Security). Callers that need the secret must
/// retrieve it through a dedicated, separately-authenticated endpoint
/// (added in A3 alongside the webhook implementation).
fn redacted_binding(b: &GitRepoBinding) -> serde_json::Value {
    json!({
        "id": b.id.0,
        "tenant_id": b.tenant_id,
        "credential_binding_id": b.credential_binding_id.map(|c| c.0),
        "repo_url": b.repo_url,
        "git_ref": b.git_ref,
        "sparse_paths": b.sparse_paths,
        "volume_id": b.volume_id.0,
        "label": b.label,
        "status": b.status,
        "clone_strategy": b.clone_strategy,
        "last_cloned_at": b.last_cloned_at,
        "last_commit_sha": b.last_commit_sha,
        "auto_refresh": b.auto_refresh,
        "webhook_secret_set": b.webhook_secret.is_some(),
        "created_at": b.created_at,
        "updated_at": b.updated_at,
    })
}

// ============================================================================
// Service access
// ============================================================================

fn git_repo_service(
    state: &AppState,
) -> Result<
    Arc<aegis_orchestrator_core::application::git_repo_service::GitRepoService>,
    (StatusCode, Json<serde_json::Value>),
> {
    state.git_repo_service.clone().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "git repo service not configured"})),
    ))
}

// ============================================================================
// Handlers
// ============================================================================

/// `POST /v1/storage/git` — create a new binding and spawn the clone.
pub(crate) async fn create_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<CreateGitRepoRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let tier = user_tier(identity_ref);
    let svc = git_repo_service(&state)?;

    let cmd = CreateGitRepoCommand {
        tenant_id,
        owner,
        zaru_tier: tier,
        credential_binding_id: body.credential_binding_id.map(CredentialBindingId),
        repo_url: body.repo_url,
        git_ref: body.git_ref.map(Into::into).unwrap_or_default(),
        sparse_paths: body.sparse_paths,
        label: body.label,
        auto_refresh: body.auto_refresh,
        shallow: body.shallow,
    };

    let binding = svc
        .create_binding(cmd)
        .await
        .map_err(git_repo_error_response)?;

    // Spawn background clone — the HTTP response returns immediately
    // with the Pending binding. Errors are surfaced to the client by
    // polling GET /v1/storage/git/:id.
    let svc_bg = svc.clone();
    let binding_id = binding.id;
    tokio::spawn(async move {
        if let Err(e) = svc_bg.clone_repo(&binding_id).await {
            tracing::warn!(%binding_id, error = %e, "background clone failed");
        }
    });

    Ok((StatusCode::CREATED, Json(redacted_binding(&binding))))
}

/// `GET /v1/storage/git` — list caller's bindings.
pub(crate) async fn list_git_repos(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let svc = git_repo_service(&state)?;

    let bindings = svc
        .list_bindings(&tenant_id, &owner)
        .await
        .map_err(git_repo_error_response)?;

    Ok(Json(
        bindings.iter().map(redacted_binding).collect::<Vec<_>>(),
    ))
}

/// `GET /v1/storage/git/:id` — binding detail.
///
/// Returns `404 Not Found` when the binding exists but is not owned by
/// the caller (avoids leaking existence to other users — ADR-081
/// §Security).
pub(crate) async fn get_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let svc = git_repo_service(&state)?;

    let binding = svc
        .get_binding(&GitRepoBindingId(id), &tenant_id, &owner)
        .await
        .map_err(git_repo_error_response)?;

    Ok(Json(redacted_binding(&binding)))
}

/// `DELETE /v1/storage/git/:id` — cascade delete.
pub(crate) async fn delete_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let svc = git_repo_service(&state)?;

    svc.delete_binding(&GitRepoBindingId(id), &tenant_id, &owner)
        .await
        .map_err(git_repo_error_response)?;

    Ok(Json(json!({ "success": true })))
}

/// `POST /v1/storage/git/:id/refresh` — A2 returns `501 Not Implemented`.
/// Wave A3 wires `GitRepoService::refresh_repo` through to
/// `fetch_and_checkout`.
pub(crate) async fn refresh_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let svc = git_repo_service(&state)?;

    svc.refresh_repo(&GitRepoBindingId(id), &tenant_id, &owner)
        .await
        .map_err(git_repo_error_response)?;

    Ok(Json(json!({ "success": true })))
}
