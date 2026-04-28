// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! TenantContext middleware (ADR-056, ADR-111 §Tenant-Context Header Extension)
//!
//! Extracts `TenantId` from validated `UserIdentity` and inserts it into
//! request extensions. Runs after `iam_auth_middleware`.
//!
//! # Tenant Resolution Order
//!
//! 1. **Operator override** — `X-Aegis-Tenant` on an admin `Operator`
//!    identity. Audited via [`TenantEvent::AdminCrossTenantAccess`].
//! 2. **Service-account delegation** — `X-Tenant-Id` on a `ServiceAccount`
//!    identity (ADR-100).
//! 3. **Consumer team switch** — `X-Tenant-Id: t-{uuid}` on a `ConsumerUser`
//!    or `TenantUser` identity. The caller MUST have an Active membership on
//!    the target team. Audited via [`TeamEvent::TenantContextSwitched`].
//! 4. **JWT default** — derive from the validated identity's claims.

use std::sync::Arc;

use chrono::Utc;

use crate::domain::events::TenantEvent;
use crate::domain::iam::{IdentityKind, UserIdentity};
use crate::domain::team::{MembershipRepository, TeamEvent, TeamRepository, TeamSlug};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use crate::domain::team::TeamStatus;

/// State carried into [`tenant_context_middleware`] via
/// [`axum::middleware::from_fn_with_state`].
///
/// Holds the collaborators required to resolve a consumer/tenant user's
/// `X-Tenant-Id` header against the team membership table (ADR-111).
#[derive(Clone)]
pub struct TenantMiddlewareState {
    /// Resolves a `t-{uuid}` slug to a `Team` (and therefore a `TeamId`).
    /// `None` disables the consumer team-switch branch — the middleware then
    /// behaves as if the header were absent for consumer callers.
    pub team_repo: Option<Arc<dyn TeamRepository>>,
    /// Authorization primitive for the consumer team-switch branch.
    pub membership_repo: Option<Arc<dyn MembershipRepository>>,
    /// Event bus used to publish [`TenantEvent::AdminCrossTenantAccess`] and
    /// [`TeamEvent::TenantContextSwitched`] audit events.
    pub event_bus: Arc<EventBus>,
}

/// Derive the base [`TenantId`] from a [`UserIdentity`] — the JWT-default
/// branch of the resolution order.
///
/// Back-compat shim: the strict implementation lives in
/// [`crate::domain::iam::derive_tenant_id_strict`] which returns
/// `Result<TenantId, IamError>`. This wrapper fails closed onto
/// `TenantId::system()` instead of the historic
/// `unwrap_or_else(|_| TenantId::consumer())` footgun (ADR-097): a
/// `TenantUser` with a malformed `tenant_slug` is an authentication
/// failure, not a silent downgrade into the shared consumer tenant.
pub fn derive_tenant_id(identity: &UserIdentity) -> TenantId {
    match crate::domain::iam::derive_tenant_id_strict(identity) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                sub = %identity.sub,
                error = %e,
                "tenant slug rejected; failing closed onto TenantId::system() (ADR-097)"
            );
            TenantId::system()
        }
    }
}

/// Paths exempt from tenant extraction (match the IAM exempt paths).
fn is_tenant_exempt(path: &str) -> bool {
    path == "/health"
        || path.starts_with("/v1/dispatch-gateway")
        || path.starts_with("/v1/seal/")
        || path.starts_with("/v1/webhooks/")
        || path == "/v1/temporal-events"
}

/// JSON error body used for consumer-team-switch failures.
fn forbid(code: &'static str, message: &'static str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// Recovery allowlist for suspended team tenants.
///
/// A suspended colony MUST remain reachable on a narrow set of paths so the
/// team owner can inspect the team and restore a Business-or-higher
/// subscription via the billing portal. Everything else 402s.
fn suspension_allowlisted(method: &Method, path: &str) -> bool {
    // Billing endpoints (checkout, portal, subscription, seats, invoices)
    // must remain reachable so the owner can self-serve recovery.
    path.starts_with("/v1/billing")
        // Read-only team views let the owner see who is affected by the
        // suspension while deciding how to recover.
        || (method == Method::GET && path.starts_with("/v1/teams"))
}

/// Apply the ADR-111 colony suspension gate.
///
/// When the resolved tenant is a team colony whose status is `Suspended`,
/// reject the request with HTTP 402 Payment Required — unless the path is in
/// the recovery allowlist ([`suspension_allowlisted`]).
///
/// Returns `Some(response)` when the gate fired (caller must short-circuit).
/// Returns `None` when the request is allowed to proceed.
async fn suspension_gate(
    state: &TenantMiddlewareState,
    method: &Method,
    path: &str,
    tenant_id: &TenantId,
) -> Option<Response> {
    if !tenant_id.is_team() {
        return None;
    }
    if suspension_allowlisted(method, path) {
        return None;
    }
    let team_repo = state.team_repo.as_ref()?;
    match team_repo.find_by_tenant_id(tenant_id).await {
        Ok(Some(team)) if team.status == TeamStatus::Suspended => {
            let body = serde_json::json!({
                "error": "colony_suspended",
                "team_id": team.id,
                "tenant_id": tenant_id,
                "reason": "owner_subscription_below_business",
                "remediation": "Team owner must restore a Business or Enterprise subscription."
            });
            Some((StatusCode::PAYMENT_REQUIRED, Json(body)).into_response())
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                error = %e,
                tenant_id = %tenant_id,
                "failed to check team status for suspension gate"
            );
            None
        }
    }
}

/// TenantContext middleware.
///
/// Resolves the effective [`TenantId`] from the authenticated
/// [`UserIdentity`] and, when present, honours a header-based override
/// according to the rules documented at the module level.
pub async fn tenant_context_middleware(
    State(state): State<TenantMiddlewareState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Skip tenant extraction for exempt paths
    if is_tenant_exempt(&path) {
        return next.run(request).await;
    }

    // Extract UserIdentity from extensions (set by iam_auth_middleware).
    let identity = request.extensions().get::<UserIdentity>().cloned();

    let Some(id) = identity else {
        // No identity — authentication was skipped or failed upstream. Fall
        // back to default tenant so handlers behave as before.
        let mut request = request;
        request.extensions_mut().insert(TenantId::default());
        return next.run(request).await;
    };

    let method = request.method().clone();

    // Use the strict derivation: a `TenantUser` carrying a malformed
    // `tenant_slug` is an authentication failure, not a silent downgrade
    // into `TenantId::consumer()` (ADR-097 footgun #5).
    let base_tenant = match crate::domain::iam::derive_tenant_id_strict(&id) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                sub = %id.sub,
                error = %e,
                path = %path,
                "rejecting request: tenant slug in token is invalid (ADR-097)"
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "invalid_tenant_slug",
                    "message": e.to_string(),
                })),
            )
                .into_response();
        }
    };

    // 1. Operator cross-tenant override.
    if let Some(override_slug) = request.headers().get("x-aegis-tenant") {
        if let IdentityKind::Operator { ref aegis_role } = id.identity_kind {
            if aegis_role.is_admin() {
                if let Ok(slug_str) = override_slug.to_str() {
                    if let Ok(target_tenant) = TenantId::from_realm_slug(slug_str) {
                        tracing::info!(
                            admin_sub = %id.sub,
                            source_tenant = %base_tenant,
                            target_tenant = %target_tenant,
                            "Admin cross-tenant access"
                        );
                        state
                            .event_bus
                            .publish_tenant_event(TenantEvent::AdminCrossTenantAccess {
                                admin_identity: id.sub.clone(),
                                target_tenant_id: target_tenant.clone(),
                                accessed_at: Utc::now(),
                            });
                        if let Some(gate) =
                            suspension_gate(&state, &method, &path, &target_tenant).await
                        {
                            return gate;
                        }
                        let mut request = request;
                        request.extensions_mut().insert(target_tenant);
                        return next.run(request).await;
                    }
                }
            }
        }
    }

    // 2/3. `X-Tenant-Id` header dispatch.
    //
    // Service-account callers follow ADR-100 delegation (branch 2); consumer /
    // tenant-user callers follow ADR-111 team-switch (branch 3). The two
    // branches are disjoint by identity kind.
    if let Some(header_val) = request
        .headers()
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    {
        match &id.identity_kind {
            IdentityKind::ServiceAccount { .. } => {
                // ADR-100: service-account delegation.
                if !header_val.is_empty() {
                    if let Ok(target) = TenantId::from_realm_slug(&header_val) {
                        if let Some(gate) = suspension_gate(&state, &method, &path, &target).await {
                            return gate;
                        }
                        let mut request = request;
                        request.extensions_mut().insert(target);
                        return next.run(request).await;
                    }
                }
                // Empty / invalid → fall through to base_tenant below.
            }
            IdentityKind::ConsumerUser { .. } | IdentityKind::TenantUser { .. } => {
                // ADR-111: consumer team-switch.
                //
                // Consumers MAY only switch to a `t-{uuid}` team slug they
                // are an Active member of. Any other value is a forbidden
                // tenant switch.
                let team_slug = match TeamSlug::parse(&header_val) {
                    Ok(s) => s,
                    Err(_) => {
                        return forbid(
                            "forbidden_tenant_switch",
                            "Consumers may only switch tenant context to teams they belong to.",
                        );
                    }
                };

                let (Some(team_repo), Some(membership_repo)) =
                    (state.team_repo.as_ref(), state.membership_repo.as_ref())
                else {
                    // Team domain not wired on this node — refuse rather than
                    // silently fall back to the JWT tenant, which would hide
                    // the misconfiguration.
                    return forbid(
                        "not_a_team_member",
                        "You are not an active member of this team.",
                    );
                };

                let team = match team_repo.find_by_slug(&team_slug).await {
                    Ok(Some(team)) => team,
                    Ok(None) => {
                        // Don't leak existence.
                        return forbid(
                            "not_a_team_member",
                            "You are not an active member of this team.",
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "team_repo.find_by_slug failed");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({ "error": "internal" })),
                        )
                            .into_response();
                    }
                };

                match membership_repo.is_active_member(&id.sub, &team.id).await {
                    Ok(true) => {
                        let target_tenant = team.tenant_id.clone();
                        tracing::info!(
                            user_sub = %id.sub,
                            source_tenant = %base_tenant,
                            target_tenant = %target_tenant,
                            team_id = %team.id,
                            "Consumer tenant-context switch"
                        );
                        state
                            .event_bus
                            .publish_team_event(TeamEvent::TenantContextSwitched {
                                caller_user_id: id.sub.clone(),
                                from_tenant_id: base_tenant.clone(),
                                to_tenant_id: target_tenant.clone(),
                                via_header: "X-Tenant-Id".to_string(),
                                switched_at: Utc::now(),
                            });
                        if let Some(gate) =
                            suspension_gate(&state, &method, &path, &target_tenant).await
                        {
                            return gate;
                        }
                        let mut request = request;
                        request.extensions_mut().insert(target_tenant);
                        return next.run(request).await;
                    }
                    Ok(false) => {
                        return forbid(
                            "not_a_team_member",
                            "You are not an active member of this team.",
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "membership_repo.is_active_member failed");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({ "error": "internal" })),
                        )
                            .into_response();
                    }
                }
            }
            IdentityKind::Operator { .. } => {
                // Operators use `X-Aegis-Tenant`, not `X-Tenant-Id`. Ignore
                // the header and fall through to the base tenant.
            }
        }
    }

    // 4. JWT default.
    if let Some(gate) = suspension_gate(&state, &method, &path, &base_tenant).await {
        return gate;
    }
    let mut request = request;
    request.extensions_mut().insert(base_tenant);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::iam::{AegisRole, ZaruTier};
    use crate::domain::repository::RepositoryError;
    use crate::domain::team::{
        Membership, MembershipRole, MembershipStatus, Team, TeamId, TeamInvitation,
        TeamInvitationId, TeamInvitationRepository,
    };
    use async_trait::async_trait;
    use axum::{body::Body, http::Request as HttpRequest, routing::get, Router};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tower::util::ServiceExt;

    // ----- derive_tenant_id unit tests (unchanged behaviour) -----

    #[test]
    fn derive_consumer_tenant() {
        let id = UserIdentity {
            sub: "user-1".to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        };
        assert_eq!(derive_tenant_id(&id), TenantId::consumer());
    }

    #[test]
    fn derive_operator_tenant() {
        let id = UserIdentity {
            sub: "admin-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Admin,
            },
        };
        assert_eq!(derive_tenant_id(&id), TenantId::system());
    }

    #[test]
    fn derive_tenant_user() {
        let id = UserIdentity {
            sub: "tu-1".to_string(),
            realm_slug: "tenant-acme".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "tenant-acme".to_string(),
            },
        };
        let tid = derive_tenant_id(&id);
        assert_eq!(tid.as_str(), "tenant-acme");
    }

    #[test]
    fn derive_service_account_tenant() {
        let id = UserIdentity {
            sub: "sa-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "sdk-python".to_string(),
            },
        };
        assert_eq!(derive_tenant_id(&id), TenantId::system());
    }

    // ── ADR-097 footgun #5 regression ─────────────────────────────────────
    //
    // A `TenantUser` with a malformed `tenant_slug` MUST NOT silently
    // collapse onto `TenantId::consumer()`. The back-compat shim now
    // fails closed onto `TenantId::system()` and the middleware itself
    // rejects the JWT with 401.
    #[test]
    fn derive_tenant_user_with_invalid_slug_does_not_fall_back_to_consumer() {
        let id = UserIdentity {
            sub: "tu-bad".to_string(),
            realm_slug: "tenant-Bad Slug!".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "Bad Slug!".to_string(),
            },
        };
        let tid = derive_tenant_id(&id);
        assert_ne!(
            tid,
            TenantId::consumer(),
            "ADR-097 footgun #5: malformed tenant_slug must not fall back to consumer"
        );
        assert_eq!(tid, TenantId::system(), "must fail closed onto system");
    }

    #[test]
    fn exempt_paths() {
        assert!(is_tenant_exempt("/health"));
        assert!(is_tenant_exempt("/v1/dispatch-gateway"));
        assert!(is_tenant_exempt("/v1/dispatch-gateway/abc"));
        assert!(is_tenant_exempt("/v1/seal/attest"));
        assert!(is_tenant_exempt("/v1/seal/invoke"));
        assert!(is_tenant_exempt("/v1/webhooks/github"));
        assert!(is_tenant_exempt("/v1/temporal-events"));
        assert!(!is_tenant_exempt("/v1/executions"));
        assert!(!is_tenant_exempt("/v1/agents"));
    }

    // ----- In-memory repositories (ADR-111 Phase 1.5 test doubles) -----

    /// In-memory [`TeamRepository`] for middleware tests.
    #[derive(Default)]
    struct InMemoryTeamRepo {
        by_slug: Mutex<HashMap<String, Team>>,
    }

    impl InMemoryTeamRepo {
        fn insert(&self, team: Team) {
            self.by_slug
                .lock()
                .unwrap()
                .insert(team.slug.as_str().to_string(), team);
        }
    }

    #[async_trait]
    impl TeamRepository for InMemoryTeamRepo {
        async fn save(&self, team: &Team) -> Result<(), RepositoryError> {
            self.insert(team.clone());
            Ok(())
        }
        async fn find_by_id(&self, id: &TeamId) -> Result<Option<Team>, RepositoryError> {
            Ok(self
                .by_slug
                .lock()
                .unwrap()
                .values()
                .find(|t| t.id == *id)
                .cloned())
        }
        async fn find_by_slug(&self, slug: &TeamSlug) -> Result<Option<Team>, RepositoryError> {
            Ok(self.by_slug.lock().unwrap().get(slug.as_str()).cloned())
        }
        async fn find_by_owner(&self, owner_user_id: &str) -> Result<Vec<Team>, RepositoryError> {
            Ok(self
                .by_slug
                .lock()
                .unwrap()
                .values()
                .filter(|t| t.owner_user_id == owner_user_id)
                .cloned()
                .collect())
        }
        async fn find_by_tenant_id(
            &self,
            tenant_id: &TenantId,
        ) -> Result<Option<Team>, RepositoryError> {
            Ok(self
                .by_slug
                .lock()
                .unwrap()
                .values()
                .find(|t| &t.tenant_id == tenant_id)
                .cloned())
        }
        async fn delete(&self, id: &TeamId) -> Result<(), RepositoryError> {
            self.by_slug.lock().unwrap().retain(|_, t| t.id != *id);
            Ok(())
        }
    }

    /// In-memory [`MembershipRepository`] for middleware tests.
    #[derive(Default)]
    struct InMemoryMembershipRepo {
        memberships: Mutex<Vec<Membership>>,
    }

    impl InMemoryMembershipRepo {
        fn insert(&self, m: Membership) {
            self.memberships.lock().unwrap().push(m);
        }
    }

    #[async_trait]
    impl MembershipRepository for InMemoryMembershipRepo {
        async fn save(&self, m: &Membership) -> Result<(), RepositoryError> {
            self.insert(m.clone());
            Ok(())
        }
        async fn find_by_team(&self, team_id: &TeamId) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.team_id == *team_id)
                .cloned()
                .collect())
        }
        async fn find_by_user(&self, user_id: &str) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.user_id == user_id)
                .cloned()
                .collect())
        }
        async fn find_active_for_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<Membership>, RepositoryError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.user_id == user_id && m.status == MembershipStatus::Active)
                .cloned()
                .collect())
        }
        async fn find_active_team_tenants_for_user(
            &self,
            user_id: &str,
        ) -> Result<Vec<String>, RepositoryError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.user_id == user_id && m.status == MembershipStatus::Active)
                .map(|m| {
                    crate::domain::team::TeamSlug::new(m.team_id)
                        .as_str()
                        .to_string()
                })
                .collect())
        }
        async fn is_active_member(
            &self,
            user_id: &str,
            team_id: &TeamId,
        ) -> Result<bool, RepositoryError> {
            Ok(self.memberships.lock().unwrap().iter().any(|m| {
                m.user_id == user_id
                    && m.team_id == *team_id
                    && m.status == MembershipStatus::Active
            }))
        }
        async fn count_active(&self, team_id: &TeamId) -> Result<u32, RepositoryError> {
            Ok(self
                .memberships
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.team_id == *team_id && m.status == MembershipStatus::Active)
                .count() as u32)
        }
        async fn revoke(&self, team_id: &TeamId, user_id: &str) -> Result<(), RepositoryError> {
            for m in self.memberships.lock().unwrap().iter_mut() {
                if m.team_id == *team_id && m.user_id == user_id {
                    m.status = MembershipStatus::Revoked;
                }
            }
            Ok(())
        }
    }

    /// Unused-in-middleware invitation repo — kept here so the in-memory
    /// fixtures mirror the full Team aggregate trio for future reuse.
    #[allow(dead_code)]
    struct InMemoryInvitationRepo;

    #[async_trait]
    impl TeamInvitationRepository for InMemoryInvitationRepo {
        async fn save(&self, _i: &TeamInvitation) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn find_by_id(
            &self,
            _id: &TeamInvitationId,
        ) -> Result<Option<TeamInvitation>, RepositoryError> {
            Ok(None)
        }
        async fn find_by_token_hash(
            &self,
            _t: &str,
        ) -> Result<Option<TeamInvitation>, RepositoryError> {
            Ok(None)
        }
        async fn find_pending_by_team(
            &self,
            _id: &TeamId,
        ) -> Result<Vec<TeamInvitation>, RepositoryError> {
            Ok(Vec::new())
        }
        async fn mark_accepted(&self, _id: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn mark_cancelled(&self, _id: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
        async fn mark_expired(&self, _id: &TeamInvitationId) -> Result<(), RepositoryError> {
            Ok(())
        }
    }

    // ----- Middleware integration tests -----

    fn consumer_identity(sub: &str) -> UserIdentity {
        UserIdentity {
            sub: sub.to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::consumer(),
            },
        }
    }

    fn operator_identity() -> UserIdentity {
        UserIdentity {
            sub: "op-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Admin,
            },
        }
    }

    fn service_account_identity() -> UserIdentity {
        UserIdentity {
            sub: "svc-1".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-temporal-worker".into(),
            },
        }
    }

    /// Build a minimal axum app with the middleware mounted. The inner
    /// handler extracts the resolved [`TenantId`] and echoes it back as the
    /// response body, which the test asserts against.
    fn build_app(state: TenantMiddlewareState, identity: Option<UserIdentity>) -> Router {
        let router = Router::new().route(
            "/v1/agents",
            get(|req: Request| async move {
                let tid = req
                    .extensions()
                    .get::<TenantId>()
                    .cloned()
                    .unwrap_or_else(TenantId::default);
                tid.as_str().to_string()
            }),
        );

        // Apply the tenant middleware first so it sits INSIDE the identity
        // injector (axum applies layers bottom-up — the last `.layer()` call
        // is the outermost). The injector must run first to populate
        // `UserIdentity` into request extensions before tenant_context_middleware
        // reads them, mirroring iam_auth_middleware's contract.
        let router = router.layer(axum::middleware::from_fn_with_state(
            state,
            tenant_context_middleware,
        ));

        if let Some(identity) = identity {
            router.layer(axum::middleware::from_fn(
                move |mut req: Request, next: Next| {
                    let identity = identity.clone();
                    async move {
                        req.extensions_mut().insert(identity);
                        next.run(req).await
                    }
                },
            ))
        } else {
            router
        }
    }

    fn state_with_repos(
        team_repo: Arc<dyn TeamRepository>,
        membership_repo: Arc<dyn MembershipRepository>,
    ) -> TenantMiddlewareState {
        TenantMiddlewareState {
            team_repo: Some(team_repo),
            membership_repo: Some(membership_repo),
            event_bus: Arc::new(EventBus::new(64)),
        }
    }

    async fn call(app: Router, req: HttpRequest<Body>) -> (StatusCode, String) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn consumer_without_header_falls_back_to_jwt_tenant() {
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, TenantId::consumer().as_str());
    }

    #[tokio::test]
    async fn consumer_with_header_for_active_team_overrides_tenant() {
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());

        let team = Team::provision(
            "Acme".into(),
            "user-1".into(),
            crate::domain::tenancy::TenantTier::Pro,
        )
        .unwrap();
        let slug = team.slug.as_str().to_string();
        let expected_tenant = team.tenant_id.as_str().to_string();
        let team_id = team.id;
        team_repo.insert(team);
        membership_repo.insert(Membership::new_active(
            team_id,
            "user-1".into(),
            MembershipRole::Owner,
        ));

        let state = state_with_repos(team_repo.clone(), membership_repo.clone());
        let app = build_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, expected_tenant);
    }

    #[tokio::test]
    async fn consumer_with_header_for_unrelated_team_is_forbidden() {
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());

        let team = Team::provision(
            "Someone else's team".into(),
            "other-user".into(),
            crate::domain::tenancy::TenantTier::Pro,
        )
        .unwrap();
        let slug = team.slug.as_str().to_string();
        team_repo.insert(team);

        let state = state_with_repos(team_repo, membership_repo);
        let app = build_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("not_a_team_member"));
    }

    #[tokio::test]
    async fn consumer_with_header_for_revoked_membership_is_forbidden() {
        // Regression: previously-active membership that has been revoked must
        // not allow tenant-context switching.
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());

        let team = Team::provision(
            "Acme".into(),
            "owner".into(),
            crate::domain::tenancy::TenantTier::Pro,
        )
        .unwrap();
        let slug = team.slug.as_str().to_string();
        let team_id = team.id;
        team_repo.insert(team);

        let mut m = Membership::new_active(team_id, "user-1".into(), MembershipRole::Member);
        m.status = MembershipStatus::Revoked;
        membership_repo.insert(m);

        let state = state_with_repos(team_repo, membership_repo);
        let app = build_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("not_a_team_member"));
    }

    #[tokio::test]
    async fn consumer_with_non_team_shaped_header_is_forbidden_tenant_switch() {
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", "u-some-other-user")
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("forbidden_tenant_switch"));
    }

    #[tokio::test]
    async fn service_account_with_x_tenant_id_preserves_adr_100_behavior() {
        // Regression guard: adding the consumer branch MUST NOT affect the
        // service-account delegation path (ADR-100).
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_app(state, Some(service_account_identity()));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", "u-some-tenant")
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "u-some-tenant");
    }

    // ----- ADR-111 Phase 4 colony suspension gate -----

    /// Build a team that is Active on the Business tier with `user-1` as the
    /// sole active owner-member. Returns (repo, membership_repo, team_slug,
    /// team_id, tenant_id).
    fn suspended_team_fixture() -> (
        Arc<InMemoryTeamRepo>,
        Arc<InMemoryMembershipRepo>,
        String,
        TeamId,
        TenantId,
    ) {
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());

        let mut team = Team::provision(
            "Acme".into(),
            "user-1".into(),
            crate::domain::tenancy::TenantTier::Business,
        )
        .unwrap();
        team.suspend().unwrap();
        let _ = team.take_events();
        let slug = team.slug.as_str().to_string();
        let team_id = team.id;
        let tenant_id = team.tenant_id.clone();
        team_repo.insert(team);
        membership_repo.insert(Membership::new_active(
            team_id,
            "user-1".into(),
            MembershipRole::Owner,
        ));
        (team_repo, membership_repo, slug, team_id, tenant_id)
    }

    /// Build a router whose test handler accepts arbitrary method + path,
    /// so we can exercise the suspension gate against billing / team /
    /// other routes.
    fn build_suspension_app(
        state: TenantMiddlewareState,
        identity: Option<UserIdentity>,
    ) -> Router {
        let handler = |req: Request| async move {
            let tid = req
                .extensions()
                .get::<TenantId>()
                .cloned()
                .unwrap_or_else(TenantId::default);
            tid.as_str().to_string()
        };

        let router = Router::new()
            .route("/v1/agents", get(handler))
            .route("/v1/billing/portal", axum::routing::post(handler))
            .route("/v1/teams", get(handler))
            .route("/v1/teams", axum::routing::post(handler))
            .layer(axum::middleware::from_fn_with_state(
                state,
                tenant_context_middleware,
            ));

        if let Some(identity) = identity {
            router.layer(axum::middleware::from_fn(
                move |mut req: Request, next: Next| {
                    let identity = identity.clone();
                    async move {
                        req.extensions_mut().insert(identity);
                        next.run(req).await
                    }
                },
            ))
        } else {
            router
        }
    }

    #[tokio::test]
    async fn suspended_team_tenant_request_returns_402() {
        let (team_repo, membership_repo, slug, _team_id, _tenant_id) = suspended_team_fixture();
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_suspension_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert!(
            body.contains("colony_suspended"),
            "expected colony_suspended in body, got: {body}"
        );
        assert!(body.contains("owner_subscription_below_business"));
    }

    #[tokio::test]
    async fn suspended_team_tenant_allows_billing_portal() {
        // Recovery allowlist: POST /v1/billing/portal must remain reachable
        // so the owner can restore a Business subscription.
        let (team_repo, membership_repo, slug, _team_id, tenant_id) = suspended_team_fixture();
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_suspension_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/v1/billing/portal")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "billing portal must bypass suspension gate, got {status} body={body}"
        );
        assert_eq!(body, tenant_id.as_str());
    }

    #[tokio::test]
    async fn suspended_team_tenant_allows_team_read() {
        // GET /v1/teams must remain reachable so the owner can see what is
        // affected by the suspension.
        let (team_repo, membership_repo, slug, _team_id, tenant_id) = suspended_team_fixture();
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_suspension_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/v1/teams")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, tenant_id.as_str());
    }

    #[tokio::test]
    async fn suspended_team_tenant_blocks_team_write() {
        // Write endpoints on /v1/teams are NOT allowlisted — a suspended
        // colony must refuse mutations until it is resumed.
        let (team_repo, membership_repo, slug, _team_id, _tenant_id) = suspended_team_fixture();
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_suspension_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/v1/teams")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, _body) = call(app, req).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn active_team_tenant_request_passes_gate() {
        // Regression guard: gate must only fire for Suspended teams —
        // Active teams pass through untouched.
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());
        let mut team = Team::provision(
            "Acme".into(),
            "user-1".into(),
            crate::domain::tenancy::TenantTier::Business,
        )
        .unwrap();
        let _ = team.take_events();
        let slug = team.slug.as_str().to_string();
        let tenant_id = team.tenant_id.clone();
        let team_id = team.id;
        team.status = TeamStatus::Active;
        team_repo.insert(team);
        membership_repo.insert(Membership::new_active(
            team_id,
            "user-1".into(),
            MembershipRole::Owner,
        ));

        let state = state_with_repos(team_repo, membership_repo);
        let app = build_suspension_app(state, Some(consumer_identity("user-1")));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-tenant-id", slug)
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, tenant_id.as_str());
    }

    #[tokio::test]
    async fn operator_with_x_aegis_tenant_still_overrides() {
        // Regression guard: the admin cross-tenant path must still function.
        let team_repo = Arc::new(InMemoryTeamRepo::default());
        let membership_repo = Arc::new(InMemoryMembershipRepo::default());
        let state = state_with_repos(team_repo, membership_repo);
        let app = build_app(state, Some(operator_identity()));

        let req = HttpRequest::builder()
            .uri("/v1/agents")
            .header("x-aegis-tenant", "u-target")
            .body(Body::empty())
            .unwrap();
        let (status, body) = call(app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "u-target");
    }
}
