// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AegisProfile {
    pub name: String,
    /// Environment hostname, e.g. "dev.100monkeys.ai"
    pub env: String,
    pub client_id: String,
    pub access_key: String,
    pub refresh_key: String,
    pub expires_at: DateTime<Utc>,
    pub roles: Vec<String>,
    pub scopes: Vec<String>,
}

impl AegisProfile {
    pub fn auth_url(&self) -> String {
        format!("https://auth.{}/realms/aegis-system", self.env)
    }

    pub fn api_url(&self) -> String {
        format!("https://api.{}", self.env)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    pub active_profile: String,
    pub profiles: HashMap<String, AegisProfile>,
}

fn auth_store_path() -> PathBuf {
    dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aegis")
        .join("auth.json")
}

pub fn load_store() -> Result<AuthStore> {
    let path = auth_store_path();
    if !path.exists() {
        return Ok(AuthStore::default());
    }
    let raw = std::fs::read(&path).context("Failed to read ~/.aegis/auth.json")?;
    let decrypted = decrypt_store(&raw)?;
    serde_json::from_slice(&decrypted).context("Failed to parse auth store")
}

pub fn save_store(store: &AuthStore) -> Result<()> {
    let path = auth_store_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create ~/.aegis directory")?;
    }
    let json = serde_json::to_vec_pretty(store)?;
    let encrypted = encrypt_store(&json)?;
    std::fs::write(&path, encrypted).context("Failed to write ~/.aegis/auth.json")?;
    Ok(())
}

fn encryption_key() -> [u8; 32] {
    use sha2::Digest;

    // 1. Try system keyring
    if let Ok(entry) = keyring::Entry::new("aegis-cli", "auth-store-key") {
        if let Ok(stored) = entry.get_password() {
            let hash = sha2::Sha256::digest(stored.as_bytes());
            let mut key = [0u8; 32];
            key.copy_from_slice(&hash);
            return key;
        }
    }

    // 2. AEGIS_AUTH_KEY env var
    if let Ok(val) = std::env::var("AEGIS_AUTH_KEY") {
        let hash = sha2::Sha256::digest(val.as_bytes());
        let mut key = [0u8; 32];
        key.copy_from_slice(&hash);
        return key;
    }

    // 3. Machine-local fallback (HOSTNAME env var)
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "aegis-local".to_string());
    let hash = sha2::Sha256::digest(format!("aegis-auth-{hostname}").as_bytes());
    let mut key = [0u8; 32];
    key.copy_from_slice(&hash);
    key
}

fn encrypt_store(plaintext: &[u8]) -> Result<Vec<u8>> {
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
    };

    let key_bytes = encryption_key();
    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to init cipher: {e}"))?;

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {e}"))?;

    let mut result = nonce_bytes.to_vec();
    result.extend(ciphertext);
    Ok(result)
}

fn decrypt_store(data: &[u8]) -> Result<Vec<u8>> {
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit},
    };

    if data.len() < 12 {
        anyhow::bail!("Auth store is corrupted (too short to contain nonce)");
    }
    let (nonce_bytes, ciphertext) = data.split_at(12);
    let key_bytes = encryption_key();
    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to init cipher: {e}"))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ciphertext).map_err(|_| {
        anyhow::anyhow!(
            "Failed to decrypt auth store. If AEGIS_AUTH_KEY changed, run 'aegis auth login' again."
        )
    })
}
