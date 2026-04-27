// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SEAL Session Aggregate (BC-12, ADR-035)
//!
//! `BC-12` refers to the bounded context that owns SEAL session lifecycle logic, and
//! `ADR-035` is the architecture decision record that defines the design of this
//! aggregate and its invariants (see the project's architecture decision records).
//!
//! Domain model for the **Signed Envelope Attestation Layer** session lifecycle.
//! Each agent execution that uses MCP tools goes through an attestation handshake
//! (see [`crate::application::attestation_service`]) to receive a [`SealSession`],
//! which then authorises every subsequent tool call.
//!
//! ## Session Lifecycle
//!
//! ```text
//! AttestationRequest (agent sends ephemeral Ed25519 public key + container ID)
//!   └─ AttestationService validates container identity
//!   └─ SealSession::new(agent_id, execution_id, public_key, jwt, security_context)
//!         └─ SealSession::evaluate_call(envelope) ← called on every tool invocation
//!         └─ SealSession::revoke(reason)           ← on execution end or security incident
//! ```
//!
//! ## Invariants
//!
//! - There is at most **one** `Active` session per (`agent_id`, `execution_id`) pair.
//! - A session is valid for 1 hour from creation; [`SealSession::evaluate_call`] rejects
//!   calls after `expires_at`.
//! - The agent's `agent_public_key` (Ed25519) is **ephemeral** — generated per-execution
//!   and never written to persistent storage.
//! - `evaluate_call` is the single enforcement point: it checks status, expiry, signature,
//!   and `SecurityContext` policy in that order.
//!
//! ## Anti-Corruption Layer
//!
//! [`EnvelopeVerifier`] is a domain trait that abstracts over the cryptographic
//! details of SEAL envelope parsing. The infrastructure implementation lives in
//! [`crate::infrastructure::seal::envelope`] and uses `ed25519-dalek` for verification.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::mcp::PolicyViolation;
use crate::domain::security_context::SecurityContext;
use crate::domain::tenant::TenantId;

/// Default session time-to-live in hours.
///
/// Operators can adjust this constant to change how long SEAL sessions remain valid
/// after creation, without modifying the rest of the session lifecycle logic.
const SESSION_TTL_HOURS: i64 = 1;

/// Opaque identifier for a single SEAL session (one per agent execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generate a new random session ID.
    ///
    /// This uses a UUID v4 and relies on its statistical uniqueness; the probability of
    /// collision is negligible for realistic volumes of sessions. Any additional collision
    /// handling (for example, enforcing a unique constraint at the persistence layer) is
    /// expected to be performed outside this constructor.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle state of an [`SealSession`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// The session is valid and can authorise tool calls.
    Active,
    /// The session's `expires_at` timestamp has passed. No new tool calls are permitted.
    Expired,
    /// The session was explicitly revoked by the orchestrator, with a human-readable reason.
    Revoked { reason: String },
}

/// Errors that can occur when evaluating an SEAL envelope against a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SealSessionError {
    /// The session is not in `Active` state. Includes the current status for diagnostics.
    SessionInactive(SessionStatus),
    /// The session's TTL has elapsed (checked against `expires_at`).
    SessionExpired,
    /// The tool call was rejected by the agent's [`crate::domain::security_context::SecurityContext`].
    PolicyViolation(PolicyViolation),
    /// The SEAL envelope could not be parsed (missing required fields).
    MalformedPayload(String),
    /// Replay protection rejected the envelope metadata.
    ReplayProtectionFailed(String),
    /// The Ed25519 signature on the envelope did not verify against the session's stored public key.
    SignatureVerificationFailed(String),
    /// A semantic judge agent did not respond within the allotted timeout window.
    JudgeTimeout(String),
    /// An unexpected internal error occurred (e.g. infrastructure or service failure).
    InternalError(String),
    /// Tool call arguments failed required-field validation against the tool's input schema.
    ///
    /// Returned by [`crate::application::tool_invocation_service::ToolInvocationService`] before
    /// dispatch when a known tool is called without all of its required parameters present or
    /// when a parameter value is semantically invalid (e.g. `cmd.run` with an empty `command`).
    /// Maps to MCP JSON-RPC error code `-32602` (Invalid params).
    InvalidArguments(String),
    /// A requested resource (e.g. agent, execution) could not be found.
    NotFound(String),
    /// A required configuration value is missing or invalid.
    ConfigurationError(String),
    /// A tool argument supplied a `tenant_id` that does not match the
    /// authenticated caller's tenant, and the identity is not permitted to
    /// delegate to other tenants. ADR-097, ADR-100.
    TenantMismatch {
        authenticated: String,
        requested: String,
    },
}

impl std::fmt::Display for SealSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionInactive(status) => write!(f, "Session is inactive: {status:?}"),
            Self::SessionExpired => write!(f, "Session has expired"),
            Self::PolicyViolation(v) => write!(f, "Policy violation: {v:?}"),
            Self::MalformedPayload(msg) => write!(f, "Malformed MCP payload: {msg}"),
            Self::ReplayProtectionFailed(msg) => write!(f, "Replay protection failed: {msg}"),
            Self::SignatureVerificationFailed(e) => {
                write!(f, "Signature verification failed: {e}")
            }
            Self::JudgeTimeout(msg) => write!(f, "Judge timed out: {msg}"),
            Self::InternalError(msg) => write!(f, "Internal error: {msg}"),
            Self::InvalidArguments(msg) => write!(f, "Invalid tool arguments: {msg}"),
            Self::NotFound(msg) => write!(f, "Not found: {msg}"),
            Self::ConfigurationError(msg) => write!(f, "Configuration error: {msg}"),
            Self::TenantMismatch {
                authenticated,
                requested,
            } => write!(
                f,
                "Tenant mismatch: caller is authenticated as tenant '{authenticated}' but requested operation on tenant '{requested}'"
            ),
        }
    }
}

impl std::error::Error for SealSessionError {}

/// Domain-level abstraction over SEAL envelope cryptography.
///
/// This trait keeps the domain layer free of `ed25519-dalek` and JSON-Web-Token
/// dependencies. The infrastructure implementation ([`crate::infrastructure::seal::envelope`])
/// provides the concrete verification logic.
///
/// # Security
///
/// Implementations **must** perform constant-time signature verification. Timing
/// side-channels on `verify_signature` can leak private key material.
pub trait EnvelopeVerifier {
    /// Return the raw SEAL security token carried by the envelope.
    fn security_token(&self) -> &str;

    /// Verify that the envelope's Ed25519 signature was produced by the holder of `public_key_bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`SealSessionError::SignatureVerificationFailed`] if the signature is invalid
    /// or `public_key_bytes` is not a valid Ed25519 public key.
    fn verify_signature(&self, public_key_bytes: &[u8]) -> Result<(), SealSessionError>;

    /// Extract the MCP tool name from the inner MCP payload of the envelope.
    ///
    /// Returns `None` if the payload is missing or the `method` field is absent.
    fn extract_tool_name(&self) -> Option<String>;

    /// Extract the MCP tool arguments from the inner MCP payload.
    ///
    /// Returns `None` if the payload is missing or the `params` field is absent.
    fn extract_arguments(&self) -> Option<serde_json::Value>;
}

/// Aggregate root for the SEAL session lifecycle (BC-12, ADR-035).
///
/// Represents the security contract between one agent execution and the orchestrator.
/// Created during attestation; authorises every MCP tool call via [`SealSession::evaluate_call`].
///
/// # Invariants
///
/// - `status` starts as `Active` and transitions monotonically to `Expired` or `Revoked`.
/// - `agent_public_key` is the ephemeral Ed25519 public key generated per-execution.
/// - `expires_at` is set to 1 hour from `created_at` at construction time.
/// - Only one `Active` session exists per `(agent_id, execution_id)` pair — enforced
///   by [`crate::domain::seal_session_repository::SealSessionRepository`].
#[derive(Debug, Clone)]
pub struct SealSession {
    /// Session ID (UUID)
    pub id: SessionId,

    /// Agent ID
    pub agent_id: AgentId,

    /// Execution ID
    pub execution_id: ExecutionId,

    /// Agent's public key bytes (for signature verification)
    pub agent_public_key: Vec<u8>,

    /// Issued SecurityToken raw string (abstracted)
    pub security_token_raw: String,

    /// Assigned SecurityContext
    pub security_context: SecurityContext,

    /// Optional upstream principal subject for audit correlation.
    pub principal_subject: Option<String>,

    /// Optional upstream user identifier for consumer-facing sessions.
    pub user_id: Option<String>,

    /// Optional workload identifier associated with this session.
    pub workload_id: Option<String>,

    /// Optional Zaru subscription tier bound at attestation time.
    pub zaru_tier: Option<String>,

    /// Tenant that owns this session, extracted from the JWT claims at attestation time.
    pub tenant_id: TenantId,

    /// Session status
    pub status: SessionStatus,

    /// Timestamps
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl SealSession {
    /// Initialise a new session immediately following successful attestation.
    ///
    /// Sets `status` to `Active` and `expires_at` to 1 hour from now.
    pub fn new(
        agent_id: AgentId,
        execution_id: ExecutionId,
        agent_public_key: Vec<u8>,
        security_token_raw: String,
        security_context: SecurityContext,
        tenant_id: TenantId,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            agent_id,
            execution_id,
            agent_public_key,
            security_token_raw,
            security_context,
            principal_subject: None,
            user_id: None,
            workload_id: None,
            zaru_tier: None,
            tenant_id,
            status: SessionStatus::Active,
            created_at: now,
            expires_at: now + chrono::Duration::hours(SESSION_TTL_HOURS),
        }
    }

    /// Attach optional upstream identity metadata captured during attestation.
    pub fn with_principal_metadata(
        mut self,
        principal_subject: Option<String>,
        user_id: Option<String>,
        workload_id: Option<String>,
        zaru_tier: Option<String>,
    ) -> Self {
        self.principal_subject = principal_subject.filter(|value| !value.trim().is_empty());
        self.user_id = user_id.filter(|value| !value.trim().is_empty());
        self.workload_id = workload_id.filter(|value| !value.trim().is_empty());
        self.zaru_tier = zaru_tier.filter(|value| !value.trim().is_empty());
        self
    }

    /// Authorise a single MCP tool call against this session's policy.
    ///
    /// Enforces the following checks **in order** (first failure returns immediately):
    /// 1. Session is `Active`
    /// 2. Current time is before `expires_at`
    /// 3. Envelope signature verifies against `agent_public_key`
    /// 4. Envelope contains a parseable tool name and arguments
    /// 5. `SecurityContext::evaluate` permits the tool call
    ///
    /// # Errors
    ///
    /// - [`SealSessionError::SessionInactive`] — session is `Expired` or `Revoked`
    /// - [`SealSessionError::SessionExpired`] — TTL exceeded
    /// - [`SealSessionError::SignatureVerificationFailed`] — bad Ed25519 signature
    /// - [`SealSessionError::MalformedPayload`] — envelope missing tool name or args
    /// - [`SealSessionError::PolicyViolation`] — `SecurityContext` denied the call
    ///
    /// # Security
    ///
    /// This is the **single enforcement point** for all SEAL policy checks. Every
    /// tool call from any agent must pass through this method before being forwarded
    /// to the MCP server. See ADR-035 §4 (Enforcement Architecture).
    pub fn evaluate_call(
        &mut self,
        envelope: &impl EnvelopeVerifier,
    ) -> Result<(), SealSessionError> {
        let now = Utc::now();

        // 1. Check session is active
        if self.status != SessionStatus::Active {
            return Err(SealSessionError::SessionInactive(self.status.clone()));
        }

        // 2. Check not expired
        if now > self.expires_at {
            // Once the session is past its expiry time, transition it to a terminal
            // `Expired` state so that future calls observe a consistent status.
            self.status = SessionStatus::Expired;
            return Err(SealSessionError::SessionExpired);
        }

        // 3. Ensure the presented token matches the token issued for this session.
        if envelope.security_token() != self.security_token_raw {
            return Err(SealSessionError::SignatureVerificationFailed(
                "security token does not match the active SEAL session".to_string(),
            ));
        }

        // 4. Verify signature
        envelope.verify_signature(&self.agent_public_key)?;

        // 5. Extract tool name from MCP payload
        let tool_name = envelope
            .extract_tool_name()
            .ok_or(SealSessionError::MalformedPayload(
                "missing tool name".to_string(),
            ))?;

        let args = envelope
            .extract_arguments()
            .ok_or(SealSessionError::MalformedPayload(
                "missing arguments".to_string(),
            ))?;

        // 6. Evaluate against SecurityContext
        self.security_context
            .evaluate(&tool_name, &args)
            .map_err(SealSessionError::PolicyViolation)
    }

    /// Revoke this session, preventing any further tool calls.
    ///
    /// Should be called when the associated execution terminates (normally or abnormally)
    /// or when a security incident requires immediate session termination.
    /// After revocation, `evaluate_call` will return [`SealSessionError::SessionInactive`].
    pub fn revoke(&mut self, reason: String) {
        // Make revocation idempotent: once the session has reached a terminal state
        // (`Expired` or `Revoked`), do not change its status again. This avoids
        // masking logic bugs where revocation is attempted multiple times.
        match self.status {
            SessionStatus::Active => {
                self.status = SessionStatus::Revoked { reason };
            }
            SessionStatus::Revoked { .. } | SessionStatus::Expired => {
                // Already in a terminal state; ignore subsequent revocation attempts.
            }
        }
    }
}
