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
//! ## Phase Note
//!
//! ⚠️ Phase 1 — Context resolution hard-codes `"default"` as the security context
//! name. Phase 2 will look up the context name from the agent manifest via the
//! `ExecutionRepository`.
//!
//! See ADR-035 §4.1, AGENTS.md §Attestation.

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

/// Concrete implementation of the SMCP attestation ceremony.
///
/// Orchestrates the three domain/infrastructure dependencies needed to
/// complete a full attestation: context lookup, JWT issuance, and session
/// persistence.
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
