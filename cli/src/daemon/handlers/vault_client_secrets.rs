// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Client secrets (AEGIS ADR-150 V10)
//!
//! A declared confidential client keeps the tokens of its own sessions in
//! the vault instead of its own database:
//!
//! | Route | Answer |
//! |-------|--------|
//! | `PUT /v1/vault/client-secrets/{kind}/{id}` | writes `{owner_sub, value}` |
//! | `POST /v1/vault/client-secrets/{kind}/{id}/value` | answers `{owner_sub, value}`, `Cache-Control: no-store` |
//! | `DELETE /v1/vault/client-secrets/{kind}/{id}` | deletes |
//!
//! `{kind}` is `session` or `revocation`; `{id}` is the client's row id, a
//! UUID. Only an identity of kind service account whose `client_id` the node
//! configuration's `spec.vault.client_secret_clients` lists may call them;
//! every other caller is refused 403 `vault_client_not_declared`. A client
//! reaches only paths under its own client id: the value is kept in the
//! consumer realm's mount `tenant-zaru-consumer/kv` at
//! `clients/<client_id>/<kind>/<id>`, so a path another client wrote answers
//! 404, as a path that does not exist.
//!
//! Every act writes one audit row (V7, V10) with no value; a read writes its
//! row before it answers and is refused 503 `vault_unavailable` when the row
//! cannot be written. A store that cannot be reached answers 503
//! `vault_unavailable`.

use std::collections::HashMap;
use std::sync::Arc;

use aegis_orchestrator_core::domain::iam::{IdentityKind, UserIdentity};
use aegis_orchestrator_core::domain::node_config::VaultConfig;
use aegis_orchestrator_core::domain::secrets::{AccessContext, SecretsError, SensitiveString};
use aegis_orchestrator_core::domain::vault_audit::{
    append_after_act, ClientSecretAuditState, VaultAction, VaultAuditRow, VaultAuditSink,
    OUTCOME_ANSWERED,
};
use aegis_orchestrator_core::infrastructure::secrets_manager::SecretsManager;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{post, put},
    Json, Router,
};
use serde_json::json;

use crate::daemon::handlers::credentials::{
    read_purpose, vault_refusal, VAULT_PURPOSE_REQUIRED, VAULT_UNAVAILABLE,
};

/// The mount every client secret is kept in: the consumer realm's.
pub(crate) const CLIENT_SECRETS_ENGINE: &str = "tenant-zaru-consumer/kv";

/// The kinds of secret a client keeps.
const KINDS: [&str; 2] = ["session", "revocation"];

const VAULT_CLIENT_NOT_DECLARED: &str = "vault_client_not_declared";
const VAULT_CLIENT_NOT_DECLARED_SENTENCE: &str = "This client may not keep secrets in the vault.";

/// The fields a client secret is kept under.
const OWNER_FIELD: &str = "owner_sub";
const VALUE_FIELD: &str = "value";

/// The state of the client-secret routes.
#[derive(Clone)]
pub(crate) struct VaultClientSecretsState {
    pub(crate) secrets_manager: Arc<SecretsManager>,
    /// `spec.vault`: which clients are declared.
    pub(crate) vault: VaultConfig,
    /// Where the vault's audit rows go.
    pub(crate) audit: Arc<dyn VaultAuditSink>,
}

/// The client-secret routes, over their own narrow state. Merged into the
/// daemon router by `router::create_router`, beneath the same
/// authentication layers as every other route.
pub(crate) fn vault_client_secrets_router(state: VaultClientSecretsState) -> Router {
    Router::new()
        .route(
            "/v1/vault/client-secrets/{kind}/{id}",
            put(write_client_secret_handler).delete(delete_client_secret_handler),
        )
        .route(
            "/v1/vault/client-secrets/{kind}/{id}/value",
            post(read_client_secret_handler),
        )
        .with_state(state)
}

/// The caller, when it is a declared client's service account: its `sub`
/// and its client id. Anyone else is refused.
#[allow(clippy::result_large_err)]
fn declared_client(
    extensions: &axum::http::Extensions,
    vault: &VaultConfig,
) -> Result<(String, String), Response> {
    let identity = extensions.get::<UserIdentity>().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Authentication required"})),
        )
            .into_response()
    })?;
    match &identity.identity_kind {
        IdentityKind::ServiceAccount { client_id } if vault.declares_client(client_id) => {
            Ok((identity.sub.clone(), client_id.clone()))
        }
        _ => Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": VAULT_CLIENT_NOT_DECLARED,
                "message": VAULT_CLIENT_NOT_DECLARED_SENTENCE,
            })),
        )
            .into_response()),
    }
}

/// The path of a client secret, from the route's `kind` and `id`; `None`
/// when the kind is not one a client keeps or the id is not a UUID.
fn secret_path(client_id: &str, kind: &str, id: &str) -> Option<String> {
    if !KINDS.contains(&kind) {
        return None;
    }
    let id = uuid::Uuid::parse_str(id).ok()?;
    Some(format!("clients/{client_id}/{kind}/{id}"))
}

fn invalid_path() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "Invalid client secret path: the kind is session or revocation, the id a UUID"
        })),
    )
        .into_response()
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "Client secret not found"})),
    )
        .into_response()
}

/// The client secret an act is about.
struct Target<'a> {
    client_id: &'a str,
    kind: &'a str,
    id: &'a str,
}

fn row(
    actor: &str,
    action: VaultAction,
    target: &Target<'_>,
    owner_sub: Option<String>,
    purpose: Option<String>,
    outcome: &str,
) -> VaultAuditRow {
    VaultAuditRow::client_secret(
        actor,
        action,
        target.id,
        &ClientSecretAuditState {
            client_id: target.client_id.to_string(),
            kind: target.kind.to_string(),
            owner_sub,
            purpose,
            outcome: outcome.to_string(),
        },
    )
}

/// The secret at `path`, `Ok(None)` when there is none.
async fn read_stored(
    sm: &SecretsManager,
    path: &str,
    sub: &str,
) -> Result<Option<HashMap<String, SensitiveString>>, SecretsError> {
    match sm
        .read_secret(CLIENT_SECRETS_ENGINE, path, &AccessContext::system(sub))
        .await
    {
        Ok(stored) => Ok(Some(stored)),
        Err(SecretsError::SecretNotFound { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `PUT /v1/vault/client-secrets/{kind}/{id}` — write the token set
/// `value` (a JSON object) the client keeps for the person `owner_sub`.
pub(crate) async fn write_client_secret_handler(
    State(state): State<VaultClientSecretsState>,
    Path((kind, id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (sub, client_id) = match declared_client(request.extensions(), &state.vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(path) = secret_path(&client_id, &kind, &id) else {
        return invalid_path();
    };
    let body = axum::body::to_bytes(request.into_body(), 1024 * 64)
        .await
        .unwrap_or_default();
    // serde's message can quote the offending value, which may be a token,
    // so the refusal names only the shape.
    let parsed: Option<(String, String)> = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            let owner = v.get(OWNER_FIELD)?.as_str()?.to_string();
            let value = v.get(VALUE_FIELD)?;
            (!owner.is_empty() && value.is_object()).then(|| (owner, value.to_string()))
        });
    let Some((owner_sub, value)) = parsed else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Invalid request body: owner_sub is a non-empty string and value a JSON object"
            })),
        )
            .into_response();
    };

    let mut secret = HashMap::new();
    secret.insert(OWNER_FIELD.to_string(), SensitiveString::new(&owner_sub));
    secret.insert(VALUE_FIELD.to_string(), SensitiveString::new(value));
    if let Err(e) = state
        .secrets_manager
        .write_secret(
            CLIENT_SECRETS_ENGINE,
            &path,
            secret,
            &AccessContext::system(&sub),
        )
        .await
    {
        tracing::error!(error = %e, kind = %kind, "a client secret could not be written");
        return vault_refusal(StatusCode::SERVICE_UNAVAILABLE, VAULT_UNAVAILABLE);
    }
    append_after_act(
        state.audit.as_ref(),
        row(
            &sub,
            VaultAction::ClientSecretWritten,
            &Target {
                client_id: &client_id,
                kind: &kind,
                id: &id,
            },
            Some(owner_sub),
            None,
            OUTCOME_ANSWERED,
        ),
    )
    .await;
    (
        StatusCode::OK,
        Json(json!({"status": "written", "kind": kind, "id": id})),
    )
        .into_response()
}

/// `POST /v1/vault/client-secrets/{kind}/{id}/value` — answer the token set
/// the client keeps at its own path, for a named purpose.
pub(crate) async fn read_client_secret_handler(
    State(state): State<VaultClientSecretsState>,
    Path((kind, id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (sub, client_id) = match declared_client(request.extensions(), &state.vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(path) = secret_path(&client_id, &kind, &id) else {
        return invalid_path();
    };
    let body = axum::body::to_bytes(request.into_body(), 1024 * 64)
        .await
        .unwrap_or_default();
    let refused = |owner: Option<String>, purpose: Option<String>, outcome: &str| {
        row(
            &sub,
            VaultAction::ClientSecretRead,
            &Target {
                client_id: &client_id,
                kind: &kind,
                id: &id,
            },
            owner,
            purpose,
            outcome,
        )
    };
    let Some(purpose) = read_purpose(&body) else {
        append_after_act(
            state.audit.as_ref(),
            refused(None, None, VAULT_PURPOSE_REQUIRED),
        )
        .await;
        return vault_refusal(StatusCode::BAD_REQUEST, VAULT_PURPOSE_REQUIRED);
    };

    let stored = match read_stored(&state.secrets_manager, &path, &sub).await {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            append_after_act(
                state.audit.as_ref(),
                refused(None, Some(purpose), "not_found"),
            )
            .await;
            return not_found();
        }
        Err(e) => {
            tracing::error!(error = %e, kind = %kind, "a client secret could not be read");
            append_after_act(
                state.audit.as_ref(),
                refused(None, Some(purpose), VAULT_UNAVAILABLE),
            )
            .await;
            return vault_refusal(StatusCode::SERVICE_UNAVAILABLE, VAULT_UNAVAILABLE);
        }
    };
    let owner_sub = stored.get(OWNER_FIELD).map(|o| o.expose().to_string());
    let value = stored
        .get(VALUE_FIELD)
        .and_then(|v| serde_json::from_str::<serde_json::Value>(v.expose()).ok());
    let Some(value) = value else {
        tracing::error!(kind = %kind, "a client secret holds no readable value field");
        append_after_act(
            state.audit.as_ref(),
            refused(owner_sub, Some(purpose), VAULT_UNAVAILABLE),
        )
        .await;
        return vault_refusal(StatusCode::SERVICE_UNAVAILABLE, VAULT_UNAVAILABLE);
    };

    // A value is never answered without its row.
    let answered = refused(owner_sub.clone(), Some(purpose), OUTCOME_ANSWERED);
    if state.audit.append(&answered).await.is_err() {
        tracing::error!(
            action = %VaultAction::ClientSecretRead,
            target = %answered.target_resource,
            "the vault audit row was not written; the client secret read is refused"
        );
        return vault_refusal(StatusCode::SERVICE_UNAVAILABLE, VAULT_UNAVAILABLE);
    }
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(json!({"owner_sub": owner_sub, "value": value})),
    )
        .into_response()
}

/// `DELETE /v1/vault/client-secrets/{kind}/{id}` — delete the secret the
/// client keeps at its own path.
pub(crate) async fn delete_client_secret_handler(
    State(state): State<VaultClientSecretsState>,
    Path((kind, id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let (sub, client_id) = match declared_client(request.extensions(), &state.vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(path) = secret_path(&client_id, &kind, &id) else {
        return invalid_path();
    };
    let owner_sub = match read_stored(&state.secrets_manager, &path, &sub).await {
        Ok(Some(stored)) => stored.get(OWNER_FIELD).map(|o| o.expose().to_string()),
        Ok(None) => return not_found(),
        Err(e) => {
            tracing::error!(error = %e, kind = %kind, "a client secret could not be read");
            return vault_refusal(StatusCode::SERVICE_UNAVAILABLE, VAULT_UNAVAILABLE);
        }
    };
    if let Err(e) = state
        .secrets_manager
        .delete_secret(CLIENT_SECRETS_ENGINE, &path, &AccessContext::system(&sub))
        .await
    {
        tracing::error!(error = %e, kind = %kind, "a client secret could not be deleted");
        return vault_refusal(StatusCode::SERVICE_UNAVAILABLE, VAULT_UNAVAILABLE);
    }
    append_after_act(
        state.audit.as_ref(),
        row(
            &sub,
            VaultAction::ClientSecretDeleted,
            &Target {
                client_id: &client_id,
                kind: &kind,
                id: &id,
            },
            owner_sub,
            None,
            OUTCOME_ANSWERED,
        ),
    )
    .await;
    (
        StatusCode::OK,
        Json(json!({"status": "deleted", "kind": kind, "id": id})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    //! The client-secret routes (AEGIS ADR-150 V10), driven through the
    //! daemon's real authentication stack over an in-memory secret store and
    //! audit sink.

    use super::{vault_client_secrets_router, VaultClientSecretsState};
    use crate::daemon::handlers::test_support::{
        consumer, identity_provider, operator, send, send_with_headers, serve, service_account,
    };
    use aegis_orchestrator_core::domain::iam::{AegisRole, IdentityKind, UserIdentity};
    use aegis_orchestrator_core::domain::node_config::VaultConfig;
    use aegis_orchestrator_core::domain::secrets::{
        DomainDynamicSecret, SecretStore, SecretsError, SensitiveString,
    };
    use aegis_orchestrator_core::domain::vault_audit::VaultAction;
    use aegis_orchestrator_core::infrastructure::event_bus::EventBus;
    use aegis_orchestrator_core::infrastructure::repositories::postgres_vault_audit::InMemoryVaultAuditSink;
    use aegis_orchestrator_core::infrastructure::secrets_manager::{
        SecretsManager, TestSecretStore,
    };
    use reqwest::Method;
    use serde_json::{json, Value};
    use std::sync::Arc;

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
        async fn transit_encrypt(
            &self,
            key: &str,
            plaintext: &[u8],
        ) -> Result<String, SecretsError> {
            self.inner.transit_encrypt(key, plaintext).await
        }
        async fn transit_decrypt(
            &self,
            key: &str,
            ciphertext: &str,
        ) -> Result<Vec<u8>, SecretsError> {
            self.inner.transit_decrypt(key, ciphertext).await
        }
    }

    const SESSION: &str = "0b6c2f4e-1a2b-4c3d-8e9f-0a1b2c3d4e5f";
    const REVOCATION: &str = "7d1e9a20-55c4-4b8e-9f00-1a2b3c4d5e6f";
    const TOKEN: &str = "kc-access-token-fixture-Zq9";

    fn client(client_id: &str) -> UserIdentity {
        UserIdentity {
            sub: format!("{client_id}-sa-sub"),
            realm_slug: "zaru-consumer".into(),
            email: None,
            email_verified: false,
            name: None,
            identity_kind: IdentityKind::ServiceAccount {
                client_id: client_id.into(),
            },
        }
    }

    struct Clients {
        base: String,
        audit: Arc<InMemoryVaultAuditSink>,
    }

    async fn clients(audit: InMemoryVaultAuditSink) -> Clients {
        let audit = Arc::new(audit);
        let router = vault_client_secrets_router(VaultClientSecretsState {
            secrets_manager: Arc::new(SecretsManager::from_store(
                Arc::new(DeletingStore::default()),
                Arc::new(EventBus::new(16)),
            )),
            vault: VaultConfig {
                client_secret_clients: vec!["zaru-web".into(), "other-web".into()],
            },
            audit: audit.clone(),
        });
        let base = serve(
            router,
            Some(identity_provider(&[
                ("zaru-web-token", client("zaru-web"), ""),
                ("other-web-token", client("other-web"), ""),
                ("undeclared-token", service_account(), ""),
                ("consumer-token", consumer("a-person-sub"), ""),
                ("admin-token", operator(AegisRole::Admin), ""),
            ])),
            None,
        )
        .await;
        Clients { base, audit }
    }

    fn token_set() -> Value {
        json!({"consumer": {"accessToken": TOKEN, "refreshToken": "kc-refresh-fixture"}})
    }

    impl Clients {
        async fn put(&self, kind: &str, id: &str, token: &str) -> (u16, Value) {
            send(
                &self.base,
                &Method::PUT,
                &format!("/v1/vault/client-secrets/{kind}/{id}"),
                &Some(json!({"owner_sub": "a-person-sub", "value": token_set()})),
                Some(token),
            )
            .await
        }
        async fn read(&self, kind: &str, id: &str, token: &str) -> (u16, Value) {
            send(
                &self.base,
                &Method::POST,
                &format!("/v1/vault/client-secrets/{kind}/{id}/value"),
                &Some(json!({"purpose": "session-read"})),
                Some(token),
            )
            .await
        }
        async fn delete(&self, kind: &str, id: &str, token: &str) -> (u16, Value) {
            send(
                &self.base,
                &Method::DELETE,
                &format!("/v1/vault/client-secrets/{kind}/{id}"),
                &None,
                Some(token),
            )
            .await
        }
    }

    #[tokio::test]
    async fn a_declared_client_writes_reads_and_deletes_its_own_session_and_revocation_secrets() {
        let c = clients(InMemoryVaultAuditSink::new()).await;
        for (kind, id) in [("session", SESSION), ("revocation", REVOCATION)] {
            let (status, body) = c.put(kind, id, "zaru-web-token").await;
            assert_eq!(
                (status, body),
                (200, json!({"status": "written", "kind": kind, "id": id}))
            );
            let (status, headers, body) = send_with_headers(
                &c.base,
                &Method::POST,
                &format!("/v1/vault/client-secrets/{kind}/{id}/value"),
                &Some(json!({"purpose": "session-read"})),
                Some("zaru-web-token"),
            )
            .await;
            assert_eq!(
                (status, body),
                (
                    200,
                    json!({"owner_sub": "a-person-sub", "value": token_set()})
                ),
                "{kind}"
            );
            assert_eq!(
                headers
                    .get(reqwest::header::CACHE_CONTROL)
                    .and_then(|h| h.to_str().ok()),
                Some("no-store")
            );
            let (status, body) = c.delete(kind, id, "zaru-web-token").await;
            assert_eq!(
                (status, body),
                (200, json!({"status": "deleted", "kind": kind, "id": id}))
            );
            let (status, _) = c.read(kind, id, "zaru-web-token").await;
            assert_eq!(status, 404, "{kind} was not deleted");
        }
    }

    #[tokio::test]
    async fn every_caller_but_a_declared_client_is_refused() {
        let c = clients(InMemoryVaultAuditSink::new()).await;
        let refused = json!({
            "error": "vault_client_not_declared",
            "message": "This client may not keep secrets in the vault.",
        });
        let mut failures = Vec::new();
        for token in ["undeclared-token", "consumer-token", "admin-token"] {
            for (verb, (status, body)) in [
                ("put", c.put("session", SESSION, token).await),
                ("read", c.read("session", SESSION, token).await),
                ("delete", c.delete("session", SESSION, token).await),
            ] {
                if (status, &body) != (403, &refused) {
                    failures.push(format!("{token} {verb}: {status} {body}"));
                }
            }
        }
        for (verb, (status, _)) in [
            (
                "put",
                c.put("session", SESSION, "aegis_live_0123456789").await,
            ),
            (
                "read",
                c.read("session", SESSION, "aegis_live_0123456789").await,
            ),
            (
                "delete",
                c.delete("session", SESSION, "aegis_live_0123456789").await,
            ),
        ] {
            if status != 401 {
                failures.push(format!("aegis_ key {verb}: {status}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
        assert!(c.audit.rows().await.is_empty());
    }

    #[tokio::test]
    async fn a_client_reading_another_clients_path_gets_404() {
        let c = clients(InMemoryVaultAuditSink::new()).await;
        let (status, _) = c.put("session", SESSION, "zaru-web-token").await;
        assert_eq!(status, 200);
        let (status, body) = c.read("session", SESSION, "other-web-token").await;
        assert_eq!(
            (status, body),
            (404, json!({"error": "Client secret not found"}))
        );
        let (status, _) = c.delete("session", SESSION, "other-web-token").await;
        assert_eq!(status, 404);
        let (status, _) = c.read("session", SESSION, "zaru-web-token").await;
        assert_eq!(status, 200, "another client's delete reached it");
    }

    #[tokio::test]
    async fn each_client_secret_act_writes_its_row_with_no_value() {
        let c = clients(InMemoryVaultAuditSink::new()).await;
        c.put("session", SESSION, "zaru-web-token").await;
        c.read("session", SESSION, "zaru-web-token").await;
        c.delete("session", SESSION, "zaru-web-token").await;
        let rows = c.audit.rows().await;
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.action, r.actor_id.as_str(), r.target_resource.as_str()))
            .collect();
        let target = format!("client-secret:zaru-web/session/{SESSION}");
        assert_eq!(
            got,
            vec![
                (
                    VaultAction::ClientSecretWritten,
                    "zaru-web-sa-sub",
                    target.as_str()
                ),
                (
                    VaultAction::ClientSecretRead,
                    "zaru-web-sa-sub",
                    target.as_str()
                ),
                (
                    VaultAction::ClientSecretDeleted,
                    "zaru-web-sa-sub",
                    target.as_str()
                ),
            ]
        );
        assert_eq!(
            rows[1].after_state,
            json!({
                "client_id": "zaru-web",
                "kind": "session",
                "owner_sub": "a-person-sub",
                "purpose": "session-read",
                "outcome": "answered",
            })
        );
        for row in &rows {
            let text = format!("{row:?}");
            assert!(!text.contains(TOKEN), "a row carries the value: {text}");
        }
    }

    #[tokio::test]
    async fn a_client_secret_read_is_refused_without_its_row() {
        let c = clients(InMemoryVaultAuditSink::failing()).await;
        let (status, _) = c.put("session", SESSION, "zaru-web-token").await;
        assert_eq!(status, 200, "a write stands when its row fails");
        let (status, body) = c.read("session", SESSION, "zaru-web-token").await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(body["error"], json!("vault_unavailable"));
        assert!(!body.to_string().contains(TOKEN));
    }

    #[tokio::test]
    async fn a_client_secret_read_needs_a_purpose() {
        let c = clients(InMemoryVaultAuditSink::new()).await;
        c.put("session", SESSION, "zaru-web-token").await;
        let (status, body) = send(
            &c.base,
            &Method::POST,
            &format!("/v1/vault/client-secrets/session/{SESSION}/value"),
            &Some(json!({"purpose": "Session"})),
            Some("zaru-web-token"),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"], json!("vault_purpose_required"));
    }
}
