// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL gateway operator token (ADR-088 §6.6)
//!
//! The orchestrator calls the SEAL gateway's operator API — SEAL session
//! pre-creation over HTTP, tool enumeration and invocation over gRPC — as an
//! operator. It authenticates with an access token it obtains from the
//! identity provider by the OAuth 2.0 client-credentials grant, for its own
//! confidential client, and refreshes before the token expires. There is no
//! static bearer: a long-lived token in an environment file is a credential
//! that never rotates.
//!
//! The gateway accepts the token only if the identity provider put the
//! gateway's audience and the operator role in it; in the AEGIS deployment
//! that is the audience and role mappers `bootstrap-keycloak.sh` installs on
//! the `aegis-orchestrator` client of the `aegis-system` realm.
//!
//! Configured from three environment variables, all or none:
//! `AEGIS_SEAL_OPERATOR_TOKEN_URL` (the realm's token endpoint),
//! `AEGIS_SEAL_OPERATOR_CLIENT_ID` and `AEGIS_SEAL_OPERATOR_CLIENT_SECRET`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::domain::secrets::SensitiveString;

/// Environment variable naming the identity provider's token endpoint.
pub const TOKEN_URL_ENV: &str = "AEGIS_SEAL_OPERATOR_TOKEN_URL";
/// Environment variable naming the confidential client.
pub const CLIENT_ID_ENV: &str = "AEGIS_SEAL_OPERATOR_CLIENT_ID";
/// Environment variable holding the client's secret.
pub const CLIENT_SECRET_ENV: &str = "AEGIS_SEAL_OPERATOR_CLIENT_SECRET";

/// A token is refreshed once less than this much of its life remains, or
/// once less than a tenth of its lifetime remains, whichever is longer.
const MIN_REFRESH_MARGIN: chrono::Duration = chrono::Duration::seconds(30);

#[derive(Debug, thiserror::Error)]
pub enum OperatorTokenError {
    #[error(
        "SEAL gateway operator credentials are incomplete: {missing} is not set, while \
         the other of AEGIS_SEAL_OPERATOR_TOKEN_URL, AEGIS_SEAL_OPERATOR_CLIENT_ID and \
         AEGIS_SEAL_OPERATOR_CLIENT_SECRET are"
    )]
    Incomplete { missing: &'static str },
    #[error("SEAL gateway operator token request failed: {0}")]
    Request(String),
    #[error("SEAL gateway operator token endpoint answered HTTP {status}")]
    Status { status: u16 },
    #[error("SEAL gateway operator token response is not usable: {0}")]
    Response(String),
}

/// Client-credentials configuration for the gateway's operator API.
#[derive(Debug, Clone)]
pub struct OperatorCredentials {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: SensitiveString,
}

impl OperatorCredentials {
    /// Read the three variables. `Ok(None)` when none is set; an error when
    /// some but not all are, so a half-configured orchestrator refuses to
    /// start instead of calling the gateway without credentials.
    pub fn from_env() -> Result<Option<Self>, OperatorTokenError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, OperatorTokenError> {
        let read = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let token_url = read(TOKEN_URL_ENV);
        let client_id = read(CLIENT_ID_ENV);
        let client_secret = read(CLIENT_SECRET_ENV);
        match (token_url, client_id, client_secret) {
            (None, None, None) => Ok(None),
            (Some(token_url), Some(client_id), Some(client_secret)) => Ok(Some(Self {
                token_url,
                client_id,
                client_secret: SensitiveString::new(client_secret),
            })),
            (token_url, client_id, _) => Err(OperatorTokenError::Incomplete {
                missing: if token_url.is_none() {
                    TOKEN_URL_ENV
                } else if client_id.is_none() {
                    CLIENT_ID_ENV
                } else {
                    CLIENT_SECRET_ENV
                },
            }),
        }
    }
}

struct CachedToken {
    access_token: SensitiveString,
    refresh_after: DateTime<Utc>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    #[serde(default)]
    token_type: Option<String>,
}

/// Obtains, caches and refreshes the operator access token.
pub struct OperatorTokenSource {
    http: reqwest::Client,
    credentials: OperatorCredentials,
    cached: Mutex<Option<CachedToken>>,
}

impl OperatorTokenSource {
    pub fn new(credentials: OperatorCredentials) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("operator token http client must build with valid defaults");
        Self {
            http,
            credentials,
            cached: Mutex::new(None),
        }
    }

    /// The value of the `Authorization` header for a call to the gateway's
    /// operator API: `Bearer <access token>`, from the cache while the token
    /// has enough life left, otherwise from a fresh client-credentials grant.
    /// Concurrent callers share one refresh.
    pub async fn authorization(&self) -> Result<SensitiveString, OperatorTokenError> {
        let mut cached = self.cached.lock().await;
        if let Some(token) = cached.as_ref() {
            if Utc::now() < token.refresh_after {
                return Ok(SensitiveString::new(format!(
                    "Bearer {}",
                    token.access_token.expose()
                )));
            }
        }
        let fresh = self.fetch().await?;
        let header = SensitiveString::new(format!("Bearer {}", fresh.access_token.expose()));
        *cached = Some(fresh);
        Ok(header)
    }

    async fn fetch(&self) -> Result<CachedToken, OperatorTokenError> {
        let requested_at = Utc::now();
        let response = self
            .http
            .post(&self.credentials.token_url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.credentials.client_id.as_str()),
                ("client_secret", self.credentials.client_secret.expose()),
            ])
            .send()
            .await
            .map_err(|e| OperatorTokenError::Request(e.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            // The body is not logged or returned: an identity provider's
            // error body can echo request parameters.
            return Err(OperatorTokenError::Status {
                status: status.as_u16(),
            });
        }
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|e| OperatorTokenError::Response(e.without_url().to_string()))?;
        if body.access_token.is_empty() {
            return Err(OperatorTokenError::Response("empty access_token".into()));
        }
        if let Some(kind) = body.token_type.as_deref() {
            if !kind.eq_ignore_ascii_case("bearer") {
                return Err(OperatorTokenError::Response(format!(
                    "token_type {kind}, expected Bearer"
                )));
            }
        }
        if body.expires_in <= 0 {
            return Err(OperatorTokenError::Response(format!(
                "expires_in {}",
                body.expires_in
            )));
        }
        let lifetime = chrono::Duration::seconds(body.expires_in);
        let margin = std::cmp::max(MIN_REFRESH_MARGIN, lifetime / 10);
        Ok(CachedToken {
            access_token: SensitiveString::new(body.access_token),
            refresh_after: requested_at + lifetime - margin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Form, Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A test identity provider: a real HTTP token endpoint on a loopback
    /// port that issues `token-<n>` to the right client, with the lifetime
    /// the test chooses, and counts the grants it served.
    struct TestIdp {
        issued: AtomicUsize,
        expires_in: i64,
    }

    async fn token_endpoint(
        State(idp): State<Arc<TestIdp>>,
        Form(form): Form<HashMap<String, String>>,
    ) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
        if form.get("grant_type").map(String::as_str) != Some("client_credentials")
            || form.get("client_id").map(String::as_str) != Some("aegis-orchestrator")
            || form.get("client_secret").map(String::as_str) != Some("test-secret")
        {
            return Err(axum::http::StatusCode::UNAUTHORIZED);
        }
        let n = idp.issued.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Json(serde_json::json!({
            "access_token": format!("token-{n}"),
            "expires_in": idp.expires_in,
            "token_type": "Bearer",
        })))
    }

    async fn start_idp(expires_in: i64) -> (Arc<TestIdp>, String) {
        let idp = Arc::new(TestIdp {
            issued: AtomicUsize::new(0),
            expires_in,
        });
        let app = Router::new()
            .route(
                "/realms/aegis-system/protocol/openid-connect/token",
                post(token_endpoint),
            )
            .with_state(idp.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test idp");
        });
        (
            idp,
            format!("http://{addr}/realms/aegis-system/protocol/openid-connect/token"),
        )
    }

    fn credentials(token_url: String, secret: &str) -> OperatorCredentials {
        OperatorCredentials {
            token_url,
            client_id: "aegis-orchestrator".to_string(),
            client_secret: SensitiveString::new(secret),
        }
    }

    #[tokio::test]
    async fn obtains_a_token_by_client_credentials_and_reuses_it_while_fresh() {
        let (idp, url) = start_idp(300).await;
        let source = OperatorTokenSource::new(credentials(url, "test-secret"));
        let first = source.authorization().await.expect("first token");
        let second = source.authorization().await.expect("second token");
        assert_eq!(first.expose(), "Bearer token-1");
        assert_eq!(second.expose(), "Bearer token-1");
        assert_eq!(idp.issued.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refreshes_a_token_before_it_expires() {
        // A 20-second token is inside the 30-second refresh margin as soon as
        // it is issued, so every call obtains a new one.
        let (idp, url) = start_idp(20).await;
        let source = OperatorTokenSource::new(credentials(url, "test-secret"));
        assert_eq!(
            source.authorization().await.unwrap().expose(),
            "Bearer token-1"
        );
        assert_eq!(
            source.authorization().await.unwrap().expose(),
            "Bearer token-2"
        );
        assert_eq!(idp.issued.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_refused_grant_is_an_error_that_carries_no_secret() {
        let (_idp, url) = start_idp(300).await;
        let source = OperatorTokenSource::new(credentials(url, "wrong-secret"));
        let error = source.authorization().await.expect_err("refused");
        let text = error.to_string();
        assert!(text.contains("HTTP 401"), "{text}");
        assert!(!text.contains("wrong-secret"), "{text}");
    }

    #[test]
    fn credentials_are_all_or_none() {
        let none = OperatorCredentials::from_lookup(|_| None).expect("none is fine");
        assert!(none.is_none());
        let partial = OperatorCredentials::from_lookup(|name| {
            (name != CLIENT_SECRET_ENV).then(|| "x".to_string())
        });
        assert!(matches!(
            partial,
            Err(OperatorTokenError::Incomplete {
                missing: CLIENT_SECRET_ENV
            })
        ));
        let all = OperatorCredentials::from_lookup(|_| Some("x".to_string()))
            .expect("all")
            .expect("some");
        assert_eq!(format!("{:?}", all.client_secret), "[REDACTED]");
    }
}
