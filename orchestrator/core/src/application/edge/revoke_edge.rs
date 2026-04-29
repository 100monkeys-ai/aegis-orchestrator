// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — operator-initiated edge revocation.

use std::sync::Arc;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::shared_kernel::{NodeId, TenantId};
use crate::infrastructure::edge::EdgeConnectionRegistry;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum RevokeEdgeError {
    #[error("edge daemon not found")]
    NotFound,
    #[error("cross-tenant revoke refused")]
    Forbidden,
    #[error("repository error: {0}")]
    Repo(String),
}

pub struct RevokeEdgeService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    registry: EdgeConnectionRegistry,
}

impl RevokeEdgeService {
    pub fn new(edge_repo: Arc<dyn EdgeDaemonRepository>, registry: EdgeConnectionRegistry) -> Self {
        Self {
            edge_repo,
            registry,
        }
    }

    pub async fn revoke(&self, tenant: &TenantId, node_id: NodeId) -> Result<(), RevokeEdgeError> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await
            .map_err(|e| RevokeEdgeError::Repo(e.to_string()))?
            .ok_or(RevokeEdgeError::NotFound)?;
        if &edge.tenant_id != tenant {
            return Err(RevokeEdgeError::Forbidden);
        }
        // Mark Unhealthy (revoked); the existing token blacklist mechanism is
        // engaged at the SEAL middleware boundary by deletion of the edge row.
        self.edge_repo
            .update_status(&node_id, NodePeerStatus::Unhealthy)
            .await
            .map_err(|e| RevokeEdgeError::Repo(e.to_string()))?;
        // Drop the live stream (guard's Drop fires on remove).
        // The registry doesn't expose direct removal; closing the channel is
        // handled when the gRPC handler drops its registration guard. Here we
        // additionally fail any pending oneshots tagged with this node so
        // in-flight callers terminate immediately.
        self.registry.pending().disconnect_node(&node_id);
        Ok(())
    }
}
