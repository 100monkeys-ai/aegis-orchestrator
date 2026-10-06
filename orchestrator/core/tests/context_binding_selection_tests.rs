// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! An execution's dispatch chooses the credential of a remote tool server's
//! calls (Zaru ADR-0055 D2, D9, D15): a chosen binding is the credential, no
//! grant needed, only when it is the acting person's own active binding for
//! that server in their tenant; a choice of none gives no credential
//! whatever is granted; with no choice the per-agent grant path (AEGIS
//! ADR-132 S6) stands. The real `StandardCredentialManagementService` over
//! in-memory bindings and secrets.

use aegis_orchestrator_core::application::credential_service::{
    CredentialManagementService, OAuthProviderRegistry, StandardCredentialManagementService,
    StoreApiKeyCommand, ToolCallActor, ToolCredentialSource,
};
use aegis_orchestrator_core::domain::agent::AgentId;
use aegis_orchestrator_core::domain::credential::{
    CredentialBindingId, CredentialBindingRepository, CredentialGrant, CredentialProvider,
    CredentialScope, CredentialType, GrantTarget, OAuthPendingState, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::execution::ContextChoice;
use aegis_orchestrator_core::domain::secrets::SensitiveString;
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
use aegis_orchestrator_core::infrastructure::secrets_manager::{SecretsManager, TestSecretStore};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const SERVER: &str = "nuclear-notes";
const ALICE: &str = "context-alice-sub";
const BOB: &str = "context-bob-sub";

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

struct Vault {
    service: StandardCredentialManagementService,
    bindings: Arc<Bindings>,
}

fn vault() -> Vault {
    let bindings = Arc::new(Bindings::default());
    let event_bus = Arc::new(EventBus::new(64));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(TestSecretStore::default()),
        event_bus.clone(),
    ));
    let service = StandardCredentialManagementService::new(
        bindings.clone(),
        secrets,
        event_bus,
        Arc::new(OAuthProviderRegistry::new()),
    );
    Vault { service, bindings }
}

fn tenant_of(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).expect("a person's tenant")
}

impl Vault {
    /// Store `value` as `owner`'s binding for `provider` in `tenant`.
    async fn store(
        &self,
        owner: &str,
        tenant: &TenantId,
        provider: &str,
        value: &str,
    ) -> CredentialBindingId {
        let id = self
            .service
            .store_api_key(StoreApiKeyCommand {
                owner_user_id: owner.to_string(),
                tenant_id: tenant.clone(),
                provider: CredentialProvider::new(provider),
                label: value.to_string(),
                scope: CredentialScope::Personal,
                api_key_value: SensitiveString::new(value),
                credential_type: CredentialType::Secret,
            })
            .await
            .unwrap_or_else(|e| panic!("store {value}: {e:#}"));
        // Bindings stored in one test are told apart by their creation time.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        id
    }

    async fn grant_all_agents(&self, id: CredentialBindingId, owner: &str) {
        let mut binding = self.bindings.find_by_id(&id).await.unwrap().unwrap();
        binding.add_grant(GrantTarget::AllAgents, owner.to_string());
        self.bindings.save(&binding).await.unwrap();
    }

    async fn revoke(&self, id: CredentialBindingId) {
        let mut binding = self.bindings.find_by_id(&id).await.unwrap().unwrap();
        binding.revoke();
        self.bindings.save(&binding).await.unwrap();
    }

    /// The credential Alice's call of `SERVER` carries under `context`.
    async fn alices_credential(&self, context: ContextChoice) -> Option<String> {
        let tenant = tenant_of(ALICE);
        let actor = ToolCallActor {
            tenant_id: &tenant,
            user_id: ALICE,
            agent_id: AgentId::new(),
            workflow_id: None,
            context,
        };
        self.service
            .tool_server_credential(&actor, SERVER)
            .await
            .expect("no error")
            .map(|secret| secret.expose().to_string())
    }
}

/// D15: an id selects that binding though it is neither the newest nor
/// granted, where no choice keeps the newest granted one.
#[tokio::test]
async fn a_chosen_binding_is_the_credential_though_older_and_ungranted() {
    let v = vault();
    let alice = tenant_of(ALICE);
    let chosen = v
        .store(ALICE, &alice, SERVER, "chosen-older-ungranted")
        .await;
    let newest = v.store(ALICE, &alice, SERVER, "newest-granted").await;
    v.grant_all_agents(newest, ALICE).await;

    assert_eq!(
        v.alices_credential(ContextChoice::Binding(chosen))
            .await
            .as_deref(),
        Some("chosen-older-ungranted"),
        "the chosen binding was not the call's credential"
    );
    assert_eq!(
        v.alices_credential(ContextChoice::NotGiven)
            .await
            .as_deref(),
        Some("newest-granted"),
        "with no choice the newest granted binding is no longer the credential"
    );
}

/// D15: a chosen id that is not the acting person's own active binding for
/// the server, in their tenant, answers nothing.
#[tokio::test]
async fn a_chosen_binding_that_is_not_alices_active_one_for_the_server_answers_nothing() {
    let v = vault();
    let alice = tenant_of(ALICE);
    let bobs = v.store(BOB, &tenant_of(BOB), SERVER, "bobs-token").await;
    let other_tenant = v
        .store(ALICE, &tenant_of(BOB), SERVER, "alice-in-bobs-tenant")
        .await;
    let other_provider = v.store(ALICE, &alice, "openai", "alices-openai-key").await;
    let revoked = v.store(ALICE, &alice, SERVER, "alices-revoked-token").await;
    v.revoke(revoked).await;
    // Alice's own binding for the server, granted to all her agents: a
    // refused choice must not fall back to it.
    let granted = v.store(ALICE, &alice, SERVER, "alices-granted-token").await;
    v.grant_all_agents(granted, ALICE).await;
    // Every one granted to all agents: the grant must not rescue a choice.
    for (id, owner) in [(bobs, BOB), (other_tenant, ALICE), (other_provider, ALICE)] {
        v.grant_all_agents(id, owner).await;
    }
    let unknown = CredentialBindingId::new();

    let mut carried = Vec::new();
    for (case, id) in [
        ("another owner", bobs),
        ("another tenant", other_tenant),
        ("another provider", other_provider),
        ("a revoked binding", revoked),
        ("an unknown id", unknown),
    ] {
        if let Some(secret) = v.alices_credential(ContextChoice::Binding(id)).await {
            carried.push(format!("{case} carried {secret}"));
        }
    }
    assert!(carried.is_empty(), "a chosen binding that is not Alice's own active one for the server was the credential: {carried:?}");
}

/// D2, D15: `null` chose no credential, even under an all-agents grant.
#[tokio::test]
async fn a_choice_of_none_answers_nothing_under_an_all_agents_grant() {
    let v = vault();
    let alice = tenant_of(ALICE);
    let granted = v
        .store(ALICE, &alice, SERVER, "granted-to-all-agents")
        .await;
    v.grant_all_agents(granted, ALICE).await;

    assert_eq!(
        v.alices_credential(ContextChoice::None).await,
        None,
        "a choice of none carried the all-agents grant's credential"
    );
    assert_eq!(
        v.alices_credential(ContextChoice::NotGiven)
            .await
            .as_deref(),
        Some("granted-to-all-agents"),
        "the grant path no longer answers with no choice"
    );
}
