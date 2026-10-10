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
use aegis_orchestrator_core::domain::iam::{
    resolve_effective_tenant, AegisRole, IdentityKind, UserIdentity, ZaruTier,
};
use aegis_orchestrator_core::domain::rate_limit::{
    tier_defaults, RateLimitBucket, RateLimitPolicyResolver, RateLimitResourceType, RateLimitScope,
};
use aegis_orchestrator_core::infrastructure::rate_limit::override_repository::{
    CreateOverrideRequest, RateLimitOverrideRow, UsageRow,
};
use aegis_orchestrator_core::infrastructure::rate_limit::policy_resolver::HierarchicalPolicyResolver;
use aegis_orchestrator_core::infrastructure::rate_limit::{
    PostgresWindowEnforcer, RateLimitOverrideRepository,
};

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
                "message": "Rate-limit administration spans all tenants and users and is restricted to operators.",
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

    match user_rate_limit_usage(&repo, &identity).await {
        Ok(items) => {
            let count = items.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({ "usage": items, "count": count })),
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

/// The caller's usage of every resource and window its policy names
/// (Zaru ADR-0064 D2).
///
/// The counters are read under the key every writer writes for this
/// identity: the identity's own tenant claim, as the execution, inner-loop
/// and SEAL writers bind it, through the enforcer's own
/// [`PostgresWindowEnforcer::scope_parts`]. Each window reads the sum of the
/// rows inside it. Per-minute is counted only in memory by the burst
/// enforcer, so no per-minute item is reported.
pub(crate) async fn user_rate_limit_usage(
    repo: &RateLimitOverrideRepository,
    identity: &UserIdentity,
) -> Result<Vec<UserRateLimitUsageItem>, sqlx::Error> {
    let tenant_id = resolve_effective_tenant(Some(identity), None);
    let scope = RateLimitScope::User {
        tenant_id: tenant_id.clone(),
        user_id: identity.sub.clone(),
    };
    let (scope_type, scope_id) = PostgresWindowEnforcer::scope_parts(&scope);

    let tier = match &identity.identity_kind {
        IdentityKind::ConsumerUser { zaru_tier, .. } => zaru_tier.clone(),
        _ => ZaruTier::Enterprise,
    };

    let now = Utc::now();
    let windows: Vec<(String, DateTime<Utc>)> = STORED_BUCKETS
        .iter()
        .map(|bucket| {
            (
                bucket_to_str(bucket),
                PostgresWindowEnforcer::window_lower_bound(now, bucket),
            )
        })
        .collect();
    let sums: std::collections::HashMap<(String, String), (i64, DateTime<Utc>)> = repo
        .window_usage(scope_type, &scope_id, &windows)
        .await?
        .into_iter()
        .map(|w| {
            (
                (w.resource_type, w.bucket),
                (w.total, w.oldest_window_start),
            )
        })
        .collect();

    let resolver = HierarchicalPolicyResolver::new(repo.pool().clone());

    // Seed from full tier policy so new users see all limits at zero
    let all_policies = tier_defaults(&tier);
    let mut items: Vec<UserRateLimitUsageItem> = Vec::new();

    for default_policy in &all_policies {
        let resource_type = &default_policy.resource_type;
        let resource_str = resource_type_to_db_str(resource_type);
        let policy = match resolver
            .resolve_policy(identity, &tenant_id, resource_type)
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
            if *bucket == RateLimitBucket::PerMinute {
                continue;
            }
            let bucket_str = bucket_to_str(bucket);
            let (current_count, resets_at) =
                match sums.get(&(resource_str.clone(), bucket_str.clone())) {
                    Some((total, oldest_window_start)) => {
                        (*total, window_expiry(*oldest_window_start, bucket))
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

    Ok(items)
}

/// The most model calls one `POST /v1/user/rate-limits/usage` records.
pub(crate) const USAGE_RECORD_MAX_LLM_CALLS: u64 = 1_000;
/// The most tokens one `POST /v1/user/rate-limits/usage` records.
pub(crate) const USAGE_RECORD_MAX_LLM_TOKENS: u64 = 50_000_000;

/// The body of `POST /v1/user/rate-limits/usage`: the model calls a client
/// made for the caller and the tokens they used (Zaru ADR-0064 D3).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserUsageRecordRequest {
    pub llm_calls: u64,
    pub llm_tokens: u64,
}

/// A window that refused a charge. A refused charge is stored in no window
/// of its resource.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RefusedWindow {
    pub resource_type: String,
    pub bucket: String,
}

/// `POST /v1/user/rate-limits/usage`: record model usage for the caller.
pub(crate) async fn record_user_rate_limit_usage_handler(
    State(state): State<Arc<AppState>>,
    identity: Option<Extension<UserIdentity>>,
    body: Result<Json<UserUsageRecordRequest>, JsonRejection>,
) -> axum::response::Response {
    user_usage_record_response(
        state.rate_limit_override_repo.as_deref(),
        identity.map(|Extension(id)| id),
        body,
    )
    .await
}

/// The answer of `POST /v1/user/rate-limits/usage` (Zaru ADR-0064 D3).
///
/// The caller is the identity its own access token resolved to, as for the
/// GET: none, or one with an empty `sub`, is refused 401. A body over the
/// request bound is refused 400. Otherwise the calls and tokens are charged
/// for the caller and the answer is 200 with the GET's usage view after the
/// write, and the windows that refused a charge under `refused`.
pub(crate) async fn user_usage_record_response(
    repo: Option<&RateLimitOverrideRepository>,
    identity: Option<UserIdentity>,
    body: Result<Json<UserUsageRecordRequest>, JsonRejection>,
) -> axum::response::Response {
    let Some(identity) = identity.filter(|id| !id.sub.trim().is_empty()) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Authentication required"})),
        )
            .into_response();
    };
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if request.llm_calls > USAGE_RECORD_MAX_LLM_CALLS
        || request.llm_tokens > USAGE_RECORD_MAX_LLM_TOKENS
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "usage over the request bound"})),
        )
            .into_response();
    }
    let Some(repo) = repo else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Rate-limit override repository not configured"})),
        )
            .into_response();
    };

    let refused = match record_user_usage(repo, &identity, &request).await {
        Ok(refused) => refused,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    match user_rate_limit_usage(repo, &identity).await {
        Ok(items) => {
            let count = items.len();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "usage": items,
                    "count": count,
                    "refused": refused,
                })),
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

/// Charge the caller's model calls as `llm_call` and their tokens as
/// `llm_token`, under the key every writer writes and the GET reads: the
/// identity's own tenant claim and its `sub`, through
/// [`PostgresWindowEnforcer::scope_parts`].
///
/// Each charge goes through the policy the writers resolve and the Postgres
/// window enforcer's `check_and_increment`, the stored half of the writers'
/// enforcer. A window that refuses a charge stores it in no window of its
/// resource, and is returned. Per-minute is the burst enforcer's, counted in
/// memory where the writers run and never stored, and is not charged here.
pub(crate) async fn record_user_usage(
    repo: &RateLimitOverrideRepository,
    identity: &UserIdentity,
    request: &UserUsageRecordRequest,
) -> Result<Vec<RefusedWindow>, aegis_orchestrator_core::domain::rate_limit::RateLimitError> {
    let tenant_id = resolve_effective_tenant(Some(identity), None);
    let scope = RateLimitScope::User {
        tenant_id: tenant_id.clone(),
        user_id: identity.sub.clone(),
    };
    let resolver = HierarchicalPolicyResolver::new(repo.pool().clone());
    let enforcer = PostgresWindowEnforcer::new(repo.pool().clone());

    let mut refused = Vec::new();
    for (resource_type, cost) in [
        (RateLimitResourceType::LlmCall, request.llm_calls),
        (RateLimitResourceType::LlmToken, request.llm_tokens),
    ] {
        if cost == 0 {
            continue;
        }
        let policy = resolver
            .resolve_policy(identity, &tenant_id, &resource_type)
            .await?;
        if let Err((bucket, _)) = enforcer.check_and_increment(&scope, &policy, cost).await {
            refused.push(RefusedWindow {
                resource_type: resource_type_to_db_str(&resource_type),
                bucket: bucket_to_str(&bucket),
            });
        }
    }
    Ok(refused)
}

/// The buckets the Postgres window enforcer stores; per-minute is not one.
const STORED_BUCKETS: [RateLimitBucket; 4] = [
    RateLimitBucket::Hourly,
    RateLimitBucket::Daily,
    RateLimitBucket::Weekly,
    RateLimitBucket::Monthly,
];

/// When the charge stored with this `window_start` leaves the bucket's
/// window: its charge time (`window_start + window`) plus the window.
fn window_expiry(window_start: DateTime<Utc>, bucket: &RateLimitBucket) -> DateTime<Utc> {
    window_start + chrono::Duration::seconds(2 * bucket.window_seconds() as i64)
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
#[path = "usage_meter_tests.rs"]
mod usage_meter_tests;

#[cfg(test)]
mod tests {
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
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, service_account, tenant_user,
    };
    use aegis_orchestrator_core::domain::iam::{
        AegisRole, IdentityKind, IdentityProvider, UserIdentity,
    };
    use aegis_orchestrator_core::infrastructure::rate_limit::override_repository::{
        CreateOverrideRequest, RateLimitOverrideRow, UsageRow,
    };
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

    /// Serve the admin sub-router over `store` beneath the daemon's
    /// authentication stack (see `test_support::serve`).
    async fn serve(store: Arc<RecordingStore>, iam: Option<Arc<dyn IdentityProvider>>) -> String {
        crate::daemon::handlers::test_support::serve(
            admin_rate_limit_router(AdminRateLimitState {
                store: Some(store as Arc<dyn RateLimitOverrideStore>),
            }),
            iam,
            None,
        )
        .await
    }

    /// The admin routes need no JWT scope; only the identity decides.
    fn provider(callers: &[(&str, UserIdentity)]) -> Arc<dyn IdentityProvider> {
        let rows: Vec<(&str, UserIdentity, &str)> =
            callers.iter().map(|(t, id)| (*t, id.clone(), "")).collect();
        identity_provider(&rows)
    }

    #[tokio::test]
    async fn admin_rate_limit_routes_refuse_non_operators_before_the_store() {
        let callers = [
            ("consumer-token", consumer("consumer-sub")),
            ("tenant-user-token", tenant_user("tenant-user-sub", "acme")),
            ("service-account-token", service_account()),
        ];
        let mut failures = Vec::new();
        for (token, identity) in &callers {
            for (label, method, path, body) in admin_routes() {
                let store = Arc::new(RecordingStore::default());
                let base = serve(store.clone(), Some(provider(&callers))).await;
                let (status, _) = send(&base, &method, &path, &body, Some(token)).await;
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
            let base = serve(store.clone(), Some(provider(&[]))).await;
            let (status, _) = send(&base, &method, &path, &body, None).await;
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
            let (status, _) = send(&base, &method, &path, &body, None).await;
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
                let base = serve(store.clone(), Some(provider(&callers))).await;
                let (status, _) = send(&base, &method, &path, &body, Some(token)).await;
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
