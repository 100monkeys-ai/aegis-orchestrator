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
use crate::domain::secrets::SensitiveString;
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

/// The operator escalation a SEAL session was attested under (AEGIS ADR-129
/// D14): which escalation to re-check at every call (D19) and the role it
/// grants. The session's own `tenant_id` is the holding key's home tenant
/// (the Update's U1).
#[derive(Debug, Clone, PartialEq)]
pub struct SealOperatorEscalation {
    pub escalation_id: uuid::Uuid,
    pub aegis_role: crate::domain::iam::AegisRole,
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
    /// An upstream service used by a built-in tool (e.g. Brave Search,
    /// remote HTTP fetch) returned a transient failure such as HTTP 429
    /// Too Many Requests, 5xx, transport error, or unparseable response.
    ///
    /// This variant exists so that transient external-service failures are
    /// **never** conflated with SEAL signature verification failures. It is
    /// classified as `Recoverable` by the inner-loop error classifier so
    /// the LLM receives the error as a normal tool-call failure and can
    /// adapt (per ADR-005 iterative refinement).
    UpstreamUnavailable(String),
    /// The session was attested under an operator escalation that has since
    /// ended (AEGIS ADR-129 D19): a call arriving after the end is refused.
    /// Displayed as the record's error code, `operator_escalation_expired`.
    OperatorEscalationExpired,
    /// A refusal whose answer to the caller of `POST /v1/seal/invoke` was
    /// decided where it was built (AEGIS ADR-035, Update of 2026-10-04, R5).
    /// `shown` is the error as the inner loop and the operator's log have
    /// always seen it: its text and its class are unchanged. `answer` is what
    /// the route tells the caller, built only from the caller's own inputs.
    Answered {
        answer: CallerAnswer,
        shown: Box<SealSessionError>,
    },
}

/// What the tool invoke route tells its caller for a refusal built as
/// [`SealSessionError::Answered`] (AEGIS ADR-035, Update of 2026-10-04, R4).
/// Each caller-facing message holds only the caller's own business: what they
/// sent, named as they named it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallerAnswer {
    /// The caller's arguments are invalid (422 `INVALID_ARGUMENTS`).
    InvalidArguments(String),
    /// A resource of the caller's was not found (404 `NOT_FOUND`).
    NotFound(String),
    /// The call conflicts with the caller's own state (409 `CONFLICT`).
    Conflict(String),
    /// The caller reached a quota or tier limit of their own (422 `QUOTA_EXCEEDED`).
    QuotaExceeded(String),
    /// The semantic judge rejected the caller's call (403 `JUDGE_REJECTED`).
    JudgeRejected(String),
    /// The caller's filesystem policy does not permit the path they named
    /// (403 `PATH_NOT_ALLOWED`).
    PathNotAllowed(String),
    /// The caller named a resource of another tenant (403 `TENANT_MISMATCH`).
    TenantMismatch(String),
    /// The caller asked `POST /v1/seal/attest` for a security context its
    /// verified identity does not entitle it to (403 `CONTEXT_NOT_ALLOWED`,
    /// AEGIS ADR-035 — Updates, the attest-authority clauses). The message is
    /// fixed: it names no context, so it tells nothing of which exist.
    ContextNotAllowed,
    /// The operation the caller asked for is not built yet (501 `NOT_IMPLEMENTED`).
    NotImplemented(String),
    /// The caller's own edge did not answer (503 `EDGE_UNAVAILABLE`).
    EdgeUnavailable(String),
    /// An internal failure: the caller is told only its class.
    Internal(InternalFailure),
}

/// The class of an internal failure, which alone reaches the caller
/// (AEGIS ADR-035, Update of 2026-10-04, R3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InternalFailure {
    /// 500 `INTERNAL_ERROR`: a database, repository or platform-state failure.
    Server,
    /// 502 `UPSTREAM_UNAVAILABLE`: a service the tool depends on did not answer.
    Upstream,
    /// 503 `SERVICE_UNAVAILABLE`: the tool is not configured or not available on this node.
    Unavailable,
}

/// The rate limit a 429 answer carries in its headers (AEGIS ADR-072 §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHint {
    pub limit: u64,
    pub remaining: u64,
    pub retry_after_seconds: u64,
}

/// The answer of `POST /v1/seal/invoke` to one refusal (AEGIS ADR-035,
/// Update of 2026-10-04, R1 to R4): the status, the stable machine code,
/// ADR-035's `status` member and the message. An internal failure's message
/// is the fixed sentence of its class; its detail goes only to the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealRefusal {
    /// HTTP status code.
    pub http_status: u16,
    /// `error.code`: the stable machine code.
    pub code: &'static str,
    /// The body's `status` member: `policy_violation` or `error`.
    pub status: &'static str,
    /// `error.message`.
    pub message: String,
    /// True when the refusal is an internal failure (5xx, detail logged at error level).
    pub internal: bool,
    /// The rate limit, on a 429.
    pub rate_limit: Option<RateLimitHint>,
}

/// The fixed message of an attestation refused for a context the caller's
/// identity is not entitled to.
pub const CONTEXT_NOT_ALLOWED_MESSAGE: &str =
    "The security context this attestation asked for is not one your identity may attest.";
/// The fixed message of an internal failure of each class (R3).
pub const INTERNAL_ERROR_MESSAGE: &str =
    "The request could not be completed because of an internal error.";
/// The fixed message of an upstream failure (R3).
pub const UPSTREAM_UNAVAILABLE_MESSAGE: &str =
    "A service this tool depends on did not answer. Try again in a moment.";
/// The fixed message of a tool not available on this node (R3).
pub const SERVICE_UNAVAILABLE_MESSAGE: &str = "This tool is not available right now.";

impl SealRefusal {
    fn caller(http_status: u16, code: &'static str, message: String) -> Self {
        Self {
            http_status,
            code,
            status: "error",
            message,
            internal: false,
            rate_limit: None,
        }
    }

    fn policy(http_status: u16, code: &'static str, message: String) -> Self {
        Self {
            status: "policy_violation",
            ..Self::caller(http_status, code, message)
        }
    }

    fn internal(class: InternalFailure) -> Self {
        let (http_status, code, message) = match class {
            InternalFailure::Server => (500, "INTERNAL_ERROR", INTERNAL_ERROR_MESSAGE),
            InternalFailure::Upstream => {
                (502, "UPSTREAM_UNAVAILABLE", UPSTREAM_UNAVAILABLE_MESSAGE)
            }
            InternalFailure::Unavailable => {
                (503, "SERVICE_UNAVAILABLE", SERVICE_UNAVAILABLE_MESSAGE)
            }
        };
        Self {
            http_status,
            code,
            status: "error",
            message: message.to_string(),
            internal: true,
            rate_limit: None,
        }
    }
}

impl SealSessionError {
    /// Attach the answer the tool invoke route gives the caller, decided
    /// where the error is built (R5). What the inner loop and the log see is
    /// unchanged.
    pub fn answered(self, answer: CallerAnswer) -> Self {
        Self::Answered {
            answer,
            shown: Box::new(self),
        }
    }

    /// The answer of `POST /v1/seal/invoke` to this refusal (AEGIS ADR-035,
    /// Update of 2026-10-04, R4). One exhaustive match: a new variant does
    /// not compile until it is classified. Never reads an error's text to
    /// classify it.
    pub fn refusal(&self) -> SealRefusal {
        match self {
            Self::SessionInactive(_) => SealRefusal::caller(
                401,
                "SESSION_INACTIVE",
                "Your session is no longer active. Attest again to start a new one.".to_string(),
            ),
            Self::SessionExpired => SealRefusal::caller(
                401,
                "SESSION_EXPIRED",
                "Your session has expired. Attest again to start a new one.".to_string(),
            ),
            Self::OperatorEscalationExpired => SealRefusal::caller(
                401,
                "OPERATOR_ESCALATION_EXPIRED",
                "Your operator escalation has ended.".to_string(),
            ),
            Self::SignatureVerificationFailed(_) => SealRefusal::caller(
                401,
                "SIGNATURE_INVALID",
                "The request's signature or security token did not verify. Attest again to start a new session."
                    .to_string(),
            ),
            Self::ReplayProtectionFailed(_) => {
                SealRefusal::caller(401, "ENVELOPE_REPLAYED", self.to_string())
            }
            Self::MalformedPayload(_) => {
                SealRefusal::caller(400, "MALFORMED_ENVELOPE", self.to_string())
            }
            Self::PolicyViolation(violation) => {
                let code = match violation {
                    PolicyViolation::ToolNotAllowed { .. } => "TOOL_NOT_ALLOWED",
                    PolicyViolation::ToolExplicitlyDenied { .. } => "TOOL_DENIED",
                    PolicyViolation::RateLimitExceeded {
                        limit,
                        current,
                        retry_after_seconds,
                        ..
                    } => {
                        let mut refusal =
                            SealRefusal::policy(429, "RATE_LIMIT_EXCEEDED", self.to_string());
                        refusal.rate_limit = Some(RateLimitHint {
                            limit: *limit,
                            remaining: limit.saturating_sub(*current),
                            retry_after_seconds: *retry_after_seconds,
                        });
                        return refusal;
                    }
                    PolicyViolation::PathOutsideBoundary { .. } => "PATH_NOT_ALLOWED",
                    PolicyViolation::PathTraversalAttempt { .. } => "PATH_TRAVERSAL",
                    PolicyViolation::DomainNotAllowed { .. } => "DOMAIN_NOT_ALLOWED",
                    PolicyViolation::MissingRequiredArgument(_) => "POLICY_ARGUMENT_REQUIRED",
                    PolicyViolation::CommandNotAllowed { .. } => "COMMAND_NOT_ALLOWED",
                    PolicyViolation::SubcommandNotAllowed { .. } => "SUBCOMMAND_NOT_ALLOWED",
                    PolicyViolation::TimeoutExceeded { .. }
                    | PolicyViolation::ConcurrentExecLimitExceeded { .. }
                    | PolicyViolation::OutputSizeLimitExceeded { .. }
                    | PolicyViolation::ExecTimeoutCeilingExceeded { .. } => "LIMIT_EXCEEDED",
                };
                SealRefusal::policy(403, code, self.to_string())
            }
            Self::InvalidArguments(_) => {
                SealRefusal::caller(422, "INVALID_ARGUMENTS", self.to_string())
            }
            Self::NotFound(_) => SealRefusal::caller(404, "NOT_FOUND", self.to_string()),
            Self::TenantMismatch { .. } => {
                SealRefusal::caller(403, "TENANT_MISMATCH", self.to_string())
            }
            Self::InternalError(_) | Self::JudgeTimeout(_) => {
                SealRefusal::internal(InternalFailure::Server)
            }
            Self::ConfigurationError(_) => SealRefusal::internal(InternalFailure::Unavailable),
            Self::UpstreamUnavailable(_) => SealRefusal::internal(InternalFailure::Upstream),
            Self::Answered { answer, .. } => match answer {
                CallerAnswer::InvalidArguments(m) => {
                    SealRefusal::caller(422, "INVALID_ARGUMENTS", m.clone())
                }
                CallerAnswer::NotFound(m) => SealRefusal::caller(404, "NOT_FOUND", m.clone()),
                CallerAnswer::Conflict(m) => SealRefusal::caller(409, "CONFLICT", m.clone()),
                CallerAnswer::QuotaExceeded(m) => {
                    SealRefusal::caller(422, "QUOTA_EXCEEDED", m.clone())
                }
                CallerAnswer::JudgeRejected(m) => {
                    SealRefusal::policy(403, "JUDGE_REJECTED", m.clone())
                }
                CallerAnswer::PathNotAllowed(m) => {
                    SealRefusal::policy(403, "PATH_NOT_ALLOWED", m.clone())
                }
                CallerAnswer::TenantMismatch(m) => {
                    SealRefusal::caller(403, "TENANT_MISMATCH", m.clone())
                }
                CallerAnswer::ContextNotAllowed => SealRefusal::policy(
                    403,
                    "CONTEXT_NOT_ALLOWED",
                    CONTEXT_NOT_ALLOWED_MESSAGE.to_string(),
                ),
                CallerAnswer::NotImplemented(m) => {
                    SealRefusal::caller(501, "NOT_IMPLEMENTED", m.clone())
                }
                CallerAnswer::EdgeUnavailable(m) => {
                    SealRefusal::caller(503, "EDGE_UNAVAILABLE", m.clone())
                }
                CallerAnswer::Internal(class) => SealRefusal::internal(*class),
            },
        }
    }
}

impl std::fmt::Display for SealSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionInactive(status) => write!(f, "Session is inactive: {status:?}"),
            Self::SessionExpired => write!(f, "Session has expired"),
            Self::PolicyViolation(v) => write!(f, "Policy violation: {v}"),
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
            Self::UpstreamUnavailable(msg) => {
                write!(f, "Upstream service unavailable: {msg}")
            }
            Self::OperatorEscalationExpired => write!(f, "operator_escalation_expired"),
            Self::Answered { shown, .. } => shown.fmt(f),
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
    /// Return the SEAL security token carried by the envelope. It prints
    /// redacted; read it with `expose()` only where it is compared or signed.
    fn security_token(&self) -> &SensitiveString;

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

    /// Return a stable per-envelope nonce identifier used for replay detection.
    ///
    /// Audit 002 §4.17 requires that recently-seen envelopes be rejected
    /// inside the 30 s freshness window. The returned string MUST be unique
    /// per envelope (the signature bytes are a natural choice — they bind
    /// payload + timestamp + security_token via Ed25519 over the canonical
    /// message). Callers consult a nonce store keyed by this value.
    fn replay_nonce(&self) -> String;
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

    /// Issued SecurityToken (a bearer JWT). Prints redacted.
    pub security_token_raw: SensitiveString,

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

    /// The operator escalation this session was attested under, when its API
    /// key held one (AEGIS ADR-129 D14). Every call on the session re-checks
    /// that the escalation is still active (D19).
    pub operator_escalation: Option<SealOperatorEscalation>,

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
        security_token_raw: impl Into<SensitiveString>,
        security_context: SecurityContext,
        tenant_id: TenantId,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            agent_id,
            execution_id,
            agent_public_key,
            security_token_raw: security_token_raw.into(),
            security_context,
            principal_subject: None,
            user_id: None,
            workload_id: None,
            zaru_tier: None,
            tenant_id,
            operator_escalation: None,
            status: SessionStatus::Active,
            created_at: now,
            expires_at: now + chrono::Duration::hours(SESSION_TTL_HOURS),
        }
    }

    /// Bind the session to the operator escalation its API key held at
    /// attestation (AEGIS ADR-129 D14).
    pub fn with_operator_escalation(mut self, escalation: SealOperatorEscalation) -> Self {
        self.operator_escalation = Some(escalation);
        self
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
    ///
    /// # Concurrency Contract (Audit 002 §4.37.2)
    ///
    /// `evaluate_call` takes `&mut self` and mutates the session's `status` on
    /// the expiry-transition path. Callers MUST hold an exclusive borrow for the
    /// duration of the call — a `SealSession` MUST NOT be shared across tasks
    /// behind a non-exclusive primitive (`Arc<SealSession>`, `Arc<RwLock<_>>`
    /// in read mode, etc.) while it is being evaluated. The canonical access
    /// pattern is the per-call ownership cycle enforced by
    /// [`crate::domain::seal_session_repository::SealSessionRepository`]:
    /// `find_by_id` → mutate via `evaluate_call` → `save`. Each call gets its
    /// own owned session instance, so concurrent evaluations on the same
    /// `(agent_id, execution_id)` pair are serialised by the repository's
    /// optimistic-concurrency contract rather than by an in-memory mutex on
    /// the aggregate. Wrapping in a `tokio::Mutex` would push the boundary
    /// into the wrong layer (the domain) and is not the chosen design.
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
        // Constant-time: the time taken must not tell a caller how much of a
        // guessed token was right.
        if envelope.security_token() != &self.security_token_raw {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::security_context::SecurityContextMetadata;

    #[test]
    fn seal_session_debug_does_not_print_the_security_token() {
        let session = SealSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![1, 2, 3],
            "Mk7-seal-session-token-marker".to_string(),
            SecurityContext {
                name: "ctx".to_string(),
                description: "ctx".to_string(),
                capabilities: vec![],
                deny_list: vec![],
                metadata: SecurityContextMetadata {
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    version: 1,
                },
            },
            TenantId::consumer(),
        );
        let printed = format!("{session:?}");
        assert!(
            !printed.contains("Mk7-seal-session-token-marker"),
            "SealSession's Debug printed its security token: {printed}"
        );
        assert!(
            printed.contains("SealSession"),
            "Debug lost the type: {printed}"
        );
    }

    /// AEGIS operations/known-defects-7: after zaru.operator.release an
    /// operator-only tool call was answered 400 with the refusal printed in
    /// Rust's Debug form. The refusal names the tool in words, the form
    /// `PolicyViolation`'s own Display gives it.
    #[test]
    fn a_policy_refusal_names_the_tool_in_words() {
        let refusal = SealSessionError::PolicyViolation(PolicyViolation::ToolNotAllowed {
            tool_name: "aegis.system.info".to_string(),
            allowed_tools: vec!["zaru.*".to_string()],
        });
        assert_eq!(
            refusal.to_string(),
            "Policy violation: tool 'aegis.system.info' is not allowed; permitted tools: [zaru.*]"
        );
    }

    /// AEGIS ADR-035, Update of 2026-10-04, R4: every refusal has one status,
    /// one stable code and ADR-035's `status` member; an internal failure's
    /// message is its class's fixed sentence and holds none of its detail.
    #[test]
    fn every_refusal_maps_to_its_status_code_and_message() {
        let policy = |v: PolicyViolation| SealSessionError::PolicyViolation(v);
        let cases: Vec<(SealSessionError, u16, &str, &str)> = vec![
            (
                SealSessionError::SessionInactive(SessionStatus::Revoked {
                    reason: "Mk7-revocation-reason".into(),
                }),
                401,
                "SESSION_INACTIVE",
                "error",
            ),
            (
                SealSessionError::SessionExpired,
                401,
                "SESSION_EXPIRED",
                "error",
            ),
            (
                SealSessionError::OperatorEscalationExpired,
                401,
                "OPERATOR_ESCALATION_EXPIRED",
                "error",
            ),
            (
                SealSessionError::SignatureVerificationFailed("Mk7-library-text".into()),
                401,
                "SIGNATURE_INVALID",
                "error",
            ),
            (
                SealSessionError::ReplayProtectionFailed(
                    "envelope nonce already seen within freshness window".into(),
                ),
                401,
                "ENVELOPE_REPLAYED",
                "error",
            ),
            (
                SealSessionError::MalformedPayload("missing tool name".into()),
                400,
                "MALFORMED_ENVELOPE",
                "error",
            ),
            (
                policy(PolicyViolation::ToolNotAllowed {
                    tool_name: "aegis.system.info".into(),
                    allowed_tools: vec!["zaru.*".into()],
                }),
                403,
                "TOOL_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::ToolExplicitlyDenied {
                    tool_name: "cmd.run".into(),
                }),
                403,
                "TOOL_DENIED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::RateLimitExceeded {
                    resource_type: "tool_call".into(),
                    bucket: "per_minute".into(),
                    limit: 60,
                    current: 61,
                    retry_after_seconds: 12,
                }),
                429,
                "RATE_LIMIT_EXCEEDED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::PathOutsideBoundary {
                    path: "/etc/passwd".into(),
                    allowed_paths: vec!["/workspace".into()],
                }),
                403,
                "PATH_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::PathTraversalAttempt {
                    path: "../x".into(),
                }),
                403,
                "PATH_TRAVERSAL",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::DomainNotAllowed {
                    domain: "evil.example".into(),
                    allowed_domains: vec![],
                }),
                403,
                "DOMAIN_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::MissingRequiredArgument("path".into())),
                403,
                "POLICY_ARGUMENT_REQUIRED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::CommandNotAllowed {
                    command: "rm".into(),
                    allowed_commands: vec![],
                }),
                403,
                "COMMAND_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::SubcommandNotAllowed {
                    command: "git".into(),
                    subcommand: "push".into(),
                    allowed_subcommands: vec![],
                }),
                403,
                "SUBCOMMAND_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                policy(PolicyViolation::ConcurrentExecLimitExceeded {
                    limit: 1,
                    active: 2,
                }),
                403,
                "LIMIT_EXCEEDED",
                "policy_violation",
            ),
            (
                SealSessionError::InvalidArguments("required field 'path' is missing".into()),
                422,
                "INVALID_ARGUMENTS",
                "error",
            ),
            (
                SealSessionError::NotFound("approval request 1".into()),
                404,
                "NOT_FOUND",
                "error",
            ),
            (
                SealSessionError::TenantMismatch {
                    authenticated: "u-1".into(),
                    requested: "u-2".into(),
                },
                403,
                "TENANT_MISMATCH",
                "error",
            ),
            (
                SealSessionError::InternalError("Database error: Mk7-db-text".into()),
                500,
                "INTERNAL_ERROR",
                "error",
            ),
            (
                SealSessionError::JudgeTimeout("Mk7-judge-agent".into()),
                500,
                "INTERNAL_ERROR",
                "error",
            ),
            (
                SealSessionError::ConfigurationError("seal_gateway.url is not configured".into()),
                503,
                "SERVICE_UNAVAILABLE",
                "error",
            ),
            (
                SealSessionError::UpstreamUnavailable("Brave API returned 429".into()),
                502,
                "UPSTREAM_UNAVAILABLE",
                "error",
            ),
        ];
        let answered = |a: CallerAnswer| {
            SealSessionError::InternalError("Mk7-shown-detail".into()).answered(a)
        };
        let mut cases = cases;
        cases.extend([
            (
                answered(CallerAnswer::InvalidArguments("bad".into())),
                422,
                "INVALID_ARGUMENTS",
                "error",
            ),
            (
                answered(CallerAnswer::NotFound("x".into())),
                404,
                "NOT_FOUND",
                "error",
            ),
            (
                answered(CallerAnswer::Conflict("x".into())),
                409,
                "CONFLICT",
                "error",
            ),
            (
                answered(CallerAnswer::QuotaExceeded("x".into())),
                422,
                "QUOTA_EXCEEDED",
                "error",
            ),
            (
                answered(CallerAnswer::JudgeRejected("x".into())),
                403,
                "JUDGE_REJECTED",
                "policy_violation",
            ),
            (
                answered(CallerAnswer::PathNotAllowed("x".into())),
                403,
                "PATH_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                answered(CallerAnswer::TenantMismatch("x".into())),
                403,
                "TENANT_MISMATCH",
                "error",
            ),
            (
                answered(CallerAnswer::ContextNotAllowed),
                403,
                "CONTEXT_NOT_ALLOWED",
                "policy_violation",
            ),
            (
                answered(CallerAnswer::NotImplemented("x".into())),
                501,
                "NOT_IMPLEMENTED",
                "error",
            ),
            (
                answered(CallerAnswer::EdgeUnavailable("x".into())),
                503,
                "EDGE_UNAVAILABLE",
                "error",
            ),
            (
                answered(CallerAnswer::Internal(InternalFailure::Server)),
                500,
                "INTERNAL_ERROR",
                "error",
            ),
            (
                answered(CallerAnswer::Internal(InternalFailure::Upstream)),
                502,
                "UPSTREAM_UNAVAILABLE",
                "error",
            ),
            (
                answered(CallerAnswer::Internal(InternalFailure::Unavailable)),
                503,
                "SERVICE_UNAVAILABLE",
                "error",
            ),
        ]);
        for (error, http_status, code, status) in cases {
            let refusal = error.refusal();
            assert_eq!(refusal.http_status, http_status, "{error:?}");
            assert_eq!(refusal.code, code, "{error:?}");
            assert_eq!(refusal.status, status, "{error:?}");
            let internal = matches!(
                code,
                "INTERNAL_ERROR" | "UPSTREAM_UNAVAILABLE" | "SERVICE_UNAVAILABLE"
            );
            assert_eq!(refusal.internal, internal, "{error:?}");
            assert!(
                !refusal.message.contains("Mk7-"),
                "a refusal's message carried detail the caller did not send: {error:?} -> {}",
                refusal.message
            );
        }
    }

    /// R4: the policy refusal keeps 675984dc's words; a 429 carries ADR-072
    /// §9's numbers.
    #[test]
    fn a_policy_refusal_keeps_its_words_and_a_rate_limit_its_numbers() {
        let refusal = SealSessionError::PolicyViolation(PolicyViolation::ToolNotAllowed {
            tool_name: "aegis.system.info".to_string(),
            allowed_tools: vec!["zaru.*".to_string()],
        })
        .refusal();
        assert_eq!(
            refusal.message,
            "Policy violation: tool 'aegis.system.info' is not allowed; permitted tools: [zaru.*]"
        );
        let limited = SealSessionError::PolicyViolation(PolicyViolation::RateLimitExceeded {
            resource_type: "tool_call".into(),
            bucket: "per_minute".into(),
            limit: 60,
            current: 61,
            retry_after_seconds: 12,
        })
        .refusal();
        assert_eq!(
            limited.rate_limit,
            Some(RateLimitHint {
                limit: 60,
                remaining: 0,
                retry_after_seconds: 12
            })
        );
    }

    /// R6: what the inner loop and the log see of an answered refusal is the
    /// error it was built from, unchanged.
    #[test]
    fn an_answered_refusal_shows_the_error_it_was_built_from() {
        let shown = SealSessionError::InternalError("not found: /aegis/volumes/v/f".into());
        let answered = shown
            .clone()
            .answered(CallerAnswer::NotFound("file 'f' not found".into()));
        assert_eq!(answered.to_string(), shown.to_string());
    }
}
