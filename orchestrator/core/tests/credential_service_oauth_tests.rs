// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Regression tests for the OAuth 2.0 authorization-code exchange path of
//! [`StandardCredentialManagementService`] (BC-11, ADR-078).
//!
//! These tests exist because `complete_oauth_connection` previously wrote the
//! literal string `"PENDING_REAL_TOKEN"` to OpenBao instead of calling the
//! provider's token endpoint. Every OAuth-connected credential was non-
//! functional. The tests stand up a mockito server impersonating the
//! provider's token endpoint and assert that the real token is stored and
//! that error paths surface a typed error.

use aegis_orchestrator_core::application::credential_service::{
    CredentialError, CredentialManagementService, OAuthProviderConfig, OAuthProviderRegistry,
    StandardCredentialManagementService,
};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialMetadata,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::{AccessContext, SecretPath, SensitiveString};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// In-memory test repository
// ---------------------------------------------------------------------------

#[derive(Default)]
struct InMemoryCredentialRepo {
    bindings: RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>,
    pending: RwLock<HashMap<String, OAuthPendingState>>,
}

#[async_trait]
impl CredentialBindingRepository for InMemoryCredentialRepo {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.bindings
            .write()
            .await
            .insert(binding.id, binding.clone());
        Ok(())
    }

    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.bindings.read().await.get(id).cloned())
    }

    async fn find_by_owner(
        &self,
        _tenant_id: &TenantId,
        _owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self.bindings.read().await.values().cloned().collect())
    }

    async fn find_active_grants_for_target(
        &self,
        _tenant_id: &TenantId,
        _owner_user_id: &str,
        _provider: &CredentialProvider,
        _target: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }

    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.bindings.write().await.remove(id);
        Ok(())
    }

    async fn save_oauth_state(
        &self,
        state: &str,
        binding_id: &CredentialBindingId,
        pkce_verifier: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<()> {
        self.pending.write().await.insert(
            state.to_string(),
            OAuthPendingState {
                state: state.to_string(),
                binding_id: *binding_id,
                pkce_verifier: pkce_verifier.to_string(),
                redirect_uri: redirect_uri.to_string(),
                created_at: Utc::now(),
            },
        );
        Ok(())
    }

    async fn find_oauth_state(&self, state: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(self.pending.read().await.get(state).cloned())
    }

    async fn delete_oauth_state(&self, state: &str) -> anyhow::Result<()> {
        self.pending.write().await.remove(state);
        Ok(())
    }

    async fn delete_expired_oauth_states(&self, _older_than: DateTime<Utc>) -> anyhow::Result<u64> {
        Ok(0)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    service: StandardCredentialManagementService,
    repo: Arc<InMemoryCredentialRepo>,
    secrets: Arc<SecretsManager>,
    binding_id: CredentialBindingId,
    state: String,
}

async fn setup_harness(token_url: String) -> Harness {
    let repo = Arc::new(InMemoryCredentialRepo::default());
    let event_bus = Arc::new(EventBus::new(64));
    let secret_store = Arc::new(TestSecretStore::new());
    let secrets = Arc::new(SecretsManager::from_store(
        secret_store.clone(),
        event_bus.clone(),
    ));

    let mut registry: OAuthProviderRegistry = HashMap::new();
    registry.insert(
        CredentialProvider::GitHub,
        OAuthProviderConfig {
            token_url,
            client_id: "test-client-id".to_string(),
            client_secret: Some(SensitiveString::new("test-client-secret")),
        },
    );
    let oauth_providers = Arc::new(registry);

    let tenant_id = TenantId::consumer();
    let binding_id = CredentialBindingId::new();
    let now = Utc::now();
    let binding = UserCredentialBinding {
        id: binding_id,
        owner_user_id: "user-sub-abc".to_string(),
        tenant_id: tenant_id.clone(),
        credential_type: CredentialType::OAuth2,
        provider: CredentialProvider::GitHub,
        secret_path: SecretPath::new("PENDING_OAUTH", "PENDING_OAUTH", "PENDING_OAUTH"),
        scope: CredentialScope::Personal,
        status: CredentialStatus::PendingOAuth,
        metadata: CredentialMetadata {
            label: "GitHub OAuth connection".to_string(),
            tags: None,
            service_url: None,
            external_account_id: None,
            oauth_scopes: None,
        },
        grants: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    repo.save(&binding).await.unwrap();

    let state = "test-state-token".to_string();
    repo.save_oauth_state(
        &state,
        &binding_id,
        "test-pkce-verifier",
        "https://app.example/oauth/callback",
    )
    .await
    .unwrap();

    let service = StandardCredentialManagementService::with_http_client(
        repo.clone(),
        secrets.clone(),
        event_bus.clone(),
        oauth_providers,
        reqwest::Client::new(),
    );

    Harness {
        service,
        repo,
        secrets,
        binding_id,
        state,
    }
}

fn downcast_credential_error(err: &anyhow::Error) -> &CredentialError {
    err.downcast_ref::<CredentialError>()
        .expect("expected CredentialError variant at error chain root")
}

// ---------------------------------------------------------------------------
// Test 1 — Happy path: provider returns 200 with real token.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oauth_exchange_happy_path_stores_real_access_token() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/token")
        .match_header("content-type", "application/x-www-form-urlencoded")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("grant_type".into(), "authorization_code".into()),
            mockito::Matcher::UrlEncoded("code".into(), "provider-auth-code-123".into()),
            mockito::Matcher::UrlEncoded(
                "redirect_uri".into(),
                "https://app.example/oauth/callback".into(),
            ),
            mockito::Matcher::UrlEncoded("client_id".into(), "test-client-id".into()),
            mockito::Matcher::UrlEncoded("client_secret".into(), "test-client-secret".into()),
            mockito::Matcher::UrlEncoded("code_verifier".into(), "test-pkce-verifier".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "access_token": "gha_realAccessTokenFromProvider",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "gha_realRefreshToken",
                "scope": "repo user:email"
            }"#,
        )
        .create_async()
        .await;

    let harness = setup_harness(format!("{}/token", server.url())).await;

    let binding_id = harness
        .service
        .complete_oauth_connection(&harness.state, "provider-auth-code-123")
        .await
        .expect("exchange should succeed");

    mock.assert_async().await;
    assert_eq!(binding_id, harness.binding_id);

    // Binding transitioned to Active with real secret_path.
    let binding = harness
        .repo
        .find_by_id(&binding_id)
        .await
        .unwrap()
        .expect("binding present");
    assert_eq!(binding.status, CredentialStatus::Active);
    assert_ne!(binding.secret_path.path, "PENDING_OAUTH");

    // Stored secret contains the real access token — NOT the placeholder.
    let stored = harness
        .secrets
        .read_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            &AccessContext::system("test"),
        )
        .await
        .expect("secret should be written");

    assert_eq!(
        stored.get("access_token").map(|s| s.expose()),
        Some("gha_realAccessTokenFromProvider")
    );
    assert_eq!(
        stored.get("refresh_token").map(|s| s.expose()),
        Some("gha_realRefreshToken")
    );
    assert_eq!(
        stored.get("scope").map(|s| s.expose()),
        Some("repo user:email")
    );
    assert!(
        stored.contains_key("expires_at"),
        "expires_at should be stored when expires_in is present"
    );

    // Pending state removed (PKCE verifier is single-use per RFC 7636 §4.6).
    assert!(harness
        .repo
        .find_oauth_state(&harness.state)
        .await
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// Test 2 — Error path: provider returns 400 invalid_grant.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oauth_exchange_error_returns_typed_error_and_writes_no_secret() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/token")
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error": "invalid_grant", "error_description": "code expired"}"#)
        .create_async()
        .await;

    let harness = setup_harness(format!("{}/token", server.url())).await;

    let err = harness
        .service
        .complete_oauth_connection(&harness.state, "expired-code")
        .await
        .expect_err("exchange should fail");

    mock.assert_async().await;

    match downcast_credential_error(&err) {
        CredentialError::OAuthExchangeFailed { error, description } => {
            assert_eq!(error, "invalid_grant");
            assert_eq!(description.as_deref(), Some("code expired"));
        }
        other => panic!("unexpected error variant: {other:?}"),
    }

    // No secret should have been written — the binding's secret_path is still
    // the pre-exchange placeholder.
    let binding = harness
        .repo
        .find_by_id(&harness.binding_id)
        .await
        .unwrap()
        .expect("binding present");
    assert_eq!(binding.secret_path.path, "PENDING_OAUTH");
    let read = harness
        .secrets
        .read_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            &AccessContext::system("test"),
        )
        .await;
    assert!(
        read.is_err(),
        "no secret should be written on exchange failure"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — Regression: `PENDING_REAL_TOKEN` must never appear in the store.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_pending_real_token_placeholder_is_never_stored() {
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("POST", "/token")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{
                "access_token": "real-token-xyz",
                "token_type": "Bearer",
                "expires_in": 1800
            }"#,
        )
        .create_async()
        .await;

    let harness = setup_harness(format!("{}/token", server.url())).await;

    harness
        .service
        .complete_oauth_connection(&harness.state, "auth-code")
        .await
        .expect("exchange should succeed");

    let binding = harness
        .repo
        .find_by_id(&harness.binding_id)
        .await
        .unwrap()
        .expect("binding present");
    let stored = harness
        .secrets
        .read_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            &AccessContext::system("test"),
        )
        .await
        .expect("secret written");

    for (k, v) in stored.iter() {
        assert_ne!(
            v.expose(),
            "PENDING_REAL_TOKEN",
            "stored field {k} contains the placeholder — OAuth exchange regressed"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 4 — HTTPS enforcement on the token_url.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oauth_exchange_rejects_insecure_non_localhost_token_url() {
    // Use an http:// URL on a non-localhost host. The guard must reject this
    // before any HTTP call is issued.
    let harness = setup_harness("http://evil.example.com/token".to_string()).await;

    let err = harness
        .service
        .complete_oauth_connection(&harness.state, "auth-code")
        .await
        .expect_err("insecure token_url must be rejected");

    match downcast_credential_error(&err) {
        CredentialError::InsecureTokenUrl(url) => {
            assert!(url.contains("evil.example.com"));
        }
        other => panic!("expected InsecureTokenUrl, got: {other:?}"),
    }

    // No secret should have been written.
    let binding = harness
        .repo
        .find_by_id(&harness.binding_id)
        .await
        .unwrap()
        .expect("binding present");
    assert_eq!(binding.secret_path.path, "PENDING_OAUTH");
}
