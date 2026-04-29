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
            ("refresh_token", profile.refresh_key.as_str()),
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
        access_key,
        refresh_key,
        expires_at,
        roles,
        scopes,
    })
}
