// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Attestation Service (BC-12, ADR-035 §4.1)
//!
//! Application service that implements the SMCP attestation ceremony:
//! the one-time handshake by which an agent proves its identity and receives
//! a `SecurityToken` that authorises future MCP tool calls.
//!
//! ## Attestation Flow
//!
//! ```text
//! Agent container
//!   │  AttestationRequest { agent_id, execution_id, public_key_pem }
//!   ▼
//! AttestationServiceImpl.attest()
//!   1. Resolve SecurityContext for execution (loads from registry)
//!   2. Build ContextClaims (agent/exec IDs, 1hr expiry)
//!   3. SecurityTokenIssuer.issue()  →  RS256 JWT signed by orchestrator key
//!   4. Create SmcpSession  (stores session + public key)
//!   5. SmcpSessionRepository.save(session)
//!   ──────────────────────────────────────────────────────────────────────────
//!   AttestationResponse { security_token: JWT }
//!   ▼
//! Agent container  ← uses JWT in every subsequent SmcpEnvelope
//! ```
//!
//! ## Context Resolution
//!
//! Attestation must bind the issued token to a real execution-specific or
//! manifest-specific SMCP `SecurityContext`. If that mapping is unavailable,
//! attestation fails closed rather than minting a token against an inferred
//! default context.
//!
//! See ADR-035 §4.1, AGENTS.md §Attestation.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::application::ports::{AttestationTokenClaims, SecurityTokenIssuerPort, TokenAudience};
use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::security_context::validate_context_ownership;
use crate::domain::smcp_session::SmcpSession;
use crate::domain::smcp_session_repository::SmcpSessionRepository;
use crate::infrastructure::smcp::attestation::{
    AttestationRequest, AttestationResponse, AttestationService,
};
use crate::infrastructure::smcp::envelope::{
    normalize_public_key_bytes, AudienceClaim, ContextClaims,
};
use crate::infrastructure::smcp::signature::SecurityTokenIssuer;

/// Concrete implementation of the SMCP attestation ceremony.
///
/// Orchestrates the three domain/infrastructure dependencies needed to
/// complete a full attestation: context lookup, JWT issuance, and session
/// persistence.
pub struct AttestationServiceImpl {
    security_context_repo: Arc<dyn SecurityContextRepository>,
    smcp_session_repo: Arc<dyn SmcpSessionRepository>,
    token_issuer: Arc<dyn SecurityTokenIssuerPort>,
}

impl AttestationServiceImpl {
    pub fn new(
        security_context_repo: Arc<dyn SecurityContextRepository>,
        smcp_session_repo: Arc<dyn SmcpSessionRepository>,
        token_issuer: Arc<dyn SecurityTokenIssuerPort>,
    ) -> Self {
        Self {
            security_context_repo,
            smcp_session_repo,
            token_issuer,
        }
    }

    fn resolve_security_context_name(&self, request: &AttestationRequest) -> Result<String> {
        if let Some(security_context) = request
            .security_context
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            return Ok(security_context.to_string());
        }

        tracing::warn!(
            agent_id = ?request.agent_id,
            execution_id = ?request.execution_id,
            "Rejecting SMCP attestation because no execution-bound security context can be derived"
        );
        Err(anyhow::anyhow!(
            "SMCP attestation requires an explicit security_context or an execution-bound security context; none can be derived for agent {:?} execution {:?}",
            request.agent_id,
            request.execution_id
        ))
    }

    fn resolve_ids(&self, request: &AttestationRequest) -> Result<(AgentId, ExecutionId)> {
        match (&request.agent_id, &request.execution_id) {
            (Some(agent_id), Some(execution_id)) => Ok((
                AgentId::from_string(agent_id)?,
                ExecutionId(uuid::Uuid::parse_str(execution_id)?),
            )),
            _ if request.security_context.is_some() => Ok((AgentId::new(), ExecutionId::new())),
            _ => Err(anyhow::anyhow!(
                "SMCP attestation requires both agent_id and execution_id unless security_context is explicitly provided"
            )),
        }
    }

    fn resolve_principal_subject(&self, request: &AttestationRequest) -> Option<String> {
        request
            .principal_subject
            .as_ref()
            .or(request.user_id.as_ref())
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }
}

#[async_trait]
impl AttestationService for AttestationServiceImpl {
    async fn attest(&self, request: AttestationRequest) -> Result<AttestationResponse> {
        let result = self.attest_inner(request).await;
        if result.is_err() {
            metrics::counter!("aegis_smcp_attestations_total", "result" => "failure").increment(1);
        }
        result
    }
}

impl AttestationServiceImpl {
    async fn attest_inner(&self, request: AttestationRequest) -> Result<AttestationResponse> {
        let (agent_id, execution_id) = self.resolve_ids(&request)?;

        // 1. Resolve agent identity and applicable security context.
        let context_name = self.resolve_security_context_name(&request)?;

        // 1b. Enforce tenant ownership of the SecurityContext name (ADR-056 Phase 5).
        validate_context_ownership(&context_name, &request.tenant_id).map_err(|msg| {
            tracing::warn!(
                context_name = %context_name,
                tenant_id = %request.tenant_id,
                "SecurityContext ownership violation: {msg}"
            );
            anyhow::anyhow!("SecurityContext ownership violation: {msg}")
        })?;

        let security_context = self
            .security_context_repo
            .find_by_name(&context_name)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Security context '{}' not found for agent {:?}",
                    context_name,
                    request.agent_id
                )
            })?;

        // 2. Generate Claims
        let mut claims = AttestationTokenClaims {
            agent_id: agent_id.0.to_string(),
            execution_id: execution_id.0.to_string(),
            security_context: security_context.name.clone(),
            iss: None, // Filled by issuer
            aud: Some(TokenAudience::Single("aegis-agents".to_string())),
            exp: Some(chrono::Utc::now().timestamp() + 3600), // 1 hour expiration
            iat: Some(chrono::Utc::now().timestamp()),
            nbf: None,
        };

        // 3. Issue Token
        let token = self.token_issuer.issue(&mut claims)?;

        // 4. Create and persist SMCP Session
        let principal_subject = self.resolve_principal_subject(&request);
        let session = SmcpSession::new(
            agent_id,
            execution_id,
            normalize_public_key_bytes(&request.public_key_pem)?,
            token.clone(),
            security_context,
        )
        .with_principal_metadata(
            principal_subject.clone(),
            request.user_id.clone(),
            request.workload_id.clone(),
            request.zaru_tier.clone(),
        );
        self.smcp_session_repo.save(session).await?;

        tracing::info!(
            agent_id = %claims.agent_id,
            execution_id = %claims.execution_id,
            security_context = %claims.security_context,
            principal_subject = principal_subject.as_deref().unwrap_or(""),
            user_id = request.user_id.as_deref().unwrap_or(""),
            workload_id = request.workload_id.as_deref().unwrap_or(""),
            zaru_tier = request.zaru_tier.as_deref().unwrap_or(""),
            "Issued SMCP session during attestation"
        );

        // 5. Emit metrics (ADR-058 BC-4)
        metrics::counter!("aegis_smcp_attestations_total", "result" => "success").increment(1);
        metrics::gauge!("aegis_smcp_sessions_active").increment(1.0);

        // 6. Return Response
        Ok(AttestationResponse {
            security_token: token,
        })
    }
}

impl SecurityTokenIssuerPort for SecurityTokenIssuer {
    fn issue(&self, claims: &mut AttestationTokenClaims) -> Result<String> {
        let aud = claims.aud.clone().map(|a| match a {
            TokenAudience::Single(s) => AudienceClaim::Single(s),
            TokenAudience::Multiple(v) => AudienceClaim::Multiple(v),
        });

        let mut inner_claims = ContextClaims {
            agent_id: claims.agent_id.clone(),
            execution_id: claims.execution_id.clone(),
            security_context: claims.security_context.clone(),
            iss: claims.iss.clone(),
            aud,
            exp: claims.exp,
            iat: claims.iat,
            nbf: claims.nbf,
        };

        SecurityTokenIssuer::issue(self, &mut inner_claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::security_context::capability::Capability;
    use crate::domain::security_context::{SecurityContext, SecurityContextMetadata};
    use crate::domain::tenant::TenantId;
    use crate::infrastructure::security_context::InMemorySecurityContextRepository;
    use crate::infrastructure::smcp::session_repository::InMemorySmcpSessionRepository;
    use crate::infrastructure::smcp::signature::SecurityTokenVerifier;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;

    const TEST_RSA_PRIVATE_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAmWtpvUNARl+B9DenjbtDMcwfwkX4k7xYgkbLBJ7ON2VUPEfx\nHfOe50KqxX6AJzvHIaEWyOPM/J4YYIzO12nNzjKRElPSp5PDDigKYJePhxPl1bQn\nrY2A/L1GaVWx2rDjZqtldjJiuOI6CdsDT+GF+Twd1O4H2OMhYk6iATQqGzJQxKnd\nHEMdQqFa2NhDpuyEl9xhcUUVUboQR0+a8hfdoNTqhedK2ImTQ0JDFwt5e1c/XCLT\nj5PWfKJeHxqBYrt2hPgo8fjE0S6BX2fCOqUQ//4kPyI0ik5AZAOZ0o2RSEZn0Gei\nW3HiUl0kIMDuIMD12AMjzN5ePcHcl39zq96syQIDAQABAoIBAAEnNkNJUYPRDSzj\n6N6BEZeAp5WrVdIEhQLiR0dJXqhJ/4qD+CkWzpr2J0Lv6qmXIqYaLub+UzqqJBgp\nFdGIsFyK9T6egbTnilWcitSEXqM0zMdltix03/PQE4y+5bo/FkAvT3EEe5Kx4o8/\n64SDhqjwM3e/eRGRAJQVzOuiAIB5oy2JdDxa0JZXHU8ilKahu2GjpBAGajLD5T17\nZjHKsIfLJAQSqfxfCMnBIhqLVlUuWDoEIoBKv6bGHC7D6ElxvZRpb9JFuuigs/l5\n8rg+R7bv+7Uz9P0FVyyLFRt5puQJa1SuwgHhfK0KDnssWbeJhVXvmeSa3Z2cl0Wp\nbWT/XgECgYEA0iCyFhn3hnLlXBJHZGlTm/6qJpcSX9fIoLKMm1/GEXHJqSqyhWdE\nC7vJOkySHbNQ36sxxI+P2DteaEZMMwimzNFmw7Em1g334eTmXAhr/1qrFWzjysTN\nJWlsDfh7uDg/RO52P0kK723uvIrh82lf5Dva3wt99TH/R3TzLKXNbEsCgYEAuul/\nbE4glHKI9v4OZowrhBMnNCjpHMzS0aMLKpsu07ZVPn1HKnqxtt4IioiHQ9O0UcV6\nbXSYLhf42VxJYZ4xQ7uDGeB0Z84Pkd+d1S7ughV7QgweaIHmfAQAg+iSolOlcvyz\nM58zShVXiSaqzNp75Ai1tjkbuo/HWgLwvIDydrsCgYEAkwQXNYlzepkWykVrt+BN\nhD44lAls7KvQDkb+Q5NNxFTFkFt0TgwDOuZnEygRr0APnH5tsqXzMYnQMsrEc4xh\nD7qO2OowTuG1BlKdrdSioyWvv6zQ78Sj98H7vQaWoTyRX8wr5XlYck6LE1VkY2bd\nlZUfPKEQvqX9guRbY2iaAmMCgYA5Ptpv6V3BGXMpcpYmgjexs8wGBaGf2HuZCT6a\nRf0JioaBJQ1uzTUwtMAY7ce/1k8b3EeqzlLtixoEOGehJjogbIWynzQHtuy92KcW\na9FQthOSHvQRPffBc9hUjh6a6NN7bDnWTaP/xJmSv+z/4MqhBKnirYr4kKCVyODC\nWxvnkQKBgQDAL4bBoWRBtJJHLmMMgweY421W497kl4BvAiur36WT99fknp5ktqRU\nPxTp4+a+lU1gc393kfJvUeIVYX1vJs0tS+YkNVpCrC5hBmVaemd5Vav1q13+/sZ/\ncpc0iRy0EDCDXsAbf/guJdqShW1x1cB1moHFiM+8FsM80SsAZavjnQ==\n-----END RSA PRIVATE KEY-----";
    const TEST_RSA_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAmWtpvUNARl+B9DenjbtD\nMcwfwkX4k7xYgkbLBJ7ON2VUPEfxHfOe50KqxX6AJzvHIaEWyOPM/J4YYIzO12nN\nzjKRElPSp5PDDigKYJePhxPl1bQnrY2A/L1GaVWx2rDjZqtldjJiuOI6CdsDT+GF\n+Twd1O4H2OMhYk6iATQqGzJQxKndHEMdQqFa2NhDpuyEl9xhcUUVUboQR0+a8hfd\noNTqhedK2ImTQ0JDFwt5e1c/XCLTj5PWfKJeHxqBYrt2hPgo8fjE0S6BX2fCOqUQ\n//4kPyI0ik5AZAOZ0o2RSEZn0GeiW3HiUl0kIMDuIMD12AMjzN5ePcHcl39zq96s\nyQIDAQAB\n-----END PUBLIC KEY-----";

    fn test_context(name: &str) -> SecurityContext {
        SecurityContext {
            name: name.to_string(),
            description: "test context".to_string(),
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

    #[tokio::test]
    async fn attest_accepts_explicit_security_context_without_agent_or_execution_ids() {
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(test_context("zaru-pro"))
            .await
            .unwrap();
        let smcp_session_repo = Arc::new(InMemorySmcpSessionRepository::new());
        let issuer =
            Arc::new(SecurityTokenIssuer::new(TEST_RSA_PRIVATE_PEM, "aegis-orchestrator").unwrap());
        let verifier = SecurityTokenVerifier::new(
            TEST_RSA_PUBLIC_PEM,
            "aegis-orchestrator",
            &["aegis-agents"],
        )
        .unwrap();
        let service =
            AttestationServiceImpl::new(security_context_repo, smcp_session_repo.clone(), issuer);

        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let response = service
            .attest(AttestationRequest {
                agent_id: None,
                execution_id: None,
                container_id: None,
                public_key_pem: STANDARD.encode(signing_key.verifying_key().as_bytes()),
                security_context: Some("zaru-pro".to_string()),
                principal_subject: Some("user-123".to_string()),
                user_id: Some("user-123".to_string()),
                workload_id: Some("librechat-session-42".to_string()),
                zaru_tier: Some("pro".to_string()),
                tenant_id: TenantId::consumer(),
            })
            .await
            .unwrap();

        let token = verifier.verify(&response.security_token).unwrap();
        assert_eq!(token.claims.security_context, "zaru-pro");
        assert!(uuid::Uuid::parse_str(&token.claims.agent_id).is_ok());
        assert!(uuid::Uuid::parse_str(&token.claims.execution_id).is_ok());
        let saved_session = smcp_session_repo
            .find_active_by_agent(&AgentId::from_string(&token.claims.agent_id).unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved_session.principal_subject.as_deref(), Some("user-123"));
        assert_eq!(saved_session.user_id.as_deref(), Some("user-123"));
        assert_eq!(
            saved_session.workload_id.as_deref(),
            Some("librechat-session-42")
        );
        assert_eq!(saved_session.zaru_tier.as_deref(), Some("pro"));
    }

    #[tokio::test]
    async fn attest_rejects_unknown_explicit_security_context() {
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        let smcp_session_repo = Arc::new(InMemorySmcpSessionRepository::new());
        let issuer =
            Arc::new(SecurityTokenIssuer::new(TEST_RSA_PRIVATE_PEM, "aegis-orchestrator").unwrap());
        let service = AttestationServiceImpl::new(security_context_repo, smcp_session_repo, issuer);

        let response = service
            .attest(AttestationRequest {
                agent_id: None,
                execution_id: None,
                container_id: None,
                public_key_pem: STANDARD.encode([1u8; 32]),
                security_context: Some("zaru-enterprise".to_string()),
                principal_subject: Some("user-456".to_string()),
                user_id: Some("user-456".to_string()),
                workload_id: Some("librechat-session-99".to_string()),
                zaru_tier: Some("enterprise".to_string()),
                tenant_id: TenantId::consumer(),
            })
            .await;

        assert!(response.is_err());
    }

    #[tokio::test]
    async fn attest_rejects_zaru_context_for_non_consumer_tenant() {
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(test_context("zaru-pro"))
            .await
            .unwrap();
        let smcp_session_repo = Arc::new(InMemorySmcpSessionRepository::new());
        let issuer =
            Arc::new(SecurityTokenIssuer::new(TEST_RSA_PRIVATE_PEM, "aegis-orchestrator").unwrap());
        let service = AttestationServiceImpl::new(security_context_repo, smcp_session_repo, issuer);

        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let response = service
            .attest(AttestationRequest {
                agent_id: None,
                execution_id: None,
                container_id: None,
                public_key_pem: STANDARD.encode(signing_key.verifying_key().as_bytes()),
                security_context: Some("zaru-pro".to_string()),
                principal_subject: None,
                user_id: None,
                workload_id: None,
                zaru_tier: None,
                tenant_id: TenantId::system(),
            })
            .await;

        assert!(response.is_err());
        let err_msg = response.unwrap_err().to_string();
        assert!(
            err_msg.contains("ownership violation"),
            "Expected ownership violation error, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn attest_rejects_legacy_bare_context_name() {
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(test_context("research-safe"))
            .await
            .unwrap();
        let smcp_session_repo = Arc::new(InMemorySmcpSessionRepository::new());
        let issuer =
            Arc::new(SecurityTokenIssuer::new(TEST_RSA_PRIVATE_PEM, "aegis-orchestrator").unwrap());
        let service = AttestationServiceImpl::new(security_context_repo, smcp_session_repo, issuer);

        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let response = service
            .attest(AttestationRequest {
                agent_id: None,
                execution_id: None,
                container_id: None,
                public_key_pem: STANDARD.encode(signing_key.verifying_key().as_bytes()),
                security_context: Some("research-safe".to_string()),
                principal_subject: None,
                user_id: None,
                workload_id: None,
                zaru_tier: None,
                tenant_id: TenantId::consumer(),
            })
            .await;

        assert!(response.is_err());
        let err_msg = response.unwrap_err().to_string();
        assert!(
            err_msg.contains("tenant-namespaced naming convention"),
            "Expected legacy name rejection, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn attest_rejects_tenant_context_for_wrong_tenant() {
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(test_context("tenant-acme-corp-research"))
            .await
            .unwrap();
        let smcp_session_repo = Arc::new(InMemorySmcpSessionRepository::new());
        let issuer =
            Arc::new(SecurityTokenIssuer::new(TEST_RSA_PRIVATE_PEM, "aegis-orchestrator").unwrap());
        let service = AttestationServiceImpl::new(security_context_repo, smcp_session_repo, issuer);

        // Use a different tenant slug
        let wrong_tenant = TenantId::from_realm_slug("other-corp").unwrap();
        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let response = service
            .attest(AttestationRequest {
                agent_id: None,
                execution_id: None,
                container_id: None,
                public_key_pem: STANDARD.encode(signing_key.verifying_key().as_bytes()),
                security_context: Some("tenant-acme-corp-research".to_string()),
                principal_subject: None,
                user_id: None,
                workload_id: None,
                zaru_tier: None,
                tenant_id: wrong_tenant,
            })
            .await;

        assert!(response.is_err());
        let err_msg = response.unwrap_err().to_string();
        assert!(
            err_msg.contains("ownership violation"),
            "Expected ownership violation, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn attest_accepts_tenant_context_for_correct_tenant() {
        let security_context_repo = Arc::new(InMemorySecurityContextRepository::new());
        security_context_repo
            .save(test_context("tenant-acme-corp-research"))
            .await
            .unwrap();
        let smcp_session_repo = Arc::new(InMemorySmcpSessionRepository::new());
        let issuer =
            Arc::new(SecurityTokenIssuer::new(TEST_RSA_PRIVATE_PEM, "aegis-orchestrator").unwrap());
        let service =
            AttestationServiceImpl::new(security_context_repo, smcp_session_repo.clone(), issuer);

        let correct_tenant = TenantId::from_realm_slug("acme-corp").unwrap();
        let signing_key = SigningKey::from_bytes(&[9u8; 32]);
        let response = service
            .attest(AttestationRequest {
                agent_id: None,
                execution_id: None,
                container_id: None,
                public_key_pem: STANDARD.encode(signing_key.verifying_key().as_bytes()),
                security_context: Some("tenant-acme-corp-research".to_string()),
                principal_subject: None,
                user_id: None,
                workload_id: None,
                zaru_tier: None,
                tenant_id: correct_tenant,
            })
            .await;

        assert!(response.is_ok());
    }
}
