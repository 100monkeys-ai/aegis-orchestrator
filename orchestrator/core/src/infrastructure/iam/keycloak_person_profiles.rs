// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # A person's name and email over Keycloak (AEGIS ADR-136 G5d)
//!
//! Implements [`PersonProfiles`] by reading the person's Keycloak user by
//! their `sub`, through [`KeycloakAdminClient::get_user`], in the node's
//! realm of kind `consumer` and then its realm of kind `system`: the first
//! realm that holds the subject answers. The name is the first and last
//! names joined, or the email when both are empty; a user with no email
//! answers none. No cache: a run's author is read once, when its
//! repositories are prepared.

use std::sync::Arc;

use async_trait::async_trait;

use crate::application::git_repo_service::PersonProfiles;
use crate::infrastructure::iam::keycloak_admin_client::{KeycloakAdminClient, KeycloakUser};

/// Reads a person's name and email from their Keycloak user.
pub struct KeycloakPersonProfiles {
    client: Arc<KeycloakAdminClient>,
    realms: Vec<String>,
}

impl KeycloakPersonProfiles {
    /// `realms` in the order they are read: the consumer realm, then the
    /// system realm.
    pub fn new(client: Arc<KeycloakAdminClient>, realms: Vec<String>) -> Self {
        Self { client, realms }
    }

    pub fn realms(&self) -> &[String] {
        &self.realms
    }
}

/// The name and email a Keycloak user gives: none without an email.
pub fn person_name_and_email(user: &KeycloakUser) -> Option<(String, String)> {
    let email = user
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())?;
    let name = [user.first_name.as_deref(), user.last_name.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let name = if name.is_empty() {
        email.to_string()
    } else {
        name
    };
    Some((name, email.to_string()))
}

#[async_trait]
impl PersonProfiles for KeycloakPersonProfiles {
    async fn name_and_email(&self, sub: &str) -> Result<Option<(String, String)>, String> {
        // `get_user` answers `Ok(None)` on 404 only; every other failure is
        // an error, never a person with no profile.
        for realm in &self.realms {
            match self.client.get_user(realm, sub).await {
                Ok(Some(user)) => return Ok(person_name_and_email(&user)),
                Ok(None) => continue,
                Err(e) => return Err(format!("keycloak admin read failed: {e}")),
            }
        }
        Ok(None)
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

    const CONSUMER: &str = "zaru-consumer";
    const SYSTEM: &str = "aegis-system";

    /// A Keycloak answering the master-realm token grant and one user read
    /// per realm and id: in the consumer realm `ada` (first and last names
    /// and an email), `mono` (an email, no names), `nomail` (names, no
    /// email) and `broken` (500); in the system realm `operator`; any other
    /// read 404.
    async fn keycloak() -> String {
        async fn token() -> Json<Value> {
            Json(json!({ "access_token": "admin-token", "expires_in": 300 }))
        }
        async fn user(Path((realm, id)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
            let found = |first: Value, last: Value, email: Value| {
                (
                    StatusCode::OK,
                    Json(json!({ "id": id, "firstName": first, "lastName": last,
                        "email": email, "createdTimestamp": 0 })),
                )
            };
            match (realm.as_str(), id.as_str()) {
                (CONSUMER, "ada") => {
                    found(json!("Ada"), json!("Lovelace"), json!("ada@example.com"))
                }
                (CONSUMER, "mono") => found(json!(""), json!(null), json!("mono@example.com")),
                (CONSUMER, "nomail") => found(json!("No"), json!("Mail"), json!(null)),
                (CONSUMER, "broken") => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "boom" })),
                ),
                (SYSTEM, "operator") => found(json!("Op"), json!(null), json!("op@example.com")),
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

    fn profiles_at(host: String) -> KeycloakPersonProfiles {
        KeycloakPersonProfiles::new(
            Arc::new(KeycloakAdminClient::new(KeycloakAdminConfig {
                host,
                admin_username: "admin".to_string(),
                admin_password: SensitiveString::new("not-a-secret".to_string()),
            })),
            vec![CONSUMER.to_string(), SYSTEM.to_string()],
        )
    }

    fn pair(name: &str, email: &str) -> Option<(String, String)> {
        Some((name.to_string(), email.to_string()))
    }

    /// The consumer realm first, then the system realm; the names joined,
    /// the email standing for an empty name, none without an email or a
    /// user; every answer checked before failing.
    #[tokio::test]
    async fn reads_the_persons_name_and_email_from_the_first_realm_holding_them() {
        let profiles = profiles_at(keycloak().await);
        let mut wrong = Vec::new();
        for (sub, expected) in [
            ("ada", pair("Ada Lovelace", "ada@example.com")),
            ("mono", pair("mono@example.com", "mono@example.com")),
            ("nomail", None),
            ("operator", pair("Op", "op@example.com")),
            ("nobody", None),
        ] {
            let answered = profiles.name_and_email(sub).await;
            if answered != Ok(expected.clone()) {
                wrong.push(format!("{sub}: answered {answered:?}, not {expected:?}"));
            }
        }
        assert!(wrong.is_empty(), "the profile reads are wrong: {wrong:?}");
    }

    /// A status other than success and 404 is an error, never a person
    /// with no profile.
    #[tokio::test]
    async fn any_other_status_is_an_error() {
        let profiles = profiles_at(keycloak().await);
        let err = profiles
            .name_and_email("broken")
            .await
            .expect_err("a 500 was read as a profile");
        assert!(err.starts_with("keycloak admin read failed: "), "{err}");
    }
}
