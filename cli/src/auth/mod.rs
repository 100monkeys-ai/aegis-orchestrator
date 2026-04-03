// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
mod device_flow;
mod env;
mod profile;
mod refresh;

pub use profile::{AegisProfile, AuthStore, load_store, save_store};

use anyhow::Result;

/// Returns the bearer key for the current invocation.
///
/// Precedence:
/// 1. `AEGIS_KEY` env var — returned directly (CI/CD path)
/// 2. Active profile in `~/.aegis/auth.json`:
///    a. Access key still valid → return it
///    b. Access key expired, refresh key valid → silently refresh, persist, return new key
///    c. Refresh key expired → error prompting `aegis auth login`
/// 3. No profile → error prompting `aegis auth login`
pub async fn require_key() -> Result<String> {
    if let Some(key) = env::aegis_key() {
        return Ok(key);
    }

    let mut store = load_store()?;
    let profile_name = store.active_profile.clone();
    let profile = store
        .profiles
        .get(&profile_name)
        .ok_or_else(|| anyhow::anyhow!("Not authenticated. Run 'aegis auth login'."))?
        .clone();

    if chrono::Utc::now() < profile.expires_at {
        return Ok(profile.access_key.clone());
    }

    // Silent refresh
    let refreshed = refresh::refresh_token(&profile).await?;
    store.profiles.insert(profile_name, refreshed.clone());
    save_store(&store)?;
    Ok(refreshed.access_key)
}

pub use device_flow::run_device_flow;
