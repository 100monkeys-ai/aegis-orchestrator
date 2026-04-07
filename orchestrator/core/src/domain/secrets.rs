// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Secrets Domain Types (BC-11, ADR-034)
//!
//! Domain-layer value objects and entities for the Secrets & Identity Management
//! bounded context. See AGENTS.md §BC-11 and ADR-034.
//!
//! ## Key Types
//!
//! | Type | Role |
//! |------|------|
//! | [`SensitiveString`] | Credential wrapper that redacts itself in `Debug`/`Display` |
//! | [`SecretPath`] | Namespace-aware structured path value object |
//! | [`AccessContext`] | Audit metadata for every secret access operation |
//! | [`DomainDynamicSecret`] | Short-lived credential entity with TTL lifecycle methods |

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::shared_kernel::TenantId;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use thiserror::Error;

// ---------------------------------------------------------------------------
// SensitiveString — credential wrapper that prevents accidental logging
// ---------------------------------------------------------------------------

/// A `String` wrapper that prevents accidental credential exposure in logs and
/// error messages.
///
/// Both `Debug` and `Display` emit `[REDACTED]` regardless of the inner value.
/// Call [`SensitiveString::expose`] only at intentional, audited injection
/// points (e.g. env-var injection into an MCP server process).
///
/// ## Design Rationale
///
/// Named `expose()` rather than implementing `Deref<Target = str>` to make
/// credential access sites visually obvious during code review. Any call to
/// `.expose()` is an intentional act that reviewers can grep for.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SensitiveString(String);

impl SensitiveString {
    /// Construct a new `SensitiveString`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Access the inner value at an intentional, audited injection point.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Consume `self` and return the inner `String` at an intentional injection point.
    pub fn expose_owned(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for SensitiveString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

impl std::fmt::Display for SensitiveString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

// ---------------------------------------------------------------------------
// SecretPath — namespace-aware path value object (ADR-034 §SecretPath)
// ---------------------------------------------------------------------------

/// Namespace-aware, structured identifier for a secret location in the
/// orchestrator's configured secret backend.
///
/// Encodes `{namespace}/{mount_point}/{path}` as a validated value object.
/// Use [`SecretPath::full_path`] to get the canonical string representation.
///
/// ## Per-Tenant Namespace Routing (ADR-056 §Wave 3)
///
/// When `tenant_id` is `Some(tid)` and `!tid.is_system()`, the effective KV engine
/// mount is prefixed with `tenant-{slug}/` to route the operation into the tenant's
/// dedicated OpenBao namespace. System-tier secrets (`tenant_id` is `None` or
/// `is_system()`) continue to use the global namespace from the startup config.
///
/// Use [`SecretPath::effective_mount`] to obtain the correctly-routed engine mount
/// string when calling the secret store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecretPath {
    /// Backend namespace (e.g. `"aegis-system"`, `"tenant-acme"`).
    pub namespace: String,
    /// Engine mount point (e.g. `"kv"`, `"transit"`).
    pub mount_point: String,
    /// Path within the mount (e.g. `"mcp-tools/gmail"`).
    pub path: String,
    /// Optional tenant owning this secret. When set to a non-system tenant,
    /// read/write operations are routed to the tenant's OpenBao namespace.
    pub tenant_id: Option<TenantId>,
}

impl SecretPath {
    /// Construct a `SecretPath` without tenant routing (system / global secrets).
    pub fn new(
        namespace: impl Into<String>,
        mount_point: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            mount_point: mount_point.into(),
            path: path.into(),
            tenant_id: None,
        }
    }

    /// Construct a `SecretPath` scoped to a specific tenant (ADR-056).
    pub fn for_tenant(
        tenant_id: TenantId,
        mount_point: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        let namespace = if tenant_id.is_system() {
            "aegis-system".to_string()
        } else {
            format!("tenant-{}", tenant_id.as_str())
        };
        Self {
            namespace: namespace.clone(),
            mount_point: mount_point.into(),
            path: path.into(),
            tenant_id: Some(tenant_id),
        }
    }

    /// Returns the engine mount string to pass to the secret store.
    ///
    /// When the path carries a non-system tenant, the mount is prefixed with
    /// `tenant-{slug}/` so that the OpenBao path-based namespace routing
    /// directs the operation into the tenant's isolated namespace.
    ///
    /// System or unscoped paths return the bare `mount_point`.
    pub fn effective_mount(&self) -> String {
        match &self.tenant_id {
            Some(tid) if !tid.is_system() => {
                format!("tenant-{}/{}", tid.as_str(), self.mount_point)
            }
            _ => self.mount_point.clone(),
        }
    }

    /// Returns the fully-qualified canonical path: `namespace/mount_point/path`.
    pub fn full_path(&self) -> String {
        format!("{}/{}/{}", self.namespace, self.mount_point, self.path)
    }
}

impl std::fmt::Display for SecretPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.full_path())
    }
}

// ---------------------------------------------------------------------------
// AccessContext — audit metadata for every secret access (ADR-034 §AccessContext)
// ---------------------------------------------------------------------------

/// Audit metadata attached to every secret access call.
///
/// Provides the who/when/why columns required for compliance audit trails
/// (ADR-034 §Consequences → SOC 2 / HIPAA / GDPR).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessContext {
    /// Orchestrator node identifier.
    pub orchestrator_id: String,
    /// Optional: execution that triggered this access.
    pub execution_id: Option<ExecutionId>,
    /// Optional: agent on whose behalf the access is made.
    pub agent_id: Option<AgentId>,
    /// Wall-clock timestamp at which the access was initiated.
    pub requested_at: DateTime<Utc>,
}

impl AccessContext {
    /// Create an `AccessContext` for a specific agent execution.
    pub fn for_execution(
        orchestrator_id: impl Into<String>,
        execution_id: ExecutionId,
        agent_id: AgentId,
    ) -> Self {
        Self {
            orchestrator_id: orchestrator_id.into(),
            execution_id: Some(execution_id),
            agent_id: Some(agent_id),
            requested_at: Utc::now(),
        }
    }

    /// Create an `AccessContext` for orchestrator-level system access (no agent context).
    pub fn system(orchestrator_id: impl Into<String>) -> Self {
        Self {
            orchestrator_id: orchestrator_id.into(),
            execution_id: None,
            agent_id: None,
            requested_at: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// DomainDynamicSecret — dynamic credential entity with TTL lifecycle
// ---------------------------------------------------------------------------

/// A short-lived credential generated by the configured dynamic secrets engine.
///
/// Carries [`SensitiveString`]-wrapped values and provides TTL lifecycle
/// methods ([`DomainDynamicSecret::is_expired`], [`DomainDynamicSecret::remaining_ttl`])
/// so callers can decide whether to renew before use.
///
/// See ADR-034 §Dynamic Secrets and AGENTS.md §BC-11.
#[derive(Debug, Clone)]
pub struct DomainDynamicSecret {
    /// Backend lease identifier (used for renewal and revocation).
    pub lease_id: String,
    /// Credential key-value pairs (e.g. `"username"` / `"password"`).
    /// Values are wrapped in [`SensitiveString`] to prevent accidental logging.
    pub values: HashMap<String, SensitiveString>,
    /// Duration granted by the secret backend for this lease.
    pub lease_duration: Duration,
    /// Whether the lease is eligible for renewal.
    pub renewable: bool,
    /// Monotonic clock time at which this secret was created locally.
    pub created_at: Instant,
}

// ---------------------------------------------------------------------------
// SecretStore Port + Error
// ---------------------------------------------------------------------------

/// Errors for Secrets & Identity operations (BC-11).
#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("Secret not found: {path}")]
    SecretNotFound { path: String },

    #[error("Secret store connection error: {0}")]
    ConnectionError(String),

    #[error("Invalid secret path: {0}")]
    InvalidPath(String),

    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    #[error("Dynamic secret error: {0}")]
    DynamicSecretError(String),

    #[error("Transit operation error: {0}")]
    TransitError(String),

    #[error("Credential resolution error: {0}")]
    CredentialResolutionError(String),
}

/// Domain/application-owned secret storage abstraction (ADR-034).
///
/// Implementations in infrastructure map these calls to concrete backends
/// such as a vault adapter or in-memory test doubles.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn read(
        &self,
        engine: &str,
        path: &str,
    ) -> Result<HashMap<String, SensitiveString>, SecretsError>;

    async fn write(
        &self,
        engine: &str,
        path: &str,
        secret: HashMap<String, SensitiveString>,
    ) -> Result<(), SecretsError>;

    async fn generate_dynamic(
        &self,
        engine: &str,
        role: &str,
    ) -> Result<DomainDynamicSecret, SecretsError>;

    async fn renew_lease(
        &self,
        lease_id: &str,
        increment: Duration,
    ) -> Result<Duration, SecretsError>;

    async fn revoke_lease(&self, lease_id: &str) -> Result<(), SecretsError>;

    async fn transit_sign(&self, key_name: &str, data: &[u8]) -> Result<String, SecretsError>;

    async fn transit_verify(
        &self,
        key_name: &str,
        data: &[u8],
        signature: &str,
    ) -> Result<bool, SecretsError>;

    async fn transit_encrypt(
        &self,
        key_name: &str,
        plaintext: &[u8],
    ) -> Result<String, SecretsError>;

    async fn transit_decrypt(
        &self,
        key_name: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>, SecretsError>;

    /// Create an OpenBao namespace for tenant isolation (ADR-056).
    ///
    /// Default implementation is a no-op (test/dev stores do not model namespaces).
    /// The production `OpenBaoSecretStore` overrides this to POST to `/v1/sys/namespaces/{name}`.
    async fn create_namespace(&self, name: &str) -> Result<(), SecretsError> {
        let _ = name;
        Ok(())
    }

    /// Delete an OpenBao namespace (rollback / deprovisioning).
    ///
    /// Default implementation is a no-op.
    async fn delete_namespace(&self, name: &str) -> Result<(), SecretsError> {
        let _ = name;
        Ok(())
    }

    /// Delete a secret at `engine/path` from the KV store.
    ///
    /// Default implementation is a no-op so that test and dev stores do not need
    /// to implement deletion logic. The production `OpenBaoSecretStore` overrides
    /// this to issue the appropriate KV delete API call.
    async fn delete(&self, engine: &str, path: &str) -> Result<(), SecretsError> {
        let _ = (engine, path);
        Ok(())
    }
}

impl DomainDynamicSecret {
    /// Returns `true` if the lease TTL has elapsed since `created_at`.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= self.lease_duration
    }

    /// Returns the remaining TTL, saturating at [`Duration::ZERO`] if already expired.
    pub fn remaining_ttl(&self) -> Duration {
        let elapsed = self.created_at.elapsed();
        self.lease_duration.saturating_sub(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SensitiveString ──────────────────────────────────────────────────────

    #[test]
    fn sensitive_string_redacts_in_debug() {
        let s = SensitiveString::new("super-secret-api-key");
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(format!("{s}"), "[REDACTED]");
    }

    #[test]
    fn sensitive_string_expose_returns_value() {
        let s = SensitiveString::new("my-token");
        assert_eq!(s.expose(), "my-token");
    }

    #[test]
    fn sensitive_string_expose_owned_consumes() {
        let s = SensitiveString::new("token-xyz");
        assert_eq!(s.expose_owned(), "token-xyz");
    }

    #[test]
    fn sensitive_string_equality_compares_by_value() {
        let a = SensitiveString::new("token-abc");
        let b = SensitiveString::new("token-abc");
        let c = SensitiveString::new("token-xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // ── SecretPath ───────────────────────────────────────────────────────────

    #[test]
    fn secret_path_full_path() {
        let path = SecretPath::new("aegis-system", "kv", "mcp-tools/gmail");
        assert_eq!(path.full_path(), "aegis-system/kv/mcp-tools/gmail");
        assert_eq!(format!("{path}"), "aegis-system/kv/mcp-tools/gmail");
    }

    // ── DomainDynamicSecret ──────────────────────────────────────────────────

    #[test]
    fn domain_dynamic_secret_is_expired_after_ttl() {
        let secret = DomainDynamicSecret {
            lease_id: "lease-001".to_string(),
            values: HashMap::new(),
            lease_duration: Duration::from_millis(1),
            renewable: false,
            created_at: Instant::now(),
        };
        std::thread::sleep(Duration::from_millis(5));
        assert!(secret.is_expired());
        assert_eq!(secret.remaining_ttl(), Duration::ZERO);
    }

    #[test]
    fn domain_dynamic_secret_not_expired_when_fresh() {
        let secret = DomainDynamicSecret {
            lease_id: "lease-002".to_string(),
            values: HashMap::new(),
            lease_duration: Duration::from_secs(300),
            renewable: true,
            created_at: Instant::now(),
        };
        assert!(!secret.is_expired());
        assert!(secret.remaining_ttl() > Duration::ZERO);
    }
}
