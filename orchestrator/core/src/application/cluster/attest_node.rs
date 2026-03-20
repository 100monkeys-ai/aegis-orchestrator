// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::cluster::{
    NodeCapabilityAdvertisement, NodeChallenge, NodeChallengeRepository, NodeId, NodeRole,
};
use anyhow::Result;

pub struct AttestNodeRequest {
    pub node_id: NodeId,
    pub role: NodeRole,
    pub public_key: Vec<u8>,
    pub capabilities: NodeCapabilityAdvertisement,
    pub grpc_address: String,
}

pub struct AttestNodeResponse {
    pub challenge_nonce: Vec<u8>,
    pub challenge_id: uuid::Uuid,
}

pub struct AttestNodeUseCase {
    challenge_repo: std::sync::Arc<dyn NodeChallengeRepository>,
}

impl AttestNodeUseCase {
    pub fn new(challenge_repo: std::sync::Arc<dyn NodeChallengeRepository>) -> Self {
        Self { challenge_repo }
    }

    pub async fn execute(&self, req: AttestNodeRequest) -> Result<AttestNodeResponse> {
        let mut nonce = Vec::with_capacity(32);
        nonce.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        nonce.extend_from_slice(uuid::Uuid::new_v4().as_bytes());

        // 1. Create a new challenge
        let challenge = NodeChallenge {
            challenge_id: uuid::Uuid::new_v4(),
            node_id: req.node_id,
            nonce,
            public_key: req.public_key,
            role: req.role,
            capabilities: req.capabilities,
            grpc_address: req.grpc_address,
            created_at: chrono::Utc::now(),
        };

        // 2. Persist challenge (TTL handled by repo/domain)
        self.challenge_repo.save_challenge(&challenge).await?;

        Ok(AttestNodeResponse {
            challenge_nonce: challenge.nonce,
            challenge_id: challenge.challenge_id,
        })
    }
}
