// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::agent::AgentId;
use crate::domain::execution::ExecutionId;
use crate::domain::security_context::repository::SecurityContextRepository;
use crate::domain::smcp_session::SmcpSession;
use crate::domain::smcp_session_repository::SmcpSessionRepository;
use crate::infrastructure::smcp::attestation::{AttestationRequest, AttestationResponse, AttestationService};
use crate::infrastructure::smcp::envelope::ContextClaims;
use crate::infrastructure::smcp::signature::SecurityTokenIssuer;

pub struct AttestationServiceImpl {
    security_context_repo: Arc<dyn SecurityContextRepository>,
    smcp_session_repo: Arc<dyn SmcpSessionRepository>,
    token_issuer: Arc<SecurityTokenIssuer>,
}

impl AttestationServiceImpl {
    pub fn new(
        security_context_repo: Arc<dyn SecurityContextRepository>,
        smcp_session_repo: Arc<dyn SmcpSessionRepository>,
        token_issuer: Arc<SecurityTokenIssuer>,
    ) -> Self {
        Self {
            security_context_repo,
            smcp_session_repo,
            token_issuer,
        }
    }
}

#[async_trait]
impl AttestationService for AttestationServiceImpl {
    async fn attest(&self, request: AttestationRequest) -> Result<AttestationResponse> {
        let agent_id = AgentId::from_string(&request.agent_id)?;
        let execution_id = ExecutionId(uuid::Uuid::parse_str(&request.execution_id)?);

        // 1. Resolve agent identity and applicable security context.
        // For MVP, we'll try to load "default" context. If it doesn't exist, we error.
        let context_name = "default";
        let security_context = self.security_context_repo
            .find_by_name(context_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Security context '{}' not found for agent {}", context_name, request.agent_id))?;

        // 2. Generate Claims
        let mut claims = ContextClaims {
            agent_id: request.agent_id.clone(),
            execution_id: request.execution_id.clone(),
            security_context: security_context.name.clone(),
            iss: None, // Filled by issuer
            aud: Some(crate::infrastructure::smcp::envelope::AudienceClaim::Single("aegis-orchestrator".to_string())),
            exp: Some(chrono::Utc::now().timestamp() + 3600), // 1 hour expiration
            iat: Some(chrono::Utc::now().timestamp()),
            nbf: None,
        };

        // 3. Issue Token
        let token = self.token_issuer.issue(&mut claims)?;

        // 4. Create and persist SMCP Session
        let session = SmcpSession::new(
            agent_id,
            execution_id,
            request.public_key_pem.into_bytes(),
            token.clone(),
            security_context,
        );
        self.smcp_session_repo.save(session).await?;

        // 5. Return Response
        Ok(AttestationResponse {
            security_token: token,
        })
    }
}
