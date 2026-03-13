// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Secrets Manager (BC-11, ADR-034)
//!
//! Implements the OpenBao secrets management integration described in ADR-034.
//!
//! ## Implementation details
//!
//! 1. **OpenBao Deployment** — HA cluster with Raft backend.
//! 2. **AppRole Auth** — orchestrator nodes authenticate with Role ID + Secret ID.
//! 3. **KV Engine** — static API keys stored at `<tenant>/kv/<name>`.
//! 4. **Dynamic Secrets** — database credentials auto-rotated per `DynamicSecret.ttl`.
//! 5. **Transit Engine** — `sign()`, `verify()`, `encrypt()`, and `decrypt()`
//!    without key distribution.
//! 6. **Keymaster Pattern** — only the orchestrator accesses OpenBao;
//!    agents receive credentials via `spec.environment` injection, never via
//!    direct vault calls.
//!
//! See ADR-034 (OpenBao Secrets Management), AGENTS.md §BC-11.

use crate::domain::events::SecretEvent;
use crate::domain::mcp::{CredentialRef, CredentialStoreType};
use crate::domain::node_config::SecretBackendConfig;
use crate::domain::secrets::{
    AccessContext, DomainDynamicSecret, SecretStore, SecretsError, SensitiveString,
};
use crate::infrastructure::event_bus::EventBus;
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use chrono::Utc;
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};
use vaultrs::client::{Client, VaultClient, VaultClientSettingsBuilder};

impl From<vaultrs::error::ClientError> for SecretsError {
    fn from(err: vaultrs::error::ClientError) -> Self {
        match err {
            vaultrs::error::ClientError::APIError { code: 404, .. } => {
                SecretsError::SecretNotFound {
                    path: "unknown".into(),
                }
            }
            _ => SecretsError::ConnectionError(err.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenBaoSecretStore — production implementation using AppRole auth
// ---------------------------------------------------------------------------

/// OpenBao implementation using AppRole authentication and token auto-renewal.
pub struct OpenBaoSecretStore {
    client: Arc<RwLock<VaultClient>>,
}

impl OpenBaoSecretStore {
    /// Creates a new `OpenBaoSecretStore`, authenticates via AppRole, and spawns the renewal loop.
    pub async fn new(config: SecretBackendConfig) -> Result<Self, SecretsError> {
        let unauthenticated = Self::build_client(&config)?;
        let authenticated = Self::authenticate(unauthenticated, &config).await?;

        let config = Arc::new(config);
        let shared_client = Arc::new(RwLock::new(authenticated));

        let renewal_client = shared_client.clone();
        let renewal_config = config.clone();
        tokio::spawn(Self::token_renewal_loop(renewal_client, renewal_config));

        Ok(Self {
            client: shared_client,
        })
    }

    /// Build an unauthenticated `VaultClient` from the given config.
    fn build_client(config: &SecretBackendConfig) -> Result<VaultClient, SecretsError> {
        let mut builder = VaultClientSettingsBuilder::default();
        builder.address(&config.address);

        if let Some(namespace) = &config.namespace {
            builder.set_namespace(namespace.clone());
        }

        if let Some(ca_cert) = &config.tls.ca_cert {
            builder.ca_certs(vec![ca_cert.clone()]);
        }

        let settings = builder
            .build()
            .map_err(|e| SecretsError::ConfigError(e.to_string()))?;
        VaultClient::new(settings).map_err(|e| SecretsError::ConfigError(e.to_string()))
    }

    /// Perform AppRole authentication and return the authenticated client.
    async fn authenticate(
        mut client: VaultClient,
        config: &SecretBackendConfig,
    ) -> Result<VaultClient, SecretsError> {
        if config.auth_method != "approle" {
            return Err(SecretsError::ConfigError(format!(
                "Unsupported auth method: {}",
                config.auth_method
            )));
        }

        let secret_id = std::env::var(&config.approle.secret_id_env_var).map_err(|_| {
            SecretsError::ConfigError(format!(
                "Environment variable '{}' for AppRole Secret ID is not set",
                config.approle.secret_id_env_var
            ))
        })?;

        debug!("Authenticating with OpenBao AppRole...");
        let auth_response =
            vaultrs::auth::approle::login(&client, "approle", &config.approle.role_id, &secret_id)
                .await?;

        client.set_token(&auth_response.client_token);
        info!("Successfully authenticated with OpenBao AppRole");
        Ok(client)
    }

    /// Background loop: renew the token every 30 minutes.
    /// On renewal failure, re-authenticates from scratch so that subsequent
    /// secret operations are not permanently broken by an expired token.
    async fn token_renewal_loop(
        client: Arc<RwLock<VaultClient>>,
        config: Arc<SecretBackendConfig>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_secs(1800));
        loop {
            interval.tick().await;

            let mut client_write = client.write().await;
            match vaultrs::token::renew_self(&*client_write, None).await {
                Ok(_) => debug!("Successfully renewed OpenBao token"),
                Err(renew_err) => {
                    warn!(
                        "OpenBao token renewal failed ({}); attempting re-authentication...",
                        renew_err
                    );
                    match Self::build_client(&config) {
                        Ok(new_client) => match Self::authenticate(new_client, &config).await {
                            Ok(authenticated) => {
                                *client_write = authenticated;
                                info!("OpenBao re-authentication succeeded after renewal failure");
                            }
                            Err(re_auth_err) => {
                                warn!(
                                        "OpenBao re-authentication failed: {}. \
                                        Subsequent secret operations will fail until the token is valid.",
                                        re_auth_err
                                    );
                            }
                        },
                        Err(build_err) => {
                            warn!(
                                "Failed to rebuild OpenBao client for re-auth: {}",
                                build_err
                            );
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl SecretStore for OpenBaoSecretStore {
    async fn read(
        &self,
        engine: &str,
        path: &str,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
        let client = self.client.read().await;
        let raw: HashMap<String, String> = vaultrs::kv2::read(&*client, engine, path).await?;
        Ok(raw
            .into_iter()
            .map(|(k, v)| (k, SensitiveString::new(v)))
            .collect())
    }

    async fn write(
        &self,
        engine: &str,
        path: &str,
        secret: HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError> {
        let client = self.client.read().await;
        // Expose sensitive values only at the point of transmission to OpenBao.
        let raw: HashMap<String, String> = secret
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect();
        vaultrs::kv2::set(&*client, engine, path, &raw).await?;
        Ok(())
    }

    async fn generate_dynamic(
        &self,
        engine_path: &str,
        role: &str,
    ) -> Result<DomainDynamicSecret, SecretsError> {
        let client = self.client.read().await;
        // Dynamic secrets are generated via GET /v1/{engine}/creds/{role}.
        // This uses the OpenBao logical read path, not the KV2 API.
        // vaultrs 0.7 does not expose a generic logical Read endpoint, so we
        // use raw reqwest (same pattern already used for lease management).
        let address = client.settings().address.clone();
        let token = client.settings().token.clone();
        let url = format!("{address}v1/{engine_path}/creds/{role}");
        let http_resp = reqwest::Client::new()
            .get(&url)
            .header("X-Vault-Token", &token)
            .send()
            .await
            .map_err(|e| {
                SecretsError::DynamicSecretError(format!(
                    "HTTP GET {address}/{engine_path}/creds/{role} failed: {e}"
                ))
            })?;

        if !http_resp.status().is_success() {
            return Err(SecretsError::DynamicSecretError(format!(
                "Dynamic secret generation returned status {} for {}/creds/{}",
                http_resp.status(),
                engine_path,
                role
            )));
        }

        let resp_json: serde_json::Value = http_resp.json().await.map_err(|e| {
            SecretsError::DynamicSecretError(format!("Dynamic secret response parse failed: {e}"))
        })?;

        let lease_id = resp_json["lease_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let lease_duration_secs = resp_json["lease_duration"].as_u64().unwrap_or(300);
        let renewable = resp_json["renewable"].as_bool().unwrap_or(false);
        let data: HashMap<String, SensitiveString> = resp_json["data"]
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), SensitiveString::new(s))))
                    .collect()
            })
            .unwrap_or_default();

        Ok(DomainDynamicSecret {
            lease_id,
            values: data,
            lease_duration: Duration::from_secs(lease_duration_secs),
            renewable,
            created_at: Instant::now(),
        })
    }

    async fn renew_lease(
        &self,
        lease_id: &str,
        increment: Duration,
    ) -> Result<Duration, SecretsError> {
        // vaultrs 0.7 does not expose sys/leases endpoints directly.
        // Use the raw HTTP client to call PUT /v1/sys/leases/renew.
        let client = self.client.read().await;
        let address = client.settings().address.clone();
        let token = client.settings().token.clone();
        let url = format!("{address}v1/sys/leases/renew");
        let body = serde_json::json!({
            "lease_id": lease_id,
            "increment": format!("{}s", increment.as_secs()),
        });
        let http_resp = reqwest::Client::new()
            .put(&url)
            .header("X-Vault-Token", &token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                SecretsError::DynamicSecretError(format!("Lease renewal HTTP failed: {e}"))
            })?;
        if !http_resp.status().is_success() {
            return Err(SecretsError::DynamicSecretError(format!(
                "Lease renewal failed with status: {}",
                http_resp.status()
            )));
        }
        // The response contains lease_duration in the top-level JSON
        let resp_json: serde_json::Value = http_resp.json().await.map_err(|e| {
            SecretsError::DynamicSecretError(format!("Lease renewal response parse failed: {e}"))
        })?;
        let duration_secs = resp_json["lease_duration"]
            .as_u64()
            .unwrap_or(increment.as_secs());
        Ok(Duration::from_secs(duration_secs))
    }

    async fn revoke_lease(&self, lease_id: &str) -> Result<(), SecretsError> {
        // vaultrs 0.7 does not expose sys/leases endpoints directly.
        // Use the raw HTTP client to call PUT /v1/sys/leases/revoke.
        let client = self.client.read().await;
        let address = client.settings().address.clone();
        let token = client.settings().token.clone();
        let url = format!("{address}v1/sys/leases/revoke");
        let body = serde_json::json!({ "lease_id": lease_id });
        let http_resp = reqwest::Client::new()
            .put(&url)
            .header("X-Vault-Token", &token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                SecretsError::DynamicSecretError(format!("Lease revocation HTTP failed: {e}"))
            })?;
        if !http_resp.status().is_success() {
            return Err(SecretsError::DynamicSecretError(format!(
                "Lease revocation failed with status: {}",
                http_resp.status()
            )));
        }
        Ok(())
    }

    async fn transit_sign(&self, key_name: &str, data: &[u8]) -> Result<String, SecretsError> {
        let client = self.client.read().await;
        let encoded_data = base64::engine::general_purpose::STANDARD.encode(data);
        let resp = vaultrs::transit::data::sign(&*client, "transit", key_name, &encoded_data, None)
            .await
            .map_err(|e| SecretsError::TransitError(format!("Sign failed: {e}")))?;
        Ok(resp.signature)
    }

    async fn transit_verify(
        &self,
        key_name: &str,
        data: &[u8],
        signature: &str,
    ) -> Result<bool, SecretsError> {
        use vaultrs::api::transit::requests::VerifySignedDataRequestBuilder;
        let client = self.client.read().await;
        let encoded_data = base64::engine::general_purpose::STANDARD.encode(data);
        let mut opts = VerifySignedDataRequestBuilder::default();
        opts.signature(signature);
        let resp = vaultrs::transit::data::verify(
            &*client,
            "transit",
            key_name,
            &encoded_data,
            Some(&mut opts),
        )
        .await
        .map_err(|e| SecretsError::TransitError(format!("Verify failed: {e}")))?;
        Ok(resp.valid)
    }

    async fn transit_encrypt(
        &self,
        key_name: &str,
        plaintext: &[u8],
    ) -> Result<String, SecretsError> {
        let client = self.client.read().await;
        let encoded_data = base64::engine::general_purpose::STANDARD.encode(plaintext);
        let resp =
            vaultrs::transit::data::encrypt(&*client, "transit", key_name, &encoded_data, None)
                .await
                .map_err(|e| SecretsError::TransitError(format!("Encrypt failed: {e}")))?;
        Ok(resp.ciphertext)
    }

    async fn transit_decrypt(
        &self,
        key_name: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>, SecretsError> {
        let client = self.client.read().await;
        let resp = vaultrs::transit::data::decrypt(&*client, "transit", key_name, ciphertext, None)
            .await
            .map_err(|e| SecretsError::TransitError(format!("Decrypt failed: {e}")))?;
        base64::engine::general_purpose::STANDARD
            .decode(&resp.plaintext)
            .map_err(|e| SecretsError::TransitError(format!("Base64 decode failed: {e}")))
    }
}

// ---------------------------------------------------------------------------
// MockSecretStore — in-memory implementation for unit tests
// ---------------------------------------------------------------------------

/// In-memory mock for `SecretStore` used in unit tests (ADR-034 §Consequences).
///
/// Stores KV data in a `HashMap`, returns deterministic results for transit
/// operations, and generates synthetic dynamic secrets.
pub struct MockSecretStore {
    kv_store: RwLock<HashMap<String, HashMap<String, SensitiveString>>>,
}

impl MockSecretStore {
    pub fn new() -> Self {
        Self {
            kv_store: RwLock::new(HashMap::new()),
        }
    }

    fn kv_key(engine: &str, path: &str) -> String {
        format!("{engine}/{path}")
    }
}

impl Default for MockSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for MockSecretStore {
    async fn read(
        &self,
        engine: &str,
        path: &str,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
        let store = self.kv_store.read().await;
        store
            .get(&Self::kv_key(engine, path))
            .cloned()
            .ok_or_else(|| SecretsError::SecretNotFound {
                path: format!("{engine}/{path}"),
            })
    }

    async fn write(
        &self,
        engine: &str,
        path: &str,
        secret: HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError> {
        let mut store = self.kv_store.write().await;
        store.insert(Self::kv_key(engine, path), secret);
        Ok(())
    }

    async fn generate_dynamic(
        &self,
        engine: &str,
        role: &str,
    ) -> Result<DomainDynamicSecret, SecretsError> {
        Ok(DomainDynamicSecret {
            lease_id: format!("{engine}/creds/{role}/lease-mock-001"),
            values: HashMap::from([
                (
                    "username".to_string(),
                    SensitiveString::new(format!("v-mock-{role}-abc123")),
                ),
                (
                    "password".to_string(),
                    SensitiveString::new("mock-dynamic-password"),
                ),
            ]),
            lease_duration: Duration::from_secs(300),
            renewable: true,
            created_at: Instant::now(),
        })
    }

    async fn renew_lease(
        &self,
        _lease_id: &str,
        increment: Duration,
    ) -> Result<Duration, SecretsError> {
        Ok(increment)
    }

    async fn revoke_lease(&self, _lease_id: &str) -> Result<(), SecretsError> {
        Ok(())
    }

    async fn transit_sign(&self, key_name: &str, data: &[u8]) -> Result<String, SecretsError> {
        // Deterministic mock signature: "secret:v1:<key_name>:<base64(data)>"
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        Ok(format!("secret:v1:{key_name}:{encoded}"))
    }

    async fn transit_verify(
        &self,
        key_name: &str,
        data: &[u8],
        signature: &str,
    ) -> Result<bool, SecretsError> {
        let expected = self.transit_sign(key_name, data).await?;
        Ok(signature == expected)
    }

    async fn transit_encrypt(
        &self,
        key_name: &str,
        plaintext: &[u8],
    ) -> Result<String, SecretsError> {
        // Deterministic mock ciphertext: "secret:v1:<key_name>:<base64(plaintext)>"
        let encoded = base64::engine::general_purpose::STANDARD.encode(plaintext);
        Ok(format!("secret:v1:{key_name}:{encoded}"))
    }

    async fn transit_decrypt(
        &self,
        key_name: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>, SecretsError> {
        let prefix = format!("secret:v1:{key_name}:");
        let encoded = ciphertext.strip_prefix(&prefix).ok_or_else(|| {
            SecretsError::TransitError(format!(
                "Invalid mock ciphertext format (expected prefix '{prefix}')"
            ))
        })?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| SecretsError::TransitError(format!("Base64 decode failed: {e}")))
    }
}

// ---------------------------------------------------------------------------
// SecretsManager — Application-layer facade (Keymaster pattern)
// ---------------------------------------------------------------------------

/// Total capacity of the read-through LRU cache inside `SecretsManager`.
const CACHE_CAPACITY: usize = 128;

/// How long a cached KV secret is considered fresh before a live read is issued.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// LRU cache mapping `"engine/path"` → `(secret values, population Instant)`.
type SecretsCache = LruCache<String, (HashMap<String, SensitiveString>, Instant)>;

/// The main entry point for the Application layer to interact with secrets,
/// following the Keymaster architectural pattern (ADR-034).
///
/// Only the orchestrator accesses the secret store; agents never communicate
/// with OpenBao directly. This struct wraps a `SecretStore` implementation
/// and provides convenience methods including:
/// - a 30-second read-through LRU cache (capacity 128) to reduce vault pressure
/// - structured `AccessContext` on every call for the audit trail
/// - `SecretEvent` publishing to the `EventBus` after each operation
pub struct SecretsManager {
    store: Arc<dyn SecretStore>,
    event_bus: Arc<EventBus>,
    cache: Arc<Mutex<SecretsCache>>,
}

impl SecretsManager {
    /// Creates a new `SecretsManager` backed by a live OpenBao connection.
    pub async fn from_config(
        config: &SecretBackendConfig,
        event_bus: Arc<EventBus>,
    ) -> Result<Self, SecretsError> {
        let store = OpenBaoSecretStore::new(config.clone()).await?;
        Ok(Self {
            store: Arc::new(store),
            event_bus,
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(CACHE_CAPACITY).unwrap(),
            ))),
        })
    }

    /// Creates a `SecretsManager` wrapping an arbitrary `SecretStore`.
    /// Used in tests via `MockSecretStore` and in dev/CI via a no-op store.
    pub fn from_store(store: Arc<dyn SecretStore>, event_bus: Arc<EventBus>) -> Self {
        Self {
            store,
            event_bus,
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(CACHE_CAPACITY).unwrap(),
            ))),
        }
    }

    fn cache_key(engine: &str, path: &str) -> String {
        format!("{engine}/{path}")
    }

    /// Read a secret from the KV engine, served from cache when fresh.
    pub async fn read_secret(
        &self,
        engine: &str,
        path: &str,
        context: &AccessContext,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError> {
        let key = Self::cache_key(engine, path);

        // Cache hit
        {
            let mut cache = self.cache.lock().await;
            if let Some((values, populated_at)) = cache.get(&key) {
                if populated_at.elapsed() < CACHE_TTL {
                    return Ok(values.clone());
                }
            }
        }

        // Cache miss — live read
        let result = self.store.read(engine, path).await;

        match &result {
            Ok(values) => {
                {
                    let mut cache = self.cache.lock().await;
                    cache.put(key, (values.clone(), Instant::now()));
                }
                self.event_bus
                    .publish_secret_event(SecretEvent::SecretRetrieved {
                        engine: engine.to_string(),
                        path: path.to_string(),
                        access_context: context.clone(),
                        retrieved_at: Utc::now(),
                    });
            }
            Err(e) => {
                self.event_bus
                    .publish_secret_event(SecretEvent::SecretAccessDenied {
                        engine: Some(engine.to_string()),
                        path: Some(path.to_string()),
                        reason: e.to_string(),
                        access_context: context.clone(),
                        denied_at: Utc::now(),
                    });
            }
        }

        result
    }

    /// Read a single field from a KV engine path.
    pub async fn read_secret_field(
        &self,
        engine: &str,
        path: &str,
        field: &str,
        context: &AccessContext,
    ) -> Result<SensitiveString, SecretsError> {
        let mut secret = self.read_secret(engine, path, context).await?;
        secret
            .remove(field)
            .ok_or_else(|| SecretsError::SecretNotFound {
                path: format!("{engine}/{path}#{field}"),
            })
    }

    /// Write a secret to the KV engine and invalidate the cache entry.
    pub async fn write_secret(
        &self,
        engine: &str,
        path: &str,
        secret: HashMap<String, SensitiveString>,
        context: &AccessContext,
    ) -> Result<(), SecretsError> {
        let result = self.store.write(engine, path, secret).await;
        // Invalidate cache regardless of success/failure
        self.cache.lock().await.pop(&Self::cache_key(engine, path));

        match &result {
            Ok(()) => {
                self.event_bus
                    .publish_secret_event(SecretEvent::SecretWritten {
                        engine: engine.to_string(),
                        path: path.to_string(),
                        access_context: context.clone(),
                        written_at: Utc::now(),
                    });
            }
            Err(e) => {
                self.event_bus
                    .publish_secret_event(SecretEvent::SecretAccessDenied {
                        engine: Some(engine.to_string()),
                        path: Some(path.to_string()),
                        reason: e.to_string(),
                        access_context: context.clone(),
                        denied_at: Utc::now(),
                    });
            }
        }

        result
    }

    /// Generate short-lived credentials from a dynamic engine (e.g. database/PKI).
    pub async fn generate_dynamic_secret(
        &self,
        engine: &str,
        role: &str,
        context: &AccessContext,
    ) -> Result<DomainDynamicSecret, SecretsError> {
        let result = self.store.generate_dynamic(engine, role).await;

        match &result {
            Ok(raw) => {
                self.event_bus
                    .publish_secret_event(SecretEvent::DynamicSecretGenerated {
                        engine: engine.to_string(),
                        role: role.to_string(),
                        lease_id: raw.lease_id.clone(),
                        lease_duration_secs: raw.lease_duration.as_secs(),
                        access_context: context.clone(),
                        generated_at: Utc::now(),
                    });
                Ok(raw.clone())
            }
            Err(e) => {
                self.event_bus
                    .publish_secret_event(SecretEvent::SecretAccessDenied {
                        engine: Some(engine.to_string()),
                        path: Some(format!("creds/{role}")),
                        reason: e.to_string(),
                        access_context: context.clone(),
                        denied_at: Utc::now(),
                    });
                Err(result.unwrap_err())
            }
        }
    }

    /// Renew a dynamic secret lease.
    pub async fn renew_lease(
        &self,
        lease_id: &str,
        increment: Duration,
        context: &AccessContext,
    ) -> Result<Duration, SecretsError> {
        let result = self.store.renew_lease(lease_id, increment).await;
        if let Ok(new_duration) = &result {
            self.event_bus
                .publish_secret_event(SecretEvent::LeaseRenewed {
                    lease_id: lease_id.to_string(),
                    new_duration_secs: new_duration.as_secs(),
                    access_context: context.clone(),
                    renewed_at: Utc::now(),
                });
        }
        result
    }

    /// Revoke a dynamic secret lease early.
    pub async fn revoke_lease(
        &self,
        lease_id: &str,
        context: &AccessContext,
    ) -> Result<(), SecretsError> {
        let result = self.store.revoke_lease(lease_id).await;
        if result.is_ok() {
            self.event_bus
                .publish_secret_event(SecretEvent::LeaseRevoked {
                    lease_id: lease_id.to_string(),
                    access_context: context.clone(),
                    revoked_at: Utc::now(),
                });
        }
        result
    }

    /// Sign data using OpenBao Transit Engine.
    pub async fn sign(&self, key_name: &str, data: &[u8]) -> Result<String, SecretsError> {
        self.store.transit_sign(key_name, data).await
    }

    /// Verify a signature using OpenBao Transit Engine.
    pub async fn verify(
        &self,
        key_name: &str,
        data: &[u8],
        signature: &str,
    ) -> Result<bool, SecretsError> {
        self.store.transit_verify(key_name, data, signature).await
    }

    /// Encrypt data using OpenBao Transit Engine.
    pub async fn encrypt(&self, key_name: &str, plaintext: &[u8]) -> Result<String, SecretsError> {
        self.store.transit_encrypt(key_name, plaintext).await
    }

    /// Decrypt data using OpenBao Transit Engine.
    pub async fn decrypt(&self, key_name: &str, ciphertext: &str) -> Result<Vec<u8>, SecretsError> {
        self.store.transit_decrypt(key_name, ciphertext).await
    }

    /// Resolve a [`CredentialRef`] to its sensitive value.
    ///
    /// Dispatches to the appropriate backend:
    /// - `CredentialStoreType::Environment` → reads from `std::env::var`
    /// - `CredentialStoreType::SecretStore` → reads from KV engine via the store
    ///
    /// The `key` field encodes the lookup coordinates:
    /// - Environment: raw env-var name (e.g. `"GMAIL_TOKEN"`)
    /// - OpenBao: `"secret:engine/path#field"` or `"secret:engine/path"`
    pub async fn resolve_credential(
        &self,
        credential: &CredentialRef,
        context: &AccessContext,
    ) -> Result<SensitiveString, SecretsError> {
        match credential.store_type {
            CredentialStoreType::Environment => std::env::var(&credential.key)
                .map(SensitiveString::new)
                .map_err(|_| {
                    SecretsError::CredentialResolutionError(format!(
                        "Environment variable '{}' not set",
                        credential.key
                    ))
                }),
            CredentialStoreType::SecretStore => {
                // Parse "secret:engine/path" or "secret:engine/path#field"
                let raw = credential
                    .key
                    .strip_prefix("secret:")
                    .unwrap_or(&credential.key);

                let (kv_path, field) = if let Some((path, f)) = raw.rsplit_once('#') {
                    (path, Some(f))
                } else {
                    (raw, None)
                };

                let (engine, path) = kv_path.split_once('/').ok_or_else(|| {
                    SecretsError::InvalidPath(format!(
                        "OpenBao credential key must contain at least engine/path: '{raw}'"
                    ))
                })?;

                if let Some(field) = field {
                    self.read_secret_field(engine, path, field, context).await
                } else {
                    let secret = self.read_secret(engine, path, context).await?;
                    secret
                        .into_values()
                        .next()
                        .ok_or_else(|| SecretsError::SecretNotFound {
                            path: kv_path.to_string(),
                        })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_store() -> Arc<MockSecretStore> {
        Arc::new(MockSecretStore::new())
    }

    fn mock_event_bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(16))
    }

    fn mock_manager() -> SecretsManager {
        SecretsManager::from_store(mock_store(), mock_event_bus())
    }

    fn test_context() -> AccessContext {
        AccessContext::system("test-orchestrator")
    }

    fn sensitive_map(pairs: &[(&str, &str)]) -> HashMap<String, SensitiveString> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), SensitiveString::new(v.to_string())))
            .collect()
    }

    // -- MockSecretStore KV tests --

    #[tokio::test]
    async fn test_mock_secret_store_read_write() {
        let store = MockSecretStore::new();
        let data = sensitive_map(&[("api_key", "sk-abc123"), ("region", "us-east-1")]);

        store.write("kv", "openai", data.clone()).await.unwrap();
        let result = store.read("kv", "openai").await.unwrap();
        assert_eq!(result["api_key"].expose(), data["api_key"].expose());
        assert_eq!(result["region"].expose(), data["region"].expose());
    }

    #[tokio::test]
    async fn test_mock_secret_store_read_not_found() {
        let store = MockSecretStore::new();
        let result = store.read("kv", "nonexistent").await;
        assert!(matches!(result, Err(SecretsError::SecretNotFound { .. })));
    }

    // -- MockSecretStore Transit tests --

    #[tokio::test]
    async fn test_mock_secret_store_transit_sign_verify() {
        let store = MockSecretStore::new();
        let data = b"agent output payload";
        let signature = store.transit_sign("agent-identity", data).await.unwrap();

        // Valid signature should verify
        let valid = store
            .transit_verify("agent-identity", data, &signature)
            .await
            .unwrap();
        assert!(valid);

        // Tampered data should not verify
        let tampered = store
            .transit_verify("agent-identity", b"tampered", &signature)
            .await
            .unwrap();
        assert!(!tampered);
    }

    #[tokio::test]
    async fn test_mock_secret_store_transit_encrypt_decrypt() {
        let store = MockSecretStore::new();
        let plaintext = b"workflow blackboard state with PII";

        let ciphertext = store
            .transit_encrypt("workflow-state-key", plaintext)
            .await
            .unwrap();

        let decrypted = store
            .transit_decrypt("workflow-state-key", &ciphertext)
            .await
            .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn test_mock_secret_store_transit_decrypt_wrong_key() {
        let store = MockSecretStore::new();
        let plaintext = b"data";

        let ciphertext = store.transit_encrypt("key-a", plaintext).await.unwrap();

        let result = store.transit_decrypt("key-b", &ciphertext).await;
        assert!(matches!(result, Err(SecretsError::TransitError(_))));
    }

    // -- MockSecretStore Dynamic Secrets tests --

    #[tokio::test]
    async fn test_mock_secret_store_dynamic_secret() {
        let store = MockSecretStore::new();
        let creds = store
            .generate_dynamic("database", "agent-readonly")
            .await
            .unwrap();

        assert!(creds.lease_id.contains("agent-readonly"));
        assert_eq!(creds.lease_duration, Duration::from_secs(300));
        assert!(creds.renewable);
        assert!(creds.values.contains_key("username"));
        assert!(creds.values.contains_key("password"));
    }

    #[tokio::test]
    async fn test_mock_secret_store_lease_lifecycle() {
        let store = MockSecretStore::new();

        // Renew returns the requested increment
        let renewed = store
            .renew_lease("some-lease", Duration::from_secs(600))
            .await
            .unwrap();
        assert_eq!(renewed, Duration::from_secs(600));

        // Revoke succeeds
        store.revoke_lease("some-lease").await.unwrap();
    }

    // -- SecretsManager facade tests --

    #[tokio::test]
    async fn test_secrets_manager_read_secret_field() {
        let manager = mock_manager();
        let ctx = test_context();
        let data = sensitive_map(&[("oauth_token", "ya29.mock"), ("client_id", "123.apps")]);

        manager
            .store
            .write("kv", "mcp-tools/gmail", data)
            .await
            .unwrap();

        let token = manager
            .read_secret_field("kv", "mcp-tools/gmail", "oauth_token", &ctx)
            .await
            .unwrap();
        assert_eq!(token.expose(), "ya29.mock");

        // Missing field returns error
        let result = manager
            .read_secret_field("kv", "mcp-tools/gmail", "nonexistent", &ctx)
            .await;
        assert!(matches!(result, Err(SecretsError::SecretNotFound { .. })));
    }

    #[tokio::test]
    async fn test_secrets_manager_sign_verify() {
        let manager = mock_manager();
        let data = b"code output";

        let sig = manager.sign("agent-identity", data).await.unwrap();
        let valid = manager.verify("agent-identity", data, &sig).await.unwrap();
        assert!(valid);
    }

    #[tokio::test]
    async fn test_secrets_manager_encrypt_decrypt() {
        let manager = mock_manager();
        let plaintext = b"sensitive blackboard data";

        let ct = manager
            .encrypt("workflow-state-key", plaintext)
            .await
            .unwrap();
        let pt = manager.decrypt("workflow-state-key", &ct).await.unwrap();
        assert_eq!(pt, plaintext);
    }

    #[tokio::test]
    async fn test_secrets_manager_read_cache() {
        let manager = mock_manager();
        let ctx = test_context();
        let data = sensitive_map(&[("key", "value-123")]);
        manager
            .store
            .write("kv", "cached/path", data)
            .await
            .unwrap();

        // First read hits store
        let first = manager
            .read_secret("kv", "cached/path", &ctx)
            .await
            .unwrap();
        assert_eq!(first["key"].expose(), "value-123");

        // Second read within TTL returns same value from cache
        let second = manager
            .read_secret("kv", "cached/path", &ctx)
            .await
            .unwrap();
        assert_eq!(second["key"].expose(), "value-123");
    }

    #[tokio::test]
    async fn test_secrets_manager_write_invalidates_cache() {
        let manager = mock_manager();
        let ctx = test_context();

        let initial = sensitive_map(&[("secret", "original")]);
        manager.store.write("kv", "my/path", initial).await.unwrap();
        // warm the cache
        manager.read_secret("kv", "my/path", &ctx).await.unwrap();

        // write via facade should invalidate cache
        let updated = sensitive_map(&[("secret", "updated")]);
        manager
            .write_secret("kv", "my/path", updated, &ctx)
            .await
            .unwrap();

        // next read should go to store and get the updated value
        let result = manager.read_secret("kv", "my/path", &ctx).await.unwrap();
        assert_eq!(result["secret"].expose(), "updated");
    }

    #[tokio::test]
    async fn test_secrets_manager_dynamic_secret() {
        let manager = mock_manager();
        let ctx = test_context();

        let domain = manager
            .generate_dynamic_secret("database", "agent-readonly", &ctx)
            .await
            .unwrap();
        assert!(domain.lease_id.contains("agent-readonly"));
        assert!(domain.values.contains_key("username"));
        assert!(domain.values.contains_key("password"));
        assert!(!domain.is_expired());
    }

    // -- Credential Resolution tests --

    #[tokio::test]
    async fn test_credential_ref_resolution_env() {
        let manager = mock_manager();
        let ctx = test_context();
        std::env::set_var("AEGIS_TEST_SECRET_XYZ", "test-value-123");

        let cred = CredentialRef {
            store_type: CredentialStoreType::Environment,
            key: "AEGIS_TEST_SECRET_XYZ".to_string(),
        };
        let value = manager.resolve_credential(&cred, &ctx).await.unwrap();
        assert_eq!(value.expose(), "test-value-123");

        std::env::remove_var("AEGIS_TEST_SECRET_XYZ");
    }

    #[tokio::test]
    async fn test_credential_ref_resolution_env_missing() {
        let manager = mock_manager();
        let ctx = test_context();
        let cred = CredentialRef {
            store_type: CredentialStoreType::Environment,
            key: "AEGIS_NONEXISTENT_VAR_Z9X8".to_string(),
        };
        let result = manager.resolve_credential(&cred, &ctx).await;
        assert!(matches!(
            result,
            Err(SecretsError::CredentialResolutionError(_))
        ));
    }

    #[tokio::test]
    async fn test_credential_ref_resolution_openbao() {
        let manager = mock_manager();
        let ctx = test_context();

        let data = sensitive_map(&[("oauth_token", "ya29.vault-token")]);
        manager
            .store
            .write("kv", "mcp-tools/gmail", data)
            .await
            .unwrap();

        let cred = CredentialRef {
            store_type: CredentialStoreType::SecretStore,
            key: "secret:kv/mcp-tools/gmail#oauth_token".to_string(),
        };
        let value = manager.resolve_credential(&cred, &ctx).await.unwrap();
        assert_eq!(value.expose(), "ya29.vault-token");
    }

    #[tokio::test]
    async fn test_credential_ref_resolution_openbao_no_field() {
        let manager = mock_manager();
        let ctx = test_context();

        let data = sensitive_map(&[("value", "single-secret")]);
        manager
            .store
            .write("kv", "simple/secret", data)
            .await
            .unwrap();

        let cred = CredentialRef {
            store_type: CredentialStoreType::SecretStore,
            key: "secret:kv/simple/secret".to_string(),
        };
        let value = manager.resolve_credential(&cred, &ctx).await.unwrap();
        assert_eq!(value.expose(), "single-secret");
    }

    #[tokio::test]
    async fn test_credential_ref_resolution_openbao_invalid_path() {
        let manager = mock_manager();
        let ctx = test_context();
        let cred = CredentialRef {
            store_type: CredentialStoreType::SecretStore,
            key: "secret:no-slash-here".to_string(),
        };
        let result = manager.resolve_credential(&cred, &ctx).await;
        assert!(matches!(result, Err(SecretsError::InvalidPath(_))));
    }
}
