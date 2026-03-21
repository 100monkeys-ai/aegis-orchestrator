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

use crate::domain::smcp_session::{EnvelopeVerifier, SmcpSession, SmcpSessionError};

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
        session: &mut SmcpSession,
        envelope: &impl EnvelopeVerifier,
    ) -> Result<Value, SmcpSessionError> {
        info!("Verifying SMCP envelope for session {}", session.id);

        match session.evaluate_call(envelope) {
            Ok(()) => {
                info!("SMCP envelope verified successfully");
                if let Some(args) = envelope.extract_arguments() {
                    Ok(args)
                } else {
                    Err(SmcpSessionError::MalformedPayload(
                        "missing arguments after envelope verification".to_string(),
                    ))
                }
            }
            Err(e) => {
                warn!("SMCP envelope verification failed: {}", e);
                Err(e)
            }
        }
    }
}

impl Default for SmcpMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentId;
    use crate::domain::execution::ExecutionId;
    use crate::domain::mcp::PolicyViolation;
    use crate::domain::security_context::{Capability, SecurityContext, SecurityContextMetadata};
    use serde_json::json;

    struct DummyEnvelope {
        signature_result: Result<(), SmcpSessionError>,
        tool_name: Option<String>,
        arguments: Option<Value>,
    }

    impl EnvelopeVerifier for DummyEnvelope {
        fn verify_signature(&self, _public_key_bytes: &[u8]) -> Result<(), SmcpSessionError> {
            self.signature_result.clone()
        }

        fn extract_tool_name(&self) -> Option<String> {
            self.tool_name.clone()
        }

        fn extract_arguments(&self) -> Option<Value> {
            self.arguments.clone()
        }
    }

    fn allow_all_context() -> SecurityContext {
        SecurityContext {
            name: "test".to_string(),
            description: "test".to_string(),
            capabilities: vec![Capability {
                tool_pattern: "*".to_string(),
                path_allowlist: None,
                command_allowlist: None,
                subcommand_allowlist: None,
                domain_allowlist: None,
                rate_limit: None,
                max_response_size: None,
            }],
            deny_list: vec![],
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    fn denied_context() -> SecurityContext {
        SecurityContext {
            name: "locked-down".to_string(),
            description: "locked-down".to_string(),
            capabilities: vec![],
            deny_list: vec!["tool.run".to_string()],
            metadata: SecurityContextMetadata {
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                version: 1,
            },
        }
    }

    fn session_with_context(context: SecurityContext) -> SmcpSession {
        SmcpSession::new(
            AgentId::new(),
            ExecutionId::new(),
            vec![1, 2, 3],
            "token".to_string(),
            context,
        )
    }

    #[test]
    fn verify_and_unwrap_returns_only_inner_arguments() {
        let middleware = SmcpMiddleware::new();
        let mut session = session_with_context(allow_all_context());
        let envelope = DummyEnvelope {
            signature_result: Ok(()),
            tool_name: Some("tool.run".to_string()),
            arguments: Some(json!({
                "path": "/workspace/file.txt",
                "flags": ["--check"]
            })),
        };

        let args = middleware
            .verify_and_unwrap(&mut session, &envelope)
            .unwrap();

        assert_eq!(
            args,
            json!({
                "path": "/workspace/file.txt",
                "flags": ["--check"]
            })
        );
        assert!(args.get("security_token").is_none());
        assert!(args.get("signature").is_none());
    }

    #[test]
    fn verify_and_unwrap_propagates_signature_failures() {
        let middleware = SmcpMiddleware::new();
        let mut session = session_with_context(allow_all_context());
        let envelope = DummyEnvelope {
            signature_result: Err(SmcpSessionError::SignatureVerificationFailed(
                "bad signature".to_string(),
            )),
            tool_name: Some("tool.run".to_string()),
            arguments: Some(json!({"path": "/workspace/file.txt"})),
        };

        let error = middleware
            .verify_and_unwrap(&mut session, &envelope)
            .unwrap_err();

        assert_eq!(
            error,
            SmcpSessionError::SignatureVerificationFailed("bad signature".to_string())
        );
    }

    #[test]
    fn verify_and_unwrap_rejects_missing_arguments_as_malformed_payload() {
        let middleware = SmcpMiddleware::new();
        let mut session = session_with_context(allow_all_context());
        let envelope = DummyEnvelope {
            signature_result: Ok(()),
            tool_name: Some("tool.run".to_string()),
            arguments: None,
        };

        let error = middleware
            .verify_and_unwrap(&mut session, &envelope)
            .unwrap_err();

        assert_eq!(
            error,
            SmcpSessionError::MalformedPayload(
                "missing arguments after envelope verification".to_string()
            )
        );
    }

    #[test]
    fn verify_and_unwrap_propagates_policy_violations() {
        let middleware = SmcpMiddleware::new();
        let mut session = session_with_context(denied_context());
        let envelope = DummyEnvelope {
            signature_result: Ok(()),
            tool_name: Some("tool.run".to_string()),
            arguments: Some(json!({"path": "/workspace/file.txt"})),
        };

        let error = middleware
            .verify_and_unwrap(&mut session, &envelope)
            .unwrap_err();

        assert_eq!(
            error,
            SmcpSessionError::PolicyViolation(PolicyViolation::ToolExplicitlyDenied {
                tool_name: "tool.run".to_string(),
            })
        );
    }
}
