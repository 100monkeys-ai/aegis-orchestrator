// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Admin handlers: rate-limit override management (ADR-072).

use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use axum::{Json, Router};

use crate::daemon::state::AppState;
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
use aegis_orchestrator_core::domain::rate_limit::{
    tier_defaults, RateLimitBucket, RateLimitPolicyResolver, RateLimitResourceType,
};
use aegis_orchestrator_core::infrastructure::rate_limit::override_repository::{
    CreateOverrideRequest, RateLimitOverrideRow, UsageRow,
};
use aegis_orchestrator_core::infrastructure::rate_limit::policy_resolver::HierarchicalPolicyResolver;
use aegis_orchestrator_core::infrastructure::rate_limit::RateLimitOverrideRepository;

use super::tenant_id_from_identity;
use axum::Extension;
use chrono::{DateTime, Utc};

/// Persistence port the `/v1/admin/rate-limits/*` handlers depend on
/// (ADR-072 §7). Implemented by the Postgres-backed
/// [`RateLimitOverrideRepository`]; narrowed to the four operations the
/// admin surface performs so the handlers can be driven through the real
/// router with a recording double.
#[async_trait::async_trait]
pub(crate) trait RateLimitOverrideStore: Send + Sync {
    async fn list(
        &self,
        tenant_id: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<Vec<RateLimitOverrideRow>, sqlx::Error>;
    async fn upsert(
        &self,
        req: &CreateOverrideRequest,
    ) -> Result<RateLimitOverrideRow, sqlx::Error>;
    async fn delete(&self, id: uuid::Uuid) -> Result<bool, sqlx::Error>;
    async fn get_usage(
        &self,
        scope_type: &str,
        scope_id: &str,
    ) -> Result<Vec<UsageRow>, sqlx::Error>;
}

#[async_trait::async_trait]
impl RateLimitOverrideStore for RateLimitOverrideRepository {
    async fn list(
        &self,
        tenant_id: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<Vec<RateLimitOverrideRow>, sqlx::Error> {
        RateLimitOverrideRepository::list(self, tenant_id, user_id).await
    }
    async fn upsert(
        &self,
        req: &CreateOverrideRequest,
    ) -> Result<RateLimitOverrideRow, sqlx::Error> {
        RateLimitOverrideRepository::upsert(self, req).await
    }
    async fn delete(&self, id: uuid::Uuid) -> Result<bool, sqlx::Error> {
        RateLimitOverrideRepository::delete(self, id).await
    }
    async fn get_usage(
        &self,
        scope_type: &str,
        scope_id: &str,
    ) -> Result<Vec<UsageRow>, sqlx::Error> {
        RateLimitOverrideRepository::get_usage(self, scope_type, scope_id).await
    }
}

/// State of the admin rate-limit sub-router.
#[derive(Clone)]
pub(crate) struct AdminRateLimitState {
    /// `None` when the node has no Postgres pool; every route then answers
    /// 503 to an authorised caller.
    pub(crate) store: Option<Arc<dyn RateLimitOverrideStore>>,
}

/// The `/v1/admin/rate-limits/*` routes (ADR-072, ADR-073 §9). Merged into
/// the daemon router by `router::create_router`, beneath the same
/// authentication layers as every other route.
pub(crate) fn admin_rate_limit_router(state: AdminRateLimitState) -> Router {
    Router::new()
        .route(
            "/v1/admin/rate-limits/overrides",
            get(list_rate_limit_overrides_handler).post(upsert_rate_limit_override_handler),
        )
        .route(
            "/v1/admin/rate-limits/overrides/{id}",
            delete(delete_rate_limit_override_handler),
        )
        .route(
            "/v1/admin/rate-limits/usage",
            get(get_rate_limit_usage_handler),
        )
        .with_state(state)
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ListOverridesQuery {
    tenant_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UsageQuery {
    scope_type: String,
    scope_id: String,
}

/// What a caller is asking to do on the admin rate-limit surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateLimitAdminAccess {
    /// List overrides or read usage counters for any tenant or user.
    Read,
    /// Create, change or delete an override for any tenant or user.
    Write,
}

/// Operator-only gate for `/v1/admin/rate-limits/*`.
///
/// These routes read and write overrides and usage counters for every
/// tenant and user on the platform, so they are platform administration in
/// the sense of ADR-073 §2: cross-tenant, operator-only. The orchestrator's
/// check is the authoritative one (ADR-073 §3d); the Zaru client's
/// `/admin` gating is a UX guard, not a security boundary.
///
/// - No identity: 401. A route reached without a `UserIdentity` (no bearer
///   token, or a node configured without `spec.iam`, where no IAM layer is
///   mounted) is refused, never served.
/// - Consumer users, tenant users and service accounts: 403
///   `operator_required`. Service accounts are deliberately not operators
///   (see `handlers::is_operator`).
/// - `aegis:readonly` operators may read and may not write (ADR-073 §3e);
///   `aegis:admin` and `aegis:operator` may both. Overrides are tenant- and
///   user-scoped values, which ADR-073 §5b and §12 put within an operator's
///   reach; the tier defaults they override are code, not an editable
///   global layer.
///
/// The refusal bodies match the repository's existing ones for the same
/// refusals: `observability.rs` for a non-operator and `credentials.rs`
/// for a missing identity and for a role that may not write.
#[allow(clippy::result_large_err)]
fn authorize_rate_limit_admin(
    identity: Option<&UserIdentity>,
    access: RateLimitAdminAccess,
) -> Result<(), axum::response::Response> {
    let Some(identity) = identity else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Authentication required"})),
        )
            .into_response());
    };
    match (&identity.identity_kind, access) {
        (IdentityKind::Operator { .. }, RateLimitAdminAccess::Read) => Ok(()),
        (
            IdentityKind::Operator {
                aegis_role: AegisRole::Admin | AegisRole::Operator,
            },
            RateLimitAdminAccess::Write,
        ) => Ok(()),
        (IdentityKind::Operator { .. }, RateLimitAdminAccess::Write) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Operator or Admin role required"})),
        )
            .into_response()),
        _ => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "operator_required",
                "message": "Rate-limit administration spans all tenants and users and is restricted to operators (ADR-073).",
            })),
        )
            .into_response()),
    }
}

fn store_not_configured() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
    )
        .into_response()
}

fn internal_error(e: sqlx::Error) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": e.to_string()})),
    )
        .into_response()
}

pub(crate) async fn list_rate_limit_overrides_handler(
    State(state): State<AdminRateLimitState>,
    identity: Option<Extension<UserIdentity>>,
    params: Result<Query<ListOverridesQuery>, QueryRejection>,
) -> axum::response::Response {
    if let Err(refusal) = authorize_rate_limit_admin(
        identity.as_ref().map(|Extension(id)| id),
        RateLimitAdminAccess::Read,
    ) {
        return refusal;
    }
    let Query(params) = match params {
        Ok(p) => p,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(store) = state.store.clone() else {
        return store_not_configured();
    };

    match store
        .list(params.tenant_id.as_deref(), params.user_id.as_deref())
        .await
    {
        Ok(overrides) => {
            let count = overrides.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "overrides": overrides,
                    "count": count,
                })),
            )
                .into_response()
        }
        Err(e) => internal_error(e),
    }
}

pub(crate) async fn upsert_rate_limit_override_handler(
    State(state): State<AdminRateLimitState>,
    identity: Option<Extension<UserIdentity>>,
    payload: Result<Json<CreateOverrideRequest>, JsonRejection>,
) -> axum::response::Response {
    if let Err(refusal) = authorize_rate_limit_admin(
        identity.as_ref().map(|Extension(id)| id),
        RateLimitAdminAccess::Write,
    ) {
        return refusal;
    }
    let Json(payload) = match payload {
        Ok(p) => p,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(store) = state.store.clone() else {
        return store_not_configured();
    };

    // Validate: exactly one of tenant_id or user_id must be set (matches DB constraint)
    if payload.tenant_id.is_some() == payload.user_id.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "Exactly one of tenant_id or user_id must be provided"}),
            ),
        )
            .into_response();
    }

    match store.upsert(&payload).await {
        Ok(row) => (
            StatusCode::OK,
            Json(serde_json::to_value(&row).unwrap_or(serde_json::json!({"status": "upserted"}))),
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}

pub(crate) async fn delete_rate_limit_override_handler(
    State(state): State<AdminRateLimitState>,
    identity: Option<Extension<UserIdentity>>,
    id: Result<Path<uuid::Uuid>, PathRejection>,
) -> axum::response::Response {
    if let Err(refusal) = authorize_rate_limit_admin(
        identity.as_ref().map(|Extension(id)| id),
        RateLimitAdminAccess::Write,
    ) {
        return refusal;
    }
    let Path(id) = match id {
        Ok(p) => p,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(store) = state.store.clone() else {
        return store_not_configured();
    };

    match store.delete(id).await {
        Ok(true) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "deleted", "id": id.to_string()})),
        )
            .into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Override not found"})),
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}

pub(crate) async fn get_rate_limit_usage_handler(
    State(state): State<AdminRateLimitState>,
    identity: Option<Extension<UserIdentity>>,
    params: Result<Query<UsageQuery>, QueryRejection>,
) -> axum::response::Response {
    if let Err(refusal) = authorize_rate_limit_admin(
        identity.as_ref().map(|Extension(id)| id),
        RateLimitAdminAccess::Read,
    ) {
        return refusal;
    }
    let Query(params) = match params {
        Ok(p) => p,
        Err(rejection) => return rejection.into_response(),
    };
    let Some(store) = state.store.clone() else {
        return store_not_configured();
    };

    match store.get_usage(&params.scope_type, &params.scope_id).await {
        Ok(rows) => {
            let count = rows.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "usage": rows,
                    "count": count,
                })),
            )
                .into_response()
        }
        Err(e) => internal_error(e),
    }
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct UserRateLimitUsageItem {
    pub resource_type: String,
    pub bucket: String,
    pub current_count: i64,
    pub limit_value: i64,
    pub window_seconds: u64,
    pub resets_at: DateTime<Utc>,
}

pub(crate) async fn get_user_rate_limit_usage_handler(
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

    let repo = match &state.rate_limit_override_repo {
        Some(r) => r.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
            )
                .into_response();
        }
    };

    let tenant_id = tenant_id_from_identity(Some(&identity));

    let tier = match &identity.identity_kind {
        IdentityKind::ConsumerUser { zaru_tier, .. } => zaru_tier.clone(),
        _ => ZaruTier::Enterprise,
    };

    let usage_rows = match repo.get_usage("user", &identity.sub).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    // Build lookup: (resource_type_str, bucket_str) -> (counter, window_start)
    let mut counter_map: std::collections::HashMap<
        (String, String),
        (i64, chrono::DateTime<chrono::Utc>),
    > = std::collections::HashMap::new();
    for row in &usage_rows {
        // Rows are ordered window_start DESC; use or_insert so the first (newest)
        // entry for each (resource_type, bucket) key wins.
        counter_map
            .entry((row.resource_type.clone(), row.bucket.clone()))
            .or_insert((row.counter, row.window_start));
    }

    let resolver = HierarchicalPolicyResolver::new(repo.pool().clone());
    let now = chrono::Utc::now();

    // Seed from full tier policy so new users see all limits at zero
    let all_policies = tier_defaults(&tier);
    let mut items: Vec<UserRateLimitUsageItem> = Vec::new();

    for default_policy in &all_policies {
        let resource_type = &default_policy.resource_type;
        let resource_str = resource_type_to_db_str(resource_type);
        let policy = match resolver
            .resolve_policy(&identity, &tenant_id, resource_type)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    resource_type = %resource_str,
                    error = %e,
                    "Failed to resolve rate-limit policy; skipping resource type"
                );
                continue;
            }
        };
        for (bucket, window) in &policy.windows {
            let bucket_str = bucket_to_str(bucket);
            let (current_count, resets_at) =
                match counter_map.get(&(resource_str.clone(), bucket_str.clone())) {
                    Some((count, window_start)) => {
                        let resets_at =
                            *window_start + chrono::Duration::seconds(window.window_seconds as i64);
                        (*count, resets_at)
                    }
                    None => (
                        0i64,
                        now + chrono::Duration::seconds(window.window_seconds as i64),
                    ),
                };
            items.push(UserRateLimitUsageItem {
                resource_type: resource_str.clone(),
                bucket: bucket_str,
                current_count,
                limit_value: window.limit as i64,
                window_seconds: window.window_seconds,
                resets_at,
            });
        }
    }

    let count = items.len();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "usage": items, "count": count })),
    )
        .into_response()
}

fn bucket_to_str(bucket: &RateLimitBucket) -> String {
    match bucket {
        RateLimitBucket::PerMinute => "per_minute".into(),
        RateLimitBucket::Hourly => "hourly".into(),
        RateLimitBucket::Daily => "daily".into(),
        RateLimitBucket::Weekly => "weekly".into(),
        RateLimitBucket::Monthly => "monthly".into(),
    }
}

fn resource_type_to_db_str(rt: &RateLimitResourceType) -> String {
    match rt {
        RateLimitResourceType::AgentExecution => "agent_execution".into(),
        RateLimitResourceType::WorkflowExecution => "workflow_execution".into(),
        RateLimitResourceType::LlmCall => "llm_call".into(),
        RateLimitResourceType::LlmToken => "llm_token".into(),
        RateLimitResourceType::SealToolCall { tool_pattern } => {
            format!("seal_tool:{tool_pattern}")
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn counter_map_dedup_uses_newest_window() {
        use chrono::{Duration, TimeZone, Utc};
        use std::collections::HashMap;

        let now = Utc.with_ymd_and_hms(2026, 4, 10, 12, 0, 0).unwrap();
        let older = now - Duration::days(1);
        let oldest = now - Duration::days(2);

        // Simulate rows returned ORDER BY window_start DESC (newest first)
        let rows: Vec<(String, String, i64, chrono::DateTime<Utc>)> = vec![
            ("agent_execution".into(), "daily".into(), 42, now),
            ("agent_execution".into(), "daily".into(), 10, older),
            ("agent_execution".into(), "daily".into(), 5, oldest),
        ];

        let mut counter_map: HashMap<(String, String), (i64, chrono::DateTime<Utc>)> =
            HashMap::new();
        for (resource_type, bucket, counter, window_start) in &rows {
            counter_map
                .entry((resource_type.clone(), bucket.clone()))
                .or_insert((*counter, *window_start));
        }

        let (count, ws) = counter_map[&("agent_execution".into(), "daily".into())];
        assert_eq!(count, 42, "must use newest window counter, not stale one");
        assert_eq!(ws, now, "must use newest window_start");
    }

    // ------------------------------------------------------------------
    // Operator-only gate on `/v1/admin/rate-limits/*` (ADR-072, ADR-073
    // §3d/§3e). These routes read and write overrides and usage for every
    // tenant and user, so only an `IdentityKind::Operator` may reach the
    // store: any operator role may read, only `Admin` or `Operator` may
    // write, and a request carrying no identity is refused.
    //
    // Driven through the daemon's own `apply_request_auth_layers` (tenant
    // context middleware + IAM middleware) over a loopback listener, with a
    // stub `IdentityProvider` resolving bearer tokens to identities and a
    // recording store counting every call that reaches persistence.
    // ------------------------------------------------------------------

    use super::{admin_rate_limit_router, AdminRateLimitState, RateLimitOverrideStore};
    use crate::daemon::router::apply_request_auth_layers;
    use aegis_orchestrator_core::domain::iam::{
        AegisRole, IamError, IdentityKind, IdentityProvider, IdentityRealm, UserIdentity,
        ValidatedIdentityToken, ZaruTier,
    };
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use aegis_orchestrator_core::infrastructure::rate_limit::override_repository::{
        CreateOverrideRequest, RateLimitOverrideRow, UsageRow,
    };
    use aegis_orchestrator_core::presentation::tenant_middleware::TenantMiddlewareState;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct RecordingStore {
        calls: AtomicUsize,
    }

    impl RecordingStore {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl RateLimitOverrideStore for RecordingStore {
        async fn list(
            &self,
            _tenant_id: Option<&str>,
            _user_id: Option<&str>,
        ) -> Result<Vec<RateLimitOverrideRow>, sqlx::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
        async fn upsert(
            &self,
            req: &CreateOverrideRequest,
        ) -> Result<RateLimitOverrideRow, sqlx::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = chrono::Utc::now();
            Ok(RateLimitOverrideRow {
                id: uuid::Uuid::nil(),
                tenant_id: req.tenant_id.clone(),
                user_id: req.user_id.clone(),
                resource_type: req.resource_type.clone(),
                bucket: req.bucket.clone(),
                limit_value: req.limit_value,
                burst_value: req.burst_value,
                created_at: now,
                updated_at: now,
            })
        }
        async fn delete(&self, _id: uuid::Uuid) -> Result<bool, sqlx::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
        async fn get_usage(
            &self,
            _scope_type: &str,
            _scope_id: &str,
        ) -> Result<Vec<UsageRow>, sqlx::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    /// Resolves a bearer token to a fixed identity, the way
    /// `StandardIamService` resolves a validated JWT. Unknown tokens fail
    /// validation.
    struct TokenTableIdentityProvider {
        identities: HashMap<String, UserIdentity>,
    }

    #[async_trait::async_trait]
    impl IdentityProvider for TokenTableIdentityProvider {
        async fn validate_token(&self, raw_jwt: &str) -> Result<ValidatedIdentityToken, IamError> {
            let identity =
                self.identities
                    .get(raw_jwt)
                    .cloned()
                    .ok_or_else(|| IamError::MissingClaim {
                        claim: "sub".to_string(),
                    })?;
            Ok(ValidatedIdentityToken {
                identity,
                issued_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
                raw_claims: serde_json::json!({}),
            })
        }
        fn resolve_tier(&self, _token: &ValidatedIdentityToken) -> Result<ZaruTier, IamError> {
            Ok(ZaruTier::Free)
        }
        fn resolve_role(&self, _token: &ValidatedIdentityToken) -> Result<AegisRole, IamError> {
            Err(IamError::MissingClaim {
                claim: "aegis_role".to_string(),
            })
        }
        fn known_realms(&self) -> Vec<IdentityRealm> {
            Vec::new()
        }
    }

    fn operator(role: AegisRole) -> UserIdentity {
        UserIdentity {
            sub: format!("op-{}", role.as_claim_str()),
            realm_slug: "aegis-system".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator { aegis_role: role },
        }
    }

    fn free_consumer() -> UserIdentity {
        UserIdentity {
            sub: "consumer-sub".into(),
            realm_slug: "zaru-consumer".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: TenantId::for_consumer_user("consumer-sub").expect("per-user tenant id"),
            },
        }
    }

    fn tenant_user() -> UserIdentity {
        UserIdentity {
            sub: "tenant-user-sub".into(),
            realm_slug: "tenant-acme".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "acme".into(),
            },
        }
    }

    fn service_account() -> UserIdentity {
        UserIdentity {
            sub: "svc-sub".into(),
            realm_slug: "aegis-system".into(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-temporal-worker".into(),
            },
        }
    }

    /// The four admin routes, each as (label, method, path, json body).
    fn admin_routes() -> Vec<(
        &'static str,
        reqwest::Method,
        String,
        Option<serde_json::Value>,
    )> {
        vec![
            (
                "GET overrides",
                reqwest::Method::GET,
                "/v1/admin/rate-limits/overrides".to_string(),
                None,
            ),
            (
                "POST overrides",
                reqwest::Method::POST,
                "/v1/admin/rate-limits/overrides".to_string(),
                Some(serde_json::json!({
                    "tenant_id": "u-victim",
                    "resource_type": "agent_execution",
                    "bucket": "daily",
                    "limit_value": 1_000_000,
                })),
            ),
            (
                "DELETE override",
                reqwest::Method::DELETE,
                format!("/v1/admin/rate-limits/overrides/{}", uuid::Uuid::new_v4()),
                None,
            ),
            (
                "GET usage",
                reqwest::Method::GET,
                "/v1/admin/rate-limits/usage?scope_type=user&scope_id=victim".to_string(),
                None,
            ),
        ]
    }

    fn is_write(method: &reqwest::Method) -> bool {
        *method != reqwest::Method::GET
    }

    /// Serve the admin sub-router beneath the daemon's authentication stack
    /// on a loopback port. `iam` of `None` reproduces a node configured
    /// without `spec.iam`, where no authentication layer is mounted.
    async fn serve(store: Arc<RecordingStore>, iam: Option<Arc<dyn IdentityProvider>>) -> String {
        let app = apply_request_auth_layers(
            admin_rate_limit_router(AdminRateLimitState {
                store: Some(store as Arc<dyn RateLimitOverrideStore>),
            }),
            TenantMiddlewareState {
                team_repo: None,
                membership_repo: None,
                event_bus: Arc::new(EventBus::new(16)),
            },
            iam,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve admin router");
        });
        format!("http://{addr}")
    }

    fn identity_provider(identities: &[(&str, UserIdentity)]) -> Arc<dyn IdentityProvider> {
        Arc::new(TokenTableIdentityProvider {
            identities: identities
                .iter()
                .map(|(token, id)| (token.to_string(), id.clone()))
                .collect(),
        })
    }

    async fn send(
        base: &str,
        method: &reqwest::Method,
        path: &str,
        body: &Option<serde_json::Value>,
        bearer: Option<&str>,
    ) -> u16 {
        let mut req = reqwest::Client::new().request(method.clone(), format!("{base}{path}"));
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        if let Some(json) = body {
            req = req.json(json);
        }
        req.send()
            .await
            .expect("loopback request")
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn admin_rate_limit_routes_refuse_non_operators_before_the_store() {
        let callers = [
            ("consumer-token", free_consumer()),
            ("tenant-user-token", tenant_user()),
            ("service-account-token", service_account()),
        ];
        let mut failures = Vec::new();
        for (token, identity) in &callers {
            for (label, method, path, body) in admin_routes() {
                let store = Arc::new(RecordingStore::default());
                let base = serve(store.clone(), Some(identity_provider(&callers))).await;
                let status = send(&base, &method, &path, &body, Some(token)).await;
                if status != 403 || store.calls() != 0 {
                    failures.push(format!(
                        "{label} as {:?} answered {status} after {} store call(s); expected 403 and none",
                        identity.identity_kind,
                        store.calls()
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "a non-operator reached cross-tenant rate-limit administration:\n{}",
            failures.join("\n")
        );
    }

    #[tokio::test]
    async fn admin_rate_limit_routes_refuse_a_request_with_no_identity() {
        let mut failures = Vec::new();
        for (label, method, path, body) in admin_routes() {
            // Full stack, no bearer token: the IAM layer refuses.
            let store = Arc::new(RecordingStore::default());
            let base = serve(store.clone(), Some(identity_provider(&[]))).await;
            let status = send(&base, &method, &path, &body, None).await;
            if status != 401 || store.calls() != 0 {
                failures.push(format!(
                    "{label} without a bearer token answered {status} after {} store call(s); expected 401 and none",
                    store.calls()
                ));
            }
            // No IAM layer mounted (`spec.iam` absent): the handler itself
            // must refuse, because no identity reaches it.
            let store = Arc::new(RecordingStore::default());
            let base = serve(store.clone(), None).await;
            let status = send(&base, &method, &path, &body, None).await;
            if status != 401 || store.calls() != 0 {
                failures.push(format!(
                    "{label} with no IAM layer and no identity answered {status} after {} store call(s); expected 401 and none",
                    store.calls()
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "an unauthenticated request reached rate-limit administration:\n{}",
            failures.join("\n")
        );
    }

    #[tokio::test]
    async fn admin_rate_limit_routes_serve_operators_by_role() {
        let callers = [
            ("admin-token", operator(AegisRole::Admin)),
            ("operator-token", operator(AegisRole::Operator)),
            ("readonly-token", operator(AegisRole::Readonly)),
        ];
        let mut failures = Vec::new();
        for (token, identity) in &callers {
            let read_only = matches!(
                identity.identity_kind,
                IdentityKind::Operator {
                    aegis_role: AegisRole::Readonly
                }
            );
            for (label, method, path, body) in admin_routes() {
                let store = Arc::new(RecordingStore::default());
                let base = serve(store.clone(), Some(identity_provider(&callers))).await;
                let status = send(&base, &method, &path, &body, Some(token)).await;
                let (want_status, want_calls) = if read_only && is_write(&method) {
                    (403, 0)
                } else {
                    (200, 1)
                };
                if status != want_status || store.calls() != want_calls {
                    failures.push(format!(
                        "{label} as {:?} answered {status} after {} store call(s); expected {want_status} and {want_calls}",
                        identity.identity_kind,
                        store.calls()
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "an operator was served incorrectly by rate-limit administration:\n{}",
            failures.join("\n")
        );
    }
}
