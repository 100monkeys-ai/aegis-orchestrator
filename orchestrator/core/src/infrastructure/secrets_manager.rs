// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Secrets Manager Stub (BC-11, ADR-034)
//!
//! ⚠️ Phase 4 stub — **not yet implemented.**
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
//! ## Panics
//!
//! **Every method on [`SecretsManager`] panics** with `todo!()`. Do not call
//! any method until Phase 4 is implemented.
//!
//! See ADR-034 (OpenBao Secrets Management), AGENTS.md §BC-11.

// ============================================================================
// ADR-034: OpenBao for Secrets Management (DEFERRED to Phase 4)
// ============================================================================
// Purpose: Centralized secrets storage, dynamic credential generation, encryption-as-a-service
// Current Status: Not implemented - Phase 1 uses environment variables only
// 
// Phase 1 Workaround:
// - Credentials passed as environment variables to agent containers
// - No encryption at rest
// - No dynamic secret generation
// - No audit trail
//
// Phase 4 Implementation Plan:
// 1. OpenBao Deployment: HA setup with Raft backend
// 2. Orchestrator Integration: AppRole authentication (Role ID + Secret ID)
// 3. Dynamic Secrets: Database credentials with auto-rotation
// 4. Transit Engine: Encryption-as-a-service for signing/encryption
// 5. Keymaster Pattern: Only orchestrator accesses vault, agents never do
//
// See: adrs/034-openbao-secrets-management.md
// ============================================================================

use anyhow::Result;
use async_trait::async_trait;

/// ⚠️ Phase 4 stub — placeholder for ADR-034 OpenBao integration.
///
/// # Panics
///
/// `SecretsManager::new()` and all instance methods unconditionally panic
/// with `todo!()`. Do not construct this type until Phase 4 is implemented.
pub struct SecretsManager {
    // TODO: VaultClient connection (ADR-034)
}

impl SecretsManager {
    pub fn new() -> Self {
        // TODO: Initialize OpenBao client
        todo!("ADR-034: OpenBao secrets integration not yet implemented")
    }

    /// Read a secret from OpenBao
    pub async fn read_secret(&self, path: &str) -> Result<String> {
        // TODO: Implement secret read with caching
        todo!("ADR-034: Secret read not implemented")
    }

    /// Generate a dynamic secret (e.g., database credentials with TTL)
    pub async fn generate_dynamic_secret(&self, engine: &str, role: &str) -> Result<String> {
        // TODO: Implement dynamic secret generation with lease management
        todo!("ADR-034: Dynamic secret generation not implemented")
    }

    /// Sign data using OpenBao Transit Engine
    pub async fn sign(&self, key_name: &str, data: &[u8]) -> Result<String> {
        // TODO: Implement signing for SMCP/non-repudiation
        todo!("ADR-034: Transit engine signing not implemented")
    }

    /// Encrypt data using OpenBao Transit Engine
    pub async fn encrypt(&self, key_name: &str, plaintext: &[u8]) -> Result<String> {
        // TODO: Implement encryption for at-rest storage
        todo!("ADR-034: Transit engine encryption not implemented")
    }
}
