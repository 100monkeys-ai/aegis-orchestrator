// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Vibe-Code Canvas session HTTP handlers (ADR-106, Wave C2).
//!
//! Serves the five endpoints described in ADR-106 §Orchestrator REST API:
//!
//! - `POST   /v1/canvas/sessions`          — create a session + provision workspace
//! - `GET    /v1/canvas/sessions`          — list caller's active sessions
//! - `GET    /v1/canvas/sessions/:id`      — session detail
//! - `DELETE /v1/canvas/sessions/:id`      — terminate + clean up ephemeral volumes
//! - `GET    /v1/canvas/sessions/:id/events` — **SSE** stream of [`CanvasEvent`]s
//!
//! All endpoints require Keycloak JWT authentication; ownership is verified
//! against the tenant derived from the token's claims.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream::Stream;
use uuid::Uuid;

use aegis_orchestrator_core::application::canvas_service::{
    CanvasError, CreateCanvasSessionCommand,
};
use aegis_orchestrator_core::domain::canvas::{
    CanvasEvent, CanvasSession, CanvasSessionId, ConversationId, WorkspaceMode,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

// ============================================================================
// Request / response types
// ============================================================================

/// Request body for `POST /v1/canvas/sessions`.
///
/// `workspace_mode` is the tagged [`WorkspaceMode`] variant (its payload is
/// embedded in the JSON via serde's default tagged representation). The
/// separate `git_binding_id` field is a redundant top-level shortcut — when
/// present it MUST match the `binding_id` inside a `GitLinked` variant.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct CreateCanvasSessionRequest {
    /// Optional — if omitted a new UUID is generated (useful for new conversations).
    #[serde(default)]
    pub(crate) conversation_id: Option<Uuid>,
    pub(crate) workspace_mode: WorkspaceMode,
    #[serde(default)]
    pub(crate) git_binding_id: Option<Uuid>,
}

/// Response envelope returned by create / get / list endpoints.
#[derive(Debug, serde::Serialize)]
pub(crate) struct CanvasSessionResponse {
    id: Uuid,
    tenant_id: String,
    conversation_id: Uuid,
    workspace_volume_id: Uuid,
    git_binding_id: Option<Uuid>,
    workspace_mode: WorkspaceMode,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
    last_active_at: chrono::DateTime<chrono::Utc>,
}

impl From<CanvasSession> for CanvasSessionResponse {
    fn from(s: CanvasSession) -> Self {
        Self {
            id: s.id.0,
            tenant_id: s.tenant_id.as_str().to_string(),
            conversation_id: s.conversation_id.0,
            workspace_volume_id: s.workspace_volume_id.0,
            git_binding_id: s.git_binding_id.map(|b| b.0),
            workspace_mode: s.workspace_mode,
            status: format!("{:?}", s.status),
            created_at: s.created_at,
            last_active_at: s.last_active_at,
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn user_sub(identity: Option<&UserIdentity>) -> String {
    identity
        .map(|i| i.sub.clone())
        .unwrap_or_else(|| "anonymous".to_string())
}

fn user_tier(identity: Option<&UserIdentity>) -> ZaruTier {
    match identity.map(|i| &i.identity_kind) {
        Some(IdentityKind::ConsumerUser { zaru_tier, .. }) => zaru_tier.clone(),
        // Non-consumer identities (operator / service account / tenant user)
        // default to Enterprise so that admin tooling is not artificially
        // blocked by Zaru-tier gating.
        _ => ZaruTier::Enterprise,
    }
}

/// Map a [`CanvasError`] to an HTTP response. Ownership and tenant-boundary
/// violations are deliberately collapsed to `404 NotFound` so the endpoint
/// does not leak the existence of other tenants' sessions.
fn canvas_error_response(e: CanvasError) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match &e {
        CanvasError::NotFound(_) | CanvasError::NotOwned => (StatusCode::NOT_FOUND, e.to_string()),
        CanvasError::NotAllowedByTier => (StatusCode::FORBIDDEN, e.to_string()),
        CanvasError::GitBindingNotFound(_) => (StatusCode::NOT_FOUND, e.to_string()),
        CanvasError::GitBindingNotReady => (StatusCode::CONFLICT, e.to_string()),
        CanvasError::InvalidCommand(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        CanvasError::VolumeProvisioningFailed(_) | CanvasError::RepositoryError(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    };
    (status, Json(serde_json::json!({ "error": message })))
}

// ============================================================================
// Handlers
// ============================================================================

/// `POST /v1/canvas/sessions` — create a canvas session.
pub(crate) async fn create_session_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Json(body): Json<CreateCanvasSessionRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("canvas:write")?;

    let service = state.canvas_service.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "canvas service not configured"})),
        )
    })?;

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);
    let tier = user_tier(identity_ref);

    // Enforce the invariant up front so `WorkspaceMode::GitLinked { binding_id: X }`
    // and a top-level `git_binding_id: Y` can never contradict.
    if let (WorkspaceMode::GitLinked { binding_id }, Some(top_level)) =
        (&body.workspace_mode, body.git_binding_id)
    {
        if binding_id.0 != top_level {
            return Err(canvas_error_response(CanvasError::InvalidCommand(
                "git_binding_id does not match WorkspaceMode::GitLinked payload".to_string(),
            )));
        }
    }

    let cmd = CreateCanvasSessionCommand {
        tenant_id,
        owner,
        tier,
        conversation_id: ConversationId(body.conversation_id.unwrap_or_else(Uuid::new_v4)),
        workspace_mode: body.workspace_mode,
    };

    service
        .create_session(cmd)
        .await
        .map(|s| {
            (
                StatusCode::CREATED,
                Json(serde_json::to_value(CanvasSessionResponse::from(s)).unwrap()),
            )
        })
        .map_err(canvas_error_response)
}

/// `GET /v1/canvas/sessions` — list caller's active sessions.
pub(crate) async fn list_sessions_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("canvas:read")?;

    let service = state.canvas_service.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "canvas service not configured"})),
        )
    })?;

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);

    let sessions = service
        .list_sessions(&tenant_id, &owner)
        .await
        .map_err(canvas_error_response)?;

    let body: Vec<CanvasSessionResponse> = sessions.into_iter().map(Into::into).collect();
    Ok(Json(serde_json::to_value(body).unwrap()))
}

/// `GET /v1/canvas/sessions/:id` — session detail.
pub(crate) async fn get_session_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("canvas:read")?;

    let service = state.canvas_service.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "canvas service not configured"})),
        )
    })?;

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);

    service
        .get_session(&CanvasSessionId(id), &tenant_id, &owner)
        .await
        .map(|s| Json(serde_json::to_value(CanvasSessionResponse::from(s)).unwrap()))
        .map_err(canvas_error_response)
}

/// `DELETE /v1/canvas/sessions/:id` — terminate session and clean up.
pub(crate) async fn terminate_session_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    scope_guard.require("canvas:write")?;

    let service = state.canvas_service.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "canvas service not configured"})),
        )
    })?;

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);

    service
        .terminate_session(&CanvasSessionId(id), &tenant_id, &owner)
        .await
        .map(|_| Json(serde_json::json!({ "success": true })))
        .map_err(canvas_error_response)
}

/// `GET /v1/canvas/sessions/:id/events` — **SSE** stream of
/// [`CanvasEvent`]s scoped to one session.
///
/// The stream subscribes to the broadcast [`EventBus`], filters
/// [`DomainEvent::Canvas`] variants to those whose `session_id` matches the
/// path parameter, and emits each as an SSE `event:` / `data:` pair. The
/// event name is the canonical `domain_event_type` string
/// (`canvas_files_written_by_agent`, etc.). A 20-second keep-alive comment
/// keeps the connection open through idle intermediaries; the stream also
/// enforces a 2-minute no-event safety timeout and closes gracefully after a
/// [`CanvasEvent::SessionTerminated`] event is observed for this session.
///
/// Pattern reference — axum 0.8 SSE: `Sse<impl Stream<Item = Result<Event>>>`
/// + `KeepAlive::new().interval(20s)`, see <https://docs.rs/axum/0.8.8/axum/response/sse/index.html>.
pub(crate) async fn stream_session_events_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    Path(id): Path<Uuid>,
) -> axum::response::Response {
    if let Err(e) = scope_guard.require("canvas:read") {
        return e.into_response();
    }

    let service = match state.canvas_service.clone() {
        Some(s) => s,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "canvas service not configured"})),
            )
                .into_response();
        }
    };

    let identity_ref = identity.as_ref().map(|e| &e.0);
    let tenant_id = tenant_id_from_identity(identity_ref);
    let owner = user_sub(identity_ref);

    let session_id = CanvasSessionId(id);
    // Verify ownership before opening the stream; an unauthorized caller
    // never gets to subscribe to the event bus in the first place.
    if let Err(e) = service.get_session(&session_id, &tenant_id, &owner).await {
        return canvas_error_response(e).into_response();
    }

    let stream = canvas_event_stream(state.event_bus.clone(), session_id);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(20))
                .text("keep-alive"),
        )
        .into_response()
}

/// Build the async stream that feeds the SSE handler.
///
/// Exposed as a separate function so the core-layer integration test can
/// consume it directly without spinning up axum. The stream:
///
/// 1. Subscribes to the broadcast [`EventBus`].
/// 2. Yields one SSE [`Event`] per [`CanvasEvent`] whose `session_id` matches.
/// 3. Closes after [`CanvasEvent::SessionTerminated`] for this session.
/// 4. Applies a 2-minute idle timeout so stale connections can be reaped.
pub fn canvas_event_stream(
    event_bus: Arc<EventBus>,
    session_id: CanvasSessionId,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> {
    let mut receiver = event_bus.subscribe();
    async_stream::stream! {
        loop {
            let next = tokio::time::timeout(Duration::from_secs(120), receiver.recv()).await;
            match next {
                // Idle timeout — close the stream.
                Err(_) => break,
                // Bus closed or unrecoverable lag — close the stream.
                Ok(Err(_)) => break,
                Ok(Ok(domain_event)) => {
                    let DomainEvent::Canvas(canvas_event) = domain_event else {
                        continue;
                    };
                    if canvas_event_session_id(&canvas_event) != session_id {
                        continue;
                    }
                    let event_name = canvas_event_name(&canvas_event);
                    let payload = match serde_json::to_string(&canvas_event) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let should_close = matches!(canvas_event, CanvasEvent::SessionTerminated { .. });
                    yield Ok::<_, std::convert::Infallible>(
                        Event::default().event(event_name).data(payload),
                    );
                    if should_close {
                        break;
                    }
                }
            }
        }
    }
}

/// Extract the `session_id` carried by every [`CanvasEvent`] variant.
fn canvas_event_session_id(event: &CanvasEvent) -> CanvasSessionId {
    match event {
        CanvasEvent::SessionCreated { session_id, .. }
        | CanvasEvent::FilesWrittenByAgent { session_id, .. }
        | CanvasEvent::GitCommitMade { session_id, .. }
        | CanvasEvent::GitPushed { session_id, .. }
        | CanvasEvent::SessionTerminated { session_id, .. } => *session_id,
    }
}

/// Canonical SSE `event:` name for a [`CanvasEvent`]. Mirrors the type-name
/// mapping in `infrastructure::event_bus::domain_event_type`.
fn canvas_event_name(event: &CanvasEvent) -> &'static str {
    match event {
        CanvasEvent::SessionCreated { .. } => "canvas_session_created",
        CanvasEvent::FilesWrittenByAgent { .. } => "canvas_files_written_by_agent",
        CanvasEvent::GitCommitMade { .. } => "canvas_git_commit_made",
        CanvasEvent::GitPushed { .. } => "canvas_git_pushed",
        CanvasEvent::SessionTerminated { .. } => "canvas_session_terminated",
    }
}
