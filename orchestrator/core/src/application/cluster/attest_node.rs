// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::cluster::{
    ClusterEnrolmentTokenError, ClusterEnrolmentTokenRepository, NodeCapabilityAdvertisement,
    NodeChallenge, NodeChallengeRepository, NodeId, NodeRole,
};
use anyhow::{anyhow, Result};
use std::sync::Arc;

pub struct AttestNodeRequest {
    pub node_id: NodeId,
    pub role: NodeRole,
    pub public_key: Vec<u8>,
    pub capabilities: NodeCapabilityAdvertisement,
    pub grpc_address: String,
    /// Cluster enrolment token (security audit 002 §4.9). Single-use, opaque
    /// token bound to `node_id` at issue time. Required for non-edge roles.
    pub enrolment_token: String,
}

#[derive(Debug)]
pub struct AttestNodeResponse {
    pub challenge_nonce: Vec<u8>,
    pub challenge_id: uuid::Uuid,
}

pub struct AttestNodeUseCase {
    challenge_repo: Arc<dyn NodeChallengeRepository>,
    enrolment_token_repo: Arc<dyn ClusterEnrolmentTokenRepository>,
}

impl AttestNodeUseCase {
    pub fn new(
        challenge_repo: Arc<dyn NodeChallengeRepository>,
        enrolment_token_repo: Arc<dyn ClusterEnrolmentTokenRepository>,
    ) -> Self {
        Self {
            challenge_repo,
            enrolment_token_repo,
        }
    }

    pub async fn execute(&self, req: AttestNodeRequest) -> Result<AttestNodeResponse> {
        // Edge nodes use a different bootstrap surface — they present a
        // short-lived enrollment JWT on `ChallengeNode` (ADR-117 §C). Cluster
        // worker / controller / hybrid roles MUST present a single-use cluster
        // enrolment token on `AttestNode` (security audit 002 §4.9).
        if req.role != NodeRole::Edge {
            if req.enrolment_token.trim().is_empty() {
                return Err(anyhow!(
                    "Cluster admission denied: missing enrolment_token (audit 002 §4.9)"
                ));
            }
            match self
                .enrolment_token_repo
                .redeem(&req.enrolment_token, &req.node_id)
                .await
            {
                Ok(_bound_node_id) => {}
                Err(ClusterEnrolmentTokenError::NodeIdMismatch { .. }) => {
                    return Err(anyhow!(
                        "Cluster admission denied: enrolment token bound to a different node_id"
                    ));
                }
                Err(ClusterEnrolmentTokenError::NotFound) => {
                    return Err(anyhow!(
                        "Cluster admission denied: enrolment token unknown or already redeemed"
                    ));
                }
                Err(ClusterEnrolmentTokenError::Expired(_)) => {
                    return Err(anyhow!("Cluster admission denied: enrolment token expired"));
                }
                Err(ClusterEnrolmentTokenError::Malformed) => {
                    return Err(anyhow!(
                        "Cluster admission denied: enrolment token malformed"
                    ));
                }
                Err(ClusterEnrolmentTokenError::Other(e)) => {
                    return Err(anyhow!("Cluster admission token redemption failed: {e}"));
                }
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cluster::{ClusterEnrolmentTokenError, ClusterEnrolmentTokenRepository};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use uuid::Uuid;

    struct InMemoryChallengeRepo {
        saved: Mutex<HashMap<Uuid, NodeChallenge>>,
    }

    #[async_trait]
    impl NodeChallengeRepository for InMemoryChallengeRepo {
        async fn save_challenge(&self, challenge: &NodeChallenge) -> anyhow::Result<()> {
            self.saved
                .lock()
                .insert(challenge.challenge_id, challenge.clone());
            Ok(())
        }
        async fn get_challenge(
            &self,
            challenge_id: &Uuid,
        ) -> anyhow::Result<Option<NodeChallenge>> {
            Ok(self.saved.lock().get(challenge_id).cloned())
        }
        async fn delete_challenge(&self, challenge_id: &Uuid) -> anyhow::Result<()> {
            self.saved.lock().remove(challenge_id);
            Ok(())
        }
    }

    /// In-memory enrolment-token repo: any non-empty token redeems exactly
    /// once and is bound to a configured `NodeId`. Mirrors the production
    /// guarantee provided by `INSERT ... ON CONFLICT DO NOTHING RETURNING`.
    struct InMemoryEnrolmentRepo {
        bound_node: NodeId,
        redeemed_tokens: Mutex<std::collections::HashSet<String>>,
    }

    #[async_trait]
    impl ClusterEnrolmentTokenRepository for InMemoryEnrolmentRepo {
        async fn redeem(
            &self,
            token: &str,
            presented_node_id: &NodeId,
        ) -> Result<NodeId, ClusterEnrolmentTokenError> {
            if token.trim().is_empty() {
                return Err(ClusterEnrolmentTokenError::Malformed);
            }
            // node_id binding is verified BEFORE redemption — a token presented
            // against the wrong node identity must NOT be consumed (so the
            // legitimate node can still redeem it).
            if presented_node_id != &self.bound_node {
                return Err(ClusterEnrolmentTokenError::NodeIdMismatch {
                    bound: self.bound_node,
                    presented: *presented_node_id,
                });
            }
            let mut set = self.redeemed_tokens.lock();
            if !set.insert(token.to_string()) {
                return Err(ClusterEnrolmentTokenError::NotFound);
            }
            Ok(self.bound_node)
        }
    }

    fn caps() -> NodeCapabilityAdvertisement {
        NodeCapabilityAdvertisement {
            gpu_count: 0,
            vram_gb: 0,
            cpu_cores: 1,
            available_memory_gb: 1,
            supported_runtimes: vec![],
            tags: vec![],
        }
    }

    /// Audit 002 §4.9 regression: an unauthenticated caller invoking
    /// `AttestNode` with a self-generated key and **no** enrolment token must
    /// be rejected. Prior to the fix this attempt silently produced a
    /// challenge and let the attacker proceed to ChallengeNode and join the
    /// cluster.
    #[tokio::test]
    async fn attest_node_rejects_self_generated_key_without_enrolment_token() {
        let challenge_repo = Arc::new(InMemoryChallengeRepo {
            saved: Mutex::new(HashMap::new()),
        });
        let bound = NodeId(Uuid::new_v4());
        let enrolment_repo = Arc::new(InMemoryEnrolmentRepo {
            bound_node: bound,
            redeemed_tokens: Mutex::new(Default::default()),
        });
        let uc = AttestNodeUseCase::new(challenge_repo.clone(), enrolment_repo);

        let attacker_node = NodeId(Uuid::new_v4());
        let attacker_pubkey = vec![0u8; 32];

        let err = uc
            .execute(AttestNodeRequest {
                node_id: attacker_node,
                role: NodeRole::Worker,
                public_key: attacker_pubkey,
                capabilities: caps(),
                grpc_address: "10.0.0.1:50056".to_string(),
                enrolment_token: String::new(),
            })
            .await
            .expect_err("attest must reject empty enrolment_token");
        let msg = format!("{err}");
        assert!(
            msg.contains("missing enrolment_token") || msg.contains("audit 002 §4.9"),
            "unexpected error: {msg}"
        );

        // Confirm no challenge was persisted — the attacker never reached
        // step 1 of the handshake.
        assert!(
            challenge_repo.saved.lock().is_empty(),
            "no challenge should be persisted on rejection"
        );
    }

    /// Audit 002 §4.9 regression: a caller presenting a token bound to a
    /// different `node_id` must be rejected (cross-node token replay).
    #[tokio::test]
    async fn attest_node_rejects_token_bound_to_different_node() {
        let challenge_repo = Arc::new(InMemoryChallengeRepo {
            saved: Mutex::new(HashMap::new()),
        });
        let bound = NodeId(Uuid::new_v4());
        let attacker_node = NodeId(Uuid::new_v4());
        let enrolment_repo = Arc::new(InMemoryEnrolmentRepo {
            bound_node: bound,
            redeemed_tokens: Mutex::new(Default::default()),
        });
        let uc = AttestNodeUseCase::new(challenge_repo, enrolment_repo);

        let err = uc
            .execute(AttestNodeRequest {
                node_id: attacker_node,
                role: NodeRole::Worker,
                public_key: vec![0u8; 32],
                capabilities: caps(),
                grpc_address: "10.0.0.1:50056".to_string(),
                enrolment_token: "tid.secret".to_string(),
            })
            .await
            .expect_err("must reject token bound to different node");
        assert!(format!("{err}").contains("different node_id"));
    }

    /// Audit 002 §4.9 regression: enrolment tokens are single-use. A second
    /// redemption of the same token must be rejected even when the bound
    /// `node_id` matches.
    #[tokio::test]
    async fn attest_node_rejects_replayed_enrolment_token() {
        let challenge_repo = Arc::new(InMemoryChallengeRepo {
            saved: Mutex::new(HashMap::new()),
        });
        let bound = NodeId(Uuid::new_v4());
        let enrolment_repo = Arc::new(InMemoryEnrolmentRepo {
            bound_node: bound,
            redeemed_tokens: Mutex::new(Default::default()),
        });
        let uc = AttestNodeUseCase::new(challenge_repo, enrolment_repo);

        let make_req = |tok: &str| AttestNodeRequest {
            node_id: bound,
            role: NodeRole::Worker,
            public_key: vec![1u8; 32],
            capabilities: caps(),
            grpc_address: "10.0.0.1:50056".to_string(),
            enrolment_token: tok.to_string(),
        };

        uc.execute(make_req("token-A"))
            .await
            .expect("first attest should succeed");
        let err = uc
            .execute(make_req("token-A"))
            .await
            .expect_err("replay must be rejected");
        assert!(format!("{err}").contains("already redeemed"));
    }
}
