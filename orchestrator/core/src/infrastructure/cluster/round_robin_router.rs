// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! # Round Robin Node Router
//!
//! Simple load balancing across healthy worker nodes.

use crate::domain::cluster::{
    NodeRouter, NodeCluster, NodeCapabilityAdvertisement, ExecutionRoute,
    NodeRouterError, NodePeer,
};
use crate::domain::node_config::NodeRole;
use crate::domain::cluster::NodePeerStatus;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct RoundRobinNodeRouter {
    pub counter: AtomicUsize,
}

impl RoundRobinNodeRouter {
    pub fn new() -> Self {
        Self {
            counter: AtomicUsize::new(0),
        }
    }
}

impl NodeRouter for RoundRobinNodeRouter {
    fn select_worker(
        &self,
        required: &NodeCapabilityAdvertisement,
        cluster: &NodeCluster,
    ) -> Result<ExecutionRoute, NodeRouterError> {
        let candidates: Vec<&NodePeer> = cluster
            .peers
            .values()
            .filter(|p| {
                p.status == NodePeerStatus::Active &&
                (p.role == NodeRole::Worker || p.role == NodeRole::Hybrid) &&
                p.capabilities.satisfies(required)
            })
            .collect();

        if candidates.is_empty() {
            let any_healthy = cluster.peers.values().any(|p| {
                p.status == NodePeerStatus::Active &&
                (p.role == NodeRole::Worker || p.role == NodeRole::Hybrid)
            });

            return if !any_healthy {
                Err(NodeRouterError::NoHealthyWorkers)
            } else {
                Err(NodeRouterError::NoCapableWorkers { required: required.clone() })
            };
        }

        let idx = self.counter.fetch_add(1, Ordering::Relaxed) % candidates.len();
        let peer = candidates[idx];

        Ok(ExecutionRoute {
            target_node_id: peer.node_id,
            worker_grpc_address: peer.grpc_address.clone(),
        })
    }
}
