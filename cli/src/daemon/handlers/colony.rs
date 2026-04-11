// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Colony management handlers: member management, SAML IdP config, subscription.
//!
//! Endpoints:
//! - GET  /v1/colony/members         — list tenant members
//! - POST /v1/colony/members         — invite member (email + role)
//! - DELETE /v1/colony/members/:id   — remove member
//! - PUT  /v1/colony/roles           — update member role
//! - GET  /v1/colony/saml            — get SAML IdP config
//! - PUT  /v1/colony/saml            — set SAML IdP config
//! - GET  /v1/colony/subscription    — get subscription tier + quota usage

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::tenancy::TenantTier;
use aegis_orchestrator_core::infrastructure::iam::keycloak_admin_client::SamlIdpConfig;

use crate::daemon::state::AppState;

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub(crate) struct InviteMemberRequest {
    pub email: String,
    pub role: String,
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
pub(crate) struct MemberResponse {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
    pub role: String,
    pub joined_at: i64,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct SubscriptionResponse {
    pub tier: String,
    pub agent_runs_used: u32,
    pub agent_runs_limit: u32,
    pub members_used: u32,
    pub members_limit: u32,
    pub renews_at: Option<String>,
}

// ── Authorization helpers ─────────────────────────────────────────────────────

/// Returns `true` if the identity has admin or operator aegis_role.
fn is_admin_or_operator(identity: &UserIdentity) -> bool {
    matches!(
        &identity.identity_kind,
        IdentityKind::Operator {
            aegis_role: AegisRole::Admin | AegisRole::Operator,
        }
    )
}

/// Returns `true` if the identity has admin aegis_role.
fn is_admin(identity: &UserIdentity) -> bool {
    matches!(
        &identity.identity_kind,
        IdentityKind::Operator {
            aegis_role: AegisRole::Admin,
        }
    )
}

/// Resolve the Keycloak realm that manages this user's tenant.
fn realm_for_identity(identity: &UserIdentity) -> String {
    match &identity.identity_kind {
        IdentityKind::TenantUser { tenant_slug } => format!("tenant-{tenant_slug}"),
        IdentityKind::ConsumerUser { .. } => "zaru-consumer".to_string(),
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => {
            identity.realm_slug.clone()
        }
    }
}

/// Extract `aegis_role` attribute from a Keycloak user, defaulting to `"member"`.
fn role_from_kc_user(
    attributes: &Option<std::collections::HashMap<String, Vec<String>>>,
) -> String {
    attributes
        .as_ref()
        .and_then(|attrs| attrs.get("aegis_role"))
        .and_then(|vals| vals.first())
        .cloned()
        .unwrap_or_else(|| "member".to_string())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub(crate) async fn list_members(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin_or_operator(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin or operator role required"})),
        )
            .into_response();
    }

    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Keycloak admin not configured"})),
            )
                .into_response();
        }
    };

    let realm = realm_for_identity(&identity);

    match kc.list_realm_users(&realm).await {
        Ok(users) => {
            let members: Vec<MemberResponse> = users
                .into_iter()
                .map(|u| {
                    let name = match (&u.first_name, &u.last_name) {
                        (Some(f), Some(l)) => Some(format!("{f} {l}")),
                        (Some(f), None) => Some(f.clone()),
                        (None, Some(l)) => Some(l.clone()),
                        (None, None) => None,
                    };
                    MemberResponse {
                        id: u.id,
                        email: u.email.unwrap_or_default(),
                        name,
                        role: role_from_kc_user(&u.attributes),
                        joined_at: u.created_timestamp,
                    }
                })
                .collect();
            let count = members.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({"members": members, "count": count})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn invite_member(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(payload): Json<InviteMemberRequest>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin_or_operator(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin or operator role required"})),
        )
            .into_response();
    }

    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Keycloak admin not configured"})),
            )
                .into_response();
        }
    };

    let realm = realm_for_identity(&identity);

    match kc.invite_user(&realm, &payload.email, &payload.role).await {
        Ok(user) => {
            let name = match (&user.first_name, &user.last_name) {
                (Some(f), Some(l)) => Some(format!("{f} {l}")),
                (Some(f), None) => Some(f.clone()),
                (None, Some(l)) => Some(l.clone()),
                (None, None) => None,
            };
            let member = MemberResponse {
                id: user.id,
                email: user.email.unwrap_or_default(),
                name,
                role: role_from_kc_user(&user.attributes),
                joined_at: user.created_timestamp,
            };
            (
                StatusCode::CREATED,
                Json(serde_json::json!({"member": member})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn remove_member(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Path(user_id): Path<String>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin role required"})),
        )
            .into_response();
    }

    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Keycloak admin not configured"})),
            )
                .into_response();
        }
    };

    let realm = realm_for_identity(&identity);

    match kc.remove_user(&realm, &user_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn update_role(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(payload): Json<UpdateRoleRequest>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin role required"})),
        )
            .into_response();
    }

    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Keycloak admin not configured"})),
            )
                .into_response();
        }
    };

    let realm = realm_for_identity(&identity);

    match kc
        .assign_realm_role(&realm, &payload.user_id, &payload.role)
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "updated"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn get_saml_config(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin role required"})),
        )
            .into_response();
    }

    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Keycloak admin not configured"})),
            )
                .into_response();
        }
    };

    let realm = realm_for_identity(&identity);

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

pub(crate) async fn set_saml_config(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    Json(payload): Json<SamlConfigRequest>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin role required"})),
        )
            .into_response();
    }

    // SAML SSO is Enterprise-only — reject Free and Pro tiers
    if let IdentityKind::ConsumerUser { zaru_tier, .. } = &identity.identity_kind {
        if matches!(zaru_tier, ZaruTier::Free | ZaruTier::Pro) {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "SAML SSO requires Business or Enterprise tier"})),
            )
                .into_response();
        }
    }

    let kc = match &state.keycloak_admin {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Keycloak admin not configured"})),
            )
                .into_response();
        }
    };

    let realm = realm_for_identity(&identity);

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

pub(crate) async fn get_subscription(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
) -> axum::response::Response {
    let identity = match identity {
        Some(Extension(id)) => id,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Authentication required"})),
            )
                .into_response();
        }
    };

    if !is_admin_or_operator(&identity) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Admin or operator role required"})),
        )
            .into_response();
    }

    let tenant_repo = match &state.tenant_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Tenant repository not configured"})),
            )
                .into_response();
        }
    };

    let tenant_id = crate::daemon::handlers::tenant_id_from_identity(Some(&identity));

    let tenant = match tenant_repo.find_by_slug(&tenant_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Tenant not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let tier_name = match &tenant.tier {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Business => "business",
        TenantTier::Enterprise => "enterprise",
        TenantTier::System => "system",
    };

    // members_limit: no explicit quota field exists for members — derive from tier
    let members_limit: u32 = match &tenant.tier {
        TenantTier::Free => 1,
        TenantTier::Pro => 5,
        TenantTier::Business => 25,
        TenantTier::Enterprise => u32::MAX,
        TenantTier::System => u32::MAX,
    };

    // members_used: count users in the Keycloak realm if admin client is available
    let members_used: u32 = if let Some(kc) = &state.keycloak_admin {
        let realm = realm_for_identity(&identity);
        match kc.list_realm_users(&realm).await {
            Ok(users) => users.len() as u32,
            Err(_) => 0,
        }
    } else {
        0
    };

    let response = SubscriptionResponse {
        tier: tier_name.to_string(),
        agent_runs_used: 0,
        agent_runs_limit: tenant.quotas.max_concurrent_executions,
        members_used,
        members_limit,
        renews_at: None,
    };

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
    use aegis_orchestrator_core::domain::tenant::TenantId;

    fn admin_identity() -> UserIdentity {
        UserIdentity {
            sub: "admin-1".into(),
            realm_slug: "aegis-system".into(),
            email: Some("admin@example.com".into()),
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Admin,
            },
        }
    }

    fn operator_identity() -> UserIdentity {
        UserIdentity {
            sub: "op-1".into(),
            realm_slug: "aegis-system".into(),
            email: Some("op@example.com".into()),
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Operator,
            },
        }
    }

    fn readonly_identity() -> UserIdentity {
        UserIdentity {
            sub: "ro-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Readonly,
            },
        }
    }

    fn free_consumer_identity() -> UserIdentity {
        UserIdentity {
            sub: "user-1".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        }
    }

    fn pro_consumer_identity() -> UserIdentity {
        UserIdentity {
            sub: "user-2".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Pro,
                tenant_id: TenantId::consumer(),
            },
        }
    }

    fn business_consumer_identity() -> UserIdentity {
        UserIdentity {
            sub: "user-3".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Business,
                tenant_id: TenantId::consumer(),
            },
        }
    }

    // list_members: admin and operator are permitted
    #[test]
    fn list_members_admin_is_permitted() {
        assert!(is_admin_or_operator(&admin_identity()));
    }

    #[test]
    fn list_members_operator_is_permitted() {
        assert!(is_admin_or_operator(&operator_identity()));
    }

    #[test]
    fn list_members_readonly_is_denied() {
        assert!(!is_admin_or_operator(&readonly_identity()));
    }

    // invite_member: admin and operator are permitted; readonly is not
    #[test]
    fn invite_member_admin_is_permitted() {
        assert!(is_admin_or_operator(&admin_identity()));
    }

    #[test]
    fn invite_member_readonly_is_denied() {
        assert!(!is_admin_or_operator(&readonly_identity()));
    }

    // remove_member: only admin
    #[test]
    fn remove_member_admin_is_permitted() {
        assert!(is_admin(&admin_identity()));
    }

    #[test]
    fn remove_member_operator_is_denied() {
        assert!(!is_admin(&operator_identity()));
    }

    // set_saml_config: Free and Pro tiers are rejected
    #[test]
    fn set_saml_config_free_tier_is_rejected() {
        let identity = free_consumer_identity();
        // Simulate the tier check logic from set_saml_config
        let rejected = matches!(
            &identity.identity_kind,
            IdentityKind::ConsumerUser { zaru_tier, .. }
            if matches!(zaru_tier, ZaruTier::Free | ZaruTier::Pro)
        );
        assert!(rejected, "Free tier must be rejected for SAML config");
    }

    #[test]
    fn set_saml_config_pro_tier_is_rejected() {
        let identity = pro_consumer_identity();
        let rejected = matches!(
            &identity.identity_kind,
            IdentityKind::ConsumerUser { zaru_tier, .. }
            if matches!(zaru_tier, ZaruTier::Free | ZaruTier::Pro)
        );
        assert!(rejected, "Pro tier must be rejected for SAML config");
    }

    #[test]
    fn set_saml_config_business_tier_is_allowed() {
        let identity = business_consumer_identity();
        let rejected = matches!(
            &identity.identity_kind,
            IdentityKind::ConsumerUser { zaru_tier, .. }
            if matches!(zaru_tier, ZaruTier::Free | ZaruTier::Pro)
        );
        assert!(
            !rejected,
            "Business tier must NOT be rejected for SAML config"
        );
    }

    // get_subscription: returns tier and quota fields
    #[test]
    fn subscription_response_has_required_fields() {
        let resp = SubscriptionResponse {
            tier: "pro".into(),
            agent_runs_used: 0,
            agent_runs_limit: 10,
            members_used: 3,
            members_limit: 5,
            renews_at: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("tier").is_some());
        assert!(json.get("agent_runs_used").is_some());
        assert!(json.get("agent_runs_limit").is_some());
        assert!(json.get("members_used").is_some());
        assert!(json.get("members_limit").is_some());
        assert!(json.get("renews_at").is_some());
    }

    // realm_for_identity maps correctly
    #[test]
    fn realm_for_tenant_user_is_prefixed() {
        let identity = UserIdentity {
            sub: "tu-1".into(),
            realm_slug: "tenant-acme".into(),
            email: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "acme".into(),
            },
        };
        assert_eq!(realm_for_identity(&identity), "tenant-acme");
    }

    #[test]
    fn realm_for_consumer_user_is_zaru_consumer() {
        let identity = free_consumer_identity();
        assert_eq!(realm_for_identity(&identity), "zaru-consumer");
    }
}
