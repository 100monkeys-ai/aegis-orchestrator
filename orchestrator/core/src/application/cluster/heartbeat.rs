// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0

use crate::domain::cluster::{NodeClusterRepository, NodeId, NodePeerStatus, ResourceSnapshot};
use anyhow::Result;
use std::sync::Arc;

pub struct HeartbeatRequest {
    pub node_id: NodeId,
    pub status: NodePeerStatus,
    pub active_executions: u32,
    pub available_memory_gb: u32,
    pub cpu_utilization_percent: f32,
}

pub struct HeartbeatResponse {
    pub pending_commands: Vec<NodeCommand>,
}

#[derive(Debug, Clone)]
pub enum NodeCommand {
    Drain(bool),
    PushConfig { version: String, payload: Vec<u8> },
    Shutdown(String),
}

pub struct HeartbeatUseCase {
    cluster_repo: Arc<dyn NodeClusterRepository>,
}

impl HeartbeatUseCase {
    pub fn new(cluster_repo: Arc<dyn NodeClusterRepository>) -> Self {
        Self { cluster_repo }
    }

    pub async fn execute(&self, req: HeartbeatRequest) -> Result<HeartbeatResponse> {
        // 1. Record heartbeat and utilization snapshot
        let snapshot = ResourceSnapshot {
            cpu_utilization: req.cpu_utilization_percent,
            // The current single-node cluster heartbeat proto does not carry GPU utilization.
            gpu_utilization: 0.0,
            active_executions: req.active_executions,
        };

        self.cluster_repo
            .record_heartbeat(&req.node_id, snapshot)
            .await?;

        // 2. The single-node Phase 1 baseline has no command queue; only synthesize a drain
        // command when the repository already marks the peer as draining.
        let mut pending_commands = Vec::new();

        if let Some(peer) = self.cluster_repo.find_peer(&req.node_id).await? {
            if peer.status == NodePeerStatus::Draining && req.status == NodePeerStatus::Active {
                // Controller wants to drain, but node still thinks it's active
                pending_commands.push(NodeCommand::Drain(true));
            }
        }

        Ok(HeartbeatResponse { pending_commands })
    }
}
