// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Session Aggregate (BC-12, ADR-035)
//!
//! Domain model for the **Secure Model Context Protocol** session lifecycle.
//! Each agent execution that uses MCP tools goes through an attestation handshake
//! (see [`crate::application::attestation_service`]) to receive a [`SmcpSession`],
//! which then authorises every subsequent tool call.
//!
//! ## Session Lifecycle
//!
//! ```text
//! AttestationRequest (agent sends ephemeral Ed25519 public key + container ID)
//!   └─ AttestationService validates container identity
//!   └─ SmcpSession::new(agent_id, execution_id, public_key, jwt, security_context)
//!         └─ SmcpSession::evaluate_call(envelope) ← called on every tool invocation
//!         └─ SmcpSession::revoke(reason)           ← on execution end or security incident
//! ```
//!
//! ## Invariants
//!
//! - There is at most **one** `Active` session per (`agent_id`, `execution_id`) pair.
//! - A session is valid for 1 hour from creation; [`SmcpSession::evaluate_call`] rejects
//!   calls after `expires_at`.
//! - The agent's `agent_public_key` (Ed25519) is **ephemeral** — generated per-execution
//!   and never written to persistent storage.
//! - `evaluate_call` is the single enforcement point: it checks status, expiry, signature,
//!   and `SecurityContext` policy in that order.
//!
//! ## Anti-Corruption Layer
//!
//! [`EnvelopeVerifier`] is a domain trait that abstracts over the cryptographic
//! details of SMCP envelope parsing. The infrastructure implementation lives in
//! [`crate::infrastructure::smcp::envelope`] and uses `ed25519-dalek` for verification.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::mcp::PolicyViolation;
use crate::domain::security_context::SecurityContext;

/// Opaque identifier for a single SMCP session (one per agent execution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generate a new random session ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Lifecycle state of an [`SmcpSession`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// The session is valid and can authorise tool calls.
    Active,
    /// The session's `expires_at` timestamp has passed. No new tool calls are permitted.
    Expired,
    /// The session was explicitly revoked by the orchestrator, with a human-readable reason.
    Revoked { reason: String },
}

/// Errors that can occur when evaluating an SMCP envelope against a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SmcpSessionError {
    /// The session is not in `Active` state. Includes the current status for diagnostics.
    SessionInactive(SessionStatus),
    /// The session's TTL has elapsed (checked against `expires_at`).
    SessionExpired,
    /// The tool call was rejected by the agent's [`crate::domain::security_context::SecurityContext`].
    PolicyViolation(PolicyViolation),
    /// The SMCP envelope could not be parsed (missing required fields).
    MalformedPayload,
    /// The Ed25519 signature on the envelope did not verify against the session's stored public key.
    SignatureVerificationFailed(String),
}

impl std::fmt::Display for SmcpSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionInactive(status) => write!(f, "Session is inactive: {:?}", status),
            Self::SessionExpired => write!(f, "Session has expired"),
            Self::PolicyViolation(v) => write!(f, "Policy violation: {:?}", v),
            Self::MalformedPayload => write!(f, "Malformed MCP payload"),
            Self::SignatureVerificationFailed(e) => write!(f, "Signature verification failed: {}", e),
        }
    }
}

impl std::error::Error for SmcpSessionError {}

/// Domain-level abstraction over SMCP envelope cryptography.
///
/// This trait keeps the domain layer free of `ed25519-dalek` and JSON-Web-Token
/// dependencies. The infrastructure implementation ([`crate::infrastructure::smcp::envelope`])
/// provides the concrete verification logic.
///
/// # Security
///
/// Implementations **must** perform constant-time signature verification. Timing
/// side-channels on `verify_signature` can leak private key material.
pub trait EnvelopeVerifier {
    /// Verify that the envelope's Ed25519 signature was produced by the holder of `public_key_bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`SmcpSessionError::SignatureVerificationFailed`] if the signature is invalid
    /// or `public_key_bytes` is not a valid Ed25519 public key.
    fn verify_signature(&self, public_key_bytes: &[u8]) -> Result<(), SmcpSessionError>;

    /// Extract the MCP tool name from the inner MCP payload of the envelope.
    ///
    /// Returns `None` if the payload is missing or the `method` field is absent.
    fn extract_tool_name(&self) -> Option<String>;

    /// Extract the MCP tool arguments from the inner MCP payload.
    ///
    /// Returns `None` if the payload is missing or the `params` field is absent.
    fn extract_arguments(&self) -> Option<serde_json::Value>;
}

/// Aggregate root for the SMCP session lifecycle (BC-12, ADR-035).
///
/// Represents the security contract between one agent execution and the orchestrator.
/// Created during attestation; authorises every MCP tool call via [`SmcpSession::evaluate_call`].
///
/// # Invariants
///
/// - `status` starts as `Active` and transitions monotonically to `Expired` or `Revoked`.
/// - `agent_public_key` is the ephemeral Ed25519 public key generated per-execution.
/// - `expires_at` is set to 1 hour from `created_at` at construction time.
/// - Only one `Active` session exists per `(agent_id, execution_id)` pair — enforced
///   by [`crate::domain::smcp_session_repository::SmcpSessionRepository`].
#[derive(Debug, Clone)]
pub struct SmcpSession {
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
    
    /// Session status
    pub status: SessionStatus,
    
    /// Timestamps
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl SmcpSession {
    /// Initialise a new session immediately following successful attestation.
    ///
    /// Sets `status` to `Active` and `expires_at` to 1 hour from now.
    pub fn new(
        agent_id: AgentId,
        execution_id: ExecutionId,
        agent_public_key: Vec<u8>,
        security_token_raw: String,
        security_context: SecurityContext,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            agent_id,
            execution_id,
            agent_public_key,
            security_token_raw,
            security_context,
            status: SessionStatus::Active,
            created_at: now,
            expires_at: now + chrono::Duration::hours(1),
        }
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
    /// - [`SmcpSessionError::SessionInactive`] — session is `Expired` or `Revoked`
    /// - [`SmcpSessionError::SessionExpired`] — TTL exceeded
    /// - [`SmcpSessionError::SignatureVerificationFailed`] — bad Ed25519 signature
    /// - [`SmcpSessionError::MalformedPayload`] — envelope missing tool name or args
    /// - [`SmcpSessionError::PolicyViolation`] — `SecurityContext` denied the call
    ///
    /// # Security
    ///
    /// This is the **single enforcement point** for all SMCP policy checks. Every
    /// tool call from any agent must pass through this method before being forwarded
    /// to the MCP server. See ADR-035 §4 (Enforcement Architecture).
    pub fn evaluate_call(
        &self,
        envelope: &impl EnvelopeVerifier,
    ) -> Result<(), SmcpSessionError> {
        // 1. Check session is active
        if self.status != SessionStatus::Active {
            return Err(SmcpSessionError::SessionInactive(self.status.clone()));
        }
        
        // 2. Check not expired
        if Utc::now() > self.expires_at {
            return Err(SmcpSessionError::SessionExpired);
        }
        
        // 3. Verify signature
        envelope.verify_signature(&self.agent_public_key)?;
        
        // 4. Extract tool name from MCP payload
        let tool_name = envelope.extract_tool_name()
            .ok_or(SmcpSessionError::MalformedPayload)?;
        
        let args = envelope.extract_arguments()
            .ok_or(SmcpSessionError::MalformedPayload)?;
        
        // 5. Evaluate against SecurityContext
        self.security_context.evaluate(&tool_name, &args)
            .map_err(SmcpSessionError::PolicyViolation)
    }
    
    /// Revoke this session, preventing any further tool calls.
    ///
    /// Should be called when the associated execution terminates (normally or abnormally)
    /// or when a security incident requires immediate session termination.
    /// After revocation, `evaluate_call` will return [`SmcpSessionError::SessionInactive`].
    pub fn revoke(&mut self, reason: String) {
        self.status = SessionStatus::Revoked { reason };
    }
}
