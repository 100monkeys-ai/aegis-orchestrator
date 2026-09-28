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
//! | [`SensitiveString`] | Credential wrapper: `Debug` prints `[REDACTED]`; there is no `Display` |
//! | [`SensitiveUrl`] | Connection URL that prints without its user info or secret query parameters |
//! | [`SensitiveBytes`] | Key material held as bytes, redacted in `Debug` |
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
/// `Debug` emits `[REDACTED]` regardless of the inner value. There is no
/// `Display`: a secret has no display form, so `format!("{}", secret)`,
/// `secret.to_string()` and `%secret` in a log macro do not compile, and a
/// site that needs the value must call [`SensitiveString::expose`], at an
/// intentional, audited point of use (e.g. env-var injection into an MCP
/// server process).
///
/// ## Design Rationale
///
/// Named `expose()` rather than implementing `Deref<Target = str>` to make
/// credential access sites visually obvious during code review. Any call to
/// `.expose()` is an intentional act that reviewers can grep for.
///
/// Serialises and deserialises as the bare string, so a wire or storage field
/// can hold one without changing its format. In the redacted view
/// ([`to_redacted_json`]) it serialises as its reference when it is one and
/// as `[REDACTED]` otherwise. Equality is constant-time.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct SensitiveString(String);

impl Serialize for SensitiveString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if in_redacted_view() {
            serializer.serialize_str(secret_reference(&self.0).unwrap_or(REDACTED))
        } else {
            serializer.serialize_str(&self.0)
        }
    }
}

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

    /// Whether the secret equals `candidate`, compared in constant time so the
    /// time taken does not tell a caller how much of a guess was right.
    pub fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
    }

    /// Whether the secret is the empty string.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Constant-time equality of two byte strings. The lengths are compared
/// first; a length is not treated as secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

impl PartialEq for SensitiveString {
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl Eq for SensitiveString {}

impl From<String> for SensitiveString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SensitiveString {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Debug for SensitiveString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

// A secret has no display form. This fails to compile if `Display` is ever
// implemented for `SensitiveString` or `SensitiveBytes`: with it, the trait below has two
// candidate impls and the path is ambiguous (the technique behind
// `static_assertions::assert_not_impl_any!`).
const _: fn() = || {
    trait AmbiguousIfDisplay<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfDisplay<()> for T {}
    struct Invalid;
    impl<T: ?Sized + std::fmt::Display> AmbiguousIfDisplay<Invalid> for T {}
    let _ = <SensitiveString as AmbiguousIfDisplay<_>>::some_item;
    let _ = <SensitiveBytes as AmbiguousIfDisplay<_>>::some_item;
};

// ---------------------------------------------------------------------------
// SensitiveBytes — key material held as bytes
// ---------------------------------------------------------------------------

/// Key material held as bytes (an HMAC key, a seed) that must not reach logs.
///
/// The byte counterpart of [`SensitiveString`], with the same properties:
/// `Debug` prints `[REDACTED]`, [`SensitiveBytes::expose`] is the one way to
/// read the bytes, serde is transparent (the form of a `Vec<u8>`), and
/// equality is constant-time. In the redacted view ([`to_redacted_json`]) it
/// serialises as `[REDACTED]`.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct SensitiveBytes(Vec<u8>);

impl Serialize for SensitiveBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if in_redacted_view() {
            serializer.serialize_str(REDACTED)
        } else {
            self.0.serialize(serializer)
        }
    }
}

impl SensitiveBytes {
    /// Construct a new `SensitiveBytes`.
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    /// Access the bytes at an intentional, audited point of use.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl PartialEq for SensitiveBytes {
    fn eq(&self, other: &Self) -> bool {
        constant_time_eq(&self.0, &other.0)
    }
}

impl Eq for SensitiveBytes {}

impl From<Vec<u8>> for SensitiveBytes {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for SensitiveBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

// ---------------------------------------------------------------------------
// SensitiveUrl — connection URL wrapper that prints itself redacted
// ---------------------------------------------------------------------------

/// Query parameter names whose values are redacted when a URL is rendered for
/// output. A parameter is redacted when its lower-cased name *contains* any of
/// these, so `sslpassword` and `X-Api-Key` are covered.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "api_key",
    "apikey",
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "auth",
    "authorization",
    "access_token",
    "refresh_token",
    "client_secret",
    "client_id",
    "credential",
    "credentials",
    "session",
    "session_id",
    "private_key",
    "signing_key",
    "signature",
    "sig",
];

/// What [`redact_url`] prints for a value it cannot read as a URL. The value
/// itself is never printed, because a string that is not a URL may be a
/// credential put in the wrong field.
const UNPARSEABLE_URL: &str = "[unparseable-url]";

/// Renders a URL for output with its credentials removed.
///
/// Scheme, host, port and path are kept. User info, whether a password, a
/// token carried as the user name, or both, is replaced by `[REDACTED]`. The
/// value of every query parameter named in `SENSITIVE_QUERY_PARAMS` is
/// replaced by `[REDACTED]`; other parameters are kept. The fragment is
/// dropped.
///
/// A value with no `://` is read as `[user:password@]host:port` (the form of
/// a Temporal address or a bare DSN authority) and printed without the user
/// info. Anything that is neither is printed as `[unparseable-url]`.
pub fn redact_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") {
        match url::Url::parse(raw) {
            Ok(parsed) => render_redacted(&parsed, true),
            Err(_) => UNPARSEABLE_URL.to_string(),
        }
    } else {
        // Read `[user:password@]host:port` through a scheme that has an
        // authority, then print it without that scheme.
        match url::Url::parse(&format!("tcp://{raw}")) {
            Ok(parsed) if parsed.host_str().is_some() && parsed.port().is_some() => {
                render_redacted(&parsed, false)
            }
            _ => UNPARSEABLE_URL.to_string(),
        }
    }
}

fn render_redacted(parsed: &url::Url, with_scheme: bool) -> String {
    let mut out = String::new();
    if with_scheme {
        out.push_str(parsed.scheme());
        out.push_str("://");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        out.push_str("[REDACTED]@");
    }
    if let Some(host) = parsed.host_str() {
        out.push_str(host);
    }
    if let Some(port) = parsed.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    let path = parsed.path();
    if with_scheme || path != "/" {
        out.push_str(path);
    }
    let pairs: Vec<String> = parsed
        .query_pairs()
        .map(|(k, v)| {
            let key_lower = k.to_lowercase();
            if SENSITIVE_QUERY_PARAMS.iter().any(|s| key_lower.contains(s)) {
                format!("{k}=[REDACTED]")
            } else {
                format!("{k}={v}")
            }
        })
        .collect();
    if !pairs.is_empty() {
        out.push('?');
        out.push_str(&pairs.join("&"));
    }
    out
}

/// A connection URL (database DSN, service endpoint) that may carry a
/// credential in its user info or query string.
///
/// `Display` and `Debug` both print the URL through [`redact_url`], so a
/// `tracing::info!(url = %url, ..)` or a `{url:?}` on a struct holding one is
/// safe by construction. [`SensitiveUrl::expose`] returns the raw value and is
/// called only where the URL is handed to the client that connects with it.
///
/// Serialises and deserialises as the bare string, so a configuration field
/// can hold one without changing its file format. In the redacted view
/// ([`to_redacted_json`]) it serialises as its reference when it is one and
/// through [`redact_url`] otherwise.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct SensitiveUrl(String);

impl Serialize for SensitiveUrl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if in_redacted_view() {
            match secret_reference(&self.0) {
                Some(reference) => serializer.serialize_str(reference),
                None => serializer.serialize_str(&self.redacted()),
            }
        } else {
            serializer.serialize_str(&self.0)
        }
    }
}

impl SensitiveUrl {
    /// Construct a new `SensitiveUrl`.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Access the raw URL at an intentional, audited connection point.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The URL as it may appear in any output: user info removed and secret
    /// query parameters replaced.
    pub fn redacted(&self) -> String {
        redact_url(&self.0)
    }
}

impl From<String> for SensitiveUrl {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SensitiveUrl {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Debug for SensitiveUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SensitiveUrl")
            .field(&self.redacted())
            .finish()
    }
}

impl std::fmt::Display for SensitiveUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.redacted())
    }
}

// ---------------------------------------------------------------------------
// RedactedUrl — a URL as a published record carries it
// ---------------------------------------------------------------------------

/// A URL as an event, or any other record that is published, carries it.
///
/// User info is dropped when the value is made, and so are the values of
/// secret query parameters (the rule of [`redact_url`]); nothing else of the
/// URL can be held, so nothing else can be serialised. A value read back
/// through serde is made the same way. An SSH address written
/// `user@host:path` keeps `host:path`.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RedactedUrl(String);

impl RedactedUrl {
    /// Make one from a raw URL, dropping any user info.
    pub fn new(raw: &str) -> Self {
        Self(without_user_info(raw))
    }

    /// The URL as held: with no user info.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn without_user_info(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") {
        match url::Url::parse(raw) {
            Ok(mut parsed) => {
                // Clearing either cannot fail on a URL that has a host, and
                // one that has none has no user info.
                let _ = parsed.set_password(None);
                let _ = parsed.set_username("");
                redact_url(parsed.as_str())
            }
            Err(_) => UNPARSEABLE_URL.to_string(),
        }
    } else if let Some((_, rest)) = raw.rsplit_once('@') {
        rest.to_string()
    } else {
        raw.to_string()
    }
}

impl<'de> Deserialize<'de> for RedactedUrl {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::new(&raw))
    }
}

impl From<&SensitiveUrl> for RedactedUrl {
    fn from(value: &SensitiveUrl) -> Self {
        Self::new(value.expose())
    }
}

impl std::fmt::Debug for RedactedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RedactedUrl").field(&self.0).finish()
    }
}

impl std::fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// The redacted view — a value as it may be shown to someone who must not see
// a secret
// ---------------------------------------------------------------------------

/// What a literal secret is shown as in the redacted view.
const REDACTED: &str = "[REDACTED]";

thread_local! {
    /// Set while [`to_redacted_json`] serialises on this thread. Serialising
    /// is synchronous, so nothing else runs on the thread meanwhile.
    static REDACTED_VIEW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn in_redacted_view() -> bool {
    REDACTED_VIEW.with(std::cell::Cell::get)
}

/// Serialise `value` to JSON in the form that may be shown to a reader who
/// must not see a secret, such as an agent and its model provider.
///
/// Every value of a secret type ([`SensitiveString`], [`SensitiveUrl`],
/// [`SensitiveBytes`]) is shown as its reference when it is one (see
/// [`secret_reference`]) and never as a literal: a string or bytes as
/// `[REDACTED]`, a URL through [`redact_url`]. The decision is made by the
/// type, wherever the value sits, so a field of a secret type added later is
/// covered without being named here. Everything else serialises as usual.
pub fn to_redacted_json<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<serde_json::Value> {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            REDACTED_VIEW.with(|flag| flag.set(self.0));
        }
    }
    let _restore = Restore(REDACTED_VIEW.with(|flag| flag.replace(true)));
    serde_json::to_value(value)
}

/// The value as a reference to where a secret is kept, when it is one.
///
/// Two forms are references, the two the orchestrator resolves:
/// `env:NAME`, where `NAME` is an environment variable name (letters,
/// digits and `_`, not starting with a digit), and `secret:path`, where the
/// path has at least two segments of letters, digits, `_`, `-` and `.`,
/// separated by `/`, optionally followed by `#field`. Anything else is a
/// literal, and a literal may be a secret.
pub fn secret_reference(raw: &str) -> Option<&str> {
    fn is_env_name(name: &str) -> bool {
        let mut chars = name.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    fn is_store_path(path: &str) -> bool {
        let (path, field) = match path.split_once('#') {
            Some((path, field)) => (path, Some(field)),
            None => (path, None),
        };
        let segment_ok = |seg: &str| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        };
        path.split('/').count() >= 2
            && path.split('/').all(segment_ok)
            && field.is_none_or(segment_ok)
    }
    let is_reference = match raw.split_once(':') {
        Some(("env", name)) => is_env_name(name),
        Some(("secret", path)) => is_store_path(path),
        _ => false,
    };
    is_reference.then_some(raw)
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

    /// `RedactedUrl` holds no user info however it is made (from a raw URL,
    /// from a `SensitiveUrl`, read back through serde), in every URL form,
    /// and keeps the address.
    #[test]
    fn redacted_url_never_holds_user_info() {
        let cases = [
            (
                "https://u:Mk7pw@git.example.invalid/o/r.git",
                "https://git.example.invalid/o/r.git",
            ),
            (
                "https://Mk7tok@git.example.invalid/o/r.git",
                "https://git.example.invalid/o/r.git",
            ),
            (
                "http://u:Mk7pw@127.0.0.1:8080/r.git",
                "http://127.0.0.1:8080/r.git",
            ),
            ("git@github.com:o/r.git", "github.com:o/r.git"),
            (
                "https://git.example.invalid/o/r.git",
                "https://git.example.invalid/o/r.git",
            ),
        ];
        for (raw, expected) in cases {
            let made = RedactedUrl::new(raw);
            let from_sensitive = RedactedUrl::from(&SensitiveUrl::new(raw));
            let read_back: RedactedUrl =
                serde_json::from_str(&serde_json::to_string(raw).unwrap()).unwrap();
            for (how, value) in [
                ("new", &made),
                ("from", &from_sensitive),
                ("serde", &read_back),
            ] {
                assert_eq!(value.as_str(), expected, "{how}: {raw}");
                let shown = format!(
                    "{value} {value:?} {}",
                    serde_json::to_string(value).unwrap()
                );
                assert!(
                    !shown.contains("Mk7"),
                    "{how}: user info survived for {raw}"
                );
            }
        }
    }
    use super::*;

    // ── The redacted view ────────────────────────────────────────────────────

    /// A value of a secret type is shown as its reference when it is one and
    /// never as a literal, wherever it sits: a field, an option, a list or a
    /// map. The decision is made by the type, so a field of a secret type
    /// added later is covered without being named anywhere.
    #[test]
    fn redacted_view_shows_references_and_no_literal_of_any_secret_type() {
        #[derive(Serialize)]
        struct Inner {
            token: SensitiveString,
            key: SensitiveBytes,
        }
        #[derive(Serialize)]
        struct Holder {
            plain: SensitiveString,
            optional: Option<SensitiveString>,
            listed: Vec<SensitiveString>,
            mapped: std::collections::BTreeMap<String, SensitiveString>,
            nested: Inner,
            url: SensitiveUrl,
            url_reference: SensitiveUrl,
            env_reference: SensitiveString,
            store_reference: SensitiveString,
            not_a_reference: SensitiveString,
            name: String,
        }
        let holder = Holder {
            plain: SensitiveString::new("Mk9-plain"),
            optional: Some(SensitiveString::new("Mk9-optional")),
            listed: vec![SensitiveString::new("Mk9-listed")],
            mapped: [("k".to_string(), SensitiveString::new("Mk9-mapped"))]
                .into_iter()
                .collect(),
            nested: Inner {
                token: SensitiveString::new("Mk9-nested"),
                key: SensitiveBytes::new(b"Mk9-bytes".to_vec()),
            },
            url: SensitiveUrl::new("https://u:Mk9-url@db.example.invalid/x?token=Mk9-query"),
            url_reference: SensitiveUrl::new("env:DATABASE_URL"),
            env_reference: SensitiveString::new("env:STRIPE_SECRET_KEY"),
            store_reference: SensitiveString::new("secret:aegis-system/kv/stripe#key"),
            not_a_reference: SensitiveString::new("env:Mk9 has spaces"),
            name: "kept".to_string(),
        };

        let shown = to_redacted_json(&holder).expect("serialises");
        let text = shown.to_string();
        assert!(
            !text.contains("Mk9"),
            "the redacted view holds a literal secret: {text}"
        );
        assert_eq!(shown["plain"], "[REDACTED]");
        assert_eq!(shown["mapped"]["k"], "[REDACTED]");
        assert_eq!(shown["nested"]["key"], "[REDACTED]");
        assert_eq!(
            shown["url"],
            "https://[REDACTED]@db.example.invalid/x?token=[REDACTED]"
        );
        assert_eq!(shown["url_reference"], "env:DATABASE_URL");
        assert_eq!(shown["env_reference"], "env:STRIPE_SECRET_KEY");
        assert_eq!(
            shown["store_reference"],
            "secret:aegis-system/kv/stripe#key"
        );
        assert_eq!(shown["name"], "kept");

        // The ordinary form is unchanged: a configuration file written back
        // still holds its values.
        let ordinary = serde_json::to_value(&holder)
            .expect("serialises")
            .to_string();
        assert!(
            ordinary.contains("Mk9-plain") && ordinary.contains("Mk9-url"),
            "the ordinary form lost a value"
        );
    }

    // ── SensitiveString ──────────────────────────────────────────────────────

    #[test]
    fn sensitive_string_redacts_in_debug() {
        let s = SensitiveString::new("super-secret-api-key");
        assert_eq!(format!("{s:?}"), "[REDACTED]");
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
        assert_ne!(a, SensitiveString::new("token-ab"));
        assert_ne!(a, SensitiveString::new("token-abcd"));
    }

    #[test]
    fn sensitive_string_matches_only_the_exact_value() {
        let s = SensitiveString::new("token-abc");
        assert!(s.matches("token-abc"));
        for wrong in ["token-abd", "token-ab", "token-abcd", "", "TOKEN-ABC"] {
            assert!(!s.matches(wrong), "matched {wrong:?}");
        }
        assert!(SensitiveString::new("").matches(""));
    }

    /// The serialised form is the bare string, byte for byte, in both
    /// directions. The fixture was the output of the derived newtype
    /// serialisation before `#[serde(transparent)]` was written down.
    #[test]
    fn sensitive_string_serialises_as_the_bare_string() {
        const FIXTURE: &str = r#"{"k":"Mk7-sensitive-string-marker"}"#;
        #[derive(Serialize, Deserialize)]
        struct Holder {
            k: SensitiveString,
        }
        let held: Holder = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(held.k.expose(), "Mk7-sensitive-string-marker");
        assert_eq!(serde_json::to_string(&held).unwrap(), FIXTURE);
        let from: SensitiveString = String::from("a").into();
        assert_eq!(from, "a".into());
    }

    // ── SensitiveBytes ───────────────────────────────────────────────────────

    #[test]
    fn sensitive_bytes_redacts_in_debug_and_keeps_its_bytes() {
        let key = SensitiveBytes::new(b"Mk7-sensitive-bytes-marker".to_vec());
        assert_eq!(format!("{key:?}"), "[REDACTED]");
        assert_eq!(key.expose(), b"Mk7-sensitive-bytes-marker");
        assert_eq!(
            key,
            SensitiveBytes::from(b"Mk7-sensitive-bytes-marker".to_vec())
        );
        assert_ne!(
            key,
            SensitiveBytes::new(b"Mk7-sensitive-bytes-marke".to_vec())
        );
    }

    #[test]
    fn sensitive_bytes_serialises_as_a_byte_vector() {
        let key = SensitiveBytes::new(vec![1u8, 2, 255]);
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, serde_json::to_string(&vec![1u8, 2, 255]).unwrap());
        assert_eq!(serde_json::from_str::<SensitiveBytes>(&json).unwrap(), key);
    }

    // ── SensitiveUrl ─────────────────────────────────────────────────────────

    /// Marker password: any appearance of it, raw or percent-encoded, in a
    /// rendered URL is a leaked credential.
    const MARKER: &str = "Mk7-redaction-marker";

    /// Assert that neither `Display` nor `Debug` of `raw` carries the marker
    /// (raw or percent-encoded) or the extra `forbidden` fragments, and that
    /// both carry every `kept` fragment. Collects every failure before
    /// panicking so one run reports all of them.
    fn assert_rendered(raw: &str, forbidden: &[&str], kept: &[&str]) -> Vec<String> {
        let url = SensitiveUrl::new(raw);
        let mut failures = Vec::new();
        for (how, text) in [("Display", format!("{url}")), ("Debug", format!("{url:?}"))] {
            for bad in std::iter::once(&MARKER).chain(forbidden.iter()) {
                if text.contains(bad) {
                    failures.push(format!("{how} of {raw:?} carries {bad:?}: {text}"));
                }
            }
            for good in kept {
                if !text.contains(good) {
                    failures.push(format!("{how} of {raw:?} lost {good:?}: {text}"));
                }
            }
        }
        failures
    }

    #[test]
    fn sensitive_url_display_and_debug_never_carry_the_credential() {
        let cases: Vec<(String, Vec<&str>, Vec<&str>)> = vec![
            // Plain password in user info.
            (
                format!("postgres://aegis:{MARKER}@db.internal:5432/aegis"),
                vec![],
                vec!["postgres://", "db.internal:5432", "/aegis"],
            ),
            // Percent-encoded password containing `@` and `:`.
            (
                format!("postgres://aegis:p%40ss%3A{MARKER}@db.internal:5432/aegis"),
                vec!["p%40ss", "p@ss"],
                vec!["db.internal:5432", "/aegis"],
            ),
            // Raw `@` and `:` inside the password: the last `@` ends user info.
            (
                format!("postgres://aegis:p@ss:{MARKER}@db.internal:5432/aegis"),
                vec!["p@ss", "p%40ss"],
                vec!["db.internal:5432", "/aegis"],
            ),
            // IPv6 host.
            (
                format!("postgres://aegis:{MARKER}@[::1]:5432/aegis"),
                vec![],
                vec!["[::1]:5432", "/aegis"],
            ),
            // A token carried as the user name, with no password.
            (
                format!("https://{MARKER}@git.example.com/org/repo.git"),
                vec![],
                vec!["git.example.com", "/org/repo.git"],
            ),
            // A secret in the query string; the harmless parameter survives.
            (
                format!("postgres://db.internal:5432/aegis?sslmode=require&password={MARKER}"),
                vec![],
                vec!["db.internal:5432", "sslmode=require"],
            ),
            // No scheme: `user:password@host:port`.
            (
                format!("aegis:{MARKER}@db.internal:5432"),
                vec![],
                vec!["db.internal:5432"],
            ),
        ];
        let mut failures = Vec::new();
        for (raw, forbidden, kept) in &cases {
            failures.extend(assert_rendered(raw, forbidden, kept));
        }
        assert!(
            failures.is_empty(),
            "SensitiveUrl rendered a credential or lost the address:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn sensitive_url_keeps_a_url_that_carries_no_credential() {
        let mut failures = Vec::new();
        for (raw, expected) in [
            (
                "postgres://db.internal:5432/aegis",
                "postgres://db.internal:5432/aegis",
            ),
            ("http://[::1]:50056", "http://[::1]:50056/"),
            (
                "https://example.com/page?q=hello",
                "https://example.com/page?q=hello",
            ),
            ("127.0.0.1:7233", "127.0.0.1:7233"),
            ("temporal:7233", "temporal:7233"),
        ] {
            let url = SensitiveUrl::new(raw);
            let display = format!("{url}");
            if display != expected {
                failures.push(format!(
                    "Display of {raw:?} is {display:?}, expected {expected:?}"
                ));
            }
            let debug = format!("{url:?}");
            let expected_debug = format!("SensitiveUrl({expected:?})");
            if debug != expected_debug {
                failures.push(format!(
                    "Debug of {raw:?} is {debug}, expected {expected_debug}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn sensitive_url_refuses_to_render_what_it_cannot_parse() {
        let url = SensitiveUrl::new(format!("not a url {MARKER}"));
        assert_eq!(format!("{url}"), "[unparseable-url]");
        assert!(!format!("{url:?}").contains(MARKER));
    }

    #[test]
    fn sensitive_url_expose_and_serde_keep_the_raw_value() {
        let raw = format!("postgres://aegis:{MARKER}@db.internal:5432/aegis");
        let url = SensitiveUrl::new(raw.clone());
        assert_eq!(url.expose(), raw);
        let json = serde_json::to_string(&url).unwrap();
        assert_eq!(json, serde_json::to_string(&raw).unwrap());
        let back: SensitiveUrl = serde_json::from_str(&json).unwrap();
        assert_eq!(back, url);
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
