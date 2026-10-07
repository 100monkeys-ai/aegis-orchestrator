//! A person's credential is kept where the deployed secret-store policy
//! admits it (AEGIS ADR-056 line 335: "Consumer user secrets reside in
//! `tenant-zaru-consumer/`."; ADR-125 Update).
//!
//! The store double enforces the `aegis-platform` policy as written in
//! `aegis-platform-deployment`'s `scripts/bootstrap-openbao.sh`, matched as
//! OpenBao matches it: literal segments, `+` for one whole segment, `*` as the
//! last character for any suffix. `AEGIS_OPENBAO_BOOTSTRAP_SCRIPT` names that
//! script (a deployment checkout); without it the checked-in copy of its
//! policy, `secrets_policy_fixture.hcl`, is enforced. The two rules a
//! person's credential needs are asserted here and by the deployment
//! repository's `tests/test-bootstrap-openbao-policy.sh`, so neither copy
//! can drift from the other unnoticed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use crate::application::credential_service::{
    CreateImapMailboxCommand, CredentialActor, CredentialManagementService, OAuthProviderRegistry,
    StandardCredentialManagementService, StoreApiKeyCommand, ToolCallActor, ToolCredentialSource,
};
use crate::domain::agent::AgentId;
use crate::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, CredentialType, GrantTarget, MailSecurity, MailboxSettings, OAuthPendingState,
    UserCredentialBinding,
};
use crate::domain::secrets::{
    AccessContext, DomainDynamicSecret, SecretPath, SecretStore, SecretsError, SensitiveString,
};
use crate::domain::tenant::TenantId;
use crate::infrastructure::event_bus::EventBus;
use crate::infrastructure::mail::{MailboxCheckFailure, MailboxProbe};
use crate::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};

const PERSON_DATA_RULE: &str = r#"path "tenant-zaru-consumer/kv/data/users/*""#;
const PERSON_METADATA_RULE: &str = r#"path "tenant-zaru-consumer/kv/metadata/users/*""#;

const ALICE: &str = "d7f81700-35d3-49b6-b237-c391ccc19035";
const BOB: &str = "0b6c2f4e-1a2b-4c3d-8e9f-0a1b2c3d4e5f";

fn policy_text() -> String {
    match std::env::var("AEGIS_OPENBAO_BOOTSTRAP_SCRIPT") {
        Ok(script) => {
            let text =
                std::fs::read_to_string(&script).unwrap_or_else(|e| panic!("read {script}: {e}"));
            let start = "bao policy write aegis-platform - <<'EOF'\n";
            let from = text.find(start).expect("the aegis-platform policy heredoc") + start.len();
            let len = text[from..].find("\nEOF\n").expect("the heredoc's end");
            text[from..from + len].to_string()
        }
        Err(_) => include_str!("secrets_policy_fixture.hcl").to_string(),
    }
}

/// The policy's rules as `(path, capabilities)`.
fn rules(policy: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut lines = policy
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'));
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("path \"") else {
            continue;
        };
        let path = rest[..rest.find('"').expect("closing quote")].to_string();
        let caps_line = lines.next().expect("a capabilities line");
        let caps = caps_line
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        out.push((path, caps));
    }
    out
}

fn matches(rule: &str, path: &str) -> bool {
    let glob = rule.ends_with('*');
    let rule = if glob { &rule[..rule.len() - 1] } else { rule };
    let r: Vec<&str> = rule.split('/').collect();
    let q: Vec<&str> = path.split('/').collect();
    if q.len() < r.len() || (!glob && q.len() != r.len()) {
        return false;
    }
    for (i, seg) in r.iter().enumerate() {
        if *seg == "+" {
            continue;
        }
        if glob && i == r.len() - 1 {
            if !q[i..].join("/").starts_with(seg) {
                return false;
            }
        } else if *seg != q[i] {
            return false;
        }
    }
    true
}

/// A KV v2 store that answers 403, as OpenBao does, for any call the policy
/// does not grant; `engine/data/path` is the API path of a KV v2 secret.
struct PolicyEnforcingStore {
    inner: TestSecretStore,
    rules: Vec<(String, Vec<String>)>,
}

impl PolicyEnforcingStore {
    fn new() -> Self {
        Self {
            inner: TestSecretStore::new(),
            rules: rules(&policy_text()),
        }
    }

    fn require(&self, caps: &[&str], engine: &str, path: &str) -> Result<(), SecretsError> {
        let api = format!("{engine}/data/{path}");
        let granted = caps.iter().all(|cap| {
            self.rules
                .iter()
                .any(|(rule, have)| matches(rule, &api) && have.iter().any(|h| h == cap))
        });
        if granted {
            Ok(())
        } else {
            Err(SecretsError::ConnectionError(format!(
                "The Vault server returned an error (status code 403): {api}"
            )))
        }
    }
}

#[async_trait]
impl SecretStore for PolicyEnforcingStore {
    async fn read(
        &self,
        engine: &str,
        path: &str,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
        self.require(&["read"], engine, path)?;
        self.inner.read(engine, path).await
    }
    async fn write(
        &self,
        engine: &str,
        path: &str,
        secret: HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError> {
        self.require(&["create", "update"], engine, path)?;
        self.inner.write(engine, path, secret).await
    }
    async fn delete(&self, engine: &str, path: &str) -> Result<(), SecretsError> {
        self.require(&["delete"], engine, path)?;
        self.inner.delete(engine, path).await
    }
    async fn generate_dynamic(
        &self,
        engine: &str,
        role: &str,
    ) -> Result<DomainDynamicSecret, SecretsError> {
        self.inner.generate_dynamic(engine, role).await
    }
    async fn renew_lease(&self, lease_id: &str, inc: Duration) -> Result<Duration, SecretsError> {
        self.inner.renew_lease(lease_id, inc).await
    }
    async fn revoke_lease(&self, lease_id: &str) -> Result<(), SecretsError> {
        self.inner.revoke_lease(lease_id).await
    }
    async fn transit_sign(&self, key: &str, data: &[u8]) -> Result<String, SecretsError> {
        self.inner.transit_sign(key, data).await
    }
    async fn transit_verify(
        &self,
        key: &str,
        data: &[u8],
        sig: &str,
    ) -> Result<bool, SecretsError> {
        self.inner.transit_verify(key, data, sig).await
    }
    async fn transit_encrypt(&self, key: &str, pt: &[u8]) -> Result<String, SecretsError> {
        self.inner.transit_encrypt(key, pt).await
    }
    async fn transit_decrypt(&self, key: &str, ct: &str) -> Result<Vec<u8>, SecretsError> {
        self.inner.transit_decrypt(key, ct).await
    }
}

#[derive(Default)]
struct InMemoryRepo {
    bindings: RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>,
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
    /// Every binding, whoever owns it: the service's own checks are tested.
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
        _state: &str,
        _binding_id: &CredentialBindingId,
        _pkce_verifier: &str,
        _redirect_uri: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn find_oauth_state(&self, _state: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _state: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(&self, _older: DateTime<Utc>) -> anyhow::Result<u64> {
        Ok(0)
    }
}

struct AcceptingProbe;

#[async_trait]
impl MailboxProbe for AcceptingProbe {
    async fn check(
        &self,
        _settings: &MailboxSettings,
        _password: &SensitiveString,
    ) -> Result<(), MailboxCheckFailure> {
        Ok(())
    }
    async fn check_xoauth2(
        &self,
        _settings: &MailboxSettings,
        _token: &SensitiveString,
    ) -> Result<(), MailboxCheckFailure> {
        Ok(())
    }
}

struct Harness {
    service: StandardCredentialManagementService,
    repo: Arc<InMemoryRepo>,
    secrets: Arc<SecretsManager>,
}

fn harness() -> Harness {
    let repo = Arc::new(InMemoryRepo::default());
    let event_bus = Arc::new(EventBus::new(64));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(PolicyEnforcingStore::new()),
        event_bus.clone(),
    ));
    let service = StandardCredentialManagementService::new(
        repo.clone(),
        secrets.clone(),
        event_bus,
        Arc::new(OAuthProviderRegistry::new()),
    )
    .with_mailbox_probe(Arc::new(AcceptingProbe));
    Harness {
        service,
        repo,
        secrets,
    }
}

fn person(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).expect("a person's tenant")
}

async fn grant_all_agents(h: &Harness, id: CredentialBindingId) {
    let mut binding = h.repo.find_by_id(&id).await.unwrap().unwrap();
    binding.add_grant(GrantTarget::AllAgents, ALICE.to_string());
    h.repo.save(&binding).await.unwrap();
}

async fn store(h: &Harness, sub: &str, provider: &str, ty: CredentialType) -> CredentialBindingId {
    h.service
        .store_api_key(StoreApiKeyCommand {
            owner_user_id: sub.to_string(),
            tenant_id: person(sub),
            provider: CredentialProvider::new(provider),
            label: provider.to_string(),
            scope: CredentialScope::Personal,
            api_key_value: SensitiveString::new(format!("{provider}-value-of-{sub}")),
            credential_type: ty,
        })
        .await
        .unwrap_or_else(|e| panic!("store {provider}: {e:#}"))
}

fn actor<'a>(tenant: &'a TenantId, sub: &'a str) -> ToolCallActor<'a> {
    ToolCallActor {
        tenant_id: tenant,
        user_id: sub,
        agent_id: AgentId::new(),
        workflow_id: None,
        context: crate::domain::execution::ContextChoice::NotGiven,
    }
}

#[test]
fn the_policy_carries_the_two_rules_a_persons_credential_needs() {
    let policy = policy_text();
    assert!(policy.contains(PERSON_DATA_RULE), "{PERSON_DATA_RULE}");
    assert!(
        policy.contains(PERSON_METADATA_RULE),
        "{PERSON_METADATA_RULE}"
    );
}

/// A secret, a variable, a service account, an OAuth token and a mailbox
/// password, each stored and read back through the credential service
/// against the policy, in the realm's mount, at the person's path.
#[tokio::test]
async fn person_credentials_are_kept_in_the_realm_mount_under_the_deployed_policy() {
    let h = harness();
    let tenant = person(ALICE);
    let ctx = AccessContext::system("test");

    for (provider, ty) in [
        ("openai", CredentialType::Secret),
        ("endpoint", CredentialType::Variable),
        ("gcp", CredentialType::ServiceAccount),
    ] {
        let id = store(&h, ALICE, provider, ty).await;
        grant_all_agents(&h, id).await;
        let binding = h.repo.find_by_id(&id).await.unwrap().unwrap();
        assert_eq!(
            binding.secret_path.effective_mount(),
            "tenant-zaru-consumer/kv"
        );
        assert_eq!(
            binding.secret_path.path,
            format!("users/{}/{ALICE}/credentials/{}", tenant.as_str(), id.0)
        );
        let read = h
            .service
            .tool_server_credential(&actor(&tenant, ALICE), provider)
            .await
            .unwrap_or_else(|e| panic!("read {provider}: {e:#}"))
            .expect("the credential");
        assert_eq!(read.expose(), format!("{provider}-value-of-{ALICE}"));
    }

    // OAuth: the token is written where the callback writes it (the
    // binding's mount and path) and read back by access_token_for.
    let id = store(&h, ALICE, "google", CredentialType::OAuth2).await;
    let binding = h.repo.find_by_id(&id).await.unwrap().unwrap();
    let mut token = HashMap::new();
    token.insert(
        "access_token".to_string(),
        SensitiveString::new("tok-alice"),
    );
    h.secrets
        .write_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            token,
            &ctx,
        )
        .await
        .expect("the OAuth token written under the policy");
    let read = h.service.access_token_for(&id).await.expect("access token");
    assert_eq!(read.expose(), "tok-alice");

    // Mailbox (SMTP with IMAP): the password is written by the service.
    let binding = h
        .service
        .create_imap_mailbox(CreateImapMailboxCommand {
            owner_user_id: ALICE.to_string(),
            tenant_id: tenant.clone(),
            label: None,
            scope: CredentialScope::Personal,
            settings: MailboxSettings {
                address: "alice@example.com".to_string(),
                display_name: None,
                imap_host: "imap.example.com".to_string(),
                imap_port: 993,
                imap_security: MailSecurity::Tls,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 465,
                smtp_security: MailSecurity::Tls,
                username: "alice@example.com".to_string(),
            },
            password: SensitiveString::new("mail-pw-alice"),
        })
        .await
        .unwrap_or_else(|e| panic!("mailbox: {e:#}"));
    let stored = h
        .secrets
        .read_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            &ctx,
        )
        .await
        .expect("the mailbox password read under the policy");
    assert_eq!(stored["password"].expose(), "mail-pw-alice");
}

#[tokio::test]
async fn another_person_cannot_read_a_persons_credential() {
    let h = harness();
    let id = store(&h, ALICE, "openai", CredentialType::Secret).await;
    grant_all_agents(&h, id).await;

    let bob = person(BOB);
    let read = h
        .service
        .tool_server_credential(&actor(&bob, BOB), "openai")
        .await
        .expect("no error");
    assert!(read.is_none(), "Bob read Alice's credential");
    let seen = h
        .service
        .get_binding(
            &CredentialActor::User {
                user_id: BOB.to_string(),
                tenant_id: bob.clone(),
            },
            &id,
        )
        .await
        .ok()
        .flatten();
    assert!(seen.is_none(), "Bob saw Alice's binding");
}

/// An organisation's, a team's and the system tenant's secrets resolve as
/// they did before this change; only a person's tenant (`u-<hex>`) and the
/// realm's own tenant use the realm's mount.
#[test]
fn organisation_team_and_system_tenants_resolve_as_before() {
    let cases = [
        ("acme", "tenant-acme", "tenant-acme/kv"),
        ("t-0b6c2f4e", "tenant-t-0b6c2f4e", "tenant-t-0b6c2f4e/kv"),
        (
            "zaru-consumer",
            "tenant-zaru-consumer",
            "tenant-zaru-consumer/kv",
        ),
        ("aegis-system", "aegis-system", "kv"),
    ];
    for (slug, namespace, mount) in cases {
        let p = SecretPath::for_tenant(TenantId::new(slug).unwrap(), "kv", "x");
        assert_eq!(
            (p.namespace.as_str(), p.effective_mount().as_str()),
            (namespace, mount),
            "{slug}"
        );
    }
    let p = SecretPath::for_tenant(person(ALICE), "kv", "x");
    assert_eq!(
        (p.namespace.as_str(), p.effective_mount().as_str()),
        ("tenant-zaru-consumer", "tenant-zaru-consumer/kv")
    );
}
