// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Model alias lookup (AEGIS ADR-124 D3)
//!
//! `GET /v1/llm/aliases/{alias}` answers `{"alias": "<alias>", "model":
//! "<model>"}`: the model the orchestrator's provider registry sends a call
//! on that alias to, so a caller outside the orchestrator (Zaru Web's chat,
//! Zaru ADR-0047) selects its model from the same alias table as every agent.
//! An alias the registry does not hold is 404. The answer carries no
//! endpoint, header or key.
//!
//! Authentication (Zaru ADR-0049 D4; AEGIS ADR-124's Update of 2026-10-01):
//! the path is exempt from the JWT-only `iam_auth_middleware`, which still
//! attaches the identity of a valid JWT here, and this handler authenticates
//! the request itself. It answers a caller holding a JWT identity placed by
//! that middleware, or an `aegis_*` API key in `Authorization: Bearer`,
//! accepted by the lookup `/v1/seal/attest` uses
//! (`daemon::api_key_identity::identity_from_api_key`); everything else,
//! including a node configured without `spec.iam` and no key, is 401. Every
//! identity gets the same answer. No other route changes its
//! authentication: `/v1/credentials` and `/v1/agents` still refuse a key.

use std::sync::Arc;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use serde_json::json;

use crate::daemon::api_key_identity::{identity_from_api_key, ApiKeyLookup};

/// State of the alias-lookup sub-router: the daemon's one provider registry
/// (`AppState::llm_registry`), whose alias table it reads.
#[derive(Clone)]
pub(crate) struct LlmAliasesState {
    pub(crate) registry: Arc<ProviderRegistry>,
}

/// The `/v1/llm/aliases/{alias}` route. Merged into the daemon router by
/// `router::create_router` through [`with_api_key_lookup`], beneath the
/// daemon's authentication layers, which exempt its path.
pub(crate) fn llm_aliases_router(state: LlmAliasesState) -> Router {
    Router::new()
        .route("/v1/llm/aliases/{alias}", get(get_llm_alias_handler))
        .with_state(state)
}

/// The API-key lookup the alias route authenticates an `aegis_*` key with,
/// carried to the handler as a request extension so the route's state stays
/// the registry alone.
#[derive(Clone)]
pub(crate) struct AliasApiKeyLookup(pub(crate) Option<Arc<dyn ApiKeyLookup>>);

/// `router` with the API-key lookup the alias handler authenticates keys
/// with. `router::create_router` mounts the alias route through this, with
/// the daemon's API-key repository; without it the route accepts no key.
pub(crate) fn with_api_key_lookup(router: Router, lookup: Option<Arc<dyn ApiKeyLookup>>) -> Router {
    router.layer(Extension(AliasApiKeyLookup(lookup)))
}

/// The caller's credential as the handler reads it, taken from the request
/// before any await.
enum Credential {
    /// A JWT identity `iam_auth_middleware` attached.
    Jwt,
    /// An `aegis_*` key in `Authorization: Bearer`, and the lookup to check
    /// it against (none when the route was mounted without one).
    ApiKey(String, Option<Arc<dyn ApiKeyLookup>>),
    None,
}

fn credential(request: &axum::extract::Request) -> Credential {
    if request.extensions().get::<UserIdentity>().is_some() {
        return Credential::Jwt;
    }
    let key = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|k| k.starts_with("aegis_"));
    match key {
        Some(key) => Credential::ApiKey(
            key.to_string(),
            request
                .extensions()
                .get::<AliasApiKeyLookup>()
                .and_then(|l| l.0.clone()),
        ),
        None => Credential::None,
    }
}

/// A JWT identity the IAM middleware attached, or a valid `aegis_*` key.
async fn is_authenticated(credential: Credential) -> bool {
    match credential {
        Credential::Jwt => true,
        Credential::ApiKey(key, lookup) => identity_from_api_key(lookup.as_deref(), &key)
            .await
            .is_some(),
        Credential::None => false,
    }
}

async fn get_llm_alias_handler(
    State(state): State<LlmAliasesState>,
    Path(alias): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if !is_authenticated(credential(&request)).await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response();
    }

    match state.registry.model_for_alias(&alias) {
        Some(model) => (
            StatusCode::OK,
            Json(json!({"alias": alias, "model": model})),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Model alias not configured"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    //! `GET /v1/llm/aliases/{alias}` (AEGIS ADR-124 D3, clause 2): the model
    //! an alias resolves to, from the registry's own alias table, behind the
    //! authentication `/v1/credentials` uses, answering no endpoint, header
    //! or key. Driven through the daemon's real authentication stack with a
    //! registry built from a provider configuration.

    use super::{llm_aliases_router, with_api_key_lookup, LlmAliasesState};
    use crate::daemon::api_key_identity::ApiKeyLookup;
    use crate::daemon::handlers::api_keys::hash_key;
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, serve,
    };
    use aegis_orchestrator_core::domain::iam::AegisRole;
    use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
    use aegis_orchestrator_core::domain::shared_kernel::TenantId;
    use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_api_key::ApiKeyRow;
    use axum::{routing::get, Router};
    use std::sync::Arc;

    const ENDPOINT: &str = "https://api.example.invalid/client/v4/accounts/acct/ai/v1";
    const KEY: &str = "literal-provider-key-value";
    const GATEWAY: &str = "inference-test-gateway";

    fn registry() -> Arc<ProviderRegistry> {
        let provider: LLMProviderConfig = serde_yaml::from_str(&format!(
            r#"name: workers-ai
type: openai-compatible
endpoint: "{ENDPOINT}"
api_key: "{KEY}"
headers:
  cf-aig-gateway-id: "{GATEWAY}"
enabled: true
models:
  - alias: "zaru-chat"
    model: "@cf/zai-org/glm-5.3-flash"
    capabilities: ["chat"]
    context_window: 8192
  - alias: "judge"
    model: "@cf/openai/gpt-oss-120b"
    capabilities: ["chat"]
    context_window: 8192
"#
        ))
        .expect("provider block parses");
        let mut manifest = NodeConfigManifest::default();
        manifest.spec.llm_providers = vec![provider];
        Arc::new(ProviderRegistry::from_config(&manifest).expect("registry builds"))
    }

    async fn base() -> String {
        let iam = identity_provider(&[
            ("consumer-token", consumer("user-1"), ""),
            ("operator-token", operator(AegisRole::Readonly), ""),
        ]);
        serve(
            llm_aliases_router(LlmAliasesState {
                registry: registry(),
            }),
            Some(iam),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn a_configured_alias_answers_its_model_and_nothing_else() {
        let base = base().await;
        let mut failures = Vec::new();
        for (token, alias, model) in [
            ("consumer-token", "zaru-chat", "@cf/zai-org/glm-5.3-flash"),
            ("operator-token", "judge", "@cf/openai/gpt-oss-120b"),
        ] {
            let path = format!("/v1/llm/aliases/{alias}");
            let (status, body) =
                send(&base, &reqwest::Method::GET, &path, &None, Some(token)).await;
            if status != 200 {
                failures.push(format!(
                    "GET {path} as {token} must answer 200 with the alias's model, got {status} {body}"
                ));
                continue;
            }
            let expected = serde_json::json!({"alias": alias, "model": model});
            if body != expected {
                failures.push(format!(
                    "GET {path} must answer exactly {expected}, got {body}"
                ));
            }
            let text = body.to_string();
            for secret_or_config in [
                ENDPOINT,
                KEY,
                GATEWAY,
                "cf-aig-gateway-id",
                "endpoint",
                "api_key",
            ] {
                if text.contains(secret_or_config) {
                    failures.push(format!(
                        "GET {path} must not answer {secret_or_config:?}, its body was {text}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[tokio::test]
    async fn an_unknown_alias_is_404() {
        let base = base().await;
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/no-such-alias",
            &None,
            Some("consumer-token"),
        )
        .await;
        assert_eq!(
            status, 404,
            "an alias the registry does not hold must answer 404, got {status} {body}"
        );
        let text = body.to_string();
        assert!(
            !text.contains(ENDPOINT) && !text.contains(KEY) && !text.contains(GATEWAY),
            "the 404 must not answer the provider's configuration, got {text}"
        );
    }

    #[tokio::test]
    async fn without_authentication_the_route_is_401() {
        // With the IAM layer mounted and no bearer token.
        let base = base().await;
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            None,
        )
        .await;
        assert_eq!(
            status, 401,
            "GET /v1/llm/aliases/zaru-chat without a bearer token must answer 401, got {status} {body}"
        );

        // With an unknown bearer token.
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            Some("not-a-token"),
        )
        .await;
        assert_eq!(
            status, 401,
            "GET /v1/llm/aliases/zaru-chat with an invalid token must answer 401, got {status} {body}"
        );

        // A node with no `spec.iam` mounts no authentication layer; the
        // handler sees no identity and must refuse.
        let unauthenticated = serve(
            llm_aliases_router(LlmAliasesState {
                registry: registry(),
            }),
            None,
            None,
        )
        .await;
        let (status, body) = send(
            &unauthenticated,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            None,
        )
        .await;
        assert_eq!(
            status, 401,
            "GET /v1/llm/aliases/zaru-chat with no identity must answer 401, got {status} {body}"
        );
    }

    // ── API keys (Zaru ADR-0049 D4; AEGIS ADR-124's Update of 2026-10-01) ──

    const VALID_KEY: &str = "aegis_valid-test-key";
    const REVOKED_KEY: &str = "aegis_revoked-test-key";
    const UNKNOWN_KEY: &str = "aegis_unknown-test-key";

    /// The `api_keys` table as `PostgresApiKeyRepository::find_by_key_hash`
    /// reads it: a row answers only while its status is `active` and it has
    /// not expired (`postgres_api_key.rs`, the query's WHERE clause).
    struct KeyTable(Vec<ApiKeyRow>);

    #[async_trait::async_trait]
    impl ApiKeyLookup for KeyTable {
        async fn find_active_by_hash(&self, key_hash: &str) -> Result<Option<ApiKeyRow>, String> {
            Ok(self
                .0
                .iter()
                .find(|r| {
                    r.key_hash == key_hash
                        && r.status == "active"
                        && r.expires_at.is_none_or(|t| t > chrono::Utc::now())
                })
                .cloned())
        }
    }

    fn key_row(key: &str, status: &str, user: &str) -> ApiKeyRow {
        ApiKeyRow {
            id: uuid::Uuid::new_v4(),
            user_id: user.to_string(),
            name: "mcp".to_string(),
            key_hash: hash_key(key),
            scopes: Vec::new(),
            expires_at: None,
            last_used_at: None,
            created_at: chrono::Utc::now(),
            status: status.to_string(),
            tenant_id: TenantId::for_consumer_user(user)
                .expect("per-user tenant id")
                .as_str()
                .to_string(),
            aegis_role: None,
            zaru_tier: Some("free".to_string()),
        }
    }

    /// Answers 200 when reached: a stand-in for a JWT-only route's handler,
    /// so a 401 on its path is the IAM layer's refusal.
    async fn reached() -> &'static str {
        "reached"
    }

    /// The alias route mounted as `router::create_router` mounts it (with
    /// the key lookup), beside stand-ins on `/v1/credentials` and
    /// `/v1/agents`, beneath the daemon's real authentication stack.
    async fn base_with_keys() -> String {
        let iam = identity_provider(&[("consumer-token", consumer("user-1"), "")]);
        let table: Arc<dyn ApiKeyLookup> = Arc::new(KeyTable(vec![
            key_row(VALID_KEY, "active", "user-1"),
            key_row(REVOKED_KEY, "revoked", "user-2"),
        ]));
        let router = with_api_key_lookup(
            llm_aliases_router(LlmAliasesState {
                registry: registry(),
            }),
            Some(table),
        )
        .merge(
            Router::new()
                .route("/v1/credentials", get(reached))
                .route("/v1/agents", get(reached)),
        );
        serve(router, Some(iam), None).await
    }

    #[tokio::test]
    async fn a_valid_api_key_answers_the_alias_and_its_model() {
        let base = base_with_keys().await;
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            Some(VALID_KEY),
        )
        .await;
        assert_eq!(
            (status, body.clone()),
            (
                200,
                serde_json::json!({"alias": "zaru-chat", "model": "@cf/zai-org/glm-5.3-flash"})
            ),
            "GET /v1/llm/aliases/zaru-chat with a valid aegis_ key must answer 200 with the alias and its model, got {status} {body}"
        );
    }

    #[tokio::test]
    async fn an_unknown_or_revoked_api_key_is_401() {
        let base = base_with_keys().await;
        let mut failures = Vec::new();
        for key in [UNKNOWN_KEY, REVOKED_KEY] {
            let (status, body) = send(
                &base,
                &reqwest::Method::GET,
                "/v1/llm/aliases/zaru-chat",
                &None,
                Some(key),
            )
            .await;
            if status != 401 {
                failures.push(format!("{key} must answer 401, got {status} {body}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }

    #[tokio::test]
    async fn beside_keys_a_jwt_still_answers_and_no_token_is_401() {
        let base = base_with_keys().await;
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            Some("consumer-token"),
        )
        .await;
        assert_eq!(
            (status, body.clone()),
            (
                200,
                serde_json::json!({"alias": "zaru-chat", "model": "@cf/zai-org/glm-5.3-flash"})
            ),
            "a JWT must answer 200 as before, got {status} {body}"
        );
        for token in [None, Some("not-a-token")] {
            let (status, body) = send(
                &base,
                &reqwest::Method::GET,
                "/v1/llm/aliases/zaru-chat",
                &None,
                token,
            )
            .await;
            assert_eq!(
                status, 401,
                "with bearer {token:?} the route must answer 401, got {status} {body}"
            );
        }
    }

    #[tokio::test]
    async fn without_the_key_lookup_a_key_is_401() {
        // The route as `llm_aliases_router` alone mounts it: no lookup, so
        // no key is accepted.
        let base = base().await;
        let (status, body) = send(
            &base,
            &reqwest::Method::GET,
            "/v1/llm/aliases/zaru-chat",
            &None,
            Some(VALID_KEY),
        )
        .await;
        assert_eq!(status, 401, "got {status} {body}");
    }

    #[tokio::test]
    async fn a_key_still_cannot_reach_credentials_or_agents() {
        let base = base_with_keys().await;
        let mut failures = Vec::new();
        for path in ["/v1/credentials", "/v1/agents"] {
            let (status, body) =
                send(&base, &reqwest::Method::GET, path, &None, Some(VALID_KEY)).await;
            if status != 401 {
                failures.push(format!(
                    "GET {path} with a valid aegis_ key must answer 401 (JWT only), got {status} {body}"
                ));
            }
            // The stand-in is reachable with a JWT, so the 401 is the IAM layer's.
            let (status, body) = send(
                &base,
                &reqwest::Method::GET,
                path,
                &None,
                Some("consumer-token"),
            )
            .await;
            if status != 200 {
                failures.push(format!(
                    "GET {path} with a JWT must reach its handler, got {status} {body}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("; "));
    }
}
