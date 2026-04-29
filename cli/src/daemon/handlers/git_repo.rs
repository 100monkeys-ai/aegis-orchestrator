// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Git Repository Binding REST Handlers (BC-7, ADR-081 Wave A2 + ADR-106 Wave B2)
//!
//! HTTP handlers for the `/v1/storage/git/*` surface.
//!
//! | Endpoint | Scope | Notes |
//! |---|---|---|
//! | `POST   /v1/storage/git` | `volume:write` | Create binding + spawn background clone |
//! | `GET    /v1/storage/git` | `volume:read`  | List caller's bindings (redacted) |
//! | `GET    /v1/storage/git/:id` | `volume:read` | Binding detail (redacted, 404 on non-owner) |
//! | `DELETE /v1/storage/git/:id` | `volume:write` | Cascade delete binding + volume |
//! | `POST   /v1/storage/git/:id/refresh` | `volume:write` | Fetch + checkout pinned ref |
//! | `POST   /v1/storage/git/:id/commit` | `volume:write` | **B2** — stage + commit workdir changes |
//! | `POST   /v1/storage/git/:id/push` | `volume:write` | **B2** — push current branch to remote |
//! | `GET    /v1/storage/git/:id/diff` | `volume:read` | **B2** — unified diff (staged or workdir) |
//! | `POST   /v1/webhooks/git` | HMAC-only (`X-Aegis-Webhook-Secret` header) | Inbound git push webhook |
//!
//! All JSON endpoints require Keycloak JWT. The webhook endpoint
//! is exempt from JWT middleware (see
//! `presentation::keycloak_auth::EXEMPT_PATH_PREFIXES`) and is
//! authenticated **only** by HMAC signature verification against the
//! binding's `webhook_secret`. The handlers themselves do no business
//! logic — they translate JSON into command structs and defer to
//! [`GitRepoService`](aegis_orchestrator_core::application::git_repo_service::GitRepoService).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use uuid::Uuid;

use aegis_orchestrator_core::application::git_repo_service::{
    CreateGitRepoCommand, GitRepoError, WebhookAuth, WebhookProvider,
};
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

// B2 — Canvas git-write request bodies
// -----------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CommitGitRepoRequest {
    pub(crate) message: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PushGitRepoRequest {
    #[serde(default)]
    pub(crate) remote: Option<String>,
    /// Branch ref to push. Alias `ref_name` for callers that can't use
    /// the bare word `ref` (which is also accepted via serde).
    #[serde(default, rename = "ref", alias = "ref_name")]
    pub(crate) ref_name: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
pub(crate) struct DiffGitRepoQuery {
    #[serde(default)]
    pub(crate) staged: bool,
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
        GitRepoError::CloneFailed(_) | GitRepoError::GitFailed(_) => {
            (StatusCode::BAD_GATEWAY, e.to_string())
        }
        GitRepoError::VolumeProvisioningFailed(_) => {
            (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        GitRepoError::SecretResolutionFailed(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
        GitRepoError::WebhookRejected(_) => (StatusCode::UNAUTHORIZED, e.to_string()),
        GitRepoError::Repository(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        // B2 — Canvas git-write
        GitRepoError::NothingToCommit | GitRepoError::BindingBusy(_) => {
            (StatusCode::CONFLICT, e.to_string())
        }
        GitRepoError::NoHeadBranch => (StatusCode::BAD_REQUEST, e.to_string()),
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

/// Resolve `(author_name, author_email)` for a canvas commit.
///
/// [`UserIdentity`] exposes only `sub` and `email` (no display name).
/// For the commit signature we fall back to the local-part of the
/// email as the name. Missing email → `"user@aegis.local"` paired
/// with `"User"` so the commit still records a valid RFC-5322 address
/// (libgit2's `Signature::now` rejects empty strings).
fn commit_author(identity: Option<&UserIdentity>) -> (String, String) {
    let email = identity
        .and_then(|i| i.email.clone())
        .unwrap_or_else(|| "user@aegis.local".to_string());
    let name = email
        .split('@')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("User")
        .to_string();
    (name, email)
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
        // Audit 002 §4.37.13 — `webhook_secret` is transient (cleartext is
        // never stored). Probe the persisted ciphertext column instead.
        "webhook_secret_set": b.webhook_secret_ciphertext.is_some(),
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

/// `POST /v1/storage/git/:id/refresh` — fetch + checkout the binding's
/// pinned [`GitRef`] and update `last_commit_sha`.
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

// ============================================================================
// Webhook endpoint
// ============================================================================

/// `POST /v1/webhooks/git` — inbound git push webhook.
///
/// Authentication: HMAC signature verified against the binding's stored
/// `webhook_secret`. Supports GitHub (`X-Hub-Signature-256`), GitLab
/// (`X-Gitlab-Token`), and Bitbucket (`X-Hub-Signature`) formats.
/// **No JWT is required** — the path is exempt via
/// `EXEMPT_PATH_PREFIXES` in the keycloak middleware.
///
/// Audit 002 §4.13: the per-binding `webhook_secret` MUST be supplied
/// via the `X-Aegis-Webhook-Secret` header — never in the URL path.
/// URL paths leak through proxy access logs, OTLP spans, and tracing
/// middleware, turning the secret into a bearer token observable to
/// any log reader.
pub(crate) async fn webhook_git_repo(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let svc = git_repo_service(&state)?;

    let secret = headers
        .get("x-aegis-webhook-secret")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "missing X-Aegis-Webhook-Secret header"
                })),
            )
        })?;

    let auth = match detect_webhook_auth(&headers) {
        Some(a) => a,
        None => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "error": "missing or unrecognized webhook signature header"
                })),
            ));
        }
    };

    match svc.handle_webhook(&secret, &auth, &body).await {
        Ok(()) => Ok((StatusCode::ACCEPTED, Json(json!({ "received": true })))),
        Err(e) => Err(git_repo_error_response(e)),
    }
}

fn detect_webhook_auth(headers: &HeaderMap) -> Option<WebhookAuth> {
    if let Some(v) = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        return Some(WebhookAuth {
            provider: WebhookProvider::GitHub,
            signature: v.to_string(),
        });
    }
    if let Some(v) = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok()) {
        return Some(WebhookAuth {
            provider: WebhookProvider::GitLab,
            signature: v.to_string(),
        });
    }
    if let Some(v) = headers.get("x-hub-signature").and_then(|v| v.to_str().ok()) {
        return Some(WebhookAuth {
            provider: WebhookProvider::Bitbucket,
            signature: v.to_string(),
        });
    }
    None
}

// ============================================================================
// B2 — Canvas git-write handlers (ADR-106 Wave B2)
// ============================================================================

/// `POST /v1/storage/git/:id/commit` — stage workdir + commit on HEAD.
///
/// Body: `{ "message": string }`. Author identity is resolved from the
/// JWT (email → name via local-part fallback — see [`commit_author`]).
/// Response: `200 OK` with `{ "commit_sha": "..." }`.
pub(crate) async fn commit_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Json(body): Json<CommitGitRepoRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let (author_name, author_email) = commit_author(identity_ref);
    let svc = git_repo_service(&state)?;

    let commit_sha = svc
        .commit(
            &GitRepoBindingId(id),
            &tenant_id,
            &owner,
            &body.message,
            &author_name,
            &author_email,
        )
        .await
        .map_err(git_repo_error_response)?;

    Ok(Json(json!({ "commit_sha": commit_sha })))
}

/// `POST /v1/storage/git/:id/push` — push current branch to remote.
///
/// Body: `{ "remote"?: string, "ref"?: string }` (defaults: `"origin"`
/// and the binding's current branch). Response: `204 No Content`.
pub(crate) async fn push_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    body: Option<Json<PushGitRepoRequest>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:write")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let svc = git_repo_service(&state)?;

    let (remote, ref_name) = match body {
        Some(Json(b)) => (b.remote, b.ref_name),
        None => (None, None),
    };

    svc.push(
        &GitRepoBindingId(id),
        &tenant_id,
        &owner,
        remote.as_deref(),
        ref_name.as_deref(),
    )
    .await
    .map_err(git_repo_error_response)?;

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/storage/git/:id/diff?staged=bool` — unified diff.
///
/// `staged=true` → HEAD-tree vs index; `staged=false` (default) →
/// index vs workdir. Response: `200 OK` with JSON body `{"diff": "..."}`.
/// The JSON envelope is required because downstream consumers (SEAL
/// gateway's `HttpClient`) JSON-parse every orchestrator response.
pub(crate) async fn diff_git_repo(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
    Query(query): Query<DiffGitRepoQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("volume:read")?;
    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let svc = git_repo_service(&state)?;

    let diff_text = svc
        .diff(&GitRepoBindingId(id), &tenant_id, &owner, query.staged)
        .await
        .map_err(git_repo_error_response)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "diff": diff_text })),
    ))
}
