// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — validate enrollment JWT, redeem `jti`, persist EdgeDaemon.
//!
//! Invoked from `ChallengeNode` when `bootstrap_proof::EnrollmentToken` is
//! present and the role is `Edge`. Returns the parsed claims so the caller
//! can mint a `NodeSecurityToken` carrying `tid`/`cep`.

use anyhow::{anyhow, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use std::sync::Arc;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::edge::{
    EdgeCapabilities, EdgeConnectionState, EdgeDaemon, EdgeDaemonRepository, EnrollmentTokenClaims,
    EnrollmentTokenError, EnrollmentTokenRepository,
};
use crate::domain::secrets::SecretStore;
use crate::domain::shared_kernel::{NodeId, TenantId};

#[derive(thiserror::Error, Debug)]
pub enum EnrollEdgeError {
    #[error("invalid enrollment token format")]
    InvalidTokenFormat,
    #[error("token signature invalid")]
    InvalidSignature,
    #[error("token expired or not yet valid")]
    Expired,
    #[error("token audience mismatch (expected edge-enrollment)")]
    AudienceMismatch,
    #[error("token already redeemed")]
    AlreadyRedeemed,
    #[error("invalid tenant id: {0}")]
    InvalidTenant(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct EnrollEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    token_repo: Arc<dyn EnrollmentTokenRepository>,
    secret_store: Arc<dyn SecretStore>,
    signing_key_path: String,
    expected_issuer: String,
}

impl EnrollEdgeService {
    pub fn new(
        edge_repo: Arc<dyn EdgeDaemonRepository>,
        token_repo: Arc<dyn EnrollmentTokenRepository>,
        secret_store: Arc<dyn SecretStore>,
        signing_key_path: String,
        expected_issuer: String,
    ) -> Self {
        Self {
            edge_repo,
            token_repo,
            secret_store,
            signing_key_path,
            expected_issuer,
        }
    }

    /// Validate the JWT, atomically redeem its `jti`, persist a fresh
    /// EdgeDaemon row, and return the parsed claims.
    pub async fn enroll(
        &self,
        token: &str,
        node_id: NodeId,
        public_key: Vec<u8>,
        capabilities: EdgeCapabilities,
    ) -> Result<EnrollmentTokenClaims, EnrollEdgeError> {
        let claims = self.verify_and_parse(token).await?;

        // Audience check.
        if claims.aud != "edge-enrollment" {
            return Err(EnrollEdgeError::AudienceMismatch);
        }
        // Issuer check.
        if claims.iss != self.expected_issuer {
            return Err(EnrollEdgeError::Other(anyhow!(
                "issuer mismatch: got {}, expected {}",
                claims.iss,
                self.expected_issuer
            )));
        }
        // Time window.
        let now = Utc::now().timestamp();
        if claims.exp <= now || claims.nbf > now {
            return Err(EnrollEdgeError::Expired);
        }

        let tenant_id = TenantId::new(claims.tid.clone())
            .map_err(|e| EnrollEdgeError::InvalidTenant(e.to_string()))?;

        // One-time use: atomic INSERT.
        let exp_at: DateTime<Utc> = DateTime::from_timestamp(claims.exp, 0)
            .ok_or_else(|| EnrollEdgeError::Other(anyhow!("invalid exp timestamp")))?;
        match self
            .token_repo
            .redeem(claims.jti, &tenant_id, &claims.sub, exp_at)
            .await
        {
            Ok(()) => {}
            Err(EnrollmentTokenError::AlreadyRedeemed) => {
                return Err(EnrollEdgeError::AlreadyRedeemed)
            }
            Err(EnrollmentTokenError::NotFound) => {
                return Err(EnrollEdgeError::Other(anyhow!("token not found")))
            }
            Err(EnrollmentTokenError::Other(e)) => return Err(EnrollEdgeError::Other(e)),
        }

        let edge = EdgeDaemon {
            node_id,
            tenant_id: tenant_id.clone(),
            public_key,
            capabilities,
            status: NodePeerStatus::Active,
            connection: EdgeConnectionState::Disconnected { since: Utc::now() },
            last_heartbeat_at: None,
            enrolled_at: Utc::now(),
        };
        self.edge_repo.upsert(&edge).await?;
        Ok(claims)
    }

    async fn verify_and_parse(
        &self,
        token: &str,
    ) -> Result<EnrollmentTokenClaims, EnrollEdgeError> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 {
            return Err(EnrollEdgeError::InvalidTokenFormat);
        }
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| EnrollEdgeError::InvalidTokenFormat)?;
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig_bytes);
        let vault_sig = format!("vault:v1:{sig_b64}");

        let ok = self
            .secret_store
            .transit_verify(&self.signing_key_path, signing_input.as_bytes(), &vault_sig)
            .await
            .map_err(|e| EnrollEdgeError::Other(anyhow!("transit_verify: {e}")))?;
        if !ok {
            return Err(EnrollEdgeError::InvalidSignature);
        }

        let claims_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| EnrollEdgeError::InvalidTokenFormat)?;
        let claims: EnrollmentTokenClaims = serde_json::from_slice(&claims_json)
            .map_err(|_| EnrollEdgeError::InvalidTokenFormat)?;
        Ok(claims)
    }
}
