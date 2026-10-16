// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! API key management handlers (ADR-093).
//!
//! Routes:
//!   GET    /v1/api-keys           — list keys for authenticated user
//!   POST   /v1/api-keys           — create a new key (returns key value once)
//!   PATCH  /v1/api-keys/:id       — set or clear the key's profile
//!   DELETE /v1/api-keys/:id       — revoke a key
//!   POST   /v1/api-keys/validate  — validate a key and return user identity (MCP server auth)

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Extension;
use axum::Json;
use chrono::{DateTime, Utc};
use rand::Rng;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use aegis_orchestrator_core::application::profile_service::ProfileService;
use aegis_orchestrator_core::domain::iam::{resolve_effective_tenant, IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::{
    ApiKeyRow, CreateApiKeyRow,
};
use aegis_orchestrator_core::infrastructure::repositories::PostgresApiKeyRepository;
use aegis_orchestrator_core::presentation::keycloak_auth::ScopeGuard;

use crate::daemon::handlers::tenant_id_from_identity;
use crate::daemon::state::AppState;

// ── Request / Response DTOs ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub(crate) struct CreateApiKeyRequest {
    pub name: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// The profile chosen for the key (Zaru ADR-0071 D3), one of the
    /// caller's own; absent or `null` chooses none, and the key reaches none
    /// of the caller's connections.
    #[serde(default)]
    pub profile: Option<Uuid>,
}

/// The refusal of a `profile` that is not one of the caller's profiles, or
/// a PATCH body that names no `profile` or names anything but an id or
/// `null` (Zaru ADR-0071 D3).
pub(crate) const PROFILE_REFUSAL: &str =
    "'profile' must be the id of one of your profiles, or null.";

// ── The stores the answers read ──────────────────────────────────────────────

/// The `api_keys` queries the routes make; implemented by
/// [`PostgresApiKeyRepository`], substituted by a table in tests.
#[async_trait::async_trait]
pub(crate) trait ApiKeyStore: Send + Sync {
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<ApiKeyRow>, String>;
    async fn create(&self, row: &CreateApiKeyRow) -> Result<ApiKeyRow, String>;
    /// Set or clear the profile of `user_id`'s active key `id`; `false`
    /// when there is no such key.
    async fn set_profile(
        &self,
        id: Uuid,
        user_id: &str,
        profile_id: Option<Uuid>,
    ) -> Result<bool, String>;
}

#[async_trait::async_trait]
impl ApiKeyStore for PostgresApiKeyRepository {
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<ApiKeyRow>, String> {
        PostgresApiKeyRepository::list_for_user(self, user_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn create(&self, row: &CreateApiKeyRow) -> Result<ApiKeyRow, String> {
        PostgresApiKeyRepository::create(self, row)
            .await
            .map_err(|e| e.to_string())
    }

    async fn set_profile(
        &self,
        id: Uuid,
        user_id: &str,
        profile_id: Option<Uuid>,
    ) -> Result<bool, String> {
        PostgresApiKeyRepository::set_profile(self, id, user_id, profile_id)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The caller's own profiles, by id and name (AEGIS ADR-140 D3's listing):
/// what a key may carry, and the names a key's profile is listed with.
#[async_trait::async_trait]
pub(crate) trait KeyProfiles: Send + Sync {
    async fn owned_profiles(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
    ) -> Result<Vec<(Uuid, String)>, String>;
}

#[async_trait::async_trait]
impl KeyProfiles for ProfileService {
    async fn owned_profiles(
        &self,
        owner: &UserIdentity,
        tenant: &TenantId,
    ) -> Result<Vec<(Uuid, String)>, String> {
        self.list(owner, tenant)
            .await
            .map(|views| {
                views
                    .into_iter()
                    .map(|view| (view.profile.id.0, view.profile.name))
                    .collect()
            })
            .map_err(|e| e.to_string())
    }
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Project a `UserIdentity` onto the (`aegis_role`, `zaru_tier`) column
/// values stored on `api_keys`.
///
/// The schema contract — established by the seal-side reader
/// `identity_from_api_key`, which parses the column back through
/// `AegisRole::from_claim` / `ZaruTier::from_claim` — is that both columns
/// hold the **bare claim string**:
///
/// | Identity                          | `aegis_role`        | `zaru_tier` |
/// | --------------------------------- | ------------------- | ----------- |
/// | `Operator { aegis_role }`         | `aegis:admin`/…     | `None`      |
/// | `ConsumerUser { zaru_tier, .. }`  | `None`              | `free`/`pro`/… |
/// | other                             | `None`              | `None`      |
///
/// **Anti-pattern (the bug this helper exists to prevent):** storing
/// `zaru_tier.to_security_context_name()` (`"zaru-free"`/`"zaru-pro"`/…)
/// in the column. That value cannot be parsed by `ZaruTier::from_claim`
/// on read-back and silently degrades to the default `ZaruTier::Free`,
/// then gets a second `zaru-` prefix downstream → `"zaru-zaru-free"`
/// SecurityContext lookup → 401.
fn identity_claim_columns(identity: &UserIdentity) -> (Option<String>, Option<String>) {
    match &identity.identity_kind {
        IdentityKind::Operator { aegis_role } => {
            (Some(aegis_role.as_claim_str().to_string()), None)
        }
        IdentityKind::ConsumerUser { zaru_tier, .. } => {
            (None, Some(zaru_tier.to_claim_str().to_string()))
        }
        _ => (None, None),
    }
}

/// Generate a new API key: `aegis_` prefix + 40 random alphanumeric chars.
fn generate_api_key() -> String {
    let mut rng = rand::rng();
    let suffix: String = (0..40)
        .map(|_| {
            let idx = rng.random_range(0..62u8);
            if idx < 10 {
                (b'0' + idx) as char
            } else if idx < 36 {
                (b'a' + idx - 10) as char
            } else {
                (b'A' + idx - 36) as char
            }
        })
        .collect();
    format!("aegis_{suffix}")
}

/// SHA-256 hex digest of the raw key value.
pub(crate) fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// The tenant the caller's profiles live in: the request's, else the
/// caller's own (as the profile routes read it).
fn profile_tenant(identity: &UserIdentity, tenant: Option<&TenantId>) -> TenantId {
    tenant
        .cloned()
        .unwrap_or_else(|| tenant_id_from_identity(Some(identity)))
}

fn unavailable() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "API key repository not configured"})),
    )
        .into_response()
}

/// The caller's profiles by id, or the answer to give when they cannot be
/// read. A node without profiles has none.
async fn profile_names(
    profiles: Option<&dyn KeyProfiles>,
    owner: &UserIdentity,
    tenant: &TenantId,
) -> Result<HashMap<Uuid, String>, (StatusCode, serde_json::Value)> {
    match profiles {
        Some(profiles) => profiles
            .owned_profiles(owner, tenant)
            .await
            .map(|owned| owned.into_iter().collect())
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error": e}),
                )
            }),
        None => Ok(HashMap::new()),
    }
}

/// A key as `GET /v1/api-keys` lists it: never its value or hash; its
/// profile by id, and by name while that profile is still the caller's
/// (a deleted one is listed by id with no name, and reaches none).
fn key_view(row: &ApiKeyRow, names: &HashMap<Uuid, String>) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "name": row.name,
        "scopes": row.scopes,
        "expires_at": row.expires_at,
        "last_used_at": row.last_used_at,
        "created_at": row.created_at,
        "status": row.status,
        "profile": row.profile_id,
        "profile_name": row.profile_id.and_then(|id| names.get(&id).cloned()),
    })
}

/// A `profile` a key is to carry: `None`, or one of the caller's profiles;
/// anything else is refused with [`PROFILE_REFUSAL`] before anything is
/// stored.
fn checked_profile(
    profile: Option<Uuid>,
    names: &HashMap<Uuid, String>,
) -> Result<Option<Uuid>, (StatusCode, serde_json::Value)> {
    match profile {
        Some(id) if !names.contains_key(&id) => Err((
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": PROFILE_REFUSAL}),
        )),
        chosen => Ok(chosen),
    }
}

/// `GET /v1/api-keys`'s answer: the caller's keys, newest first, each with
/// its profile (Zaru ADR-0071 D3, D6).
pub(crate) async fn list_answer(
    store: &dyn ApiKeyStore,
    profiles: Option<&dyn KeyProfiles>,
    owner: &UserIdentity,
    tenant: &TenantId,
) -> (StatusCode, serde_json::Value) {
    let rows = match store.list_for_user(&owner.sub).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({"error": e}),
            )
        }
    };
    let names = match profile_names(profiles, owner, tenant).await {
        Ok(names) => names,
        Err(answer) => return answer,
    };
    let keys: Vec<serde_json::Value> = rows.iter().map(|r| key_view(r, &names)).collect();
    (StatusCode::OK, serde_json::json!({ "api_keys": keys }))
}

/// `POST /v1/api-keys`'s answer: the key made, its value answered this once
/// and never stored, carrying the `profile` chosen for it (Zaru ADR-0071
/// D3), which must be one of the caller's own.
pub(crate) async fn create_answer(
    store: &dyn ApiKeyStore,
    profiles: Option<&dyn KeyProfiles>,
    identity: &UserIdentity,
    tenant: &TenantId,
    payload: CreateApiKeyRequest,
) -> (StatusCode, serde_json::Value) {
    let name_len = payload.name.len();
    if name_len == 0 || name_len > 64 {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "name must be between 1 and 64 characters"}),
        );
    }
    let names = match profile_names(profiles, identity, tenant).await {
        Ok(names) => names,
        Err(answer) => return answer,
    };
    let profile_id = match checked_profile(payload.profile, &names) {
        Ok(profile) => profile,
        Err(answer) => return answer,
    };

    let key_value = generate_api_key();
    let key_hash = hash_key(&key_value);

    // Capture identity context at creation time so the validate endpoint
    // can return it without hitting the IAM provider.
    let tenant_id = resolve_effective_tenant(Some(identity), None);
    let (aegis_role, zaru_tier) = identity_claim_columns(identity);

    let row = CreateApiKeyRow {
        id: Uuid::new_v4(),
        user_id: identity.sub.clone(),
        name: payload.name,
        key_hash,
        scopes: payload.scopes,
        expires_at: payload.expires_at,
        tenant_id: tenant_id.as_str().to_string(),
        aegis_role,
        zaru_tier,
        profile_id,
    };

    match store.create(&row).await {
        Ok(created) => (
            StatusCode::CREATED,
            serde_json::json!({
                "id": created.id,
                "name": created.name,
                "key": key_value,
                "scopes": created.scopes,
                "expires_at": created.expires_at,
                "created_at": created.created_at,
                "profile": created.profile_id,
                "profile_name": created.profile_id.and_then(|id| names.get(&id).cloned()),
            }),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": e}),
        ),
    }
}

/// `PATCH /v1/api-keys/{id}`'s answer: `{"profile": <id or null>}` sets or
/// clears the profile of the caller's active key (Zaru ADR-0071 D3, D6),
/// answered as `GET /v1/api-keys` lists the key. A body that names no
/// `profile`, names anything but an id or `null`, or names a profile that
/// is not the caller's is refused with [`PROFILE_REFUSAL`]; another
/// person's key, a revoked one or none answers 404.
pub(crate) async fn update_answer(
    store: &dyn ApiKeyStore,
    profiles: Option<&dyn KeyProfiles>,
    identity: &UserIdentity,
    tenant: &TenantId,
    id: Uuid,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let refused = || {
        (
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": PROFILE_REFUSAL}),
        )
    };
    let profile = match body.get("profile") {
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(raw)) => match Uuid::parse_str(raw) {
            Ok(id) => Some(id),
            Err(_) => return refused(),
        },
        _ => return refused(),
    };
    let names = match profile_names(profiles, identity, tenant).await {
        Ok(names) => names,
        Err(answer) => return answer,
    };
    let profile = match checked_profile(profile, &names) {
        Ok(profile) => profile,
        Err(answer) => return answer,
    };
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": "API key not found or already revoked"}),
        )
    };
    match store.set_profile(id, &identity.sub, profile).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({"error": e}),
            )
        }
    }
    match store.list_for_user(&identity.sub).await {
        Ok(rows) => match rows.iter().find(|row| row.id == id) {
            Some(row) => (StatusCode::OK, key_view(row, &names)),
            None => not_found(),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({"error": e}),
        ),
    }
}

/// `GET /v1/api-keys` — list API keys for the authenticated user.
pub(crate) async fn list_api_keys_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
    tenant: Option<Extension<TenantId>>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:list")?;
    let Some(repo) = state.api_key_repo.clone() else {
        return Ok(unavailable());
    };
    let tenant = profile_tenant(&identity, tenant.as_deref());
    let profiles = state
        .profile_service
        .as_deref()
        .map(|p| p as &dyn KeyProfiles);
    let (status, body) = list_answer(repo.as_ref(), profiles, &identity, &tenant).await;
    Ok((status, Json(body)).into_response())
}

/// `POST /v1/api-keys` — create a new API key.  The raw key value is returned
/// exactly once and is never stored.
pub(crate) async fn create_api_key_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
    tenant: Option<Extension<TenantId>>,
    Json(payload): Json<CreateApiKeyRequest>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:create")?;
    let Some(repo) = state.api_key_repo.clone() else {
        return Ok(unavailable());
    };
    let tenant = profile_tenant(&identity, tenant.as_deref());
    let profiles = state
        .profile_service
        .as_deref()
        .map(|p| p as &dyn KeyProfiles);
    let (status, body) = create_answer(repo.as_ref(), profiles, &identity, &tenant, payload).await;
    Ok((status, Json(body)).into_response())
}

/// `PATCH /v1/api-keys/:id` — set or clear the key's profile (Zaru ADR-0071
/// D3, D6). Choosing what a key reaches is the authority that makes a key,
/// so the route asks `key:create`.
pub(crate) async fn update_api_key_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
    tenant: Option<Extension<TenantId>>,
    Path(id): Path<Uuid>,
    Json(body): Json<serde_json::Value>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:create")?;
    let Some(repo) = state.api_key_repo.clone() else {
        return Ok(unavailable());
    };
    let tenant = profile_tenant(&identity, tenant.as_deref());
    let profiles = state
        .profile_service
        .as_deref()
        .map(|p| p as &dyn KeyProfiles);
    let (status, body) = update_answer(repo.as_ref(), profiles, &identity, &tenant, id, body).await;
    Ok((status, Json(body)).into_response())
}

/// `DELETE /v1/api-keys/:id` — revoke an API key.
pub(crate) async fn revoke_api_key_handler(
    State(state): State<Arc<AppState>>,
    scope_guard: ScopeGuard,
    Extension(identity): Extension<UserIdentity>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, (axum::http::StatusCode, axum::Json<serde_json::Value>)> {
    scope_guard.require("key:revoke")?;
    let repo = match &state.api_key_repo {
        Some(r) => r.clone(),
        None => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "API key repository not configured"})),
            )
                .into_response());
        }
    };

    match repo.revoke(id, &identity.sub).await {
        Ok(true) => {
            // AEGIS ADR-129 D19: revoking the key ends its escalation.
            if let Some(escalations) = &state.operator_escalations {
                if let Err(e) = escalations
                    .end_for_api_key(
                        id,
                        aegis_orchestrator_core::domain::operator_escalation::EscalationEndReason::ApiKeyRevoked,
                    )
                    .await
                {
                    tracing::warn!(
                        api_key_id = %id,
                        error = %e,
                        "revoked API key's operator escalation could not be ended"
                    );
                }
            }
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({"message": "API key revoked"})),
            )
                .into_response())
        }
        Ok(false) => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "API key not found or already revoked"})),
        )
            .into_response()),
        Err(e) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response()),
    }
}

/// `POST /v1/api-keys/validate` — Validate an API key and return the associated
/// user identity. Called by the MCP server to authenticate external clients.
///
/// The raw API key is passed in the `Authorization: Bearer aegis_xxx` header.
/// This endpoint is exempt from IAM/OIDC auth (it IS the auth mechanism for
/// API-key-based clients).
pub(crate) async fn validate_api_key_handler(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> axum::response::Response {
    // Extract Bearer token from Authorization header
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let token = match token {
        Some(t) if t.starts_with("aegis_") => t,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Missing or invalid API key"})),
            )
                .into_response()
        }
    };

    let lookup = crate::daemon::api_key_identity::lookup_from_repo(
        state.api_key_repo.as_ref(),
        state.operator_escalations.as_ref(),
    );
    let (status, body) = validate_answer(lookup.as_deref(), token).await;
    (status, Json(body)).into_response()
}

/// The validate route's answer for `token` (AEGIS ADR-093; ADR-129 D14 and
/// the Update's U7): the key's owner, tenant, scopes and tier; its stored
/// `aegis_role`; and, while a key with no stored role holds an active
/// operator escalation, the escalation's `aegis_role` and
/// `operator_escalation: { expires_at }`; and the key's `profile` (Zaru
/// ADR-0071 D3), which the Zaru MCP server forwards on every call and
/// listing, `null` when none was chosen.
pub(crate) async fn validate_answer(
    lookup: Option<&dyn crate::daemon::api_key_identity::ApiKeyLookup>,
    token: &str,
) -> (StatusCode, serde_json::Value) {
    let Some(lookup) = lookup else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({"error": "API key repository not configured"}),
        );
    };
    let row = match lookup.find_active_by_hash(&hash_key(token)).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            return (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"error": "Invalid or expired API key"}),
            )
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({"error": e}),
            )
        }
    };
    let escalation = if row.aegis_role.is_none() {
        match lookup.active_escalation(row.id).await {
            Ok(escalation) => escalation,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({"error": e}),
                )
            }
        }
    } else {
        None
    };
    let mut body = serde_json::json!({
        "user_id": row.user_id,
        "tenant_id": row.tenant_id,
        "aegis_role": row.aegis_role,
        "scopes": row.scopes,
        "zaru_tier": row.zaru_tier,
        "profile": row.profile_id,
    });
    if let Some(escalation) = escalation {
        body["aegis_role"] = serde_json::json!(escalation.aegis_role.as_claim_str());
        body["operator_escalation"] = serde_json::json!({ "expires_at": escalation.expires_at });
    }
    (StatusCode::OK, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};
    use aegis_orchestrator_core::domain::tenant::TenantId;

    fn consumer(sub: &str, tier: ZaruTier) -> UserIdentity {
        UserIdentity {
            sub: sub.to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: tier,
                tenant_id: TenantId::for_consumer_user(sub).unwrap(),
            },
        }
    }

    fn operator(role: AegisRole) -> UserIdentity {
        UserIdentity {
            sub: "op-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::Operator { aegis_role: role },
        }
    }

    /// Regression: pre-fix, `create_api_key_handler` stored
    /// `zaru_tier.to_security_context_name()` (e.g. `"zaru-pro"`) into the
    /// `api_keys.zaru_tier` column. The seal-side reader
    /// `identity_from_api_key` parses that column through
    /// `ZaruTier::from_claim`, which accepts only bare claims
    /// (`"free"`/`"pro"`/…). The bare-claim mismatch silently fell back to
    /// `ZaruTier::Free`, then a downstream synthesizer prefixed `zaru-`
    /// again → `"zaru-zaru-free"` → "Security context not found" → 401.
    ///
    /// This test pins the schema contract: `identity_claim_columns` must
    /// emit values that `ZaruTier::from_claim` (and `AegisRole::from_claim`)
    /// can round-trip — never the security-context-name encoding.
    #[test]
    fn consumer_user_zaru_tier_column_is_bare_claim_not_security_context_name() {
        for tier in [
            ZaruTier::Free,
            ZaruTier::Pro,
            ZaruTier::Business,
            ZaruTier::Enterprise,
        ] {
            let identity = consumer("user-alpha", tier.clone());
            let (aegis_role, zaru_tier_col) = identity_claim_columns(&identity);

            assert_eq!(
                aegis_role, None,
                "consumer user must not populate aegis_role"
            );
            let zaru_tier_col = zaru_tier_col.expect("consumer user must populate zaru_tier");

            // The exact bug: column must NOT hold the SecurityContext name.
            assert!(
                !zaru_tier_col.starts_with("zaru-"),
                "zaru_tier column must hold the bare claim, got {zaru_tier_col:?} \
                 (this is the security-context-name; storing it here makes \
                  identity_from_api_key fall back to ZaruTier::Free and \
                  synthesize a malformed `zaru-zaru-*` SecurityContext)"
            );

            // Round-trip through the seal-side reader's parser.
            let parsed = ZaruTier::from_claim(&zaru_tier_col).unwrap_or_else(|| {
                panic!(
                    "ZaruTier::from_claim({zaru_tier_col:?}) returned None — \
                     write/read encodings have drifted"
                )
            });
            assert_eq!(parsed, tier);
        }
    }

    /// Operator branch was already correct (`as_claim_str`), but pin it
    /// to the same round-trip contract so a future refactor can't regress
    /// it the way the consumer branch was regressed.
    #[test]
    fn operator_aegis_role_column_round_trips_through_from_claim() {
        for role in [AegisRole::Admin, AegisRole::Operator, AegisRole::Readonly] {
            let identity = operator(role.clone());
            let (aegis_role_col, zaru_tier) = identity_claim_columns(&identity);

            assert_eq!(zaru_tier, None, "operator must not populate zaru_tier");
            let aegis_role_col = aegis_role_col.expect("operator must populate aegis_role");

            let parsed = AegisRole::from_claim(&aegis_role_col).unwrap_or_else(|| {
                panic!("AegisRole::from_claim({aegis_role_col:?}) returned None")
            });
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn service_account_and_tenant_user_populate_neither_column() {
        let svc = UserIdentity {
            sub: "svc-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-sdk".to_string(),
            },
        };
        assert_eq!(identity_claim_columns(&svc), (None, None));

        let tenant_user = UserIdentity {
            sub: "tu-1".to_string(),
            realm_slug: "tenant-acme".to_string(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::TenantUser {
                tenant_slug: "acme".to_string(),
            },
        };
        assert_eq!(identity_claim_columns(&tenant_user), (None, None));
    }

    /// AEGIS ADR-129 D14 and the Update's U7: `validate` answers
    /// `operator_escalation` and the escalation's role only while the key
    /// holds an active escalation; before it, after its end, and for a
    /// second key of the same user, neither.
    #[tokio::test]
    async fn validate_answers_operator_escalation_only_while_active() {
        use crate::daemon::api_key_identity::test_keys::{key_row, KeyTable};
        use aegis_orchestrator_core::application::operator_escalation_service::{
            OperatorEscalationService, RedeemingKey,
        };
        use aegis_orchestrator_core::domain::node_config::OperatorEscalationConfig;
        use aegis_orchestrator_core::domain::operator_escalation::EscalationEndReason;
        use aegis_orchestrator_core::infrastructure::repositories::postgres_operator_escalation::InMemoryOperatorEscalationRepository;

        let escalations = Arc::new(OperatorEscalationService::new(
            Arc::new(InMemoryOperatorEscalationRepository::new()),
            OperatorEscalationConfig::default(),
        ));
        let sub = "c0ffee00-consumer";
        let tenant = TenantId::for_consumer_user(sub)
            .unwrap()
            .as_str()
            .to_string();
        let first = key_row("aegis_first", sub, &tenant, None);
        let second = key_row("aegis_second", sub, &tenant, None);
        let table = KeyTable {
            rows: vec![first.clone(), second.clone()],
            escalations: Some(escalations.clone()),
        };

        let (status, body) = validate_answer(Some(&table), "aegis_first").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("operator_escalation").is_none(), "{body}");
        assert!(body["aegis_role"].is_null(), "{body}");

        let minted = escalations
            .mint("system-sub", sub, AegisRole::Operator)
            .await
            .unwrap();
        let escalation = escalations
            .redeem(
                &RedeemingKey {
                    api_key_id: first.id,
                    user_id: sub.to_string(),
                    has_stored_role: false,
                },
                &minted.code,
            )
            .await
            .unwrap();

        let (_, body) = validate_answer(Some(&table), "aegis_first").await;
        assert_eq!(body["aegis_role"], "aegis:operator", "{body}");
        assert_eq!(
            body["operator_escalation"]["expires_at"],
            serde_json::json!(escalation.expires_at),
            "{body}"
        );
        let (_, other) = validate_answer(Some(&table), "aegis_second").await;
        assert!(
            other.get("operator_escalation").is_none(),
            "a second key of the same user holds nothing: {other}"
        );

        escalations
            .end_for_api_key(first.id, EscalationEndReason::AgentRelease)
            .await
            .unwrap();
        let (_, body) = validate_answer(Some(&table), "aegis_first").await;
        assert!(body.get("operator_escalation").is_none(), "{body}");
        assert!(body["aegis_role"].is_null(), "{body}");
    }
}

#[cfg(test)]
#[path = "deny_by_default_orchestrator_2_api_keys_tests.rs"]
mod deny_by_default_orchestrator_2_api_keys_tests;
