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
//! The route sits beneath the request-authentication stack every daemon
//! route does, the one `/v1/credentials` uses
//! (`router::apply_request_auth_layers`); any authenticated identity may read
//! it, and a request that reaches the handler with no identity (a node
//! configured without `spec.iam`) is refused with 401.

use std::sync::Arc;

use aegis_orchestrator_core::domain::iam::UserIdentity;
use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

/// State of the alias-lookup sub-router: the daemon's one provider registry
/// (`AppState::llm_registry`), whose alias table it reads.
#[derive(Clone)]
pub(crate) struct LlmAliasesState {
    pub(crate) registry: Arc<ProviderRegistry>,
}

/// The `/v1/llm/aliases/{alias}` route. Merged into the daemon router by
/// `router::create_router`, beneath the same authentication layers as every
/// other route.
pub(crate) fn llm_aliases_router(state: LlmAliasesState) -> Router {
    Router::new()
        .route("/v1/llm/aliases/{alias}", get(get_llm_alias_handler))
        .with_state(state)
}

async fn get_llm_alias_handler(
    State(state): State<LlmAliasesState>,
    Path(alias): Path<String>,
    request: axum::extract::Request,
) -> Response {
    if request.extensions().get::<UserIdentity>().is_none() {
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

    use super::{llm_aliases_router, LlmAliasesState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, serve,
    };
    use aegis_orchestrator_core::domain::iam::AegisRole;
    use aegis_orchestrator_core::domain::node_config::{LLMProviderConfig, NodeConfigManifest};
    use aegis_orchestrator_core::infrastructure::llm::ProviderRegistry;
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
}
