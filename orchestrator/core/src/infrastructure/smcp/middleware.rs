// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # SMCP Middleware (ADR-035 §4.3)
//!
//! The orchestrator-side middleware that intercepts every MCP tool call from
//! agent containers, verifies the SMCP envelope, and extracts the inner MCP
//! payload for forwarding to the appropriate tool server.
//!
//! ## Processing Pipeline
//!
//! ```text
//! incoming SmcpEnvelope
//!   └─ SmcpMiddleware::verify_and_unwrap(&session, &envelope)
//!         └─ SmcpSession::evaluate_call(envelope)   ← all checks
//!         └─ envelope.extract_arguments()            ← inner payload
//!               └─ forwarded to ToolRouter / MCP server
//! ```
//!
//! This component sits between the agent ingress (HTTP/gRPC) handler and the
//! `ToolRouter`. It must be invoked for **every** tool call, without exception.
use serde_json::Value;
use tracing::{info, warn};

use crate::domain::smcp_session::{SmcpSession, EnvelopeVerifier, SmcpSessionError};

/// Orchestrator middleware that verifies and unwraps incoming SMCP envelopes.
///
/// Stateless: all session state lives in the `SmcpSession` passed per invocation.
/// A singleton instance should be created at startup and shared across request
/// handlers.
pub struct SmcpMiddleware;

impl SmcpMiddleware {
    /// Create a new middleware instance.
    pub fn new() -> Self {
        Self
    }

    /// Verify the envelope against the given session and extract the inner MCP arguments.
    ///
    /// This is the **single choke-point** through which all MCP tool calls must pass.
    /// Calls [`crate::domain::smcp_session::SmcpSession::evaluate_call`] which enforces
    /// session status, TTL, Ed25519 signature, and `SecurityContext` policy in order.
    ///
    /// On success, returns the parsed arguments `Value` to be forwarded to the tool server.
    ///
    /// # Errors
    ///
    /// Returns a [`crate::domain::smcp_session::SmcpSessionError`] variant on any
    /// enforcement failure. The caller should log the violation and return an MCP
    /// error response to the agent (do **not** forward the call).
    ///
    /// # Security
    ///
    /// The returned `Value` contains only the tool arguments stripped of the SMCP
    /// wrapper. The `security_token` and `signature` fields are never forwarded to
    /// the tool server, preserving credential isolation (ADR-033).
    pub fn verify_and_unwrap(
        &self, 
        session: &SmcpSession, 
        envelope: &impl EnvelopeVerifier
    ) -> Result<Value, SmcpSessionError> {
        info!("Verifying SMCP envelope for session {}", session.id);
        
        match session.evaluate_call(envelope) {
            Ok(()) => {
                info!("SMCP envelope verified successfully");
                if let Some(args) = envelope.extract_arguments() {
                    Ok(args)
                } else {
                    Err(SmcpSessionError::MalformedPayload)
                }
            }
            Err(e) => {
                warn!("SMCP envelope verification failed: {}", e);
                Err(e)
            }
        }
    }
}
