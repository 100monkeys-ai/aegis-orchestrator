// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! The vault API's core (AEGIS ADR-150 V3, V7, V10): the audit row's action
//! names and shapes, the in-process sinks, and the credential service's
//! answer to an owner's value read under V3's conditions. The routes over
//! them are driven through the daemon's authentication stack by the handler
//! tests in `cli/src/daemon/handlers/credentials.rs` and
//! `cli/src/daemon/handlers/vault_client_secrets.rs`.

use aegis_orchestrator_core::application::credential_service::{
    CredentialManagementService, OAuthProviderRegistry, OwnerKeyValue,
    StandardCredentialManagementService, StoreApiKeyCommand,
};
use aegis_orchestrator_core::domain::credential::{
    BindingReach, CredentialBindingId, CredentialBindingRepository, CredentialGrant,
    CredentialProvider, CredentialScope, CredentialStatus, CredentialType, GrantTarget,
    OAuthPendingState, ReachKind, UserCredentialBinding,
};
use aegis_orchestrator_core::domain::secrets::{
    AccessContext, DomainDynamicSecret, SecretStore, SecretsError, SensitiveString,
};
use aegis_orchestrator_core::domain::tenant::TenantId;
use aegis_orchestrator_core::domain::vault_audit::{
    append_after_act, BindingAuditState, ClientSecretAuditState, VaultAction, VaultAuditRow,
    VaultAuditSink, VaultCaller,
};
use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
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
const VALUE: &str = "sk-vault-core-fixture-4Rw";

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

/// The in-memory store, with a delete that deletes (the test store's
/// `delete` is the trait's no-op, where OpenBao deletes).
#[derive(Default)]
struct DeletingStore {
    inner: TestSecretStore,
    deleted: tokio::sync::RwLock<std::collections::HashSet<String>>,
}

#[async_trait::async_trait]
impl SecretStore for DeletingStore {
    async fn read(
        &self,
        engine: &str,
        path: &str,
    ) -> Result<std::collections::HashMap<String, SensitiveString>, SecretsError> {
        if self
            .deleted
            .read()
            .await
            .contains(&format!("{engine}/{path}"))
        {
            return Err(SecretsError::SecretNotFound {
                path: format!("{engine}/{path}"),
            });
        }
        self.inner.read(engine, path).await
    }
    async fn write(
        &self,
        engine: &str,
        path: &str,
        secret: std::collections::HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError> {
        self.deleted
            .write()
            .await
            .remove(&format!("{engine}/{path}"));
        self.inner.write(engine, path, secret).await
    }
    async fn delete(&self, engine: &str, path: &str) -> Result<(), SecretsError> {
        self.deleted
            .write()
            .await
            .insert(format!("{engine}/{path}"));
        Ok(())
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
        increment: std::time::Duration,
    ) -> Result<std::time::Duration, SecretsError> {
        self.inner.renew_lease(lease_id, increment).await
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
        signature: &str,
    ) -> Result<bool, SecretsError> {
        self.inner.transit_verify(key, data, signature).await
    }
    async fn transit_encrypt(&self, key: &str, plaintext: &[u8]) -> Result<String, SecretsError> {
        self.inner.transit_encrypt(key, plaintext).await
    }
    async fn transit_decrypt(&self, key: &str, ciphertext: &str) -> Result<Vec<u8>, SecretsError> {
        self.inner.transit_decrypt(key, ciphertext).await
    }
}

struct Harness {
    bindings: Arc<Bindings>,
    secrets: Arc<SecretsManager>,
    service: StandardCredentialManagementService,
}

fn harness() -> Harness {
    let bindings = Arc::new(Bindings::default());
    let event_bus = Arc::new(EventBus::new(16));
    let secrets = Arc::new(SecretsManager::from_store(
        Arc::new(DeletingStore::default()),
        event_bus.clone(),
    ));
    let service = StandardCredentialManagementService::new(
        bindings.clone(),
        secrets.clone(),
        event_bus,
        Arc::new(OAuthProviderRegistry::new()),
    );
    Harness {
        bindings,
        secrets,
        service,
    }
}

fn tenant(sub: &str) -> TenantId {
    TenantId::for_consumer_user(sub).unwrap()
}

impl Harness {
    async fn store(&self, ty: CredentialType, scope: CredentialScope) -> CredentialBindingId {
        self.service
            .store_api_key(StoreApiKeyCommand {
                owner_user_id: OWNER.into(),
                tenant_id: tenant(OWNER),
                provider: CredentialProvider::new("openai"),
                label: "a key".into(),
                scope,
                api_key_value: SensitiveString::new(VALUE),
                credential_type: ty,
            })
            .await
            .unwrap()
    }

    async fn edit(
        &self,
        id: &CredentialBindingId,
        change: impl FnOnce(&mut UserCredentialBinding),
    ) {
        let mut b = self.bindings.find_by_id(id).await.unwrap().unwrap();
        change(&mut b);
        self.bindings.save(&b).await.unwrap();
    }

    async fn answer(&self, sub: &str, id: &CredentialBindingId) -> &'static str {
        match self.service.owner_key_value(sub, &tenant(sub), id).await {
            Ok(OwnerKeyValue::Value { value, .. }) => {
                assert_eq!(value.expose(), VALUE);
                "value"
            }
            Ok(OwnerKeyValue::NotReadable(_)) => "not readable",
            Ok(OwnerKeyValue::NotFound) => "not found",
            Err(_) => "error",
        }
    }
}

#[test]
fn the_audit_actions_are_v7s_ten_and_v10s_three() {
    let names: Vec<_> = VaultAction::ALL.iter().map(|a| a.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "vault.value_read",
            "vault.value_refused",
            "vault.binding_stored",
            "vault.binding_rotated",
            "vault.binding_revoked",
            "vault.grant_added",
            "vault.grant_revoked",
            "vault.secret_written",
            "vault.secret_deleted",
            "vault.tool_value_used",
            "vault.client_secret_written",
            "vault.client_secret_read",
            "vault.client_secret_deleted",
        ]
    );
}

#[test]
fn the_rows_carry_v7s_and_v10s_fields_and_their_targets() {
    let state = BindingAuditState {
        caller: VaultCaller::Session,
        tenant_id: Some("u-x".into()),
        provider: Some("openai".into()),
        credential_type: Some("secret".into()),
        purpose: Some("chat-turn".into()),
        outcome: "answered".into(),
        execution_id: None,
    };
    let row = VaultAuditRow::binding(OWNER, VaultAction::ValueRead, "b-1", &state);
    assert_eq!(row.target_resource, "credential:b-1");
    assert_eq!(
        row.after_state,
        json!({
            "caller": "session",
            "tenant_id": "u-x",
            "provider": "openai",
            "credential_type": "secret",
            "purpose": "chat-turn",
            "outcome": "answered",
        })
    );
    let row = VaultAuditRow::secret_path("op", VaultAction::SecretWritten, "kv", "a/b", &state);
    assert_eq!(row.target_resource, "secret:kv/a/b");
    let client = ClientSecretAuditState {
        client_id: "zaru-web".into(),
        kind: "session".into(),
        owner_sub: Some(OWNER.into()),
        purpose: None,
        outcome: "answered".into(),
    };
    let row = VaultAuditRow::client_secret("sa", VaultAction::ClientSecretWritten, "id-1", &client);
    assert_eq!(row.target_resource, "client-secret:zaru-web/session/id-1");
    assert_eq!(
        row.after_state,
        json!({"client_id": "zaru-web", "kind": "session", "owner_sub": OWNER, "outcome": "answered"})
    );
}

#[tokio::test]
async fn the_in_memory_sink_keeps_rows_and_the_failing_one_refuses_them() {
    let row = VaultAuditRow {
        actor_id: OWNER.into(),
        action: VaultAction::BindingStored,
        target_resource: "credential:x".into(),
        after_state: json!({}),
    };
    let sink = InMemoryVaultAuditSink::new();
    sink.append(&row).await.unwrap();
    append_after_act(&sink, row.clone()).await;
    assert_eq!(sink.rows().await, vec![row.clone(), row.clone()]);

    let failing = InMemoryVaultAuditSink::failing();
    assert!(failing.append(&row).await.is_err());
    // An act already carried out stands when its row is not written.
    append_after_act(&failing, row).await;
    assert!(failing.rows().await.is_empty());
}

/// V3: the value is answered only for the owner's own personal, active
/// `secret` binding with no reach; another person and an unknown id are
/// answered as no binding; every other binding of the owner is not
/// readable; a store that cannot be read is an error.
#[tokio::test]
async fn the_owner_is_answered_only_under_v3s_conditions() {
    let h = harness();
    let secret = h
        .store(CredentialType::Secret, CredentialScope::Personal)
        .await;
    assert_eq!(h.answer(OWNER, &secret).await, "value");
    assert_eq!(h.answer(OTHER, &secret).await, "not found");
    assert_eq!(
        h.answer(OWNER, &CredentialBindingId::new()).await,
        "not found"
    );

    let mut failures = Vec::new();
    for ty in [
        CredentialType::OAuth2,
        CredentialType::Mailbox,
        CredentialType::Calendar,
        CredentialType::Variable,
        CredentialType::ServiceAccount,
    ] {
        let id = h.store(ty.clone(), CredentialScope::Personal).await;
        let got = h.answer(OWNER, &id).await;
        if got != "not readable" {
            failures.push(format!("{ty:?}: {got}"));
        }
    }
    let team = h
        .store(
            CredentialType::Secret,
            CredentialScope::Team {
                team_id: uuid::Uuid::new_v4(),
            },
        )
        .await;
    let reach = h
        .store(CredentialType::Secret, CredentialScope::Personal)
        .await;
    h.edit(&reach, |b| {
        b.metadata.reach = Some(BindingReach {
            kind: ReachKind::Apex,
            instance_slug: None,
            instance_id: None,
            workspace_id: None,
            login: None,
            grounded_at: Utc::now(),
        })
    })
    .await;
    let expired = h
        .store(CredentialType::Secret, CredentialScope::Personal)
        .await;
    h.edit(&expired, |b| b.status = CredentialStatus::Expired)
        .await;
    for (name, id) in [("team", team), ("reach", reach), ("expired", expired)] {
        let got = h.answer(OWNER, &id).await;
        if got != "not readable" {
            failures.push(format!("{name}: {got}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");

    // The binding's secret gone from the store: an error, never a value.
    let gone = h
        .store(CredentialType::Secret, CredentialScope::Personal)
        .await;
    let b = h.bindings.find_by_id(&gone).await.unwrap().unwrap();
    h.secrets
        .delete_secret(
            &b.secret_path.effective_mount(),
            &b.secret_path.path,
            &AccessContext::system("test"),
        )
        .await
        .unwrap();
    assert_eq!(h.answer(OWNER, &gone).await, "error");
}
