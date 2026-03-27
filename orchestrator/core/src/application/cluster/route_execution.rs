// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::agent::AgentId;
use crate::domain::cluster::{
    ExecutionRoute, NodeCapabilityAdvertisement, NodeCluster, NodeClusterRepository, NodeId,
    NodePeerStatus, NodeRouter,
};
use crate::domain::execution::ExecutionId;
use crate::domain::volume::TenantId;
use anyhow::{anyhow, Result};
use std::sync::Arc;

pub struct RouteExecutionRequest {
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    pub required_capabilities: NodeCapabilityAdvertisement,
    pub preferred_tags: Vec<String>,
    pub tenant_id: TenantId,
}

pub struct RouteExecutionUseCase {
    cluster_repo: Arc<dyn NodeClusterRepository>,
    router: Arc<dyn NodeRouter>,
    controller_node_id: NodeId,
}

impl RouteExecutionUseCase {
    pub fn new(
        cluster_repo: Arc<dyn NodeClusterRepository>,
        router: Arc<dyn NodeRouter>,
        controller_node_id: NodeId,
    ) -> Self {
        Self {
            cluster_repo,
            router,
            controller_node_id,
        }
    }

    pub async fn execute(&self, req: RouteExecutionRequest) -> Result<ExecutionRoute> {
        // 1. Load active peers to build a transient NodeCluster aggregate
        // In a high-traffic system, this would be cached in memory.
        let peers = self
            .cluster_repo
            .list_peers_by_status(NodePeerStatus::Active)
            .await?;

        // ADR-062: Defensive freshness re-evaluation at routing time.
        // Even if the DB says "Active", reject peers whose last heartbeat
        // is staler than 2x the heartbeat interval (60s).
        let now = chrono::Utc::now();
        let peers: Vec<_> = peers
            .into_iter()
            .filter(|p| {
                let staleness = now.signed_duration_since(p.last_heartbeat_at);
                if staleness.num_seconds() > 60 {
                    tracing::warn!(
                        node_id = %p.node_id,
                        staleness_secs = staleness.num_seconds(),
                        "ADR-062: excluding stale peer from routing despite Active status"
                    );
                    false
                } else {
                    true
                }
            })
            .collect();

        let mut cluster = NodeCluster::new(self.controller_node_id);
        for peer in peers {
            cluster.register_peer(peer).map_err(|e| anyhow!(e))?;
        }

        // 2. Select worker using the injected router strategy
        let route = self
            .router
            .select_worker(&req.required_capabilities, &cluster)
            .map_err(|e| anyhow!(e))?;

        Ok(route)
    }
}
