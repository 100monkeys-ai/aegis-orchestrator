// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! SEAL attestation, invocation, and tool listing handlers.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use aegis_orchestrator_core::application::execution::ExecutionService;
use aegis_orchestrator_core::domain::iam::{
    AegisRole, IdentityKind, RealmKind, UserIdentity, ZaruTier,
};
use aegis_orchestrator_core::domain::shared_kernel::ExecutionId;
use aegis_orchestrator_core::domain::tenant::TenantId;

use crate::daemon::handlers::api_keys::hash_key;
use crate::daemon::state::AppState;

#[derive(serde::Deserialize)]
pub struct HttpAttestationRequest {
    pub agent_id: Option<String>,
    pub execution_id: Option<String>,
    pub container_id: Option<String>,
    #[serde(alias = "public_key_pem", alias = "agent_public_key")]
    pub public_key: String,
    pub security_context: Option<String>,
    pub principal_subject: Option<String>,
    pub user_id: Option<String>,
    pub workload_id: Option<String>,
    pub zaru_tier: Option<String>,
    pub tenant_id: Option<String>,
    pub task_summary: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct HttpSealEnvelope {
    pub protocol: Option<String>,
    pub security_token: String,
    pub signature: String,
    pub payload: serde_json::Value,
    pub timestamp: Option<String>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct SealToolsQuery {
    security_context: Option<String>,
}

/// Errors that may arise while resolving the canonical attestation tenant.
///
/// The handler maps these onto HTTP status codes; the variants are exposed for
/// targeted unit testing of [`resolve_attest_tenant`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttestTenantError {
    /// No authenticated identity was available to derive a tenant from.
    Unauthenticated,
    /// The caller supplied an explicit `tenant_id` (or an `execution_id`
    /// belonging to another tenant) that does not match its identity, and
    /// the caller is not allowed to delegate.
    TenantMismatch { expected: String, got: String },
    /// The execution referenced by `request.execution_id` could not be loaded.
    ExecutionNotFound,
    /// A system operator submitted a request without specifying a target
    /// tenant or execution. Operators must always specify what they are
    /// attesting against.
    OperatorMissingTarget,
}

impl AttestTenantError {
    fn http_status(&self) -> StatusCode {
        match self {
            AttestTenantError::Unauthenticated => StatusCode::UNAUTHORIZED,
            AttestTenantError::TenantMismatch { .. } => StatusCode::FORBIDDEN,
            AttestTenantError::ExecutionNotFound => StatusCode::NOT_FOUND,
            AttestTenantError::OperatorMissingTarget => StatusCode::BAD_REQUEST,
        }
    }

    fn message(&self) -> String {
        match self {
            AttestTenantError::Unauthenticated => "unauthorized".to_string(),
            AttestTenantError::TenantMismatch { expected, got } => format!(
                "permission_denied: tenant mismatch (identity tenant: {expected}, requested: {got})"
            ),
            AttestTenantError::ExecutionNotFound => "execution_not_found".to_string(),
            AttestTenantError::OperatorMissingTarget => {
                "bad_request: operators must specify tenant_id or execution_id".to_string()
            }
        }
    }
}

/// True if the identity is allowed to delegate to an arbitrary tenant
/// (system operators and service accounts per ADR-100).
fn may_delegate(identity: &UserIdentity) -> bool {
    matches!(
        identity.identity_kind,
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. }
    )
}

/// Derive the canonical tenant from an authenticated identity's claims.
///
/// Mirrors `derive_tenant_id` (in `aegis_orchestrator_core::domain::iam::delegation`)
/// but is local to this handler so we can reason about it deterministically. `ConsumerUser`
/// carries its per-user tenant directly per ADR-097; `TenantUser` resolves
/// from the realm slug; `Operator` and `ServiceAccount` resolve to
/// `TenantId::system()` (and only honor explicit overrides via the
/// delegation gate below).
fn identity_tenant(identity: &UserIdentity) -> TenantId {
    match &identity.identity_kind {
        IdentityKind::ConsumerUser { tenant_id, .. } => tenant_id.clone(),
        IdentityKind::TenantUser { tenant_slug } => {
            // Fail closed: an unparseable tenant slug on an authenticated
            // TenantUser indicates a misconfigured realm. Returning
            // TenantId::system() is harmless here because `may_delegate`
            // is false for this kind, so the mismatch path will reject
            // any attempt to attest against a different tenant.
            TenantId::from_realm_slug(tenant_slug).unwrap_or_else(|_| TenantId::system())
        }
        IdentityKind::Operator { .. } | IdentityKind::ServiceAccount { .. } => TenantId::system(),
    }
}

/// Resolve the canonical [`TenantId`] for an attestation request given the
/// authenticated caller identity. This is the post-fix replacement for the
/// pre-`397ef55` `TenantId::consumer()` fallback that caused the cross-tenant
/// leak in `aegis.task.list`.
///
/// # Resolution rules
///
/// 1. No identity → [`AttestTenantError::Unauthenticated`] (fail closed).
/// 2. `request.execution_id` present:
///    - Load the execution; verify its tenant matches the identity's tenant
///      OR the caller may delegate (operator / service account).
///    - Mismatch → [`AttestTenantError::TenantMismatch`].
///    - Lookup failure → [`AttestTenantError::ExecutionNotFound`].
/// 3. `request.tenant_id` present:
///    - If caller may delegate → honor the explicit slug.
///    - Otherwise → must equal identity's canonical tenant; mismatch is
///      tolerated-but-warned (deploy-order safety) when the caller is a
///      consumer/tenant user supplying their own tenant. Any *other*
///      mismatch is a [`AttestTenantError::TenantMismatch`].
/// 4. Neither set:
///    - Operators must specify a target → [`AttestTenantError::OperatorMissingTarget`].
///    - All other identities → identity's canonical tenant.
pub(crate) async fn resolve_attest_tenant<F, Fut>(
    lookup_execution_tenant: F,
    identity: Option<&UserIdentity>,
    request: &HttpAttestationRequest,
) -> Result<TenantId, AttestTenantError>
where
    F: FnOnce(ExecutionId) -> Fut,
    Fut: std::future::Future<Output = Result<TenantId, ()>>,
{
    let identity = match identity {
        Some(i) => i,
        None => {
            tracing::warn!(
                target: "aegis::seal::attest",
                variant = "Unauthenticated",
                "tenant resolution rejected: no authenticated identity"
            );
            return Err(AttestTenantError::Unauthenticated);
        }
    };
    let canonical = identity_tenant(identity);
    let delegating = may_delegate(identity);

    // (2) execution_id branch — verify the execution's tenant matches.
    if let Some(exec_id) = request
        .execution_id
        .as_deref()
        .and_then(|s| ExecutionId::from_string(s).ok())
    {
        tracing::info!(
            target: "aegis::seal::attest",
            branch = "execution_id",
            identity_tenant = %canonical.as_str(),
            delegating,
            "resolving tenant from execution_id"
        );
        let exec_tenant = match lookup_execution_tenant(exec_id).await {
            Ok(t) => t,
            Err(_) => {
                tracing::warn!(
                    target: "aegis::seal::attest",
                    variant = "ExecutionNotFound",
                    "tenant resolution rejected: execution lookup failed"
                );
                return Err(AttestTenantError::ExecutionNotFound);
            }
        };
        if exec_tenant == canonical || delegating {
            return Ok(exec_tenant);
        }
        tracing::warn!(
            target: "aegis::seal::attest",
            variant = "TenantMismatch",
            branch = "execution_id",
            identity_tenant = %canonical.as_str(),
            execution_tenant = %exec_tenant.as_str(),
            identity_sub = %identity.sub,
            "tenant resolution rejected: execution belongs to a different tenant and caller cannot delegate"
        );
        return Err(AttestTenantError::TenantMismatch {
            expected: canonical.as_str().to_string(),
            got: exec_tenant.as_str().to_string(),
        });
    }

    // (3) explicit tenant_id branch.
    if let Some(explicit_slug) = request.tenant_id.as_deref().filter(|s| !s.is_empty()) {
        tracing::info!(
            target: "aegis::seal::attest",
            branch = "explicit_tenant_id",
            identity_tenant = %canonical.as_str(),
            requested_tenant = %explicit_slug,
            delegating,
            "resolving tenant from explicit tenant_id"
        );
        let explicit = match TenantId::from_realm_slug(explicit_slug) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    target: "aegis::seal::attest",
                    variant = "TenantMismatch",
                    branch = "explicit_tenant_id",
                    identity_tenant = %canonical.as_str(),
                    requested_tenant = %explicit_slug,
                    error = %e,
                    "tenant resolution rejected: explicit tenant_id is not a valid realm slug"
                );
                return Err(AttestTenantError::TenantMismatch {
                    expected: canonical.as_str().to_string(),
                    got: explicit_slug.to_string(),
                });
            }
        };
        if delegating {
            return Ok(explicit);
        }
        if explicit == canonical {
            return Ok(canonical);
        }
        // Tolerate-but-ignore for non-delegating callers (deploy-order
        // safety per the migration plan): the body field is going away on
        // the MCP server side; we log a warning and use the JWT-derived
        // tenant instead of rejecting outright. The real defense against
        // LLM-supplied tenant args lives in `enforce_tenant_arg` on the
        // SEAL tool-call path, not here.
        tracing::warn!(
            target: "aegis::seal::attest",
            identity_tenant = %canonical.as_str(),
            requested_tenant = %explicit_slug,
            identity_sub = %identity.sub,
            "ignoring body-supplied tenant_id from non-delegating caller; using JWT-derived tenant"
        );
        return Ok(canonical);
    }

    // (4) neither set.
    if matches!(identity.identity_kind, IdentityKind::Operator { .. }) {
        tracing::warn!(
            target: "aegis::seal::attest",
            variant = "OperatorMissingTarget",
            identity_sub = %identity.sub,
            "tenant resolution rejected: operator must specify tenant_id or execution_id"
        );
        return Err(AttestTenantError::OperatorMissingTarget);
    }
    tracing::info!(
        target: "aegis::seal::attest",
        branch = "canonical",
        identity_tenant = %canonical.as_str(),
        "resolved tenant from identity canonical claim"
    );
    Ok(canonical)
}

/// Returns the first 12 hex chars of the SHA-256 hash of an API key for
/// safe correlation in logs. Never log the raw key, never log the full
/// hash — this prefix is enough to grep DB rows or correlate across
/// services without leaking material that could be used to reconstruct
/// the key.
fn key_hash_prefix(raw_token: &str) -> String {
    let h = hash_key(raw_token);
    h.chars().take(12).collect()
}

/// Synthesize a [`UserIdentity`] from an `aegis_*` API key row by looking
/// the key up in the api_keys repository. Returns `None` if the key is
/// unknown / revoked / expired.
async fn identity_from_api_key(state: &AppState, raw_token: &str) -> Option<UserIdentity> {
    let prefix = key_hash_prefix(raw_token);
    let repo = match state.api_key_repo.as_ref() {
        Some(r) => r,
        None => {
            tracing::warn!(
                target: "aegis::seal::attest",
                key_prefix = %prefix,
                "api_key_repo not configured; cannot validate API key"
            );
            return None;
        }
    };
    let hash = hash_key(raw_token);
    let row = match repo.find_by_key_hash(&hash).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!(
                target: "aegis::seal::attest",
                key_prefix = %prefix,
                "API key hash not found in repository"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                target: "aegis::seal::attest",
                key_prefix = %prefix,
                error = %e,
                "API key repository lookup errored"
            );
            return None;
        }
    };
    tracing::info!(
        target: "aegis::seal::attest",
        key_prefix = %prefix,
        user_id = %row.user_id,
        tenant_id = %row.tenant_id,
        has_aegis_role = row.aegis_role.is_some(),
        has_zaru_tier = row.zaru_tier.is_some(),
        "API key row found"
    );
    let realm_slug = if row.aegis_role.is_some() {
        "aegis-system".to_string()
    } else {
        "zaru-consumer".to_string()
    };
    let identity_kind = if let Some(role_str) = row.aegis_role.as_deref() {
        let role = match AegisRole::from_claim(role_str) {
            Some(r) => r,
            None => {
                tracing::warn!(
                    target: "aegis::seal::attest",
                    key_prefix = %prefix,
                    aegis_role = %role_str,
                    "AegisRole::from_claim returned None for stored aegis_role"
                );
                return None;
            }
        };
        IdentityKind::Operator { aegis_role: role }
    } else {
        let tenant_id = match TenantId::from_realm_slug(&row.tenant_id) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    target: "aegis::seal::attest",
                    key_prefix = %prefix,
                    stored_tenant_id = %row.tenant_id,
                    error = %e,
                    "TenantId::from_realm_slug failed; cannot synthesize ConsumerUser identity"
                );
                return None;
            }
        };
        let zaru_tier = row
            .zaru_tier
            .as_deref()
            .and_then(ZaruTier::from_claim)
            .unwrap_or(ZaruTier::Free);
        IdentityKind::ConsumerUser {
            zaru_tier,
            tenant_id,
        }
    };
    Some(UserIdentity {
        sub: row.user_id,
        realm_slug,
        email: None,
        name: None,
        identity_kind,
    })
}

/// Authenticate the caller of `/v1/seal/attest` from the `Authorization`
/// header. Accepts both JWTs (validated against the configured IAM
/// service) and `aegis_*` API keys (validated against `api_key_repo`).
///
/// Returns `None` if the header is missing/malformed or if validation
/// fails — the handler maps this to `AttestTenantError::Unauthenticated`.
async fn authenticate_attest_request(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<UserIdentity> {
    let raw_header = match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(v) => v,
        None => {
            tracing::warn!(
                target: "aegis::seal::attest",
                "no Authorization header on attestation request"
            );
            return None;
        }
    };
    let raw = match raw_header.strip_prefix("Bearer ") {
        Some(s) => s.trim(),
        None => {
            tracing::warn!(
                target: "aegis::seal::attest",
                "Authorization header lacks Bearer prefix"
            );
            return None;
        }
    };
    if raw.is_empty() {
        tracing::warn!(
            target: "aegis::seal::attest",
            "Authorization Bearer value empty"
        );
        return None;
    }
    if raw.starts_with("aegis_") {
        tracing::info!(
            target: "aegis::seal::attest",
            auth_method = "api_key",
            key_prefix = %key_hash_prefix(raw),
            "attempting API key validation"
        );
        return identity_from_api_key(state, raw).await;
    }
    tracing::info!(
        target: "aegis::seal::attest",
        auth_method = "jwt",
        "attempting JWT validation"
    );
    let iam = match state.iam_service.as_ref() {
        Some(i) => i,
        None => {
            tracing::warn!(
                target: "aegis::seal::attest",
                "iam_service not configured; cannot validate JWT"
            );
            return None;
        }
    };
    let validated = match iam.validate_token(raw).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "aegis::seal::attest",
                error = %e,
                "JWT validation failed"
            );
            return None;
        }
    };
    tracing::info!(
        target: "aegis::seal::attest",
        sub_prefix = %validated.identity.sub.chars().take(8).collect::<String>(),
        realm = %validated.identity.realm_slug,
        "JWT validated successfully"
    );
    Some(validated.identity)
}

pub(crate) async fn attest_seal_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<HttpAttestationRequest>,
) -> impl IntoResponse {
    tracing::info!(
        target: "aegis::seal::attest",
        has_explicit_tenant_id = request.tenant_id.is_some(),
        has_execution_id = request.execution_id.is_some(),
        has_authorization_header = headers.get("authorization").is_some(),
        "attestation request received"
    );

    // Authenticate the caller. /v1/seal/attest is exempt from the global
    // `iam_auth_middleware` because the orchestrator must accept BOTH
    // JWTs (Zaru consumer flow) and `aegis_*` API keys (SDK flow). The
    // global middleware only knows JWTs, so we authenticate in-handler
    // and derive a `UserIdentity` from whichever credential was supplied.
    let identity = authenticate_attest_request(&state, &headers).await;

    let lookup = |exec_id: ExecutionId| {
        let svc = state.execution_service.clone();
        async move {
            svc.get_execution_unscoped(exec_id)
                .await
                .map(|e| e.tenant_id)
                .map_err(|_| ())
        }
    };
    let tenant_id = match resolve_attest_tenant(lookup, identity.as_ref(), &request).await {
        Ok(t) => t,
        Err(err) => {
            return (
                err.http_status(),
                Json(serde_json::json!({"error": err.message()})),
            )
                .into_response();
        }
    };

    // Derive realm from the authenticated identity rather than defaulting
    // to Consumer; this preserves the strongly-typed classification per
    // ADR-097 and ensures `aegis-system-*` SecurityContext ownership
    // checks see the right realm for operator-issued attestations.
    let realm = identity
        .as_ref()
        .map(|id| id.realm_kind())
        .unwrap_or(RealmKind::Consumer);

    let internal_req =
        aegis_orchestrator_core::infrastructure::seal::attestation::AttestationRequest {
            agent_id: request.agent_id.clone(),
            execution_id: request.execution_id.clone(),
            container_id: request.container_id.clone(),
            public_key_pem: request.public_key.clone(),
            security_context: request.security_context.clone(),
            principal_subject: request.principal_subject.clone(),
            user_id: request.user_id.clone(),
            workload_id: request.workload_id.clone(),
            zaru_tier: request.zaru_tier.clone(),
            tenant_id,
            realm,
            task_summary: request.task_summary.clone(),
        };

    let tenant_for_log = internal_req.tenant_id.as_str().to_string();
    match state.attestation_service.attest(internal_req).await {
        Ok(res) => {
            tracing::info!(
                target: "aegis::seal::attest",
                tenant_id = %tenant_for_log,
                "attestation completed successfully"
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": res.status,
                    "security_token": res.security_token,
                    "expires_at": res.expires_at,
                    "session_id": res.session_id,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(
                target: "aegis::seal::attest",
                tenant_id = %tenant_for_log,
                error = %e,
                "attestation_service.attest failed"
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": e.to_string()
                })),
            )
                .into_response()
        }
    }
}

pub(crate) async fn invoke_seal_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<HttpSealEnvelope>,
) -> impl IntoResponse {
    let (protocol, timestamp) = match (request.protocol, request.timestamp) {
        (Some(p), Some(t)) => (p, t),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "SEAL envelope requires both 'protocol' and 'timestamp' fields"
                })),
            )
                .into_response();
        }
    };

    let envelope = aegis_orchestrator_core::infrastructure::seal::envelope::SealEnvelope {
        protocol,
        security_token: request.security_token,
        signature: request.signature,
        payload: request.payload,
        timestamp,
    };

    // The ToolInvocationService is responsible for validating the security_token
    // and extracting any required claims (such as agent_id) from it as appropriate.
    match state.tool_invocation_service.invoke_tool(&envelope).await {
        Ok(res) => (StatusCode::OK, Json(res)).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e.to_string()
            })),
        )
            .into_response(),
    }
}

pub(crate) async fn list_seal_tools_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SealToolsQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let security_context = headers
        .get("X-Zaru-Security-Context")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or(query.security_context);

    let tools_result = if let Some(ref security_context) = security_context {
        state
            .tool_invocation_service
            .get_available_tools_for_context(security_context)
            .await
    } else {
        state.tool_invocation_service.get_available_tools().await
    };

    match tools_result {
        Ok(tools) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "protocol": "seal/v1",
                "attestation_endpoint": "/v1/seal/attest",
                "invoke_endpoint": "/v1/seal/invoke",
                "security_context": security_context,
                "tools": tools,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": error.to_string(),
            })),
        )
            .into_response(),
    }
}

// ── Regression tests ─────────────────────────────────────────────────────────
//
// Cross-tenant leak fix: pre-`397ef55`, the attestation handler defaulted
// to `TenantId::consumer()` (a global singleton) when no explicit
// `tenant_id`/`execution_id` was supplied. Every consumer Zaru MCP session
// landed on the same singleton tenant, so subsequent `enforce_tenant_arg`
// comparisons leaked one user's executions to another. These tests cover
// the new resolution rules in `resolve_attest_tenant`, including the
// no-identity (auth-bypassed) fail-closed branch.
#[cfg(test)]
mod attest_tenant_resolution_tests {
    use super::*;
    use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity, ZaruTier};

    // ── helpers ──

    fn req() -> HttpAttestationRequest {
        HttpAttestationRequest {
            agent_id: None,
            execution_id: None,
            container_id: None,
            public_key: "k".to_string(),
            security_context: None,
            principal_subject: None,
            user_id: None,
            workload_id: None,
            zaru_tier: None,
            tenant_id: None,
            task_summary: None,
        }
    }

    fn consumer_identity(sub: &str) -> UserIdentity {
        let tenant = TenantId::for_consumer_user(sub).unwrap();
        UserIdentity {
            sub: sub.to_string(),
            realm_slug: "zaru-consumer".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ConsumerUser {
                zaru_tier: ZaruTier::Free,
                tenant_id: tenant,
            },
        }
    }

    fn operator_identity() -> UserIdentity {
        UserIdentity {
            sub: "op-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::Operator {
                aegis_role: AegisRole::Admin,
            },
        }
    }

    fn service_account_identity() -> UserIdentity {
        UserIdentity {
            sub: "svc-1".to_string(),
            realm_slug: "aegis-system".to_string(),
            email: None,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: "aegis-sdk".to_string(),
            },
        }
    }

    /// `lookup_execution_tenant` stub that returns `Ok(tenant)` for any
    /// execution_id. Use [`lookup_missing`] to test the not-found path.
    fn lookup_returning(
        tenant: TenantId,
    ) -> impl FnOnce(
        ExecutionId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TenantId, ()>> + Send>,
    > {
        move |_| {
            let t = tenant.clone();
            Box::pin(async move { Ok(t) })
        }
    }

    fn lookup_missing() -> impl FnOnce(
        ExecutionId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TenantId, ()>> + Send>,
    > {
        move |_| Box::pin(async move { Err(()) })
    }

    /// Lookup that PANICS if invoked — used to assert that the resolver
    /// short-circuited before consulting the execution service.
    fn lookup_unused() -> impl FnOnce(
        ExecutionId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<TenantId, ()>> + Send>,
    > {
        move |_| Box::pin(async move { panic!("execution lookup must not run on this branch") })
    }

    // ── tests ──

    /// Pre-fix, this case fell through to `TenantId::consumer()` (the
    /// global singleton), causing the leak. Post-fix it must resolve to
    /// the user's own per-user tenant.
    #[tokio::test]
    async fn consumer_user_with_no_explicit_args_resolves_to_personal_tenant() {
        let identity = consumer_identity("user-alpha");
        let expected = TenantId::for_consumer_user("user-alpha").unwrap();
        let result = resolve_attest_tenant(lookup_unused(), Some(&identity), &req())
            .await
            .unwrap();
        assert_eq!(result, expected);
        assert_ne!(
            result,
            TenantId::consumer(),
            "must NOT default to the global zaru-consumer singleton"
        );
    }

    /// A consumer user supplying somebody else's tenant_id in the body is
    /// tolerated-but-ignored (deploy-order safety); the JWT-derived tenant
    /// wins. The real defense against LLM-supplied tenant args is in
    /// `enforce_tenant_arg` on the SEAL tool-call path.
    #[tokio::test]
    async fn consumer_user_explicit_other_tenant_is_ignored_and_warns() {
        let identity = consumer_identity("user-alpha");
        let mut request = req();
        request.tenant_id = Some(
            TenantId::for_consumer_user("user-victim")
                .unwrap()
                .as_str()
                .to_string(),
        );
        let result = resolve_attest_tenant(lookup_unused(), Some(&identity), &request)
            .await
            .unwrap();
        let expected = TenantId::for_consumer_user("user-alpha").unwrap();
        assert_eq!(result, expected);
    }

    #[tokio::test]
    async fn service_account_with_explicit_tenant_is_honored() {
        let identity = service_account_identity();
        let mut request = req();
        let target = TenantId::for_consumer_user("user-target").unwrap();
        request.tenant_id = Some(target.as_str().to_string());
        let result = resolve_attest_tenant(lookup_unused(), Some(&identity), &request)
            .await
            .unwrap();
        assert_eq!(result, target);
    }

    /// Service accounts may delegate; this means an explicit tenant_id is
    /// honored *unconditionally* (ADR-100). A "mismatch" with the
    /// canonical service-account tenant is the entire point of delegation
    /// — not a violation. This test pins that contract: if delegation
    /// were ever silently downgraded to "must match own tenant", every
    /// SDK service account would be reduced to system-only operations.
    #[tokio::test]
    async fn service_account_delegation_returns_target_tenant_not_system() {
        let identity = service_account_identity();
        let mut request = req();
        let target = TenantId::for_consumer_user("user-target").unwrap();
        request.tenant_id = Some(target.as_str().to_string());
        let result = resolve_attest_tenant(lookup_unused(), Some(&identity), &request)
            .await
            .unwrap();
        assert_ne!(result, TenantId::system());
        assert_eq!(result, target);
    }

    #[tokio::test]
    async fn consumer_user_with_own_execution_resolves_to_own_tenant() {
        let identity = consumer_identity("user-alpha");
        let own_tenant = TenantId::for_consumer_user("user-alpha").unwrap();
        let mut request = req();
        request.execution_id = Some(uuid::Uuid::new_v4().to_string());
        let result = resolve_attest_tenant(
            lookup_returning(own_tenant.clone()),
            Some(&identity),
            &request,
        )
        .await
        .unwrap();
        assert_eq!(result, own_tenant);
    }

    /// This is a smaller, separate leak in the same handler: pre-fix, a
    /// consumer attesting against another consumer's execution_id silently
    /// adopted the victim's tenant. Post-fix it must reject.
    #[tokio::test]
    async fn consumer_user_attesting_other_users_execution_is_rejected() {
        let identity = consumer_identity("user-alpha");
        let victim_tenant = TenantId::for_consumer_user("user-victim").unwrap();
        let mut request = req();
        request.execution_id = Some(uuid::Uuid::new_v4().to_string());
        let err = resolve_attest_tenant(lookup_returning(victim_tenant), Some(&identity), &request)
            .await
            .unwrap_err();
        assert!(matches!(err, AttestTenantError::TenantMismatch { .. }));
    }

    #[tokio::test]
    async fn execution_lookup_failure_is_not_found() {
        let identity = consumer_identity("user-alpha");
        let mut request = req();
        request.execution_id = Some(uuid::Uuid::new_v4().to_string());
        let err = resolve_attest_tenant(lookup_missing(), Some(&identity), &request)
            .await
            .unwrap_err();
        assert_eq!(err, AttestTenantError::ExecutionNotFound);
    }

    #[tokio::test]
    async fn operator_with_explicit_tenant_is_honored() {
        let identity = operator_identity();
        let mut request = req();
        let target = TenantId::for_consumer_user("user-target").unwrap();
        request.tenant_id = Some(target.as_str().to_string());
        let result = resolve_attest_tenant(lookup_unused(), Some(&identity), &request)
            .await
            .unwrap();
        assert_eq!(result, target);
    }

    #[tokio::test]
    async fn operator_without_target_is_rejected() {
        let identity = operator_identity();
        let err = resolve_attest_tenant(lookup_unused(), Some(&identity), &req())
            .await
            .unwrap_err();
        assert_eq!(err, AttestTenantError::OperatorMissingTarget);
    }

    /// Fail-closed regression: if the auth path is ever bypassed (or the
    /// authentication helper short-circuits), the handler must NOT silently
    /// fall back to a default tenant. Pre-fix this branch quietly returned
    /// `TenantId::consumer()`; post-fix it returns an Unauthenticated error.
    #[tokio::test]
    async fn no_identity_fails_closed_unauthenticated() {
        let err = resolve_attest_tenant(lookup_unused(), None, &req())
            .await
            .unwrap_err();
        assert_eq!(err, AttestTenantError::Unauthenticated);
    }

    #[test]
    fn may_delegate_classifies_correctly() {
        assert!(may_delegate(&operator_identity()));
        assert!(may_delegate(&service_account_identity()));
        assert!(!may_delegate(&consumer_identity("user-1")));
    }
}
