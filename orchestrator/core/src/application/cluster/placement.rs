// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Where a new execution runs (AEGIS ADR-152 D5.3).
//!
//! A new execution starts on the node that received it, unless that node
//! already owns as many unfinished executions as it advertises
//! (`cluster.membership.max_executions`), or is draining. Then it goes to
//! the live, active peer with the most headroom, forwarded with the caller
//! as this node authenticated it (D5.5). A forward that fails falls back to
//! this node.

use std::sync::Arc;

use anyhow::Result;

use crate::domain::agent::AgentId;
use crate::domain::cluster::NodeId;
use crate::domain::cluster_membership::{PlacementCandidate, PlacementRepository};
use crate::domain::execution::{ExecutionId, ExecutionInput};
use crate::domain::iam::UserIdentity;

/// A start sent to a peer (ADR-152 D5.5).
#[derive(Debug, Clone)]
pub struct ForwardedStart {
    pub execution_id: ExecutionId,
    pub agent_id: AgentId,
    pub input: ExecutionInput,
    pub tenant_id: crate::domain::tenant::TenantId,
    pub security_context_name: String,
    /// The caller as this node authenticated it; `None` for a system start.
    pub caller: Option<UserIdentity>,
}

/// Sends a start to a peer; answers the node that owns it.
#[async_trait::async_trait]
pub trait ExecutionForwarder: Send + Sync {
    async fn forward(&self, peer: &PlacementCandidate, start: &ForwardedStart) -> Result<NodeId>;
}

/// This node's placement: its identity, its capacity, and how to reach peers.
pub struct ClusterPlacement {
    pub me: NodeId,
    pub capacity: u32,
    pub placement: Arc<dyn PlacementRepository>,
    pub forwarder: Arc<dyn ExecutionForwarder>,
}

impl ClusterPlacement {
    /// The peer a new execution goes to, or `None` to run it here: this node
    /// has room and is not draining, or no peer has room.
    pub async fn choose(&self, draining: bool) -> Result<Option<PlacementCandidate>> {
        if !draining && self.placement.load_of(self.me).await? < i64::from(self.capacity) {
            return Ok(None);
        }
        Ok(self.placement.candidates(self.me).await?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        load: i64,
        peers: Vec<PlacementCandidate>,
    }

    #[async_trait::async_trait]
    impl PlacementRepository for Fixed {
        async fn load_of(&self, _: NodeId) -> Result<i64> {
            Ok(self.load)
        }
        async fn candidates(&self, _: NodeId) -> Result<Vec<PlacementCandidate>> {
            Ok(self.peers.clone())
        }
    }

    struct Never;
    #[async_trait::async_trait]
    impl ExecutionForwarder for Never {
        async fn forward(&self, _: &PlacementCandidate, _: &ForwardedStart) -> Result<NodeId> {
            unreachable!()
        }
    }

    fn placement(load: i64, peers: Vec<PlacementCandidate>) -> ClusterPlacement {
        ClusterPlacement {
            me: NodeId::new(),
            capacity: 4,
            placement: Arc::new(Fixed { load, peers }),
            forwarder: Arc::new(Never),
        }
    }

    fn peer(headroom: i64) -> PlacementCandidate {
        PlacementCandidate {
            node_id: NodeId::new(),
            grpc_address: "10.121.0.11:50056".into(),
            headroom,
        }
    }

    #[tokio::test]
    async fn a_node_with_room_keeps_its_executions() {
        assert_eq!(
            placement(3, vec![peer(9)]).choose(false).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_full_node_sends_to_the_peer_with_most_headroom() {
        let best = peer(5);
        let chosen = placement(4, vec![best.clone(), peer(1)])
            .choose(false)
            .await
            .unwrap();
        assert_eq!(chosen, Some(best));
    }

    #[tokio::test]
    async fn a_full_node_with_no_roomy_peer_runs_it_here() {
        assert_eq!(placement(7, vec![]).choose(false).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_draining_node_sends_even_with_room() {
        let best = peer(2);
        assert_eq!(
            placement(0, vec![best.clone()]).choose(true).await.unwrap(),
            Some(best)
        );
    }
}
