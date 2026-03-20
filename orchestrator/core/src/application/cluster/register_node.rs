// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::cluster::{
    ClusterId, NodeCapabilityAdvertisement, NodeClusterRepository, NodeId, NodePeer, NodePeerStatus,
};
use anyhow::{anyhow, Result};
use chrono::Utc;
use std::sync::Arc;

pub struct RegisterNodeRequest {
    pub node_id: NodeId,
    pub capabilities: NodeCapabilityAdvertisement,
    pub grpc_address: String,
}

pub struct RegisterNodeResponse {
    pub accepted: bool,
    pub cluster_id: ClusterId,
    pub message: String,
}

pub struct RegisterNodeUseCase {
    cluster_repo: Arc<dyn NodeClusterRepository>,
    _controller_node_id: NodeId,
}

impl RegisterNodeUseCase {
    pub fn new(cluster_repo: Arc<dyn NodeClusterRepository>, controller_node_id: NodeId) -> Self {
        Self {
            cluster_repo,
            _controller_node_id: controller_node_id,
        }
    }

    pub async fn execute(&self, req: RegisterNodeRequest) -> Result<RegisterNodeResponse> {
        // 1. Find existing peer (must be attested)
        let existing = self.cluster_repo.find_peer(&req.node_id).await?;

        let role = match &existing {
            Some(p) => p.role,
            None => {
                // In some cases we might allow auto-registration if security is handled by gRPC interceptor
                // but ADR says attestation must come first.
                return Err(anyhow!("Node not attested. Handshake required."));
            }
        };

        // 2. Update peer info
        let peer = NodePeer {
            node_id: req.node_id,
            role,
            public_key: existing.unwrap().public_key, // Preserve public key from attestation
            capabilities: req.capabilities,
            grpc_address: req.grpc_address,
            status: NodePeerStatus::Active,
            last_heartbeat_at: Utc::now(),
            registered_at: Utc::now(),
        };

        self.cluster_repo.upsert_peer(&peer).await?;

        // 3. Return the fixed single-node cluster identifier used by the current baseline.
        let cluster_id = ClusterId(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            b"aegis-cluster",
        ));

        Ok(RegisterNodeResponse {
            accepted: true,
            cluster_id,
            message: "Node registered successfully".to_string(),
        })
    }
}
