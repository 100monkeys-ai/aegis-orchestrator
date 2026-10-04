// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # The operator role lookup over Keycloak (AEGIS ADR-129 — Updates, V9)
//!
//! Implements [`OperatorRoleLookup`] by reading the operator's federated
//! record in the system realm (the `spec.iam.realms` entry whose `kind` is
//! `system`; `aegis-system` in production) by its `sub`, through
//! [`KeycloakAdminClient::get_user`], with the master-realm admin credential
//! the node already holds in `spec.iam.keycloak_admin` (V7). No cache: a
//! demotion takes effect at the next escalated call (the Update's
//! alternatives: "A cached read at dispatch" rejected).

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::operator_escalation::{
    OperatorRecord, OperatorRoleLookup, RoleLookupError, AEGIS_ROLE_ATTRIBUTE,
};
use crate::infrastructure::iam::keycloak_admin_client::{KeycloakAdminClient, KeycloakUser};

/// Reads `aegis_role` and `enabled` on a user of the system realm.
pub struct KeycloakOperatorRoleLookup {
    client: Arc<KeycloakAdminClient>,
    system_realm: String,
}

impl KeycloakOperatorRoleLookup {
    pub fn new(client: Arc<KeycloakAdminClient>, system_realm: impl Into<String>) -> Self {
        Self {
            client,
            system_realm: system_realm.into(),
        }
    }

    pub fn system_realm(&self) -> &str {
        &self.system_realm
    }
}

/// What a read of the user answered, as the port states it (V1, V6): no user
/// is absent; `enabled: false` is disabled; otherwise the attribute's first
/// value, `None` when the attribute is absent or empty.
pub fn operator_record(user: Option<KeycloakUser>) -> OperatorRecord {
    let Some(user) = user else {
        return OperatorRecord::Absent;
    };
    if user.enabled == Some(false) {
        return OperatorRecord::Disabled;
    }
    OperatorRecord::Found {
        aegis_role: user
            .attributes
            .as_ref()
            .and_then(|attrs| attrs.get(AEGIS_ROLE_ATTRIBUTE))
            .and_then(|values| values.first())
            .cloned(),
    }
}

#[async_trait]
impl OperatorRoleLookup for KeycloakOperatorRoleLookup {
    async fn lookup(&self, system_sub: &str) -> Result<OperatorRecord, RoleLookupError> {
        // `get_user` answers `Ok(None)` on 404 only; every other failure (no
        // connection, the client's 5 s / 30 s bounds, a refused admin token,
        // any other status) is an error, which the port reports as one (V5).
        self.client
            .get_user(&self.system_realm, system_sub)
            .await
            .map(operator_record)
            .map_err(|e| RoleLookupError(format!("keycloak admin read failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::secrets::SensitiveString;
    use crate::infrastructure::iam::keycloak_admin_client::KeycloakAdminConfig;
    use axum::extract::Path;
    use axum::http::StatusCode;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{json, Value};

    const REALM: &str = "aegis-system";

    /// A Keycloak answering the master-realm token grant and one user read
    /// per path: `operator` holds `aegis:operator`, `demoted` holds no
    /// attribute, `disabled` is disabled, `broken` answers 500, any other id
    /// 404.
    async fn keycloak() -> String {
        async fn token() -> Json<Value> {
            Json(json!({ "access_token": "admin-token", "expires_in": 300 }))
        }
        async fn user(Path((realm, id)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
            assert_eq!(realm, REALM);
            let base = json!({ "id": id, "email": null, "firstName": null,
                "lastName": null, "createdTimestamp": 0 });
            let with = |extra: Value| {
                let mut v = base.clone();
                v.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                v
            };
            match id.as_str() {
                "operator" => (
                    StatusCode::OK,
                    Json(with(json!({ "enabled": true,
                        "attributes": { "aegis_role": ["aegis:operator"],
                                        "consumer_sub": ["c"] } }))),
                ),
                "demoted" => (
                    StatusCode::OK,
                    Json(with(json!({ "enabled": true,
                        "attributes": { "consumer_sub": ["c"] } }))),
                ),
                "disabled" => (
                    StatusCode::OK,
                    Json(with(json!({ "enabled": false,
                        "attributes": { "aegis_role": ["aegis:operator"] } }))),
                ),
                "broken" => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "boom" })),
                ),
                _ => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "User not found" })),
                ),
            }
        }
        let app = Router::new()
            .route("/realms/master/protocol/openid-connect/token", post(token))
            .route("/admin/realms/{realm}/users/{id}", get(user));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn lookup_at(host: String) -> KeycloakOperatorRoleLookup {
        KeycloakOperatorRoleLookup::new(
            Arc::new(KeycloakAdminClient::new(KeycloakAdminConfig {
                host,
                admin_username: "admin".to_string(),
                admin_password: SensitiveString::new("not-a-secret".to_string()),
            })),
            REALM,
        )
    }

    /// V1 and V6: each answer of the admin API maps to what the port states.
    #[tokio::test]
    async fn reads_role_absence_and_disabled_from_the_admin_api() {
        let lookup = lookup_at(keycloak().await);
        assert_eq!(
            lookup.lookup("operator").await,
            Ok(OperatorRecord::Found {
                aegis_role: Some("aegis:operator".to_string())
            })
        );
        assert_eq!(
            lookup.lookup("demoted").await,
            Ok(OperatorRecord::Found { aegis_role: None })
        );
        assert_eq!(
            lookup.lookup("disabled").await,
            Ok(OperatorRecord::Disabled)
        );
        assert_eq!(lookup.lookup("gone").await, Ok(OperatorRecord::Absent));
    }

    /// V5: a status other than success and 404 is an error, not a demotion.
    #[tokio::test]
    async fn any_other_status_is_a_lookup_error() {
        let lookup = lookup_at(keycloak().await);
        let err = lookup.lookup("broken").await.unwrap_err();
        assert!(err.0.starts_with("keycloak admin read failed: "), "{err}");
        assert!(err.0.contains("500"), "{err}");
    }

    /// V5: no connection is an error, not a demotion.
    #[tokio::test]
    async fn an_unreachable_keycloak_is_a_lookup_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let lookup = lookup_at(format!("http://{addr}"));
        assert!(lookup.lookup("operator").await.is_err());
    }
}
