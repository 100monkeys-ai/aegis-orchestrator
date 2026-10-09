// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Profile routes (AEGIS ADR-140 D3)
//!
//! | Route | Caller | Scope |
//! |-------|--------|-------|
//! | `POST /v1/profiles` | a person | `profile:write` |
//! | `GET /v1/profiles` | a person (their own, by name) | `profile:read` |
//! | `GET /v1/profiles/{id}` | the owner | `profile:read` |
//! | `PATCH /v1/profiles/{id}` | the owner | `profile:write` |
//! | `DELETE /v1/profiles/{id}` | the owner | `profile:write` |
//! | `POST /v1/profiles/available-tools` | a person | `profile:read` |
//!
//! Another person's profile is answered 404, exactly as one that does not
//! exist. An operator reads none and a service account is refused every
//! route, with a plain sentence (the grant's reading).

use std::sync::Arc;

use aegis_orchestrator_core::application::profile_service::{
    AvailableTool, ProfileDraft, ProfileError, ProfilePatch, ProfileService, ProfileView,
};
use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::profile::{ProfileId, PERSON_REFUSAL, UNAVAILABLE_REFUSAL};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::daemon::handlers::tenant_id_from_identity;

type Refusal = (StatusCode, Json<Value>);

/// State of the profile sub-router.
#[derive(Clone)]
pub(crate) struct ProfilesState {
    /// `None` when the node has no profile service: every route answers 503.
    pub(crate) service: Option<Arc<ProfileService>>,
}

/// The `/v1/profiles*` routes, merged into the daemon router by
/// `router::create_router` beneath the same authentication layers as every
/// other route.
pub(crate) fn profiles_router(state: ProfilesState) -> Router {
    Router::new()
        .route(
            "/v1/profiles",
            post(create_profile_handler).get(list_profiles_handler),
        )
        .route(
            "/v1/profiles/available-tools",
            post(available_tools_handler),
        )
        .route(
            "/v1/profiles/{id}",
            get(get_profile_handler)
                .patch(update_profile_handler)
                .delete(delete_profile_handler),
        )
        .with_state(state)
}

fn refusal(status: StatusCode, error: &str) -> Refusal {
    (status, Json(json!({ "error": error })))
}

fn service(state: &ProfilesState) -> Result<&Arc<ProfileService>, Refusal> {
    state
        .service
        .as_ref()
        .ok_or_else(|| refusal(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE_REFUSAL))
}

fn from_service_error(e: ProfileError) -> Refusal {
    match e {
        ProfileError::Refused(sentence) => refusal(StatusCode::BAD_REQUEST, &sentence),
        ProfileError::NotFound => refusal(StatusCode::NOT_FOUND, "Not found"),
        ProfileError::Repository(detail) => {
            tracing::error!(error = %detail, "Profile store failed");
            refusal(StatusCode::INTERNAL_SERVER_ERROR, "Profile store failed")
        }
    }
}

/// The person asking, with the tenant their profiles live in; an operator
/// or a service account is refused.
fn person<'a>(
    identity: Option<&'a UserIdentity>,
    tenant: Option<&TenantId>,
) -> Result<(&'a UserIdentity, TenantId), Refusal> {
    let identity =
        identity.ok_or_else(|| refusal(StatusCode::UNAUTHORIZED, "Authentication required"))?;
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. } => Ok((
            identity,
            tenant
                .cloned()
                .unwrap_or_else(|| tenant_id_from_identity(Some(identity))),
        )),
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => {
            Err(refusal(StatusCode::FORBIDDEN, PERSON_REFUSAL))
        }
    }
}

fn parse_id(id: &str) -> Result<ProfileId, Refusal> {
    ProfileId::parse(id).ok_or_else(|| refusal(StatusCode::NOT_FOUND, "Not found"))
}

fn profile_view(view: &ProfileView) -> Value {
    let p = &view.profile;
    json!({
        "id": p.id.to_string(),
        "name": p.name,
        "bindings": view.bindings.iter().map(|b| {
            let mut row = json!({
                "binding_id": b.binding_id.0.to_string(),
                "state": b.state,
            });
            if let (Some(label), Some(obj)) = (&b.label, row.as_object_mut()) {
                obj.insert("label".into(), json!(label));
            }
            row
        }).collect::<Vec<_>>(),
        "tools": p.tools.as_strings(),
        "repository": p.repository.map(|id| id.to_string()),
        "notes_workspace": p.notes_workspace,
        "instructions": p.instructions,
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    })
}

fn tool_view(tool: &AvailableTool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "family": tool.family,
        "gated": tool.gated,
    })
}

/// `POST /v1/profiles`.
pub(crate) async fn create_profile_handler(
    State(state): State<ProfilesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Json(draft): Json<ProfileDraft>,
) -> Result<(StatusCode, Json<Value>), Refusal> {
    scope_guard.require("profile:write")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let view = service(&state)?
        .create(owner, &tenant_id, draft)
        .await
        .map_err(from_service_error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "profile": profile_view(&view) })),
    ))
}

/// `GET /v1/profiles`.
pub(crate) async fn list_profiles_handler(
    State(state): State<ProfilesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("profile:read")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let views = service(&state)?
        .list(owner, &tenant_id)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({
        "count": views.len(),
        "profiles": views.iter().map(profile_view).collect::<Vec<_>>(),
    })))
}

/// `GET /v1/profiles/{id}`.
pub(crate) async fn get_profile_handler(
    State(state): State<ProfilesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("profile:read")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let id = parse_id(&id)?;
    let view = service(&state)?
        .get(owner, &tenant_id, &id)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "profile": profile_view(&view) })))
}

/// `PATCH /v1/profiles/{id}`.
pub(crate) async fn update_profile_handler(
    State(state): State<ProfilesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
    Json(patch): Json<ProfilePatch>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("profile:write")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let id = parse_id(&id)?;
    let view = service(&state)?
        .update(owner, &tenant_id, &id, patch)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({ "profile": profile_view(&view) })))
}

/// `DELETE /v1/profiles/{id}`.
pub(crate) async fn delete_profile_handler(
    State(state): State<ProfilesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<String>,
) -> Result<StatusCode, Refusal> {
    scope_guard.require("profile:write")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let id = parse_id(&id)?;
    service(&state)?
        .delete(owner, &tenant_id, &id)
        .await
        .map_err(from_service_error)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/profiles/available-tools` with `{bindings}`.
pub(crate) async fn available_tools_handler(
    State(state): State<ProfilesState>,
    scope_guard: ScopeGuard,
    identity: Option<Extension<UserIdentity>>,
    tenant: Option<Extension<TenantId>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Refusal> {
    scope_guard.require("profile:read")?;
    let (owner, tenant_id) = person(identity.as_deref(), tenant.as_deref())?;
    let bindings = body.get("bindings").cloned().unwrap_or(Value::Null);
    let tools = service(&state)?
        .available_tools(owner, &tenant_id, &bindings)
        .await
        .map_err(from_service_error)?;
    Ok(Json(json!({
        "count": tools.len(),
        "tools": tools.iter().map(tool_view).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
#[path = "profiles_tests.rs"]
mod profiles_tests;
