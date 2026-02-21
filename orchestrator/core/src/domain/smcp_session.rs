// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::mcp::PolicyViolation;
use crate::domain::security_context::SecurityContext;

// We need to define SessionId
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Active,
    Expired,
    Revoked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SmcpSessionError {
    SessionInactive(SessionStatus),
    SessionExpired,
    PolicyViolation(PolicyViolation),
    MalformedPayload,
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

/// Abstract away the cryptographic details so Domain doesn't depend on Infrastructure.
pub trait EnvelopeVerifier {
    fn verify_signature(&self, public_key_bytes: &[u8]) -> Result<(), SmcpSessionError>;
    fn extract_tool_name(&self) -> Option<String>;
    fn extract_arguments(&self) -> Option<serde_json::Value>;
}

/// The lifecycle of an agent's SMCP session
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
    /// Create new session (called during attestation)
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
    
    /// Verify and evaluate a tool call
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
    
    /// Revoke session (called when execution terminates or security incident)
    pub fn revoke(&mut self, reason: String) {
        self.status = SessionStatus::Revoked { reason };
    }
}
