// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Colony management handlers (ADR-111 Phase 2).
//!
//! The Colony surface is the product entry point for team tenants: CRUD over
//! `Team`, invitation lifecycle, membership management, role updates, SAML
//! configuration, and subscription summaries. All handlers delegate the
//! tenancy logic to [`TeamService`]
//! and resolve the active tenant from request extensions populated by
//! [`tenant_context_middleware`](aegis_orchestrator_core::presentation::tenant_middleware::tenant_context_middleware).
//!
//! Endpoint map:
//!
//! | Method | Path | Auth |
//! | --- | --- | --- |
//! | `GET` | `/v1/colony/teams` | Bearer JWT |
//! | `POST` | `/v1/colony/teams` | Bearer JWT |
//! | `DELETE` | `/v1/colony/teams/:team_id` | Bearer JWT (Owner) |
//! | `GET` | `/v1/colony/members` | Bearer JWT (team member) |
//! | `POST` | `/v1/colony/invitations` | Bearer JWT (Owner/Admin) |
//! | `GET` | `/v1/colony/invitations` | Bearer JWT (team member) |
//! | `DELETE` | `/v1/colony/invitations/:id` | Bearer JWT (Owner/Admin) |
//! | `POST` | `/v1/colony/invitations/:token/accept` | Bearer JWT |
//! | `DELETE` | `/v1/colony/members/:user_id` | Bearer JWT (Owner/Admin) |
//! | `PUT` | `/v1/colony/roles` | Bearer JWT (Owner/Admin) |
//! | `GET` | `/v1/colony/saml` | Bearer JWT (Owner, Enterprise) |
//! | `PUT` | `/v1/colony/saml` | Bearer JWT (Owner, Enterprise) |
//! | `GET` | `/v1/colony/subscription` | Bearer JWT |

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Extension, Path, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use uuid::Uuid;

use aegis_orchestrator_core::application::team_service::{
    AcceptInvitationCommand, InviteMemberCommand, ProvisionTeamCommand, TeamService,
    TeamServiceError,
};
use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::domain::team::{
    InvitationStatus, MembershipRole, MembershipStatus, Team, TeamId, TeamInvitationId, TeamSlug,
};
use aegis_orchestrator_core::domain::tenancy::{TenantKind, TenantTier};
use aegis_orchestrator_core::infrastructure::iam::keycloak_admin_client::SamlIdpConfig;

use crate::daemon::handlers::resolved_tenant;
use crate::daemon::state::AppState;

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CreateTeamRequest {
    pub display_name: String,
    pub tier: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct InviteRequest {
    pub invitee_email: String,
    pub role: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UpdateRoleRequest {
    pub user_id: String,
    pub role: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SamlConfigRequest {
    pub entity_id: String,
    pub sso_url: String,
    pub certificate: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct TeamSummary {
    pub id: String,
    pub slug: String,
    pub display_name: String,
    pub tier: String,
    pub tenant_id: String,
    pub role: String,
    pub status: String,
    pub member_count: u32,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct TeamCreated {
    pub id: String,
    pub slug: String,
    pub display_name: String,
    pub tier: String,
    pub tenant_id: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MemberView {
    pub user_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub role: String,
    pub status: String,
    pub joined_at: i64,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct InvitationView {
    pub id: String,
    pub team_id: String,
    pub invitee_email: String,
    pub status: String,
    pub expires_at: String,
    pub created_at: String,
    pub invited_by: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct InvitationIssuedView {
    pub id: String,
    pub team_id: String,
    pub invitee_email: String,
    pub token: String,
    pub expires_at: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SubscriptionResponse {
    pub tier: String,
    pub members_used: u32,
    pub members_limit: u32,
    pub seat_count: Option<u32>,
    pub status: Option<String>,
    pub current_period_end: Option<String>,
    pub is_team: bool,
}

// ── Small helpers ─────────────────────────────────────────────────────────────

fn parse_tier(s: &str) -> Result<TenantTier, String> {
    match s.to_ascii_lowercase().as_str() {
        "pro" => Ok(TenantTier::Pro),
        "business" => Ok(TenantTier::Business),
        "enterprise" => Ok(TenantTier::Enterprise),
        other => Err(format!("unsupported tier: {other}")),
    }
}

fn tier_str(t: &TenantTier) -> &'static str {
    match t {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Business => "business",
        TenantTier::Enterprise => "enterprise",
        TenantTier::System => "system",
    }
}

fn team_service(state: &AppState) -> Result<Arc<dyn TeamService>, Box<axum::response::Response>> {
    state.team_service.clone().ok_or_else(|| {
        Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "teams_not_configured",
                    "message": "Team tenancy is not configured on this node",
                })),
            )
                .into_response(),
        )
    })
}

fn unauthenticated() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "Authentication required"})),
    )
        .into_response()
}

fn map_service_error(err: TeamServiceError) -> axum::response::Response {
    let (status, code): (StatusCode, &str) = match &err {
        TeamServiceError::Unauthorized => (StatusCode::FORBIDDEN, "forbidden"),
        TeamServiceError::TierNotPermitted(_) => (StatusCode::FORBIDDEN, "tier_not_permitted"),
        TeamServiceError::PersonalTierTooLow { .. } => {
            (StatusCode::PAYMENT_REQUIRED, "personal_tier_too_low")
        }
        TeamServiceError::SeatCapReached { .. } => (StatusCode::CONFLICT, "seat_cap_reached"),
        TeamServiceError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        TeamServiceError::InvalidInvitation => (StatusCode::BAD_REQUEST, "invalid_invitation"),
        TeamServiceError::InvitationExpired => (StatusCode::GONE, "invitation_expired"),
        TeamServiceError::InvitationNotPending => (StatusCode::CONFLICT, "invitation_not_pending"),
        TeamServiceError::OwnerRoleImmutable => (StatusCode::BAD_REQUEST, "owner_role_immutable"),
        TeamServiceError::InvalidCommand(_) => (StatusCode::BAD_REQUEST, "invalid_command"),
        TeamServiceError::Domain(_) => (StatusCode::BAD_REQUEST, "domain_invariant"),
        TeamServiceError::InvitationsNotConfigured => (
            StatusCode::SERVICE_UNAVAILABLE,
            "invitations_not_configured",
        ),
        TeamServiceError::Repository(_) | TeamServiceError::Billing(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    };
    (
        status,
        Json(serde_json::json!({
            "error": code,
            "message": err.to_string(),
        })),
    )
        .into_response()
}

fn parse_team_id(s: &str) -> Result<TeamId, Box<axum::response::Response>> {
    match Uuid::parse_str(s) {
        Ok(u) => Ok(TeamId(u)),
        Err(_) => Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_team_id"})),
            )
                .into_response(),
        )),
    }
}

fn parse_invitation_id(s: &str) -> Result<TeamInvitationId, Box<axum::response::Response>> {
    match Uuid::parse_str(s) {
        Ok(u) => Ok(TeamInvitationId(u)),
        Err(_) => Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_invitation_id"})),
            )
                .into_response(),
        )),
    }
}

/// Resolve the team the caller is operating in. Requires the
/// `tenant_context_middleware` to have switched the tenant to a `t-{uuid}`
/// slug via the `X-Tenant-Id` header — which by construction verified the
/// caller's Active membership.
///
/// Returns `Ok(Some(team))` when the active tenant is a team tenant,
/// `Ok(None)` when the caller is operating in their personal tenant, and
/// `Err(response)` on infrastructure failure.
async fn resolve_active_team(
    state: &AppState,
    tenant_kind: TenantKind,
    tenant_slug: &str,
) -> Result<Option<Team>, axum::response::Response> {
    if tenant_kind != TenantKind::Team {
        return Ok(None);
    }
    let team_repo = state.team_repo.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "teams_not_configured"})),
        )
            .into_response()
    })?;
    let slug = TeamSlug::parse(tenant_slug).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_team_slug", "message": e})),
        )
            .into_response()
    })?;
    match team_repo.find_by_slug(&slug).await {
        Ok(Some(t)) => Ok(Some(t)),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "team_not_found"})),
        )
            .into_response()),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

/// Look up the caller's role within the given team.
async fn caller_role(
    state: &AppState,
    team_id: TeamId,
    user_id: &str,
) -> Result<Option<(MembershipRole, MembershipStatus)>, axum::response::Response> {
    let repo = state.membership_repo.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "teams_not_configured"})),
        )
            .into_response()
    })?;
    let members = repo.find_by_team(&team_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()
    })?;
    Ok(members
        .into_iter()
        .find(|m| m.user_id == user_id)
        .map(|m| (m.role, m.status)))
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/colony/teams` — list teams the caller belongs to.
pub(crate) async fn list_teams(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let memberships = match svc.list_memberships_for_user(&identity.sub).await {
        Ok(m) => m,
        Err(e) => return map_service_error(e),
    };
    let team_repo = match state.team_repo.clone() {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "teams_not_configured"})),
            )
                .into_response()
        }
    };
    let membership_repo = match state.membership_repo.clone() {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "teams_not_configured"})),
            )
                .into_response()
        }
    };
    let mut out: Vec<TeamSummary> = Vec::new();
    for m in memberships {
        if m.status != MembershipStatus::Active {
            continue;
        }
        let team = match team_repo.find_by_id(&m.team_id).await {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response()
            }
        };
        let count = membership_repo.count_active(&team.id).await.unwrap_or(0);
        out.push(TeamSummary {
            id: team.id.to_string(),
            slug: team.slug.as_str().to_string(),
            display_name: team.display_name.clone(),
            tier: tier_str(&team.tier).to_string(),
            tenant_id: team.tenant_id.as_str().to_string(),
            role: m.role.as_str().to_string(),
            status: m.status.as_str().to_string(),
            member_count: count,
        });
    }
    let count = out.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({"teams": out, "count": count})),
    )
        .into_response()
}

/// `POST /v1/colony/teams` — create a new team.
pub(crate) async fn create_team(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(payload): Json<CreateTeamRequest>,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let owner_email = match identity.email.clone() {
        Some(e) => e,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "missing_email",
                    "message": "Owner email is required to provision a Stripe customer",
                })),
            )
                .into_response()
        }
    };
    let tier = match parse_tier(&payload.tier) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_tier", "message": e})),
            )
                .into_response()
        }
    };
    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let team = match svc
        .provision_team(ProvisionTeamCommand {
            display_name: payload.display_name,
            owner_user_id: identity.sub.clone(),
            owner_email,
            tier,
        })
        .await
    {
        Ok(t) => t,
        Err(e) => return map_service_error(e),
    };

    // Enterprise tier: also materialize the dedicated Keycloak realm. For
    // Pro/Business we defer group creation until the first invite (the group
    // is cheap and idempotent there).
    if matches!(team.tier, TenantTier::Enterprise) {
        if let Some(kc) = state.keycloak_admin.clone() {
            if let Err(e) = kc.create_team_realm(team.slug.as_str()).await {
                tracing::warn!(
                    error = %e,
                    team_id = %team.id,
                    "Failed to create team realm in Keycloak; team provisioned anyway",
                );
            }
        }
    }

    let view = TeamCreated {
        id: team.id.to_string(),
        slug: team.slug.as_str().to_string(),
        display_name: team.display_name.clone(),
        tier: tier_str(&team.tier).to_string(),
        tenant_id: team.tenant_id.as_str().to_string(),
    };
    (StatusCode::CREATED, Json(serde_json::json!({"team": view}))).into_response()
}

/// `DELETE /v1/colony/teams/:team_id` — owner-only team deletion.
pub(crate) async fn delete_team(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(team_id_str): Path<String>,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let team_id = match parse_team_id(&team_id_str) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let team_repo = match state.team_repo.clone() {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "teams_not_configured"})),
            )
                .into_response()
        }
    };
    let team = match team_repo.find_by_id(&team_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not_found"})),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    if team.owner_user_id != identity.sub {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "owner_only"})),
        )
            .into_response();
    }
    // Cancel billing (best-effort) via team_service's underlying billing. The
    // trait does not expose cancel_team_subscription today at the TeamService
    // layer — delegate directly to the billing service that provisioned it.
    // Deletion of the underlying rows is repository-driven.
    if let Err(e) = team_repo.delete(&team_id).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // Tear down the Keycloak realm for Enterprise teams. For Pro/Business
    // remove the group.
    if let Some(kc) = state.keycloak_admin.clone() {
        match team.tier {
            TenantTier::Enterprise => {
                let realm = format!("team-{}", team.slug.as_str());
                if let Err(e) = kc.delete_realm(&realm).await {
                    tracing::warn!(error = %e, realm, "Failed to delete team realm");
                }
            }
            _ => {
                if let Ok(Some(group_id)) = kc
                    .find_group_by_name("zaru-consumer", team.slug.as_str())
                    .await
                {
                    if let Err(e) = kc.delete_group("zaru-consumer", &group_id).await {
                        tracing::warn!(error = %e, "Failed to delete team group");
                    }
                }
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /v1/colony/members` — list members of the active team tenant.
pub(crate) async fn list_members(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);
    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "team_context_required",
                    "message": "Send X-Tenant-Id: t-{uuid} to list team members",
                })),
            )
                .into_response()
        }
        Err(r) => return r,
    };

    // Authorization: caller must be an Active member.
    match caller_role(&state, team.id, &identity.sub).await {
        Ok(Some((_, MembershipStatus::Active))) => {}
        Ok(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "not_a_member"})),
            )
                .into_response()
        }
        Err(r) => return r,
    }

    let repo = match state.membership_repo.clone() {
        Some(r) => r,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "teams_not_configured"})),
            )
                .into_response()
        }
    };
    let members = match repo.find_by_team(&team.id).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    // Enrich with email/name from Keycloak where available.
    let kc = state.keycloak_admin.clone();
    let realm = match team.tier {
        TenantTier::Enterprise => format!("team-{}", team.slug.as_str()),
        _ => "zaru-consumer".to_string(),
    };

    let mut out = Vec::with_capacity(members.len());
    for m in members {
        let (email, name) = if let Some(kc) = &kc {
            match kc.get_user(&realm, &m.user_id).await {
                Ok(Some(u)) => {
                    let name = match (&u.first_name, &u.last_name) {
                        (Some(f), Some(l)) => Some(format!("{f} {l}")),
                        (Some(f), None) => Some(f.clone()),
                        (None, Some(l)) => Some(l.clone()),
                        (None, None) => None,
                    };
                    (u.email, name)
                }
                _ => (None, None),
            }
        } else {
            (None, None)
        };
        out.push(MemberView {
            user_id: m.user_id.clone(),
            email,
            name,
            role: m.role.as_str().to_string(),
            status: m.status.as_str().to_string(),
            joined_at: m.joined_at.timestamp_millis(),
        });
    }
    let count = out.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({"members": out, "count": count})),
    )
        .into_response()
}

/// `POST /v1/colony/invitations` — owner/admin issues a new invitation.
pub(crate) async fn create_invitation(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);

    // Body extraction (since we took Request for extension access).
    let (parts, body) = request.into_parts();
    let _ = parts;
    let bytes = match axum::body::to_bytes(body, 1 << 16).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let payload: InviteRequest = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_body", "message": e.to_string()})),
            )
                .into_response()
        }
    };

    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "team_context_required"})),
            )
                .into_response()
        }
        Err(r) => return r,
    };

    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };

    let issued = match svc
        .invite_member(InviteMemberCommand {
            team_id: team.id,
            invitee_email: payload.invitee_email.clone(),
            invited_by_user_id: identity.sub.clone(),
        })
        .await
    {
        Ok(i) => i,
        Err(e) => return map_service_error(e),
    };

    // Materialize the Keycloak user + group membership now so that when the
    // invitee logs in they are already routed to the correct realm/group.
    let role = payload
        .role
        .as_deref()
        .unwrap_or(MembershipRole::Member.as_str());
    if let Some(kc) = state.keycloak_admin.clone() {
        if let Err(e) = kc
            .invite_team_user(
                team.tier,
                team.slug.as_str(),
                &issued.invitee_email,
                role,
                &issued.raw_token,
            )
            .await
        {
            tracing::warn!(
                error = %e,
                team_id = %team.id,
                "Failed to materialize Keycloak user for invite; invitation stands",
            );
        }
    }

    let view = InvitationIssuedView {
        id: issued.invitation_id.to_string(),
        team_id: issued.team_id.to_string(),
        invitee_email: issued.invitee_email,
        token: issued.raw_token,
        expires_at: issued.expires_at.to_rfc3339(),
    };
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"invitation": view})),
    )
        .into_response()
}

/// `GET /v1/colony/invitations` — list pending invitations for the active team.
pub(crate) async fn list_invitations(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);
    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "team_context_required"})),
            )
                .into_response()
        }
        Err(r) => return r,
    };

    // Gate: must be a member.
    match caller_role(&state, team.id, &identity.sub).await {
        Ok(Some((_, MembershipStatus::Active))) => {}
        Ok(_) => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "not_a_member"})),
            )
                .into_response()
        }
        Err(r) => return r,
    }

    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let pending = match svc.list_pending_invitations(team.id).await {
        Ok(v) => v,
        Err(e) => return map_service_error(e),
    };
    let invitations: Vec<InvitationView> = pending
        .into_iter()
        .filter(|i| i.status == InvitationStatus::Pending)
        .map(|i| InvitationView {
            id: i.id.to_string(),
            team_id: i.team_id.to_string(),
            invitee_email: i.invitee_email,
            status: i.status.as_str().to_string(),
            expires_at: i.expires_at.to_rfc3339(),
            created_at: i.created_at.to_rfc3339(),
            invited_by: i.invited_by,
        })
        .collect();

    let count = invitations.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({"invitations": invitations, "count": count})),
    )
        .into_response()
}

/// `DELETE /v1/colony/invitations/:id` — cancel a pending invitation.
pub(crate) async fn cancel_invitation(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(invitation_id_str): Path<String>,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let invitation_id = match parse_invitation_id(&invitation_id_str) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match svc
        .cancel_invitation(invitation_id, identity.sub.clone())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => map_service_error(e),
    }
}

/// `POST /v1/colony/invitations/:token/accept` — invitee accepts. Does NOT
/// require `X-Tenant-Id`; the target tenant is derived from the token.
pub(crate) async fn accept_invitation(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(token): Path<String>,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let Some(email) = identity.email.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "missing_email",
                "message": "Authenticated caller must have a verified email to accept an invitation",
            })),
        )
            .into_response();
    };
    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let membership = match svc
        .accept_invitation(AcceptInvitationCommand {
            token,
            authenticated_email: email,
            authenticated_user_id: identity.sub.clone(),
        })
        .await
    {
        Ok(m) => m,
        Err(e) => return map_service_error(e),
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "team_id": membership.team_id.to_string(),
            "user_id": membership.user_id,
            "role": membership.role.as_str(),
            "status": membership.status.as_str(),
        })),
    )
        .into_response()
}

/// `DELETE /v1/colony/members/:user_id` — revoke membership.
pub(crate) async fn remove_member(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(target_user_id): Path<String>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);
    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "team_context_required"})),
            )
                .into_response()
        }
        Err(r) => return r,
    };
    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if let Err(e) = svc
        .revoke_membership(team.id, target_user_id.clone(), identity.sub.clone())
        .await
    {
        return map_service_error(e);
    }
    // Mirror to Keycloak: Pro/Business = remove from group; Enterprise = remove from realm.
    if let Some(kc) = state.keycloak_admin.clone() {
        match team.tier {
            TenantTier::Enterprise => {
                let realm = format!("team-{}", team.slug.as_str());
                if let Err(e) = kc.remove_user(&realm, &target_user_id).await {
                    tracing::warn!(error = %e, "Failed to remove user from team realm");
                }
            }
            _ => {
                if let Ok(Some(group_id)) = kc
                    .find_group_by_name("zaru-consumer", team.slug.as_str())
                    .await
                {
                    if let Err(e) = kc
                        .remove_user_from_group("zaru-consumer", &target_user_id, &group_id)
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to detach user from team group");
                    }
                }
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `PUT /v1/colony/roles` — change a member's role.
pub(crate) async fn update_role(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);

    let (_, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 1 << 16).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let payload: UpdateRoleRequest = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_body", "message": e.to_string()})),
            )
                .into_response()
        }
    };

    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "team_context_required"})),
            )
                .into_response()
        }
        Err(r) => return r,
    };

    let new_role = match MembershipRole::from_str(&payload.role) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_role", "message": e})),
            )
                .into_response()
        }
    };

    let svc = match team_service(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match svc
        .update_role(team.id, payload.user_id, new_role, identity.sub.clone())
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "updated"})),
        )
            .into_response(),
        Err(e) => map_service_error(e),
    }
}

/// `GET /v1/colony/saml` — fetch SAML IdP config (Enterprise only).
pub(crate) async fn get_saml_config(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);
    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "team_context_required"})),
            )
                .into_response()
        }
        Err(r) => return r,
    };
    if !matches!(team.tier, TenantTier::Enterprise) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "enterprise_only",
                "message": "SAML federation is only available on Enterprise teams",
            })),
        )
            .into_response();
    }
    match caller_role(&state, team.id, &identity.sub).await {
        Ok(Some((role, MembershipStatus::Active))) if role.can_manage_membership() => {}
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "forbidden"})),
            )
                .into_response()
        }
    }
    let Some(kc) = state.keycloak_admin.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Keycloak admin not configured"})),
        )
            .into_response();
    };
    let realm = format!("team-{}", team.slug.as_str());
    match kc.get_idp_config(&realm).await {
        Ok(Some(cfg)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "entity_id": cfg.entity_id,
                "sso_url": cfg.sso_url,
                "certificate": cfg.certificate,
            })),
        )
            .into_response(),
        Ok(None) => (StatusCode::OK, Json(serde_json::json!({"saml": null}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `PUT /v1/colony/saml` — configure SAML IdP (Enterprise only, owner/admin).
pub(crate) async fn set_saml_config(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);
    let (_, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 1 << 16).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let payload: SamlConfigRequest = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_body", "message": e.to_string()})),
            )
                .into_response()
        }
    };
    let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "team_context_required"})),
            )
                .into_response()
        }
        Err(r) => return r,
    };
    if !matches!(team.tier, TenantTier::Enterprise) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "enterprise_only"})),
        )
            .into_response();
    }
    match caller_role(&state, team.id, &identity.sub).await {
        Ok(Some((role, MembershipStatus::Active))) if role.can_manage_membership() => {}
        _ => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "forbidden"})),
            )
                .into_response()
        }
    }
    let Some(kc) = state.keycloak_admin.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Keycloak admin not configured"})),
        )
            .into_response();
    };
    let realm = format!("team-{}", team.slug.as_str());
    let cfg = SamlIdpConfig {
        entity_id: payload.entity_id,
        sso_url: payload.sso_url,
        certificate: payload.certificate,
    };
    match kc.set_idp_config(&realm, &cfg).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "configured"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// `GET /v1/colony/subscription` — subscription summary for the active tenant.
///
/// For a team tenant: returns seat count, status, and the per-tier seat cap.
/// For a personal tenant: returns the caller's current tier and per-tier cap
/// (members_used is always 1 for solo tenants).
pub(crate) async fn get_subscription(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    request: Request,
) -> axum::response::Response {
    let Some(Extension(identity)) = identity else {
        return unauthenticated();
    };
    let tenant_id = resolved_tenant(&request, Some(&identity));
    let kind = team_kind_for(&tenant_id);

    let Some(tenant_repo) = state.tenant_repo.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "tenant repository not configured"})),
        )
            .into_response();
    };

    let tenant = match tenant_repo.find_by_slug(&tenant_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Tenant not found"})),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    // Keep the repo probe as the tenant existence check; the tier is resolved
    // from the subscription row below (single source of truth).
    let _ = &tenant;

    // Resolve the tier from `tenant_subscriptions.tier` — there is no tier
    // column on the tenants row anymore. A missing/canceled subscription
    // collapses to Free.
    let effective_tier: TenantTier = match state.billing_repo.as_ref() {
        Some(br) => match br.get_subscription(&tenant_id).await {
            Ok(Some(sub))
                if matches!(
                    sub.status,
                    aegis_orchestrator_core::domain::billing::SubscriptionStatus::Active
                        | aegis_orchestrator_core::domain::billing::SubscriptionStatus::Trialing
                ) =>
            {
                sub.tier
            }
            _ => TenantTier::Free,
        },
        None => TenantTier::Free,
    };

    let members_limit: u32 = match &effective_tier {
        TenantTier::Free => 1,
        TenantTier::Pro => 5,
        TenantTier::Business => 25,
        TenantTier::Enterprise | TenantTier::System => u32::MAX,
    };

    let (members_used, is_team) = if kind == TenantKind::Team {
        let team = match resolve_active_team(&state, kind, tenant_id.as_str()).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "team_not_found"})),
                )
                    .into_response()
            }
            Err(r) => return r,
        };
        let repo = match state.membership_repo.clone() {
            Some(r) => r,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "teams_not_configured"})),
                )
                    .into_response()
            }
        };
        let count = repo.count_active(&team.id).await.unwrap_or(0);
        (count, true)
    } else {
        (1, false)
    };

    let (seat_count, status, period_end) = match state.billing_repo.as_ref() {
        Some(br) => match br.get_subscription(&tenant_id).await {
            Ok(Some(sub)) => (
                Some(sub.seat_count),
                Some(sub.status.as_str().to_string()),
                sub.current_period_end.map(|t| t.to_rfc3339()),
            ),
            _ => (None, None, None),
        },
        None => (None, None, None),
    };

    let resp = SubscriptionResponse {
        tier: tier_str(&effective_tier).to_string(),
        members_used,
        members_limit,
        seat_count,
        status,
        current_period_end: period_end,
        is_team,
    };
    (StatusCode::OK, Json(serde_json::json!(resp))).into_response()
}

// ── helpers ────────────────────────────────────────────────────────────────────

fn team_kind_for(tenant_id: &aegis_orchestrator_core::domain::tenant::TenantId) -> TenantKind {
    let s = tenant_id.as_str();
    if s == "aegis-system" {
        TenantKind::System
    } else if s == "zaru-consumer" || s.starts_with("u-") {
        TenantKind::Consumer
    } else if s.starts_with("t-") {
        TenantKind::Team
    } else {
        TenantKind::Enterprise
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::tenant::TenantId;

    #[test]
    fn parse_tier_accepts_known_tiers() {
        assert!(matches!(parse_tier("pro"), Ok(TenantTier::Pro)));
        assert!(matches!(parse_tier("business"), Ok(TenantTier::Business)));
        assert!(matches!(
            parse_tier("ENTERPRISE"),
            Ok(TenantTier::Enterprise)
        ));
    }

    #[test]
    fn parse_tier_rejects_free_and_system() {
        assert!(parse_tier("free").is_err());
        assert!(parse_tier("system").is_err());
    }

    #[test]
    fn team_kind_detection() {
        assert_eq!(
            team_kind_for(&TenantId::from_string("zaru-consumer").unwrap()),
            TenantKind::Consumer
        );
        assert_eq!(
            team_kind_for(&TenantId::from_string("aegis-system").unwrap()),
            TenantKind::System
        );
        assert_eq!(
            team_kind_for(&TenantId::from_string("u-12345678").unwrap()),
            TenantKind::Consumer
        );
        assert_eq!(
            team_kind_for(&TenantId::from_string("t-12345678").unwrap()),
            TenantKind::Team
        );
        assert_eq!(
            team_kind_for(&TenantId::from_string("acme-corp").unwrap()),
            TenantKind::Enterprise
        );
    }
}
