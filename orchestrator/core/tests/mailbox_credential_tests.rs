// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Mailbox connections, AEGIS ADR-125 D1 to D3.
//!
//! - D1: the type `mailbox`, the providers `google_mail` and `imap`, and an
//!   `imap` binding created only after a live IMAP and SMTP check, run here
//!   against loopback stand-ins (`support/mail_standins.rs`).
//! - D2: the OAuth provider registry built from the node configuration's
//!   `oauth_providers` block, and the authorization URL carrying `scope` and
//!   the extra authorization parameters.
//! - D3: `access_token_for`, refreshing inside 60 seconds of `expires_at`,
//!   and an `invalid_grant` answer setting the binding `Expired`.
//!
//! Every server these tests talk to is on 127.0.0.1: the mail stand-ins and
//! a mockito token endpoint. No request leaves the machine.

#[path = "support/mail_standins.rs"]
mod mail_standins;

use aegis_orchestrator_core::application::credential_service::{
    oauth_provider_registry_from_config, CreateImapMailboxCommand, CredentialError,
    CredentialManagementService, OAuthProviderConfig, OAuthProviderRegistry,
    StandardCredentialManagementService,
};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, CredentialStatus, CredentialType, GrantTarget, MailSecurity, MailboxSettings,
    OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::events::CredentialEvent;
use aegis_orchestrator_core::domain::node_config::{NodeConfigManifest, OAuthProviderEntry};
use aegis_orchestrator_core::domain::secrets::{AccessContext, SensitiveString};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus};
use aegis_orchestrator_core::infrastructure::mail::SessionMailboxProbe;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use mail_standins::{
    imap_standin, smtp_standin, smtp_submitted_a_message, PlainConnector, IMAP_REFUSAL,
    SMTP_REFUSAL,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;

const USER: &str = "user-sub-mail";
const MAILBOX_USER: &str = "jeshua@example.test";
const MAILBOX_PASSWORD: &str = "Mk7-mailbox-password-marker";
const REDIRECT: &str = "https://ask.example/vault/connections/callback";

// ---------------------------------------------------------------------------
// In-memory binding store
// ---------------------------------------------------------------------------

#[derive(Default)]
struct InMemoryRepo {
    bindings: RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>,
    pending: RwLock<HashMap<String, OAuthPendingState>>,
}

#[async_trait]
impl CredentialBindingRepository for InMemoryRepo {
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

struct Harness {
    service: StandardCredentialManagementService,
    repo: Arc<InMemoryRepo>,
    secrets: Arc<SecretsManager>,
    event_bus: Arc<EventBus>,
}

fn harness(registry: OAuthProviderRegistry) -> Harness {
    let repo = Arc::new(InMemoryRepo::default());
    let event_bus = Arc::new(EventBus::new(256));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        event_bus.clone(),
    ));
    let service = StandardCredentialManagementService::with_http_client(
        repo.clone(),
        secrets.clone(),
        event_bus.clone(),
        Arc::new(registry),
        reqwest::Client::new(),
    )
    .with_mailbox_probe(Arc::new(SessionMailboxProbe::new(Arc::new(PlainConnector))));
    Harness {
        service,
        repo,
        secrets,
        event_bus,
    }
}

fn tenant() -> TenantId {
    TenantId::for_consumer_user(USER).expect("consumer tenant")
}

fn settings(imap_port: u16, smtp_port: u16, security: MailSecurity) -> MailboxSettings {
    MailboxSettings {
        address: MAILBOX_USER.to_string(),
        display_name: Some("Jeshua".to_string()),
        imap_host: "127.0.0.1".to_string(),
        imap_port,
        imap_security: security.clone(),
        smtp_host: "127.0.0.1".to_string(),
        smtp_port,
        smtp_security: security,
        username: MAILBOX_USER.to_string(),
    }
}

fn command(settings: MailboxSettings, password: &str) -> CreateImapMailboxCommand {
    CreateImapMailboxCommand {
        owner_user_id: USER.to_string(),
        tenant_id: tenant(),
        label: None,
        scope: CredentialScope::Personal,
        settings,
        password: SensitiveString::new(password),
    }
}

fn credential_error(err: &anyhow::Error) -> &CredentialError {
    err.downcast_ref::<CredentialError>()
        .unwrap_or_else(|| panic!("expected a CredentialError, got: {err:#}"))
}

// ---------------------------------------------------------------------------
// D1 — the names
// ---------------------------------------------------------------------------

#[test]
fn mailbox_type_and_both_providers_serialise_by_their_adr_names() {
    assert_eq!(
        serde_json::to_string(&CredentialType::Mailbox).unwrap(),
        "\"mailbox\""
    );
    assert_eq!(
        serde_json::to_string(&CredentialProvider::GoogleMail).unwrap(),
        "\"google_mail\""
    );
    assert_eq!(
        serde_json::to_string(&CredentialProvider::Imap).unwrap(),
        "\"imap\""
    );
    assert_eq!(CredentialProvider::GoogleMail.to_string(), "google_mail");
    assert_eq!(CredentialProvider::Imap.to_string(), "imap");
    assert_eq!(
        CredentialProvider::from_name("google_mail"),
        CredentialProvider::GoogleMail
    );
    assert_eq!(
        CredentialProvider::from_name("imap"),
        CredentialProvider::Imap
    );
    assert_eq!(
        serde_json::to_string(&MailSecurity::Starttls).unwrap(),
        "\"starttls\""
    );
    assert_eq!(
        serde_json::to_string(&MailSecurity::Tls).unwrap(),
        "\"tls\""
    );
}

// ---------------------------------------------------------------------------
// D1 — an imap binding after both live checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn imap_mailbox_is_created_after_both_standins_accept_and_its_password_is_only_in_openbao() {
    for security in [MailSecurity::Starttls, MailSecurity::Tls] {
        let imap = imap_standin(MAILBOX_USER, MAILBOX_PASSWORD).await;
        let smtp = smtp_standin(MAILBOX_USER, MAILBOX_PASSWORD).await;
        let h = harness(OAuthProviderRegistry::new());
        let mut events = h.event_bus.subscribe();

        let binding = h
            .service
            .create_imap_mailbox(command(
                settings(imap.port(), smtp.port(), security.clone()),
                MAILBOX_PASSWORD,
            ))
            .await
            .expect("both stand-ins accept, so the binding is created");

        assert_eq!(binding.credential_type, CredentialType::Mailbox);
        assert_eq!(binding.provider, CredentialProvider::Imap);
        assert_eq!(binding.status, CredentialStatus::Active);
        let stored_settings = binding.metadata.mailbox.clone().expect("mailbox settings");
        assert_eq!(stored_settings.imap_port, imap.port());
        assert_eq!(stored_settings.smtp_security, security);

        // The IMAP session ran LOGIN, SELECT INBOX and LOGOUT; the SMTP
        // session authenticated and submitted nothing.
        let imap_cmds = imap.commands();
        assert!(
            imap_cmds.iter().any(|c| c.contains("LOGIN")),
            "{imap_cmds:?}"
        );
        assert!(
            imap_cmds.iter().any(|c| c.ends_with("SELECT INBOX")),
            "{imap_cmds:?}"
        );
        assert!(
            imap_cmds.iter().any(|c| c.ends_with("LOGOUT")),
            "{imap_cmds:?}"
        );
        let smtp_cmds = smtp.commands();
        assert!(
            smtp_cmds.iter().any(|c| c.starts_with("AUTH")),
            "{smtp_cmds:?}"
        );
        assert!(smtp_cmds.iter().any(|c| c == "QUIT"), "{smtp_cmds:?}");
        assert!(
            !smtp_submitted_a_message(&smtp_cmds),
            "the check sent a message: {smtp_cmds:?}"
        );
        let starttls_seen = imap_cmds.iter().any(|c| c.ends_with("STARTTLS"))
            && smtp_cmds.iter().any(|c| c == "STARTTLS");
        assert_eq!(starttls_seen, security == MailSecurity::Starttls);

        // The password is in OpenBao under `password`, at the binding's path.
        let stored = h
            .secrets
            .read_secret(
                &binding.secret_path.effective_mount(),
                &binding.secret_path.path,
                &AccessContext::system("test"),
            )
            .await
            .expect("secret written");
        assert_eq!(
            stored.get("password").map(|s| s.expose()),
            Some(MAILBOX_PASSWORD)
        );
        assert!(binding
            .secret_path
            .path
            .ends_with(&binding.id.0.to_string()));

        // ...and nowhere else: not in the binding as served, not in the
        // binding as stored, not in any event published.
        let served = serde_json::to_string(&binding).unwrap();
        assert!(!served.contains(MAILBOX_PASSWORD), "{served}");
        let saved = h.repo.find_by_id(&binding.id).await.unwrap().unwrap();
        assert!(!serde_json::to_string(&saved)
            .unwrap()
            .contains(MAILBOX_PASSWORD));
        while let Ok(event) = events.try_recv() {
            let printed = format!("{event:?}");
            assert!(!printed.contains(MAILBOX_PASSWORD), "{printed}");
            if let Ok(json) = serde_json::to_string(&event) {
                assert!(!json.contains(MAILBOX_PASSWORD), "{json}");
            }
        }
    }
}

#[tokio::test]
async fn imap_refusing_the_login_answers_mailbox_unreachable_with_its_reply_and_stores_nothing() {
    let imap = imap_standin(MAILBOX_USER, "the-real-password").await;
    let smtp = smtp_standin(MAILBOX_USER, MAILBOX_PASSWORD).await;
    let h = harness(OAuthProviderRegistry::new());

    let err = h
        .service
        .create_imap_mailbox(command(
            settings(imap.port(), smtp.port(), MailSecurity::Starttls),
            MAILBOX_PASSWORD,
        ))
        .await
        .expect_err("a refused IMAP login must refuse the binding");

    match credential_error(&err) {
        CredentialError::MailboxUnreachable { protocol, reply } => {
            assert_eq!(protocol, "imap");
            assert!(
                reply.contains(IMAP_REFUSAL),
                "the reply was not the server's: {reply}"
            );
            assert!(!reply.contains(MAILBOX_PASSWORD));
        }
        other => panic!("expected MailboxUnreachable, got {other:?}"),
    }
    assert!(!format!("{err:#}").contains(MAILBOX_PASSWORD));
    assert!(
        h.repo.bindings.read().await.is_empty(),
        "a binding was saved"
    );
    assert!(
        smtp.commands().is_empty(),
        "SMTP was tried after IMAP refused"
    );
}

#[tokio::test]
async fn smtp_refusing_the_auth_answers_mailbox_unreachable_with_its_reply_and_stores_nothing() {
    let imap = imap_standin(MAILBOX_USER, MAILBOX_PASSWORD).await;
    let smtp = smtp_standin(MAILBOX_USER, "the-real-password").await;
    let h = harness(OAuthProviderRegistry::new());

    let err = h
        .service
        .create_imap_mailbox(command(
            settings(imap.port(), smtp.port(), MailSecurity::Tls),
            MAILBOX_PASSWORD,
        ))
        .await
        .expect_err("a refused SMTP AUTH must refuse the binding");

    match credential_error(&err) {
        CredentialError::MailboxUnreachable { protocol, reply } => {
            assert_eq!(protocol, "smtp");
            assert!(
                reply.contains(SMTP_REFUSAL),
                "the reply was not the server's: {reply}"
            );
        }
        other => panic!("expected MailboxUnreachable, got {other:?}"),
    }
    assert!(!format!("{err:#}").contains(MAILBOX_PASSWORD));
    assert!(
        h.repo.bindings.read().await.is_empty(),
        "a binding was saved"
    );
    assert!(!smtp_submitted_a_message(&smtp.commands()));
}

#[tokio::test]
async fn a_closed_port_answers_mailbox_unreachable() {
    // Bind and drop: nothing listens on the port afterwards.
    let port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let smtp = smtp_standin(MAILBOX_USER, MAILBOX_PASSWORD).await;
    let h = harness(OAuthProviderRegistry::new());
    let err = h
        .service
        .create_imap_mailbox(command(
            settings(port, smtp.port(), MailSecurity::Tls),
            MAILBOX_PASSWORD,
        ))
        .await
        .expect_err("nothing listens");
    assert!(matches!(
        credential_error(&err),
        CredentialError::MailboxUnreachable { protocol, .. } if protocol == "imap"
    ));
}

// ---------------------------------------------------------------------------
// D2 — the registry from the node configuration
// ---------------------------------------------------------------------------

const ADR_BLOCK: &str = r#"
apiVersion: 100monkeys.ai/v1
kind: NodeConfig
metadata:
  name: mailbox-test
spec:
  node:
    id: "node-1"
    type: orchestrator
  oauth_providers:
    - provider: google_mail
      authorization_url: "https://accounts.example/o/oauth2/v2/auth"
      token_url: "https://oauth2.example/token"
      client_id: "env:MBX_TEST_CLIENT_ID"
      client_secret: "env:MBX_TEST_CLIENT_SECRET"
      redirect_uri_allowlist:
        - "https://ask.example/vault/connections/callback"
      scopes:
        - "https://www.googleapis.com/auth/gmail.modify"
        - "openid"
        - "email"
      extra_authorization_params:
        access_type: "offline"
        prompt: "consent"
    - provider: github
      authorization_url: "https://github.example/login/oauth/authorize"
      token_url: "https://github.example/login/oauth/access_token"
      client_id: "plain-github-client"
      redirect_uri_allowlist:
        - "https://ask.example/vault/connections/callback"
"#;

fn parsed_block() -> Vec<OAuthProviderEntry> {
    let manifest = NodeConfigManifest::from_yaml_str(ADR_BLOCK).expect("the block parses");
    manifest.spec.oauth_providers
}

#[test]
fn the_oauth_providers_block_parses_and_builds_the_registry() {
    std::env::set_var("MBX_TEST_CLIENT_ID", "google-client-id");
    std::env::set_var("MBX_TEST_CLIENT_SECRET", "Mk7-google-client-secret");
    let entries = parsed_block();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].provider, "google_mail");
    assert_eq!(entries[0].client_id, "env:MBX_TEST_CLIENT_ID");
    assert_eq!(entries[0].scopes.len(), 3);
    assert_eq!(
        entries[0]
            .extra_authorization_params
            .get("access_type")
            .map(String::as_str),
        Some("offline")
    );
    assert!(entries[1].scopes.is_empty());
    assert!(entries[1].client_secret.is_none());

    let registry = oauth_provider_registry_from_config(&entries).expect("registry builds");
    let google = registry
        .get(&CredentialProvider::GoogleMail)
        .expect("google_mail registered");
    assert_eq!(google.client_id, "google-client-id");
    assert_eq!(
        google.client_secret.as_ref().map(|s| s.expose()),
        Some("Mk7-google-client-secret")
    );
    assert!(!format!("{google:?}").contains("Mk7-google-client-secret"));
    assert!(registry.contains_key(&CredentialProvider::GitHub));
}

#[test]
fn a_block_with_an_empty_client_id_is_refused_at_load() {
    let mut entries = parsed_block();
    entries[1].client_id = String::new();
    assert!(entries[1].validate().is_err());
    let err = oauth_provider_registry_from_config(&entries[1..])
        .expect_err("an empty client_id is refused");
    assert!(err.to_string().contains("client_id"), "{err}");

    // The env: form resolving to nothing is refused the same way.
    std::env::set_var("MBX_TEST_EMPTY_CLIENT_ID", "");
    let mut entry = parsed_block().remove(1);
    entry.client_id = "env:MBX_TEST_EMPTY_CLIENT_ID".to_string();
    let err = oauth_provider_registry_from_config(&[entry]).expect_err("empty after env");
    assert!(err.to_string().contains("client_id"), "{err}");
}

#[test]
fn a_block_whose_redirect_allowlist_holds_a_non_https_uri_is_refused_at_load() {
    let mut entry = parsed_block().remove(1);
    entry.redirect_uri_allowlist = vec!["http://ask.example/callback".to_string()];
    assert!(entry.validate().is_err());
    assert!(oauth_provider_registry_from_config(&[entry]).is_err());
}

#[test]
fn an_extra_parameter_naming_a_flow_parameter_is_refused_at_load() {
    let mut entry = parsed_block().remove(1);
    entry.extra_authorization_params.insert(
        "redirect_uri".to_string(),
        "https://evil.example/".to_string(),
    );
    assert!(entry.validate().is_err());
}

#[tokio::test]
async fn a_redirect_uri_outside_the_configured_allowlist_is_refused_at_initiate() {
    std::env::set_var("MBX_TEST_CLIENT_ID", "google-client-id");
    std::env::set_var("MBX_TEST_CLIENT_SECRET", "Mk7-google-client-secret");
    let registry = oauth_provider_registry_from_config(&parsed_block()).unwrap();
    let h = harness(registry);
    let err = h
        .service
        .initiate_oauth_connection(
            USER,
            &tenant(),
            CredentialProvider::GoogleMail,
            "https://attacker.example/vault/connections/callback".to_string(),
        )
        .await
        .expect_err("outside the allowlist");
    assert!(matches!(
        credential_error(&err),
        CredentialError::RedirectUriNotAllowlisted
    ));
}

fn query_of(url: &str) -> Vec<(String, String)> {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn the_authorization_url_carries_scope_and_every_extra_parameter_for_a_configured_provider() {
    std::env::set_var("MBX_TEST_CLIENT_ID", "google-client-id");
    std::env::set_var("MBX_TEST_CLIENT_SECRET", "Mk7-google-client-secret");
    let registry = oauth_provider_registry_from_config(&parsed_block()).unwrap();
    let h = harness(registry);

    let init = h
        .service
        .initiate_oauth_connection(
            USER,
            &tenant(),
            CredentialProvider::GoogleMail,
            REDIRECT.to_string(),
        )
        .await
        .expect("initiate");
    let q = query_of(init.authorization_url.expose());
    let get = |k: &str| {
        q.iter()
            .filter(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        get("scope"),
        vec!["https://www.googleapis.com/auth/gmail.modify openid email".to_string()]
    );
    assert_eq!(get("access_type"), vec!["offline".to_string()]);
    assert_eq!(get("prompt"), vec!["consent".to_string()]);
    assert_eq!(get("client_id"), vec!["google-client-id".to_string()]);
    assert_eq!(get("redirect_uri"), vec![REDIRECT.to_string()]);
    // The client secret never travels in the browser's redirect.
    assert!(!init
        .authorization_url
        .expose()
        .contains("Mk7-google-client-secret"));

    // The pending binding is a mailbox.
    let pending = h.repo.bindings.read().await;
    let b = pending.values().next().expect("pending binding");
    assert_eq!(b.credential_type, CredentialType::Mailbox);
    assert_eq!(b.provider, CredentialProvider::GoogleMail);
}

#[tokio::test]
async fn the_authorization_url_carries_neither_for_a_provider_configured_without_them() {
    let registry = oauth_provider_registry_from_config(&parsed_block()[1..]).unwrap();
    let h = harness(registry);
    let init = h
        .service
        .initiate_oauth_connection(
            USER,
            &tenant(),
            CredentialProvider::GitHub,
            REDIRECT.to_string(),
        )
        .await
        .expect("initiate");
    let keys: Vec<String> = query_of(init.authorization_url.expose())
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        keys,
        vec![
            "response_type",
            "client_id",
            "state",
            "code_challenge",
            "code_challenge_method",
            "redirect_uri"
        ]
    );
}

// ---------------------------------------------------------------------------
// D2 callback and D3 refresh, against a mockito token endpoint on loopback
// ---------------------------------------------------------------------------

fn google_registry(token_url: String) -> OAuthProviderRegistry {
    let mut registry = OAuthProviderRegistry::new();
    registry.insert(
        CredentialProvider::GoogleMail,
        OAuthProviderConfig {
            authorization_url: "https://accounts.example/o/oauth2/v2/auth".into(),
            token_url: token_url.into(),
            client_id: "google-client-id".to_string(),
            client_secret: Some(SensitiveString::new("google-client-secret")),
            redirect_uri_allowlist: vec![REDIRECT.to_string()],
            scopes: vec![
                "https://www.googleapis.com/auth/gmail.modify".to_string(),
                "openid".to_string(),
                "email".to_string(),
            ],
            extra_authorization_params: BTreeMap::from([(
                "access_type".to_string(),
                "offline".to_string(),
            )]),
        },
    );
    registry
}

fn id_token(email: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(
        serde_json::json!({"iss":"https://accounts.example","email":email,"email_verified":true})
            .to_string(),
    );
    format!("{header}.{claims}.c2lnbmF0dXJl")
}

/// Connect a google_mail mailbox through initiate and callback, the token
/// endpoint answering with `expires_in`.
async fn connected_google_mailbox(
    server: &mut mockito::ServerGuard,
    expires_in: i64,
) -> (Harness, CredentialBindingId) {
    let exchange = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::UrlEncoded(
            "grant_type".into(),
            "authorization_code".into(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "access_token": "ya29.first-access-token",
                "token_type": "Bearer",
                "expires_in": expires_in,
                "refresh_token": "1//first-refresh-token",
                "scope": "https://www.googleapis.com/auth/gmail.modify openid https://www.googleapis.com/auth/userinfo.email",
                "id_token": id_token("jeshua@100monkeys.example"),
            })
            .to_string(),
        )
        .create_async()
        .await;
    let h = harness(google_registry(format!("{}/token", server.url())));
    let init = h
        .service
        .initiate_oauth_connection(
            USER,
            &tenant(),
            CredentialProvider::GoogleMail,
            REDIRECT.to_string(),
        )
        .await
        .unwrap();
    let id = h
        .service
        .complete_oauth_connection(&init.state, "auth-code")
        .await
        .expect("callback completes");
    exchange.assert_async().await;
    (h, id)
}

#[tokio::test]
async fn a_google_mail_callback_stores_a_mailbox_binding_with_the_address_and_granted_scopes() {
    let mut server = mockito::Server::new_async().await;
    let (h, id) = connected_google_mailbox(&mut server, 3600).await;
    let b = h.repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(b.credential_type, CredentialType::Mailbox);
    assert_eq!(b.provider, CredentialProvider::GoogleMail);
    assert_eq!(b.status, CredentialStatus::Active);
    assert_eq!(
        b.metadata.external_account_id.as_deref(),
        Some("jeshua@100monkeys.example")
    );
    assert_eq!(
        b.metadata.oauth_scopes,
        Some(vec![
            "https://www.googleapis.com/auth/gmail.modify".to_string(),
            "openid".to_string(),
            "https://www.googleapis.com/auth/userinfo.email".to_string(),
        ])
    );
    assert!(!serde_json::to_string(&b)
        .unwrap()
        .contains("first-access-token"));
}

#[tokio::test]
async fn access_token_for_returns_the_stored_token_without_a_refresh_when_expiry_is_far() {
    let mut server = mockito::Server::new_async().await;
    let (h, id) = connected_google_mailbox(&mut server, 3600).await;
    let refresh = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::UrlEncoded(
            "grant_type".into(),
            "refresh_token".into(),
        ))
        .expect(0)
        .create_async()
        .await;
    let token = h.service.access_token_for(&id).await.expect("token");
    assert_eq!(token.expose(), "ya29.first-access-token");
    refresh.assert_async().await;
}

#[tokio::test]
async fn access_token_for_refreshes_when_expiry_is_inside_sixty_seconds() {
    let mut server = mockito::Server::new_async().await;
    // Expires in 30 s: inside the 60-second margin.
    let (h, id) = connected_google_mailbox(&mut server, 30).await;
    let refresh = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::AllOf(vec![
            mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            mockito::Matcher::UrlEncoded("refresh_token".into(), "1//first-refresh-token".into()),
            mockito::Matcher::UrlEncoded("client_id".into(), "google-client-id".into()),
            mockito::Matcher::UrlEncoded("client_secret".into(), "google-client-secret".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"access_token":"ya29.second-access-token","token_type":"Bearer","expires_in":3599}"#)
        .expect(1)
        .create_async()
        .await;

    let token = h.service.access_token_for(&id).await.expect("refreshed");
    assert_eq!(token.expose(), "ya29.second-access-token");
    refresh.assert_async().await;

    let b = h.repo.find_by_id(&id).await.unwrap().unwrap();
    let stored = h
        .secrets
        .read_secret(
            &b.secret_path.effective_mount(),
            &b.secret_path.path,
            &AccessContext::system("test"),
        )
        .await
        .unwrap();
    assert_eq!(
        stored.get("access_token").map(|s| s.expose()),
        Some("ya29.second-access-token")
    );
    // No new refresh token was returned: the old one is kept.
    assert_eq!(
        stored.get("refresh_token").map(|s| s.expose()),
        Some("1//first-refresh-token")
    );
    let expires_at: DateTime<Utc> = stored["expires_at"].expose().parse().unwrap();
    assert!(expires_at > Utc::now() + chrono::Duration::seconds(3000));

    // A second call within the new lifetime does not refresh again.
    let again = h.service.access_token_for(&id).await.unwrap();
    assert_eq!(again.expose(), "ya29.second-access-token");
    refresh.assert_async().await;
}

#[tokio::test]
async fn invalid_grant_on_refresh_sets_the_binding_expired_and_publishes_the_event() {
    let mut server = mockito::Server::new_async().await;
    let (h, id) = connected_google_mailbox(&mut server, 10).await;
    let _refresh = server
        .mock("POST", "/token")
        .match_body(mockito::Matcher::UrlEncoded(
            "grant_type".into(),
            "refresh_token".into(),
        ))
        .with_status(400)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )
        .create_async()
        .await;
    let mut events = h.event_bus.subscribe();

    let err = h
        .service
        .access_token_for(&id)
        .await
        .expect_err("invalid_grant");
    assert!(matches!(
        credential_error(&err),
        CredentialError::OAuthExchangeFailed { error, .. } if error == "invalid_grant"
    ));

    let b = h.repo.find_by_id(&id).await.unwrap().unwrap();
    assert_eq!(b.status, CredentialStatus::Expired);

    let mut expired = false;
    while let Ok(event) = events.try_recv() {
        if let DomainEvent::Credential(CredentialEvent::CredentialExpired {
            binding_id,
            tenant_id,
        }) = event
        {
            assert_eq!(binding_id, id);
            assert_eq!(tenant_id, tenant());
            expired = true;
        }
    }
    assert!(expired, "no CredentialExpired event was published");

    // An expired binding answers without another request.
    let err = h.service.access_token_for(&id).await.expect_err("expired");
    assert!(err.to_string().contains("not active"), "{err}");
}
