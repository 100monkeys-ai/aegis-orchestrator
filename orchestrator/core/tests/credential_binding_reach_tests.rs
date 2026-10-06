// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! A remote tool server's token is grounded when it is stored, rotated or
//! introspected, and its binding records what it reaches (AEGIS ADR-132
//! (7a) S2; the coordinator's B1 to B4): the real
//! `StandardCredentialManagementService` over in-memory bindings and
//! secrets, with a fake `RemoteServerGrounding` standing where the SEAL
//! gateway's `InvokeTool` of `cortex.ground` stands in the daemon.

use aegis_orchestrator_core::application::credential_service::{
    CredentialActor, CredentialError, CredentialManagementService, GroundingRefusal,
    OAuthProviderRegistry, RemoteServerGrounding, StandardCredentialManagementService,
    StoreApiKeyCommand,
};
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, CredentialType, GrantTarget, OAuthPendingState, ReachKind,
    UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::{
    AccessContext, DomainDynamicSecret, SecretStore, SecretsError, SensitiveString,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::RwLock;

const SERVER: &str = "nuclear-notes";
const OWNER: &str = "reach-owner-sub";
const TOKEN: &str = "Mk11-nn_mcp-instance-token";
const ROTATED: &str = "Mk11-nn_mcp-rotated-token";

// ---------------------------------------------------------------------------
// In-memory bindings and a secret store that counts its writes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Bindings(RwLock<HashMap<CredentialBindingId, UserCredentialBinding>>);

#[async_trait]
impl CredentialBindingRepository for Bindings {
    async fn save(&self, binding: &UserCredentialBinding) -> anyhow::Result<()> {
        self.0.write().await.insert(binding.id, binding.clone());
        Ok(())
    }
    async fn find_by_id(
        &self,
        id: &CredentialBindingId,
    ) -> anyhow::Result<Option<UserCredentialBinding>> {
        Ok(self.0.read().await.get(id).cloned())
    }
    async fn find_by_owner(
        &self,
        tenant_id: &TenantId,
        owner_user_id: &str,
    ) -> anyhow::Result<Vec<UserCredentialBinding>> {
        Ok(self
            .0
            .read()
            .await
            .values()
            .filter(|b| &b.tenant_id == tenant_id && b.owner_user_id == owner_user_id)
            .cloned()
            .collect())
    }
    async fn find_active_grants_for_target(
        &self,
        _: &TenantId,
        _: &str,
        _: &CredentialProvider,
        _: &GrantTarget,
    ) -> anyhow::Result<Vec<CredentialGrant>> {
        Ok(Vec::new())
    }
    async fn delete(&self, id: &CredentialBindingId) -> anyhow::Result<()> {
        self.0.write().await.remove(id);
        Ok(())
    }
    async fn save_oauth_state(
        &self,
        _: &str,
        _: &CredentialBindingId,
        _: &str,
        _: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    async fn find_oauth_state(&self, _: &str) -> anyhow::Result<Option<OAuthPendingState>> {
        Ok(None)
    }
    async fn delete_oauth_state(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn delete_expired_oauth_states(&self, _: DateTime<Utc>) -> anyhow::Result<u64> {
        Ok(0)
    }
}

/// The in-memory store, counting every write.
#[derive(Default)]
struct CountedStore {
    inner: TestSecretStore,
    writes: AtomicUsize,
}

#[async_trait]
impl SecretStore for CountedStore {
    async fn read(
        &self,
        engine: &str,
        path: &str,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
        self.inner.read(engine, path).await
    }
    async fn write(
        &self,
        engine: &str,
        path: &str,
        secret: HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write(engine, path, secret).await
    }
    async fn generate_dynamic(
        &self,
        engine: &str,
        role: &str,
    ) -> Result<DomainDynamicSecret, SecretsError> {
        self.inner.generate_dynamic(engine, role).await
    }
    async fn renew_lease(
        &self,
        lease_id: &str,
        increment: Duration,
    ) -> Result<Duration, SecretsError> {
        self.inner.renew_lease(lease_id, increment).await
    }
    async fn revoke_lease(&self, lease_id: &str) -> Result<(), SecretsError> {
        self.inner.revoke_lease(lease_id).await
    }
    async fn transit_sign(&self, key_name: &str, data: &[u8]) -> Result<String, SecretsError> {
        self.inner.transit_sign(key_name, data).await
    }
    async fn transit_verify(
        &self,
        key_name: &str,
        data: &[u8],
        signature: &str,
    ) -> Result<bool, SecretsError> {
        self.inner.transit_verify(key_name, data, signature).await
    }
    async fn transit_encrypt(
        &self,
        key_name: &str,
        plaintext: &[u8],
    ) -> Result<String, SecretsError> {
        self.inner.transit_encrypt(key_name, plaintext).await
    }
    async fn transit_decrypt(
        &self,
        key_name: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>, SecretsError> {
        self.inner.transit_decrypt(key_name, ciphertext).await
    }
}

// ---------------------------------------------------------------------------
// The fake grounding
// ---------------------------------------------------------------------------

/// One grounding call as the fake received it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grounded {
    tenant: String,
    user: String,
    server: String,
    token: String,
}

/// Answers every grounding with `answer`, recording each call.
struct FakeGrounding {
    answer: Mutex<Result<Value, GroundingRefusal>>,
    calls: Mutex<Vec<Grounded>>,
}

impl FakeGrounding {
    fn answering(answer: Result<Value, GroundingRefusal>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            calls: Mutex::new(Vec::new()),
        })
    }
    fn answer(&self, answer: Result<Value, GroundingRefusal>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn calls(&self) -> Vec<Grounded> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl RemoteServerGrounding for FakeGrounding {
    async fn ground_token(
        &self,
        tenant_id: &TenantId,
        user_id: &str,
        server: &str,
        token: &SensitiveString,
    ) -> Result<Value, GroundingRefusal> {
        self.calls.lock().unwrap().push(Grounded {
            tenant: tenant_id.as_str().to_string(),
            user: user_id.to_string(),
            server: server.to_string(),
            token: token.expose().to_string(),
        });
        self.answer.lock().unwrap().clone()
    }
}

/// A grounding payload whose token reaches `instances` (id, slug).
fn payload(instances: &[(&str, &str)]) -> Value {
    json!({
        "instance": {"id": "root-id", "slug": "main"},
        "you": {
            "userId": "u-1",
            "currentWorkspace": null,
            "instances": instances
                .iter()
                .map(|(id, slug)| json!({"id": id, "slug": slug, "name": slug, "role": "member"}))
                .collect::<Vec<_>>(),
        },
    })
}

struct Vault {
    service: StandardCredentialManagementService,
    bindings: Arc<Bindings>,
    store: Arc<CountedStore>,
    secrets: Arc<SecretsManager>,
    grounding: Arc<FakeGrounding>,
    tenant: TenantId,
}

fn vault(answer: Result<Value, GroundingRefusal>) -> Vault {
    let bindings = Arc::new(Bindings::default());
    let store = Arc::new(CountedStore::default());
    let event_bus = Arc::new(EventBus::new(64));
    let secrets = Arc::new(SecretsManager::from_store(store.clone(), event_bus.clone()));
    let service = StandardCredentialManagementService::new(
        bindings.clone(),
        secrets.clone(),
        event_bus,
        Arc::new(OAuthProviderRegistry::new()),
    );
    let grounding = FakeGrounding::answering(answer);
    let weak: Weak<FakeGrounding> = Arc::downgrade(&grounding);
    let weak: Weak<dyn RemoteServerGrounding> = weak;
    assert!(service.set_remote_grounding(weak, vec![SERVER.to_string()]));
    Vault {
        service,
        bindings,
        store,
        secrets,
        grounding,
        tenant: TenantId::for_consumer_user(OWNER).unwrap(),
    }
}

impl Vault {
    async fn store(&self, provider: &str, value: &str) -> anyhow::Result<CredentialBindingId> {
        self.service
            .store_api_key(StoreApiKeyCommand {
                owner_user_id: OWNER.to_string(),
                tenant_id: self.tenant.clone(),
                provider: CredentialProvider::new(provider),
                label: "Work instance".to_string(),
                scope: CredentialScope::Personal,
                api_key_value: SensitiveString::new(value),
                credential_type: CredentialType::Secret,
            })
            .await
    }

    fn owner(&self) -> CredentialActor {
        CredentialActor::User {
            user_id: OWNER.to_string(),
            tenant_id: self.tenant.clone(),
        }
    }

    async fn binding(&self, id: CredentialBindingId) -> UserCredentialBinding {
        self.bindings
            .find_by_id(&id)
            .await
            .unwrap()
            .expect("binding stored")
    }

    fn writes(&self) -> usize {
        self.store.writes.load(Ordering::SeqCst)
    }
}

fn credential_error(err: &anyhow::Error) -> &CredentialError {
    err.downcast_ref::<CredentialError>()
        .unwrap_or_else(|| panic!("not a CredentialError: {err}"))
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// S2, B2: a token whose grounding reports one instance is stored with
/// `reach {kind: instance}` and that instance's slug and id; the grounding
/// carried the token itself, acting as the binding's owner, at the server
/// the provider names.
#[tokio::test]
async fn an_instance_tokens_binding_records_the_instance_it_reaches() {
    let v = vault(Ok(payload(&[("inst-play2-id", "play2")])));
    let id = v.store(SERVER, TOKEN).await.expect("stored");

    let reach = v
        .binding(id)
        .await
        .metadata
        .reach
        .expect("the binding records what the token reaches");
    assert_eq!(reach.kind, ReachKind::Instance);
    assert_eq!(reach.instance_slug.as_deref(), Some("play2"));
    assert_eq!(reach.instance_id.as_deref(), Some("inst-play2-id"));
    assert_eq!(reach.workspace_id, None);
    let answered = serde_json::to_value(&reach).unwrap();
    println!("reach {answered}");
    assert_eq!(answered["kind"], "instance");
    assert!(answered.get("grounded_at").is_some());
    assert_eq!(
        v.grounding.calls(),
        vec![Grounded {
            tenant: v.tenant.as_str().to_string(),
            user: OWNER.to_string(),
            server: SERVER.to_string(),
            token: TOKEN.to_string(),
        }]
    );
}

/// S2, B2: a token whose grounding reports several instances is `apex`,
/// with no instance recorded.
#[tokio::test]
async fn an_apex_tokens_binding_records_apex() {
    let v = vault(Ok(payload(&[
        ("inst-100m-id", "100monkeys-ai"),
        ("inst-main-id", "main"),
    ])));
    let id = v.store(SERVER, TOKEN).await.expect("stored");

    let reach = v
        .binding(id)
        .await
        .metadata
        .reach
        .expect("the binding records what the token reaches");
    let answered = serde_json::to_value(&reach).unwrap();
    println!("reach {answered}");
    assert_eq!(reach.kind, ReachKind::Apex);
    assert_eq!(
        (reach.instance_slug, reach.instance_id, reach.workspace_id),
        (None, None, None)
    );
}

/// S2, B5: a grounding the gateway refuses is the store's refusal, naming
/// the gateway's code and sentence; no binding is stored and no secret is
/// written.
#[tokio::test]
async fn a_refused_grounding_stores_nothing_and_names_the_refusal() {
    let v = vault(Err(GroundingRefusal::Refused {
        code: "CREDENTIAL_REJECTED".to_string(),
        message: "The 'nuclear-notes' server refused your stored credential.".to_string(),
    }));
    let err = v.store(SERVER, TOKEN).await.expect_err("refused");

    println!("refusal {err}");
    assert_eq!(
        err.to_string(),
        "The token was not stored: grounding it at the remote server 'nuclear-notes' was \
         refused (CREDENTIAL_REJECTED): The 'nuclear-notes' server refused your stored credential."
    );
    assert!(matches!(
        credential_error(&err),
        CredentialError::ReachRefused { code, .. } if code == "CREDENTIAL_REJECTED"
    ));
    assert!(v.bindings.0.read().await.is_empty(), "a binding was stored");
    assert_eq!(v.writes(), 0, "a secret was written");
}

/// S2, B2: a payload without `you.instances` refuses the store, and one
/// whose list is empty refuses it too; nothing is stored or written.
#[tokio::test]
async fn a_payload_without_instances_is_refused() {
    let v = vault(Ok(
        json!({"instance": {"id": "root-id", "slug": "main"}, "you": {}}),
    ));
    let err = v.store(SERVER, TOKEN).await.expect_err("refused");
    println!("refusal {err}");
    assert_eq!(
        err.to_string(),
        "The token was not stored: the grounding 'nuclear-notes' answered did not say which \
         instances the token reaches."
    );

    v.grounding.answer(Ok(payload(&[])));
    let err = v.store(SERVER, TOKEN).await.expect_err("refused");
    println!("refusal {err}");
    assert_eq!(
        err.to_string(),
        "The token was not stored: the grounding 'nuclear-notes' answered says the token \
         reaches no instance."
    );
    assert!(v.bindings.0.read().await.is_empty(), "a binding was stored");
    assert_eq!(v.writes(), 0, "a secret was written");
}

/// S2 "and on demand": an introspection grounds the stored token again
/// and rewrites the reach; a refused one leaves the stored reach as it was.
#[tokio::test]
async fn introspect_rewrites_the_reach_when_the_payload_changes() {
    let v = vault(Ok(payload(&[("inst-play2-id", "play2")])));
    let id = v.store(SERVER, TOKEN).await.expect("stored");
    let first = v.binding(id).await.metadata.reach.expect("reach");
    assert_eq!(first.kind, ReachKind::Instance);

    v.grounding.answer(Ok(payload(&[
        ("inst-play2-id", "play2"),
        ("inst-main-id", "main"),
    ])));
    let introspected = v
        .service
        .introspect_binding(&v.owner(), &id)
        .await
        .expect("introspected");
    let rewritten = introspected.metadata.reach.clone().expect("reach");
    println!("reach {}", serde_json::to_value(&rewritten).unwrap());
    assert_eq!(rewritten.kind, ReachKind::Apex);
    assert_eq!(v.binding(id).await.metadata.reach, Some(rewritten.clone()));
    let calls = v.grounding.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].token, TOKEN, "the stored token is grounded again");

    v.grounding.answer(Err(GroundingRefusal::Refused {
        code: "CREDENTIAL_REJECTED".to_string(),
        message: "The 'nuclear-notes' server refused your stored credential.".to_string(),
    }));
    let err = v
        .service
        .introspect_binding(&v.owner(), &id)
        .await
        .expect_err("refused");
    println!("refusal {err}");
    assert!(err
        .to_string()
        .starts_with("The binding's reach was not rewritten: grounding it at the remote server"));
    assert_eq!(v.binding(id).await.metadata.reach, Some(rewritten));
}

/// S2: a provider that is not a remote server is never grounded and its
/// binding has no reach; introspecting it is refused by name.
#[tokio::test]
async fn a_provider_that_is_no_remote_server_is_never_grounded() {
    let v = vault(Ok(payload(&[("inst-play2-id", "play2")])));
    let id = v
        .store("openai", "sk-not-a-remote-token")
        .await
        .expect("stored");

    let binding = v.binding(id).await;
    assert_eq!(binding.metadata.reach, None);
    assert!(
        serde_json::to_value(&binding).unwrap()["metadata"]
            .get("reach")
            .is_none(),
        "a binding with no reach answers no reach"
    );
    println!("grounding calls {}", v.grounding.calls().len());
    assert!(v.grounding.calls().is_empty(), "a grounding was made");

    let err = v
        .service
        .introspect_binding(&v.owner(), &id)
        .await
        .expect_err("refused");
    println!("refusal {err}");
    assert_eq!(
        err.to_string(),
        format!(
            "Binding {id} is to 'openai', which is not a remote server of this node; it has no \
             reach to introspect."
        )
    );
    assert!(v.grounding.calls().is_empty(), "a grounding was made");
}

/// B4: a rotated token is grounded before its secret is written, and the
/// binding records the new reach; a refused rotation leaves the secret as
/// it was.
#[tokio::test]
async fn a_rotated_token_is_grounded_before_its_secret_is_written() {
    let v = vault(Ok(payload(&[("inst-play2-id", "play2")])));
    let id = v.store(SERVER, TOKEN).await.expect("stored");

    v.grounding
        .answer(Ok(payload(&[("inst-100m-id", "100monkeys-ai")])));
    v.service
        .rotate_credential(&v.owner(), &id, SensitiveString::new(ROTATED))
        .await
        .expect("rotated");
    let calls = v.grounding.calls();
    println!("grounding calls {}", calls.len());
    assert_eq!(calls.len(), 2, "the rotation made no second grounding");
    assert_eq!(calls[1].token, ROTATED);
    let reach = v.binding(id).await.metadata.reach.expect("reach");
    assert_eq!(reach.instance_slug.as_deref(), Some("100monkeys-ai"));

    let writes = v.writes();
    v.grounding.answer(Err(GroundingRefusal::Unreachable {
        code: "UPSTREAM_UNAVAILABLE".to_string(),
        detail: "connect failed".to_string(),
    }));
    let err = v
        .service
        .rotate_credential(&v.owner(), &id, SensitiveString::new("Mk11-third"))
        .await
        .expect_err("refused");
    println!("refusal {err}");
    assert_eq!(
        err.to_string(),
        "The token was not stored: the remote server 'nuclear-notes' could not be reached to \
         ground it."
    );
    assert_eq!(v.writes(), writes, "a refused rotation wrote the secret");
    let binding = v.binding(id).await;
    let stored = v
        .secrets
        .read_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            &AccessContext::system("test"),
        )
        .await
        .unwrap();
    assert_eq!(stored.get("value").map(|s| s.expose()), Some(ROTATED));
}
