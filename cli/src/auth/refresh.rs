// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;

use super::profile::AegisProfile;

const CLIENT_ID: &str = "aegis-cli";

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn refresh_token(profile: &AegisProfile) -> Result<AegisProfile> {
    let auth_base = format!(
        "https://auth.{}/realms/aegis-system/protocol/openid-connect",
        profile.env
    );
    refresh_token_at(&auth_base, profile).await
}

/// Refresh against the OpenID Connect endpoints under `auth_base`.
pub(crate) async fn refresh_token_at(
    auth_base: &str,
    profile: &AegisProfile,
) -> Result<AegisProfile> {
    // Audit 002 §4.37.9 — bound the refresh wait on a frozen IdP.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("reqwest client must build");

    let resp = client
        .post(format!("{auth_base}/token"))
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            // Read to send the refresh grant to the auth server.
            ("refresh_token", profile.refresh_key.expose()),
        ])
        .send()
        .await
        .context("Failed to contact auth server for token refresh")?;

    let token: TokenResponse = resp
        .json()
        .await
        .context("Failed to parse token refresh response")?;

    if let Some(ref error) = token.error {
        let desc = token.error_description.as_deref().unwrap_or("");
        anyhow::bail!("Session expired. Run 'aegis auth login' to authenticate. ({error}: {desc})");
    }

    let access_key = token
        .access_token
        .ok_or_else(|| anyhow::anyhow!("No access token in refresh response"))?;
    let refresh_key = token
        .refresh_token
        .ok_or_else(|| anyhow::anyhow!("No refresh token in refresh response"))?;
    let expires_in = token.expires_in.unwrap_or(900);
    let expires_at = Utc::now() + chrono::Duration::seconds(expires_in as i64);

    let scopes: Vec<String> = token
        .scope
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(String::from)
        .collect();
    let roles: Vec<String> = scopes
        .iter()
        .filter(|s| s.starts_with("aegis:"))
        .cloned()
        .collect();

    Ok(AegisProfile {
        name: profile.name.clone(),
        env: profile.env.clone(),
        client_id: profile.client_id.clone(),
        access_key: access_key.into(),
        refresh_key: refresh_key.into(),
        expires_at,
        roles,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    type Seen = std::sync::Arc<std::sync::Mutex<Option<(String, HashMap<String, String>)>>>;

    async fn token_endpoint(
        axum::extract::State(seen): axum::extract::State<Seen>,
        uri: axum::http::Uri,
        axum::Form(form): axum::Form<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        *seen.lock().unwrap() = Some((uri.path().to_string(), form));
        axum::Json(serde_json::json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 900,
        }))
    }

    /// The refresh grant reaches the auth server's token endpoint carrying the
    /// refresh token exactly as held.
    #[tokio::test]
    async fn refresh_sends_the_refresh_token_as_held() {
        let seen: Seen = Default::default();
        let app = axum::Router::new()
            .route(
                "/realms/aegis-system/protocol/openid-connect/token",
                axum::routing::post(token_endpoint),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let profile = AegisProfile {
            name: "default".to_string(),
            env: "dev.example.com".to_string(),
            client_id: "aegis-cli".to_string(),
            access_key: "old-access".into(),
            refresh_key: "Mk7-refresh-grant-marker".into(),
            expires_at: Utc::now(),
            roles: vec![],
            scopes: vec![],
        };

        refresh_token_at(
            &format!("http://{addr}/realms/aegis-system/protocol/openid-connect"),
            &profile,
        )
        .await
        .expect("the refresh is granted");

        let (path, form) = seen.lock().unwrap().clone().expect("the grant arrived");
        assert_eq!(path, "/realms/aegis-system/protocol/openid-connect/token");
        assert_eq!(
            form.get("refresh_token").map(String::as_str),
            Some("Mk7-refresh-grant-marker"),
            "the refresh grant carried a different refresh token than the one held"
        );
    }
}
