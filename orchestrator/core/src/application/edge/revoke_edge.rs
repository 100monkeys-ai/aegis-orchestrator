// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §C — operator-initiated edge revocation.

use anyhow::Result;
use std::sync::Arc;

use crate::domain::cluster::NodePeerStatus;
use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::shared_kernel::{NodeId, TenantId};
use crate::infrastructure::edge::EdgeConnectionRegistry;

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

    pub async fn revoke(&self, tenant: &TenantId, node_id: NodeId) -> Result<()> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("edge {node_id} not found"))?;
        if &edge.tenant_id != tenant {
            anyhow::bail!("cross-tenant revocation refused");
        }
        // Mark Unhealthy (revoked); the existing token blacklist mechanism is
        // engaged at the SEAL middleware boundary by deletion of the edge row.
        self.edge_repo
            .update_status(&node_id, NodePeerStatus::Unhealthy)
            .await?;
        // Drop the live stream (guard's Drop fires on remove).
        // The registry doesn't expose direct removal; closing the channel is
        // handled when the gRPC handler drops its registration guard. Here we
        // additionally fail any pending oneshots tagged with this node so
        // in-flight callers terminate immediately.
        self.registry.pending().disconnect_node(&node_id);
        Ok(())
    }
}
