// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::{Context, Result};
use chrono::Utc;
use colored::Colorize;
use serde::Deserialize;

use super::profile::AegisProfile;

const CLIENT_ID: &str = "aegis-cli";

#[derive(Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn run_device_flow(env: &str) -> Result<AegisProfile> {
    let auth_base = format!("https://auth.{env}/realms/aegis-system/protocol/openid-connect");
    let client = reqwest::Client::new();

    let device_resp = client
        .post(format!("{auth_base}/auth/device"))
        .form(&[
            ("client_id", CLIENT_ID),
            (
                "scope",
                "openid profile offline_access aegis:admin aegis:operator aegis:readonly",
            ),
        ])
        .send()
        .await
        .context("Failed to connect to auth server. Check --env and network connectivity.")?;

    if !device_resp.status().is_success() {
        let status = device_resp.status();
        let body = device_resp.text().await.unwrap_or_default();
        anyhow::bail!("Auth server returned {status}: {body}");
    }

    let device_auth: DeviceAuthResponse = device_resp
        .json()
        .await
        .context("Failed to parse device auth response")?;

    println!();
    println!(
        "! First copy your one-time code: {}",
        device_auth.user_code.bold().yellow()
    );
    println!();
    println!(
        "  Open this URL in your browser:\n  {}",
        device_auth.verification_uri_complete.cyan().underline()
    );
    println!();
    println!("  Waiting for authentication...");

    let poll_interval = std::time::Duration::from_secs(device_auth.interval.max(5));
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(device_auth.expires_in);

    loop {
        tokio::time::sleep(poll_interval).await;

        if std::time::Instant::now() > deadline {
            anyhow::bail!("Authentication timed out. Run 'aegis auth login' to try again.");
        }

        let token_resp = client
            .post(format!("{auth_base}/token"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("device_code", device_auth.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .context("Failed to poll token endpoint")?;

        let token: TokenResponse = token_resp
            .json()
            .await
            .context("Failed to parse token response")?;

        if let Some(ref error) = token.error {
            match error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
                "access_denied" => anyhow::bail!("Authentication was denied."),
                "expired_token" => {
                    anyhow::bail!("Device code expired. Run 'aegis auth login' to try again.")
                }
                other => {
                    let desc = token.error_description.as_deref().unwrap_or("");
                    anyhow::bail!("Authentication failed: {other} — {desc}");
                }
            }
        }

        let access_key = token
            .access_token
            .ok_or_else(|| anyhow::anyhow!("No access token in response"))?;
        let refresh_key = token
            .refresh_token
            .ok_or_else(|| anyhow::anyhow!("No refresh token in response"))?;
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

        return Ok(AegisProfile {
            name: env.to_string(),
            env: env.to_string(),
            client_id: CLIENT_ID.to_string(),
            access_key,
            refresh_key,
            expires_at,
            roles,
            scopes,
        });
    }
}
