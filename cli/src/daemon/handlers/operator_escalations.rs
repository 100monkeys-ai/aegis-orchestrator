// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Operator escalation routes (AEGIS ADR-129)
//!
//! | Route | Caller | Answer |
//! |-------|--------|--------|
//! | `POST /v1/admin/operator-escalation-codes` | the operator web interface's session (D13) | 201 `{code, expires_at}`, once |
//! | `GET /v1/admin/operator-escalations` | the same session | 200 `{escalations: [...]}`, the caller's active ones (Update U5) |
//! | `DELETE /v1/admin/operator-escalations/{id}` | the same session | 200 `{ended_at}` (U5) |
//! | `POST /v1/admin/operator-escalations/end-for-operator` | the same session, `aegis:admin` | 200 `{ended}` (U6) |
//! | `POST /v1/operator-escalations` | an `aegis_*` key (D10, D11) | 200 `{aegis_role, expires_at}` (D14) |
//! | `DELETE /v1/operator-escalations/current` | the escalated key | 200 `{ended_at}` (U5) |
//!
//! "The operator web interface's session" is exactly what D13 admits: an
//! `aegis-system` operator token (`IdentityKind::Operator`) whose `azp` is
//! `zaru-client-system` and whose role is `aegis:admin` or `aegis:operator`.
//! Anything else is 403, an API key included, which is why these routes are
//! exempt from the JWT-only layer (it would answer an API key 401) and still
//! receive a valid JWT's identity and claims from it. The redemption routes
//! take an `aegis_*` key only; any other bearer is
//! `escalation_requires_api_key` (D11).
//!
//! A node that cannot re-read an operator's role (no Keycloak admin client,
//! ADR-129 — Updates, V5) answers every redemption 503 `unavailable`, as a
//! node without the service does.
//!
//! Errors carry `{"error": <code>, "message": <text>}`; the codes are the
//! ones Zaru ADR-0050 D3 relays: `invalid_code`, `code_expired`,
//! `escalation_requires_api_key`.

use std::sync::Arc;

use aegis_orchestrator_core::application::operator_escalation_service::{
    OperatorEscalationError, OperatorEscalationService, RedeemingKey,
};
use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::operator_escalation::{
    EscalationEndReason, OperatorEscalation, MINTING_CLIENT_ID,
};
use aegis_orchestrator_core::presentation::keycloak_auth::TokenClaims;
use axum::extract::{Extension, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::daemon::api_key_identity::{resolve_api_key, ApiKeyLookup, ResolvedApiKey};

/// State of the operator escalation sub-router.
#[derive(Clone)]
pub(crate) struct OperatorEscalationsState {
    /// `None` on a node without the service: every route answers 503.
    pub(crate) service: Option<Arc<OperatorEscalationService>>,
    /// The API-key lookup the redemption routes authenticate by.
    pub(crate) api_keys: Option<Arc<dyn ApiKeyLookup>>,
}

/// The routes, merged into the daemon router by `router::create_router`.
pub(crate) fn operator_escalations_router(state: OperatorEscalationsState) -> Router {
    Router::new()
        .route(
            "/v1/admin/operator-escalation-codes",
            post(mint_code_handler),
        )
        .route(
            "/v1/admin/operator-escalations",
            get(list_escalations_handler),
        )
        .route(
            "/v1/admin/operator-escalations/end-for-operator",
            post(end_for_operator_handler),
        )
        .route(
            "/v1/admin/operator-escalations/{id}",
            delete(end_escalation_handler),
        )
        .route("/v1/operator-escalations", post(redeem_handler))
        .route("/v1/operator-escalations/current", delete(release_handler))
        .with_state(state)
}

fn refuse(status: StatusCode, error: &str, message: &str) -> Response {
    (status, Json(json!({ "error": error, "message": message }))).into_response()
}

/// A refusal carried as an `Err`, boxed: a `Response` is large.
fn boxed(status: StatusCode, error: &str, message: &str) -> Box<Response> {
    Box::new(refuse(status, error, message))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

fn service_or_503(
    state: &OperatorEscalationsState,
) -> Result<Arc<OperatorEscalationService>, Box<Response>> {
    state.service.clone().ok_or_else(|| {
        boxed(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the operator escalation is not configured on this node",
        )
    })
}

fn internal(e: OperatorEscalationError) -> Response {
    refuse(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        &e.to_string(),
    )
}

/// The operator web interface's stepped-up session, as D13 admits it.
struct OperatorSession {
    system_sub: String,
    consumer_sub: Option<String>,
    aegis_role: AegisRole,
}

/// Admit exactly D13's token: an `aegis-system` operator identity, `azp`
/// `zaru-client-system`, role `aegis:admin` or `aegis:operator`.
fn operator_session(
    headers: &HeaderMap,
    identity: Option<&UserIdentity>,
    claims: Option<&TokenClaims>,
) -> Result<OperatorSession, Box<Response>> {
    let Some(identity) = identity else {
        return Err(match bearer(headers) {
            Some(token) if token.starts_with("aegis_") => boxed(
                StatusCode::FORBIDDEN,
                "forbidden",
                "an API key cannot reach the operator web interface's routes",
            ),
            _ => boxed(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a valid token is required",
            ),
        });
    };
    let IdentityKind::Operator { aegis_role } = &identity.identity_kind else {
        return Err(boxed(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only an aegis-system operator session may use this route",
        ));
    };
    let azp = claims.and_then(|c| c.str_claim("azp"));
    if azp != Some(MINTING_CLIENT_ID) {
        return Err(boxed(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only the operator web interface's session may use this route",
        ));
    }
    if !matches!(aegis_role, AegisRole::Admin | AegisRole::Operator) {
        return Err(boxed(
            StatusCode::FORBIDDEN,
            "forbidden",
            "aegis:readonly has no operator escalation",
        ));
    }
    Ok(OperatorSession {
        system_sub: identity.sub.clone(),
        consumer_sub: claims
            .and_then(|c| c.str_claim("consumer_sub"))
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        aegis_role: aegis_role.clone(),
    })
}

/// Authenticate a redemption route's caller: an `aegis_*` key only (D11).
async fn api_key_caller(
    state: &OperatorEscalationsState,
    headers: &HeaderMap,
) -> Result<ResolvedApiKey, Box<Response>> {
    let Some(token) = bearer(headers) else {
        return Err(boxed(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "an API key is required",
        ));
    };
    if !token.starts_with("aegis_") {
        return Err(boxed(
            StatusCode::FORBIDDEN,
            "escalation_requires_api_key",
            "only an API key can hold an operator escalation",
        ));
    }
    resolve_api_key(state.api_keys.as_deref(), token)
        .await
        .ok_or_else(|| {
            boxed(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "invalid or expired API key",
            )
        })
}

fn escalation_json(e: &OperatorEscalation) -> Value {
    json!({
        "id": e.id,
        "api_key_id": e.api_key_id,
        "aegis_role": e.aegis_role.as_claim_str(),
        "started_at": e.started_at,
        "expires_at": e.expires_at,
    })
}

/// `POST /v1/admin/operator-escalation-codes` (D13): the six digits and
/// their expiry, once, never stored and never cached.
async fn mint_code_handler(
    State(state): State<OperatorEscalationsState>,
    headers: HeaderMap,
    identity: Option<Extension<UserIdentity>>,
    claims: Option<Extension<TokenClaims>>,
) -> Response {
    let session = match operator_session(
        &headers,
        identity.as_ref().map(|e| &e.0),
        claims.as_ref().map(|e| &e.0),
    ) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let Some(consumer_sub) = session.consumer_sub.clone() else {
        return refuse(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the session's token carries no consumer_sub",
        );
    };
    let service = match service_or_503(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match service
        .mint(&session.system_sub, &consumer_sub, session.aegis_role)
        .await
    {
        Ok(minted) => {
            let mut response = (
                StatusCode::CREATED,
                Json(json!({ "code": minted.code, "expires_at": minted.expires_at })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(OperatorEscalationError::RoleNotPermitted) => refuse(
            StatusCode::FORBIDDEN,
            "forbidden",
            "aegis:readonly has no operator escalation",
        ),
        Err(e) => internal(e),
    }
}

/// `GET /v1/admin/operator-escalations` (U5): the caller's active ones.
async fn list_escalations_handler(
    State(state): State<OperatorEscalationsState>,
    headers: HeaderMap,
    identity: Option<Extension<UserIdentity>>,
    claims: Option<Extension<TokenClaims>>,
) -> Response {
    let session = match operator_session(
        &headers,
        identity.as_ref().map(|e| &e.0),
        claims.as_ref().map(|e| &e.0),
    ) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let service = match service_or_503(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match service.list_active_for_operator(&session.system_sub).await {
        Ok(list) => (
            StatusCode::OK,
            Json(json!({ "escalations": list.iter().map(escalation_json).collect::<Vec<_>>() })),
        )
            .into_response(),
        Err(e) => internal(e),
    }
}

/// `DELETE /v1/admin/operator-escalations/{id}` (U5): end one of the
/// caller's own; another operator's answers 404.
async fn end_escalation_handler(
    State(state): State<OperatorEscalationsState>,
    headers: HeaderMap,
    identity: Option<Extension<UserIdentity>>,
    claims: Option<Extension<TokenClaims>>,
    Path(id): Path<Uuid>,
) -> Response {
    let session = match operator_session(
        &headers,
        identity.as_ref().map(|e| &e.0),
        claims.as_ref().map(|e| &e.0),
    ) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let service = match service_or_503(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match service.end_by_operator(&session.system_sub, id).await {
        Ok(ended) => (StatusCode::OK, Json(json!({ "ended_at": ended.ended_at }))).into_response(),
        Err(OperatorEscalationError::NotFound) => refuse(
            StatusCode::NOT_FOUND,
            "escalation_not_found",
            "no active escalation of yours has that id",
        ),
        Err(e) => internal(e),
    }
}

/// `POST /v1/admin/operator-escalations/end-for-operator` (U6): an admin
/// ends every active escalation of one system `sub`, at demotion.
async fn end_for_operator_handler(
    State(state): State<OperatorEscalationsState>,
    headers: HeaderMap,
    identity: Option<Extension<UserIdentity>>,
    claims: Option<Extension<TokenClaims>>,
    body: Option<Json<Value>>,
) -> Response {
    let session = match operator_session(
        &headers,
        identity.as_ref().map(|e| &e.0),
        claims.as_ref().map(|e| &e.0),
    ) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if session.aegis_role != AegisRole::Admin {
        return refuse(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only aegis:admin may end another operator's escalations",
        );
    }
    let Some(system_sub) = body
        .as_ref()
        .and_then(|b| b.0.get("system_sub"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "the body must name system_sub",
        );
    };
    let service = match service_or_503(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match service.end_for_operator(system_sub).await {
        Ok(ended) => (StatusCode::OK, Json(json!({ "ended": ended.len() }))).into_response(),
        Err(e) => internal(e),
    }
}

/// `POST /v1/operator-escalations` (D10, D11, D14): redeem a code with the
/// key presenting it.
async fn redeem_handler(
    State(state): State<OperatorEscalationsState>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let key = match api_key_caller(&state, &headers).await {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let service = match service_or_503(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    if !service.can_check_roles() {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "this node cannot check an operator's role (no Keycloak admin client)",
        );
    }
    let code = body
        .as_ref()
        .and_then(|b| b.0.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let redeeming = RedeemingKey {
        api_key_id: key.api_key_id,
        user_id: key.user_id.clone(),
        has_stored_role: key.has_stored_role,
    };
    match service.redeem(&redeeming, code).await {
        Ok(e) => (
            StatusCode::OK,
            Json(json!({
                "aegis_role": e.aegis_role.as_claim_str(),
                "expires_at": e.expires_at,
            })),
        )
            .into_response(),
        Err(OperatorEscalationError::InvalidCode) => refuse(
            StatusCode::BAD_REQUEST,
            "invalid_code",
            "the code is not valid for this key",
        ),
        Err(OperatorEscalationError::CodeExpired) => refuse(
            StatusCode::BAD_REQUEST,
            "code_expired",
            "the code has expired; generate a new one",
        ),
        Err(e) => internal(e),
    }
}

/// `DELETE /v1/operator-escalations/current` (U5): the agent ends its key's
/// escalation.
async fn release_handler(
    State(state): State<OperatorEscalationsState>,
    headers: HeaderMap,
) -> Response {
    let key = match api_key_caller(&state, &headers).await {
        Ok(k) => k,
        Err(r) => return *r,
    };
    let service = match service_or_503(&state) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match service
        .end_for_api_key(key.api_key_id, EscalationEndReason::AgentRelease)
        .await
    {
        Ok(ended) => match ended.first() {
            Some(e) => (StatusCode::OK, Json(json!({ "ended_at": e.ended_at }))).into_response(),
            None => refuse(
                StatusCode::NOT_FOUND,
                "escalation_not_found",
                "this key holds no active escalation",
            ),
        },
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::api_key_identity::test_keys::{key_row, KeyTable};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider_with_claims, operator, send, serve,
    };
    use aegis_orchestrator_core::domain::node_config::OperatorEscalationConfig;
    use aegis_orchestrator_core::domain::operator_escalation::{
        audit_action, OperatorRecord, OperatorRoleLookup, RoleLookupError,
    };
    use aegis_orchestrator_core::domain::tenant::TenantId;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_operator_escalation::InMemoryOperatorEscalationRepository;
    use reqwest::Method;

    /// The operator's consumer-realm sub (the system token's consumer_sub).
    const CONSUMER_SUB: &str = "5a1e0000-consumer";
    const OTHER_SUB: &str = "6b2f0000-someone";

    const WEB_OPERATOR: &str = "jwt-web-operator";
    const WEB_ADMIN: &str = "jwt-web-admin";
    const WEB_READONLY: &str = "jwt-web-readonly";
    const CLI_OPERATOR: &str = "jwt-cli-operator";
    const CONSUMER_JWT: &str = "jwt-consumer";

    const OPERATOR_KEY: &str = "aegis_operator-consumer-key";
    const SECOND_KEY: &str = "aegis_operator-second-key";
    const OTHER_KEY: &str = "aegis_other-user-key";
    const ROLE_KEY: &str = "aegis_role-bearing-key";

    /// The operator's federated record holds `aegis:operator` (ADR-129 —
    /// Updates, V9's stub).
    struct HoldsOperator;

    #[async_trait::async_trait]
    impl OperatorRoleLookup for HoldsOperator {
        async fn lookup(&self, _: &str) -> Result<OperatorRecord, RoleLookupError> {
            Ok(OperatorRecord::Found {
                aegis_role: Some("aegis:operator".to_string()),
            })
        }
    }

    struct Harness {
        base: String,
        repo: Arc<InMemoryOperatorEscalationRepository>,
        operator_key_id: Uuid,
    }

    fn tenant_of(sub: &str) -> String {
        TenantId::for_consumer_user(sub)
            .unwrap()
            .as_str()
            .to_string()
    }

    async fn harness() -> Harness {
        harness_with(true).await
    }

    /// `role_lookup: false` is a node with no Keycloak admin client (V5).
    async fn harness_with(role_lookup: bool) -> Harness {
        let repo = Arc::new(InMemoryOperatorEscalationRepository::new());
        let mut service =
            OperatorEscalationService::new(repo.clone(), OperatorEscalationConfig::default());
        if role_lookup {
            service = service.with_role_lookup(Arc::new(HoldsOperator));
        }
        let service = Arc::new(service);
        let operator_key = key_row(OPERATOR_KEY, CONSUMER_SUB, &tenant_of(CONSUMER_SUB), None);
        let operator_key_id = operator_key.id;
        let table = KeyTable {
            rows: vec![
                operator_key,
                key_row(SECOND_KEY, CONSUMER_SUB, &tenant_of(CONSUMER_SUB), None),
                key_row(OTHER_KEY, OTHER_SUB, &tenant_of(OTHER_SUB), None),
                // Created from the operator identity: user_id is the system
                // sub, the role stored (D10).
                key_row(
                    ROLE_KEY,
                    "op-aegis:operator",
                    "system",
                    Some("aegis:operator"),
                ),
            ],
            escalations: Some(service.clone()),
        };
        let web = json!({ "azp": MINTING_CLIENT_ID, "consumer_sub": CONSUMER_SUB });
        let iam = identity_provider_with_claims(&[
            (WEB_OPERATOR, operator(AegisRole::Operator), "", web.clone()),
            (WEB_ADMIN, operator(AegisRole::Admin), "", web.clone()),
            (WEB_READONLY, operator(AegisRole::Readonly), "", web),
            (
                CLI_OPERATOR,
                operator(AegisRole::Operator),
                "",
                json!({ "azp": "aegis-cli", "consumer_sub": CONSUMER_SUB }),
            ),
            (
                CONSUMER_JWT,
                consumer(CONSUMER_SUB),
                "",
                json!({ "azp": "zaru-client" }),
            ),
        ]);
        let router = operator_escalations_router(OperatorEscalationsState {
            service: Some(service),
            api_keys: Some(Arc::new(table)),
        });
        Harness {
            base: serve(router, Some(iam), None).await,
            repo,
            operator_key_id,
        }
    }

    async fn mint(h: &Harness, bearer: &str) -> (u16, Value) {
        send(
            &h.base,
            &Method::POST,
            "/v1/admin/operator-escalation-codes",
            &None,
            Some(bearer),
        )
        .await
    }

    async fn redeem(h: &Harness, bearer: &str, code: &str) -> (u16, Value) {
        send(
            &h.base,
            &Method::POST,
            "/v1/operator-escalations",
            &Some(json!({ "code": code })),
            Some(bearer),
        )
        .await
    }

    async fn minted_code(h: &Harness) -> String {
        let (status, body) = mint(h, WEB_OPERATOR).await;
        assert_eq!(status, 201, "{body}");
        body["code"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn mint_201_for_zaru_client_system_operator_token() {
        let h = harness().await;
        let resp = reqwest::Client::new()
            .post(format!("{}/v1/admin/operator-escalation-codes", h.base))
            .bearer_auth(WEB_OPERATOR)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201);
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .map(|v| v.to_str().unwrap()),
            Some("no-store")
        );
        let body: Value = resp.json().await.unwrap();
        let code = body["code"].as_str().unwrap();
        assert!(
            code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()),
            "{body}"
        );
        assert!(body["expires_at"].is_string(), "{body}");
    }

    #[tokio::test]
    async fn mint_403_for_aegis_cli_system_token() {
        let h = harness().await;
        let (status, body) = mint(&h, CLI_OPERATOR).await;
        assert_eq!(status, 403, "{body}");
    }

    #[tokio::test]
    async fn mint_403_for_api_key() {
        let h = harness().await;
        let (status, body) = mint(&h, OPERATOR_KEY).await;
        assert_eq!(status, 403, "{body}");
        let (status, body) = mint(&h, ROLE_KEY).await;
        assert_eq!(status, 403, "{body}");
    }

    #[tokio::test]
    async fn mint_403_for_consumer_token() {
        let h = harness().await;
        let (status, body) = mint(&h, CONSUMER_JWT).await;
        assert_eq!(status, 403, "{body}");
    }

    #[tokio::test]
    async fn mint_403_for_readonly_operator() {
        let h = harness().await;
        let (status, body) = mint(&h, WEB_READONLY).await;
        assert_eq!(status, 403, "{body}");
    }

    #[tokio::test]
    async fn redeem_by_consumer_key_200() {
        let h = harness().await;
        let code = minted_code(&h).await;
        let (status, body) = redeem(&h, OPERATOR_KEY, &code).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["aegis_role"], "aegis:operator");
        let expires: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(body["expires_at"].clone()).unwrap();
        let in_seconds = (expires - chrono::Utc::now()).num_seconds();
        assert!((1790..=1800).contains(&in_seconds), "{in_seconds}");
    }

    #[tokio::test]
    async fn redeem_refused_for_other_users_key() {
        let h = harness().await;
        let code = minted_code(&h).await;
        let (status, body) = redeem(&h, OTHER_KEY, &code).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (400, Some("invalid_code"))
        );
    }

    /// The escalation is held by the redeeming key alone (D10): the
    /// operator's second key neither redeems an already-used code nor holds
    /// the first key's escalation.
    #[tokio::test]
    async fn redeem_refused_for_second_key_of_same_user() {
        let h = harness().await;
        let code = minted_code(&h).await;
        assert_eq!(redeem(&h, OPERATOR_KEY, &code).await.0, 200);
        let (status, body) = redeem(&h, SECOND_KEY, &code).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (400, Some("invalid_code"))
        );
        let (status, body) = send(
            &h.base,
            &Method::DELETE,
            "/v1/operator-escalations/current",
            &None,
            Some(SECOND_KEY),
        )
        .await;
        assert_eq!(status, 404, "the second key holds nothing: {body}");
    }

    #[tokio::test]
    async fn redeem_refused_for_role_bearing_key_of_same_operator() {
        let h = harness().await;
        let code = minted_code(&h).await;
        let (status, body) = redeem(&h, ROLE_KEY, &code).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (400, Some("invalid_code"))
        );
        // The code is still good for the operator's consumer key.
        assert_eq!(redeem(&h, OPERATOR_KEY, &code).await.0, 200);
    }

    #[tokio::test]
    async fn redeem_refused_for_jwt_escalation_requires_api_key() {
        let h = harness().await;
        let code = minted_code(&h).await;
        for jwt in [CONSUMER_JWT, WEB_OPERATOR] {
            let (status, body) = redeem(&h, jwt, &code).await;
            assert_eq!(
                (status, body["error"].as_str()),
                (403, Some("escalation_requires_api_key")),
                "{jwt}"
            );
        }
    }

    #[tokio::test]
    async fn second_use_refused() {
        let h = harness().await;
        let code = minted_code(&h).await;
        assert_eq!(redeem(&h, OPERATOR_KEY, &code).await.0, 200);
        let (status, body) = redeem(&h, OPERATOR_KEY, &code).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (400, Some("invalid_code"))
        );
    }

    #[tokio::test]
    async fn fifth_failure_invalidates_over_http() {
        let h = harness().await;
        let code = minted_code(&h).await;
        let wrong = format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000);
        for _ in 0..5 {
            assert_eq!(redeem(&h, OPERATOR_KEY, &wrong).await.0, 400);
        }
        let (status, body) = redeem(&h, OPERATOR_KEY, &code).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (400, Some("invalid_code"))
        );
    }

    #[tokio::test]
    async fn release_ends_escalation() {
        let h = harness().await;
        let code = minted_code(&h).await;
        assert_eq!(redeem(&h, OPERATOR_KEY, &code).await.0, 200);
        let (status, body) = send(
            &h.base,
            &Method::DELETE,
            "/v1/operator-escalations/current",
            &None,
            Some(OPERATOR_KEY),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body["ended_at"].is_string(), "{body}");
        let ended = h.repo.audit_entries().await;
        assert_eq!(ended.last().unwrap().action, audit_action::ENDED);
        assert_eq!(
            ended.last().unwrap().target_resource,
            h.operator_key_id.to_string()
        );
    }

    #[tokio::test]
    async fn web_session_lists_and_ends_its_own_escalations() {
        let h = harness().await;
        let code = minted_code(&h).await;
        assert_eq!(redeem(&h, OPERATOR_KEY, &code).await.0, 200);
        let (status, body) = send(
            &h.base,
            &Method::GET,
            "/v1/admin/operator-escalations",
            &None,
            Some(WEB_OPERATOR),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let list = body["escalations"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["api_key_id"], json!(h.operator_key_id));
        let id = list[0]["id"].as_str().unwrap().to_string();

        let (status, _) = send(
            &h.base,
            &Method::GET,
            "/v1/admin/operator-escalations",
            &None,
            Some(CLI_OPERATOR),
        )
        .await;
        assert_eq!(status, 403, "D13's token only");

        let (status, body) = send(
            &h.base,
            &Method::DELETE,
            &format!("/v1/admin/operator-escalations/{id}"),
            &None,
            Some(WEB_OPERATOR),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, _) = send(
            &h.base,
            &Method::DELETE,
            &format!("/v1/admin/operator-escalations/{id}"),
            &None,
            Some(WEB_OPERATOR),
        )
        .await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn end_for_operator_is_admin_only_and_ends_every_escalation() {
        let h = harness().await;
        let code = minted_code(&h).await;
        assert_eq!(redeem(&h, OPERATOR_KEY, &code).await.0, 200);
        let body = Some(json!({ "system_sub": "op-aegis:operator" }));
        let (status, _) = send(
            &h.base,
            &Method::POST,
            "/v1/admin/operator-escalations/end-for-operator",
            &body,
            Some(WEB_OPERATOR),
        )
        .await;
        assert_eq!(status, 403, "an operator may not demote");
        let (status, answer) = send(
            &h.base,
            &Method::POST,
            "/v1/admin/operator-escalations/end-for-operator",
            &body,
            Some(WEB_ADMIN),
        )
        .await;
        assert_eq!(
            (status, answer["ended"].as_u64()),
            (200, Some(1)),
            "{answer}"
        );
        let (status, _) = send(
            &h.base,
            &Method::DELETE,
            "/v1/operator-escalations/current",
            &None,
            Some(OPERATOR_KEY),
        )
        .await;
        assert_eq!(status, 404, "nothing left to release");
    }

    /// ADR-129 — Updates, V5: a node with no `spec.iam.keycloak_admin`
    /// answers a redemption 503 `unavailable`, and the code is neither
    /// consumed nor counted as a failure.
    #[tokio::test]
    async fn redemption_503_on_a_node_without_keycloak_admin() {
        let h = harness_with(false).await;
        let code = minted_code(&h).await;
        let (status, body) = redeem(&h, OPERATOR_KEY, &code).await;
        assert_eq!(
            (status, body["error"].as_str()),
            (503, Some("unavailable")),
            "{body}"
        );
        let actions: Vec<String> = h
            .repo
            .audit_entries()
            .await
            .into_iter()
            .map(|a| a.action)
            .collect();
        assert_eq!(actions, vec![audit_action::CODE_ISSUED.to_string()]);
    }
}
