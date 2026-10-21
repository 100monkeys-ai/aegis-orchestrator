// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The vault API's second orchestrator half (AEGIS ADR-150 V5, V7, V8): a
//! tool call inside a run reads its binding's value for the run's person and
//! the execution, and writes one `vault.tool_value_used` row naming the
//! execution and carrying no value; a read outside a run writes none; and
//! the secret store lists the paths under a prefix, one level, paths only.
//! `GET /v1/secrets?prefix=` over it is driven through the daemon's
//! authentication stack by the handler tests in
//! `cli/src/daemon/handlers/credentials.rs`.

use aegis_orchestrator_core::application::credential_service::{
    CredentialManagementService, OAuthProviderRegistry, StandardCredentialManagementService,
    StoreApiKeyCommand, ToolCallActor, ToolCredentialSource,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, CredentialType, GrantTarget, OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::events::SecretEvent;
use aegis_orchestrator_core::domain::execution::{ContextChoice, ExecutionId};
use aegis_orchestrator_core::domain::secrets::{
    kv_list_level, AccessContext, SecretStore, SensitiveString,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::vault_audit::{VaultAction, VaultAuditRow};
use aegis_orchestrator_core::infrastructure::event_bus::{DomainEvent, EventBus, EventReceiver};
use aegis_orchestrator_core::infrastructure::repositories::postgres_vault_audit::InMemoryVaultAuditSink;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const OWNER: &str = "d7f81700-35d3-49b6-b237-c391ccc19035";
const OTHER: &str = "0b6c2f4e-1a2b-4c3d-8e9f-0a1b2c3d4e5f";
/// The fixture key's value. Its appearance in any row is the leak.
const VALUE: &str = "sk-tool-use-fixture-9Lp3";
const EXECUTION: uuid::Uuid = uuid::Uuid::from_u128(0x0e8e_c000_0000_4000_8000_000000000150);

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

struct Harness {
    bindings: Arc<Bindings>,
    secrets: Arc<SecretsManager>,
    events: Arc<EventBus>,
    audit: Arc<InMemoryVaultAuditSink>,
    service: StandardCredentialManagementService,
}

fn harness(audit: InMemoryVaultAuditSink) -> Harness {
    let bindings = Arc::new(Bindings::default());
    let events = Arc::new(EventBus::new(64));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::new()),
        events.clone(),
    ));
    let audit = Arc::new(audit);
    let service = StandardCredentialManagementService::new(
        bindings.clone(),
        secrets.clone(),
        events.clone(),
        Arc::new(OAuthProviderRegistry::new()),
    )
    .with_vault_audit(audit.clone());
    Harness {
        bindings,
        secrets,
        events,
        audit,
        service,
    }
}

fn tenant(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).unwrap()
}

/// `sub`'s call in `tenant`, its run having chosen `context` for the server.
fn actor<'a>(
    tenant: &'a TenantId,
    sub: &'a str,
    agent_id: AgentId,
    context: ContextChoice,
) -> ToolCallActor<'a> {
    ToolCallActor {
        tenant_id: tenant,
        user_id: sub,
        agent_id,
        workflow_id: None,
        context,
    }
}

impl Harness {
    async fn store(&self, ty: CredentialType) -> CredentialBindingId {
        self.service
            .store_api_key(StoreApiKeyCommand {
                owner_user_id: OWNER.into(),
                tenant_id: tenant(OWNER),
                provider: CredentialProvider::new("openai"),
                label: "the owner's key".into(),
                scope: CredentialScope::Personal,
                api_key_value: SensitiveString::new(VALUE),
                credential_type: ty,
            })
            .await
            .unwrap()
    }

    async fn binding(&self, id: &CredentialBindingId) -> UserCredentialBinding {
        self.bindings.find_by_id(id).await.unwrap().unwrap()
    }

    async fn rows(&self) -> Vec<VaultAuditRow> {
        self.audit.rows().await
    }
}

/// The access context of every `SecretRetrieved` event `receiver` holds for
/// the path `path`.
fn reads_of(receiver: &mut EventReceiver, path: &str) -> Vec<AccessContext> {
    let mut contexts = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let DomainEvent::Secrets(SecretEvent::SecretRetrieved {
            path: read,
            access_context,
            ..
        }) = event
        {
            if read == path {
                contexts.push(access_context);
            }
        }
    }
    contexts
}

/// V5, V7: a tool call inside a run reads the chosen binding's value for the
/// run's person and the execution, not under the platform's own context, and
/// writes exactly one `vault.tool_value_used` row: actor the run's person,
/// target the binding, `after_state` naming the caller `execution`, the
/// execution's id, the tenant, the provider, the type and the outcome; no
/// row and no part of it carries the value.
#[tokio::test]
async fn a_runs_tool_call_writes_one_tool_value_used_row_naming_the_execution() {
    let h = harness(InMemoryVaultAuditSink::new());
    let id = h.store(CredentialType::Secret).await;
    let binding = h.binding(&id).await;
    let owner_tenant = tenant(OWNER);
    let agent = AgentId::new();
    let mut events = h.events.subscribe();

    let value = h
        .service
        .run_tool_server_credential(
            &actor(&owner_tenant, OWNER, agent, ContextChoice::Binding(id)),
            "openai",
            EXECUTION,
        )
        .await
        .unwrap()
        .expect("the run's chosen binding answers its value");
    assert_eq!(value.expose(), VALUE);

    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let row = &rows[0];
    assert_eq!(row.action, VaultAction::ToolValueUsed);
    assert_eq!(row.action.as_str(), "vault.tool_value_used");
    assert_eq!(row.actor_id, OWNER);
    assert_eq!(row.target_resource, format!("credential:{id}"));
    assert_eq!(
        row.after_state,
        json!({
            "caller": "execution",
            "tenant_id": owner_tenant.as_str(),
            "provider": "openai",
            "credential_type": "secret",
            "outcome": "answered",
            "execution_id": EXECUTION.to_string(),
        })
    );
    assert!(
        !format!("{rows:?}").contains(VALUE),
        "a row carries the value"
    );

    let reads = reads_of(&mut events, &binding.secret_path.path);
    assert_eq!(reads.len(), 1, "{reads:#?}");
    assert_eq!(reads[0].execution_id, Some(ExecutionId(EXECUTION)));
    assert_eq!(reads[0].agent_id, Some(agent));
    assert_eq!(reads[0].orchestrator_id, OWNER);
}

/// An OAuth binding's access token used inside a run is read for the run and
/// recorded the same way, its type named `oauth2`'s serialised form.
#[tokio::test]
async fn a_runs_oauth_token_use_is_read_for_the_run_and_recorded() {
    let h = harness(InMemoryVaultAuditSink::new());
    let id = h.store(CredentialType::OAuth2).await;
    let binding = h.binding(&id).await;
    let mut token = HashMap::new();
    token.insert(
        "access_token".to_string(),
        SensitiveString::new("oauth-access-fixture-2Hq"),
    );
    h.secrets
        .write_secret(
            &binding.secret_path.effective_mount(),
            &binding.secret_path.path,
            token,
            &AccessContext::system("test"),
        )
        .await
        .unwrap();
    let owner_tenant = tenant(OWNER);
    let agent = AgentId::new();
    let mut events = h.events.subscribe();

    let value = h
        .service
        .run_tool_server_credential(
            &actor(&owner_tenant, OWNER, agent, ContextChoice::Binding(id)),
            "openai",
            EXECUTION,
        )
        .await
        .unwrap()
        .expect("the access token");
    assert_eq!(value.expose(), "oauth-access-fixture-2Hq");

    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0].action, VaultAction::ToolValueUsed);
    assert_eq!(
        rows[0].after_state["credential_type"],
        serde_json::to_value(CredentialType::OAuth2).unwrap()
    );
    assert_eq!(
        rows[0].after_state["execution_id"],
        json!(EXECUTION.to_string())
    );
    assert!(!format!("{rows:?}").contains("oauth-access-fixture-2Hq"));

    let reads = reads_of(&mut events, &binding.secret_path.path);
    assert_eq!(reads.len(), 1, "{reads:#?}");
    assert_eq!(reads[0].execution_id, Some(ExecutionId(EXECUTION)));
    assert_eq!(reads[0].orchestrator_id, OWNER);
}

/// A call that belongs to no run reads under the platform's own context and
/// writes no row (V5 covers a run's use); a run that reaches no binding of
/// the server (none chosen, nothing named, another person's binding) is
/// answered none and writes no row.
#[tokio::test]
async fn no_row_outside_a_run_or_for_a_binding_the_run_does_not_reach() {
    let h = harness(InMemoryVaultAuditSink::new());
    let id = h.store(CredentialType::Secret).await;
    let binding = h.binding(&id).await;
    let owner_tenant = tenant(OWNER);
    let other_tenant = tenant(OTHER);
    let agent = AgentId::new();
    let mut events = h.events.subscribe();

    let outside = h
        .service
        .tool_server_credential(
            &actor(&owner_tenant, OWNER, agent, ContextChoice::Binding(id)),
            "openai",
        )
        .await
        .unwrap()
        .expect("the value, outside a run");
    assert_eq!(outside.expose(), VALUE);
    let reads = reads_of(&mut events, &binding.secret_path.path);
    assert_eq!(reads.len(), 1, "{reads:#?}");
    assert_eq!(reads[0].execution_id, None);
    assert_eq!(reads[0].orchestrator_id, "aegis-credential-service");

    for (name, tenant_id, sub, context) in [
        ("none chosen", &owner_tenant, OWNER, ContextChoice::None),
        (
            "nothing named",
            &owner_tenant,
            OWNER,
            ContextChoice::NotGiven,
        ),
        (
            "another person's binding",
            &other_tenant,
            OTHER,
            ContextChoice::Binding(id),
        ),
    ] {
        let got = h
            .service
            .run_tool_server_credential(&actor(tenant_id, sub, agent, context), "openai", EXECUTION)
            .await
            .unwrap();
        assert!(got.is_none(), "{name}: a value was answered");
    }
    assert!(h.rows().await.is_empty(), "{:#?}", h.rows().await);
}

/// V7: the row of an act other than the value read is written after the
/// act, and a row that cannot be written never undoes it: the run's call
/// still carries its credential.
#[tokio::test]
async fn a_row_that_cannot_be_written_never_undoes_the_runs_read() {
    let h = harness(InMemoryVaultAuditSink::failing());
    let id = h.store(CredentialType::Secret).await;
    let owner_tenant = tenant(OWNER);
    let value = h
        .service
        .run_tool_server_credential(
            &actor(
                &owner_tenant,
                OWNER,
                AgentId::new(),
                ContextChoice::Binding(id),
            ),
            "openai",
            EXECUTION,
        )
        .await
        .unwrap()
        .expect("the value, the row refused");
    assert_eq!(value.expose(), VALUE);
    assert!(h.rows().await.is_empty());
}

/// V8: a KV v2 `LIST` answers the paths one level under the prefix (a
/// trailing `/` added to a prefix without one), a deeper path as its first
/// folder ending in `/`, each once, sorted; the root's listing when the
/// prefix is empty; nothing for a prefix nothing is under.
#[test]
fn a_listing_is_one_level_under_the_prefix() {
    let paths = [
        "platform/stripe",
        "platform/hooks/github",
        "platform/hooks/gitlab",
        "platformer",
        "other",
    ];
    assert_eq!(
        kv_list_level(paths, "platform"),
        vec!["platform/hooks/", "platform/stripe"]
    );
    assert_eq!(
        kv_list_level(paths, "platform/"),
        vec!["platform/hooks/", "platform/stripe"]
    );
    assert_eq!(
        kv_list_level(paths, "platform/hooks"),
        vec!["platform/hooks/github", "platform/hooks/gitlab"]
    );
    assert_eq!(
        kv_list_level(paths, ""),
        vec!["other", "platform/", "platformer"]
    );
    assert!(kv_list_level(paths, "absent").is_empty());
}

/// V8: the in-memory store lists only its own engine's paths, and the
/// secrets manager answers the store's listing as it is: paths only, no
/// value and no field name.
#[tokio::test]
async fn the_store_lists_its_own_engines_paths_and_no_value() {
    let store = TestSecretStore::new();
    let secret = HashMap::from([("field".to_string(), SensitiveString::new(VALUE))]);
    for (engine, path) in [
        ("aegis-system/kv", "platform/stripe"),
        ("aegis-system/kv", "platform/hooks/github"),
        ("tenant-zaru-consumer/kv", "platform/elsewhere"),
    ] {
        store.write(engine, path, secret.clone()).await.unwrap();
    }
    assert_eq!(
        store.list("aegis-system/kv", "platform").await.unwrap(),
        vec!["platform/hooks/", "platform/stripe"]
    );
    assert_eq!(
        store.list("aegis-system/kv", "").await.unwrap(),
        vec!["platform/"]
    );

    let manager = SecretsManager::from_store(Arc::new(store), Arc::new(EventBus::new(16)));
    let listed = manager
        .list_secrets(
            "tenant-zaru-consumer/kv",
            "platform",
            &AccessContext::system("test"),
        )
        .await
        .unwrap();
    assert_eq!(listed, vec!["platform/elsewhere"]);
    assert!(!format!("{listed:?}").contains(VALUE));
    assert!(!format!("{listed:?}").contains("field"));
}
