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
use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use crate::domain::mcp::PolicyViolation;
use crate::domain::rate_limit::{
    RateLimitEnforcer, RateLimitPolicyResolver, RateLimitResourceType, RateLimitScope,
};
use crate::domain::smcp_session::{EnvelopeVerifier, SmcpSession, SmcpSessionError};

/// Orchestrator middleware that verifies and unwraps incoming SMCP envelopes.
///
/// Holds optional rate-limit collaborators. When both `rate_limit_enforcer` and
/// `rate_limit_resolver` are `Some`, the middleware performs an ADR-072 rate limit
/// check after the `SecurityContext` policy evaluation succeeds.
pub struct SmcpMiddleware {
    rate_limit_enforcer: Option<Arc<dyn RateLimitEnforcer>>,
    rate_limit_resolver: Option<Arc<dyn RateLimitPolicyResolver>>,
}

impl SmcpMiddleware {
    /// Create a new middleware instance without rate limiting.
    pub fn new() -> Self {
        Self {
            rate_limit_enforcer: None,
            rate_limit_resolver: None,
        }
    }

    /// Create a new middleware instance with optional rate limiting support.
    pub fn with_rate_limiting(
        rate_limit_enforcer: Option<Arc<dyn RateLimitEnforcer>>,
        rate_limit_resolver: Option<Arc<dyn RateLimitPolicyResolver>>,
    ) -> Self {
        Self {
            rate_limit_enforcer,
            rate_limit_resolver,
        }
    }

    /// Verify the envelope against the given session and extract the inner MCP arguments.
    ///
    /// This is the **single choke-point** through which all MCP tool calls must pass.
    /// Calls [`crate::domain::smcp_session::SmcpSession::evaluate_call`] which enforces
    /// session status, TTL, Ed25519 signature, and `SecurityContext` policy in order.
    ///
    /// When rate limiting is configured, an additional ADR-072 rate limit check is
    /// performed after the policy evaluation succeeds. If the rate limit is exceeded,
    /// a `PolicyViolation::RateLimitExceeded` error is returned.
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
    pub async fn verify_and_unwrap(
        &self,
        session: &mut SmcpSession,
        envelope: &(impl EnvelopeVerifier + Send + Sync),
    ) -> Result<Value, SmcpSessionError> {
        info!("Verifying SMCP envelope for session {}", session.id);

        match session.evaluate_call(envelope) {
            Ok(()) => {
                info!("SMCP envelope verified successfully");

                // ADR-072: Rate limit check after policy evaluation succeeds.
                if let (Some(enforcer), Some(resolver)) =
                    (&self.rate_limit_enforcer, &self.rate_limit_resolver)
                {
                    let tool_name = envelope
                        .extract_tool_name()
                        .unwrap_or_else(|| "unknown".to_string());

                    if let Err(violation) =
                        check_rate_limit(&tool_name, enforcer.as_ref(), session, resolver.as_ref())
                            .await
                    {
                        warn!(
                            "Rate limit exceeded for tool '{}': {:?}",
                            tool_name, violation
                        );
                        metrics::counter!(
                            "aegis_smcp_policy_violations_total",
                            "violation_type" => "rate_limit_exceeded"
                        )
                        .increment(1);
                        return Err(SmcpSessionError::PolicyViolation(violation));
                    }
                }

                if let Some(args) = envelope.extract_arguments() {
                    Ok(args)
                } else {
                    Err(SmcpSessionError::MalformedPayload(
                        "missing arguments after envelope verification".to_string(),
                    ))
                }
            }
            Err(ref e) => {
                warn!("SMCP envelope verification failed: {}", e);

                // Emit ADR-058 BC-4 metrics for specific failure categories.
                match e {
                    SmcpSessionError::SignatureVerificationFailed(_) => {
                        metrics::counter!("aegis_smcp_signature_failures_total").increment(1);
                    }
                    SmcpSessionError::PolicyViolation(violation) => {
                        let violation_type = match violation {
                            PolicyViolation::ToolNotAllowed { .. } => "tool_not_allowed",
                            PolicyViolation::ToolExplicitlyDenied { .. } => {
                                "tool_explicitly_denied"
                            }
                            PolicyViolation::RateLimitExceeded { .. } => "rate_limit_exceeded",
                            PolicyViolation::PathOutsideBoundary { .. } => "path_outside_boundary",
                            PolicyViolation::PathTraversalAttempt { .. } => {
                                "path_traversal_attempt"
                            }
                            PolicyViolation::DomainNotAllowed { .. } => "domain_not_allowed",
                            PolicyViolation::MissingRequiredArgument(_) => {
                                "missing_required_argument"
                            }
                            PolicyViolation::TimeoutExceeded { .. } => "timeout_exceeded",
                        };
                        metrics::counter!("aegis_smcp_policy_violations_total", "violation_type" => violation_type).increment(1);
                    }
                    _ => {}
                }

                Err(e.clone())
            }
        }
    }
}

/// Check rate limits for an SMCP tool call (ADR-072).
///
/// Resolves the effective policy for the tool's resource type, then checks and
/// increments the counter. Returns `Ok(())` if the call is within limits.
async fn check_rate_limit(
    tool_name: &str,
    enforcer: &dyn RateLimitEnforcer,
    session: &SmcpSession,
    resolver: &dyn RateLimitPolicyResolver,
) -> Result<(), PolicyViolation> {
    use crate::domain::iam::{IdentityKind, UserIdentity, ZaruTier};

    let resource_type = RateLimitResourceType::SmcpToolCall {
        tool_pattern: tool_name.to_string(),
    };

    // Build a minimal UserIdentity from the session metadata for policy resolution.
    let user_id = session
        .user_id
        .clone()
        .unwrap_or_else(|| session.agent_id.to_string());
    let tier = session
        .zaru_tier
        .as_deref()
        .and_then(|t| {
            serde_json::from_value::<ZaruTier>(serde_json::Value::String(t.to_string())).ok()
        })
        .unwrap_or(ZaruTier::Free);
    let identity = UserIdentity {
        sub: user_id.clone(),
        realm_slug: "zaru-consumer".to_string(),
        email: None,
        identity_kind: IdentityKind::ConsumerUser { zaru_tier: tier },
    };

    // Use a default tenant for now; in production the session would carry the tenant.
    let tenant_id = crate::domain::tenant::TenantId::consumer();

    let policy = resolver
        .resolve_policy(&identity, &tenant_id, &resource_type)
        .await
        .map_err(|_e| PolicyViolation::RateLimitExceeded {
            resource_type: format!("{:?}", resource_type),
            bucket: "unknown".into(),
            limit: 0,
            current: 0,
            retry_after_seconds: 60,
        })?;

    let scope = RateLimitScope::User { user_id };

    let decision = enforcer
        .check_and_increment(&scope, &policy, 1)
        .await
        .map_err(|_e| PolicyViolation::RateLimitExceeded {
            resource_type: format!("{:?}", policy.resource_type),
            bucket: "unknown".into(),
            limit: 0,
            current: 0,
            retry_after_seconds: 60,
        })?;

    if !decision.allowed {
        return Err(PolicyViolation::RateLimitExceeded {
            resource_type: format!("{:?}", policy.resource_type),
            bucket: decision
                .exhausted_bucket
                .map(|b| format!("{b:?}"))
                .unwrap_or_default(),
            limit: decision.remaining.values().next().copied().unwrap_or(0),
            current: 0,
            retry_after_seconds: decision.retry_after_seconds.unwrap_or(60),
        });
    }

    Ok(())
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
        fn security_token(&self) -> &str {
            "token"
        }

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

    #[tokio::test]
    async fn verify_and_unwrap_returns_only_inner_arguments() {
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
            .await
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

    #[tokio::test]
    async fn verify_and_unwrap_propagates_signature_failures() {
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
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SmcpSessionError::SignatureVerificationFailed("bad signature".to_string())
        );
    }

    #[tokio::test]
    async fn verify_and_unwrap_rejects_missing_arguments_as_malformed_payload() {
        let middleware = SmcpMiddleware::new();
        let mut session = session_with_context(allow_all_context());
        let envelope = DummyEnvelope {
            signature_result: Ok(()),
            tool_name: Some("tool.run".to_string()),
            arguments: None,
        };

        let error = middleware
            .verify_and_unwrap(&mut session, &envelope)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SmcpSessionError::MalformedPayload("missing arguments".to_string())
        );
    }

    #[tokio::test]
    async fn verify_and_unwrap_propagates_policy_violations() {
        let middleware = SmcpMiddleware::new();
        let mut session = session_with_context(denied_context());
        let envelope = DummyEnvelope {
            signature_result: Ok(()),
            tool_name: Some("tool.run".to_string()),
            arguments: Some(json!({"path": "/workspace/file.txt"})),
        };

        let error = middleware
            .verify_and_unwrap(&mut session, &envelope)
            .await
            .unwrap_err();

        assert_eq!(
            error,
            SmcpSessionError::PolicyViolation(PolicyViolation::ToolExplicitlyDenied {
                tool_name: "tool.run".to_string(),
            })
        );
    }
}
