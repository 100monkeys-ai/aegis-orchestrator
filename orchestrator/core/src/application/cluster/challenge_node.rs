// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::cluster::{
    NodeChallengeRepository, NodeClusterRepository, NodeId, NodePeer, NodePeerStatus,
    NodeTokenClaims,
};
use crate::domain::secrets::SecretStore;
use anyhow::{Result, anyhow};
use base64::Engine;
use chrono::Utc;
use std::sync::Arc;

pub struct ChallengeNodeRequest {
    pub challenge_id: uuid::Uuid,
    pub node_id: NodeId,
    pub challenge_signature: Vec<u8>,
}

pub struct ChallengeNodeResponse {
    pub node_security_token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

pub struct ChallengeNodeUseCase {
    challenge_repo: Arc<dyn NodeChallengeRepository>,
    cluster_repo: Arc<dyn NodeClusterRepository>,
    secret_store: Arc<dyn SecretStore>,
}

impl ChallengeNodeUseCase {
    pub fn new(
        challenge_repo: Arc<dyn NodeChallengeRepository>,
        cluster_repo: Arc<dyn NodeClusterRepository>,
        secret_store: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            challenge_repo,
            cluster_repo,
            secret_store,
        }
    }

    pub async fn execute(&self, req: ChallengeNodeRequest) -> Result<ChallengeNodeResponse> {
        // 1. Retrieve challenge
        let challenge = self
            .challenge_repo
            .get_challenge(&req.challenge_id)
            .await?
            .ok_or_else(|| anyhow!("Challenge not found or expired"))?;

        if challenge.node_id != req.node_id {
            return Err(anyhow!("Node ID mismatch"));
        }

        if challenge.is_expired() {
            return Err(anyhow!("Challenge expired"));
        }

        // 2. Verify Ed25519 signature
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let verifying_key = VerifyingKey::from_bytes(
            &challenge
                .public_key
                .clone()
                .try_into()
                .map_err(|_| anyhow!("Invalid public key length"))?,
        )
        .map_err(|e| anyhow!("Invalid public key: {}", e))?;

        let signature = Signature::from_slice(&req.challenge_signature)
            .map_err(|e| anyhow!("Invalid signature: {}", e))?;

        verifying_key
            .verify(&challenge.nonce, &signature)
            .map_err(|e| anyhow!("Signature verification failed: {}", e))?;

        // 3. Issue NodeSecurityToken (RS256 JWT)
        // ADR-059: Signed by controller's OpenBao Transit key.
        let exp = (Utc::now() + chrono::Duration::hours(1)).timestamp();
        let iat = Utc::now().timestamp();

        let claims = NodeTokenClaims {
            node_id: req.node_id,
            role: challenge.role,
            capabilities_hash: challenge.capabilities.hash(),
            iat,
            exp,
        };

        // Construct JWT header and payload
        let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&header)?);
        let claims_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&claims)?);
        let signing_input = format!("{}.{}", header_b64, claims_b64);

        // Sign with OpenBao Transit
        let signature_b64 = self
            .secret_store
            .transit_sign("aegis-node-controller-key", signing_input.as_bytes())
            .await?;

        // Note: Transit returns the signature, but we need to ensure it's in the right format for JWT.
        // Transit often returns it as "vault:v1:base64...". We need to strip the prefix if it exists.
        let signature_b64 = signature_b64
            .strip_prefix("vault:v1:")
            .unwrap_or(&signature_b64)
            .to_string();

        let token = format!("{}.{}", signing_input, signature_b64);

        // 4. Cleanup challenge
        self.challenge_repo
            .delete_challenge(&req.challenge_id)
            .await?;

        // 5. UPSERT node peer in repository (it's now attested)
        let peer = NodePeer {
            node_id: req.node_id,
            role: challenge.role,
            public_key: challenge.public_key,
            capabilities: challenge.capabilities,
            grpc_address: challenge.grpc_address,
            status: NodePeerStatus::Active,
            last_heartbeat_at: Utc::now(),
            registered_at: Utc::now(),
        };
        self.cluster_repo.upsert_peer(&peer).await?;

        Ok(ChallengeNodeResponse {
            node_security_token: token,
            expires_at: Utc::now() + chrono::Duration::hours(1),
        })
    }
}
