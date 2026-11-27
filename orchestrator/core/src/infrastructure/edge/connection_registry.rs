// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 in-memory edge connection registry.
//!
//! `EdgeConnectionRegistry` tracks the live `ConnectEdge` streams keyed by
//! `NodeId`. Each registered sender is wrapped by an [`EdgeConnectionGuard`]
//! whose `Drop` impl auto-deregisters the entry and resolves any pending
//! oneshots tagged with that node so MCP callers see `EdgeDisconnected`
//! immediately.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::domain::edge::EdgeRouterError;
use crate::domain::shared_kernel::NodeId;
use crate::infrastructure::aegis_cluster_proto::EdgeCommand;

pub type EdgeCommandTx = mpsc::Sender<EdgeCommand>;

/// In-memory map of `node_id → sender`. Cloneable; share via `Arc`.
#[derive(Clone, Default)]
pub struct EdgeConnectionRegistry {
    inner: Arc<DashMap<NodeId, EdgeCommandTx>>,
    pending: PendingEdgeCalls,
    /// Edges whose stream another process of the cluster holds (ADR-152 D8):
    /// a member's Relay Coordinator, as `edge_stream_owners` records it.
    /// Mirrored by [`Self::set_cluster_streams`]; they count as connected,
    /// and calls to them go through the dispatcher's cluster fallback.
    cluster: Arc<std::sync::RwLock<std::collections::HashSet<NodeId>>>,
}

impl EdgeConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a stream sender, returning a guard that auto-deregisters on drop.
    pub fn register(&self, node_id: NodeId, sender: EdgeCommandTx) -> EdgeConnectionGuard {
        self.inner.insert(node_id, sender);
        EdgeConnectionGuard {
            registry: self.inner.clone(),
            pending: self.pending.clone(),
            node_id,
        }
    }

    pub fn get(&self, node_id: &NodeId) -> Option<EdgeCommandTx> {
        self.inner.get(node_id).map(|r| r.clone())
    }

    /// True iff a `ConnectEdge` stream is currently registered for `node_id`.
    /// Backed by the in-memory `DashMap`, so callers get an instant, drift-free
    /// view of liveness without depending on heartbeat-staleness derivation.
    pub fn is_connected(&self, node_id: &NodeId) -> bool {
        self.inner.contains_key(node_id)
            || self
                .cluster
                .read()
                .map(|c| c.contains(node_id))
                .unwrap_or(false)
    }

    /// ADR-152 D8: replace the set of edges whose streams other processes
    /// of the cluster hold.
    pub fn set_cluster_streams(&self, edges: std::collections::HashSet<NodeId>) {
        if let Ok(mut c) = self.cluster.write() {
            *c = edges;
        }
    }

    pub fn pending(&self) -> &PendingEdgeCalls {
        &self.pending
    }

    pub fn connected_node_ids(&self) -> Vec<NodeId> {
        let mut ids: std::collections::HashSet<NodeId> =
            self.inner.iter().map(|r| *r.key()).collect();
        if let Ok(c) = self.cluster.read() {
            ids.extend(c.iter().copied());
        }
        ids.into_iter().collect()
    }

    /// Forcibly evict a node's live `ConnectEdge` stream by removing its
    /// sender from the registry. Dropping the sender closes the underlying
    /// `mpsc` channel, which causes the `ReceiverStream` wrapping it in
    /// `handle_connect_edge` to terminate — the daemon then observes its
    /// gRPC stream being closed by the server. Any pending oneshots tagged
    /// with this node are failed with `EdgeDisconnected`.
    ///
    /// Used by [`crate::application::edge::revoke_edge::RevokeEdgeService`]
    /// to enforce ADR-117 §C revocation: once an operator revokes a host,
    /// the daemon's bidi stream must drop immediately so it stops trying
    /// to use the now-invalid `NodeSecurityToken`.
    ///
    /// Returns `true` if a sender was registered (and removed), `false`
    /// when the node had no live stream.
    pub fn evict(&self, node_id: &NodeId) -> bool {
        let removed = self.inner.remove(node_id).is_some();
        if let Ok(mut c) = self.cluster.write() {
            c.remove(node_id);
        }
        self.pending.disconnect_node(node_id);
        removed
    }
}

pub struct EdgeConnectionGuard {
    registry: Arc<DashMap<NodeId, EdgeCommandTx>>,
    pending: PendingEdgeCalls,
    node_id: NodeId,
}

impl Drop for EdgeConnectionGuard {
    fn drop(&mut self) {
        self.registry.remove(&self.node_id);
        self.pending.disconnect_node(&self.node_id);
    }
}

/// Reverse-RPC correlation table: `command_id → oneshot::Sender<EdgeResult>`.
#[derive(Clone, Default)]
pub struct PendingEdgeCalls {
    inner: Arc<DashMap<Uuid, PendingEntry>>,
}

struct PendingEntry {
    node_id: NodeId,
    sender: oneshot::Sender<
        Result<crate::infrastructure::aegis_cluster_proto::EdgeResult, EdgeRouterError>,
    >,
}

impl PendingEdgeCalls {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        command_id: Uuid,
        node_id: NodeId,
    ) -> oneshot::Receiver<
        Result<crate::infrastructure::aegis_cluster_proto::EdgeResult, EdgeRouterError>,
    > {
        let (tx, rx) = oneshot::channel();
        self.inner.insert(
            command_id,
            PendingEntry {
                node_id,
                sender: tx,
            },
        );
        rx
    }

    pub fn complete(
        &self,
        command_id: &Uuid,
        result: crate::infrastructure::aegis_cluster_proto::EdgeResult,
    ) {
        if let Some((_, entry)) = self.inner.remove(command_id) {
            let _ = entry.sender.send(Ok(result));
        }
    }

    pub fn fail(&self, command_id: &Uuid, err: EdgeRouterError) {
        if let Some((_, entry)) = self.inner.remove(command_id) {
            let _ = entry.sender.send(Err(err));
        }
    }

    /// On disconnect, drain every pending entry tagged with `node_id` and
    /// resolve their oneshots with `EdgeDisconnected`.
    pub fn disconnect_node(&self, node_id: &NodeId) {
        let to_fail: Vec<Uuid> = self
            .inner
            .iter()
            .filter(|r| r.value().node_id == *node_id)
            .map(|r| *r.key())
            .collect();
        for cid in to_fail {
            if let Some((_, entry)) = self.inner.remove(&cid) {
                let _ = entry.sender.send(Err(EdgeRouterError::EdgeDisconnected));
            }
        }
    }
}

#[cfg(test)]
mod cluster_stream_tests {
    use super::*;

    #[test]
    fn an_edge_held_elsewhere_in_the_cluster_counts_until_evicted() {
        let registry = EdgeConnectionRegistry::new();
        let edge = NodeId::new();
        assert!(!registry.is_connected(&edge));
        registry.set_cluster_streams([edge].into_iter().collect());
        assert!(registry.is_connected(&edge));
        assert_eq!(registry.connected_node_ids(), vec![edge]);
        assert!(
            registry.get(&edge).is_none(),
            "no local sender: calls go through the cluster"
        );
        registry.evict(&edge);
        assert!(!registry.is_connected(&edge));
    }
}
