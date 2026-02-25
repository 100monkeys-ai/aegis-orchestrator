// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Secrets Manager Stub (BC-11, ADR-034)
//!
//! ⚠️ **Phase 4 stub — not yet implemented.**
//!
//! This module is a forward-declaration placeholder for the OpenBao
//! secrets management integration described in ADR-034. In Phase 1 all
//! credentials are passed as environment variables to agent containers —
//! no encryption at rest, no dynamic secrets, no audit trail.
//!
//! ## Phase 4 Implementation Plan
//!
//! 1. **OpenBao Deployment** — HA cluster with Raft backend.
//! 2. **AppRole Auth** — orchestrator nodes authenticate with Role ID + Secret ID.
//! 3. **KV Engine** — static API keys stored at `<tenant>/kv/<name>`.
//! 4. **Dynamic Secrets** — database credentials auto-rotated per `DynamicSecret.ttl`.
//! 5. **Transit Engine** — `sign()` and `encrypt()` without key distribution.
//! 6. **Keymaster Pattern** — only the orchestrator accesses OpenBao;
//!    agents receive credentials via `spec.environment` injection, never via 
//!    direct vault calls.
//!
//! ## Error Behavior
//!
//! **All methods return `SecretsError::NotImplemented`** until Phase 4 is complete.
//! Do not call any method in production until OpenBao integration is implemented.
//!
//! See ADR-034 (OpenBao Secrets Management), AGENTS.md §BC-11.

use anyhow::Result;
use thiserror::Error;

/// Error types for secrets management operations.
#[derive(Debug, Error)]
pub enum SecretsError {
    /// OpenBao integration not yet implemented (Phase 4).
    #[error("ADR-034: OpenBao secrets integration not yet implemented (Phase 4)")]
    NotImplemented,
    
    /// Secret not found at the specified path.
    #[error("Secret not found: {path}")]
    SecretNotFound { path: String },
    
    /// Failed to communicate with OpenBao.
    #[error("OpenBao connection error: {0}")]
    ConnectionError(String),
    
    /// Invalid secret path or format.
    #[error("Invalid secret path: {0}")]
    InvalidPath(String),
}

/// ⚠️ Phase 4 stub — placeholder for ADR-034 OpenBao integration.
///
/// # Current Behavior
///
/// All methods return `SecretsError::NotImplemented` until Phase 4 OpenBao
/// integration is complete. This prevents runtime panics while clearly
/// indicating that the functionality is not yet available.
pub struct SecretsManager {
    // Phase 4: VaultClient connection will be added here
    _phantom: std::marker::PhantomData<()>,
}

impl SecretsManager {
    /// Creates a new stub instance that returns NotImplemented errors.
    ///
    /// # Phase 4
    /// This will initialize OpenBao client with AppRole authentication.
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }

    /// Read a secret from OpenBao.
    ///
    /// # Phase 4 Implementation
    /// - Query OpenBao KV engine at `<tenant>/kv/<path>`
    /// - Cache secrets in memory with TTL
    /// - Publish SecretAccessed event to audit trail
    pub async fn read_secret(&self, _path: &str) -> Result<String, SecretsError> {
        Err(SecretsError::NotImplemented)
    }

    /// Generate a dynamic secret (e.g., database credentials with TTL).
    ///
    /// # Phase 4 Implementation
    /// - Request dynamic secret from OpenBao engine (e.g., `database/creds/<role>`)
    /// - Track lease ID for renewal and revocation
    /// - Auto-refresh before expiry
    /// - Publish DynamicSecretGenerated event
    pub async fn generate_dynamic_secret(&self, _engine: &str, _role: &str) -> Result<String, SecretsError> {
        Err(SecretsError::NotImplemented)
    }

    /// Sign data using OpenBao Transit Engine.
    ///
    /// # Phase 4 Implementation
    /// - Use Transit Engine for Ed25519 signing (SMCP non-repudiation)
    /// - Never expose private keys to orchestrator
    /// - Return base64-encoded signature
    pub async fn sign(&self, _key_name: &str, _data: &[u8]) -> Result<String, SecretsError> {
        Err(SecretsError::NotImplemented)
    }

    /// Encrypt data using OpenBao Transit Engine.
    ///
    /// # Phase 4 Implementation
    /// - Use Transit Engine for at-rest encryption (Blackboard state, sensitive logs)
    /// - Return ciphertext with version prefix (e.g., `vault:v1:ciphertext`)
    /// - Support key rotation without re-encrypting all data
    pub async fn encrypt(&self, _key_name: &str, _plaintext: &[u8]) -> Result<String, SecretsError> {
        Err(SecretsError::NotImplemented)
    }
}

impl Default for SecretsManager {
    fn default() -> Self {
        Self::new()
    }
}
