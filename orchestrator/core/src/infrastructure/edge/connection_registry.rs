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

    pub fn pending(&self) -> &PendingEdgeCalls {
        &self.pending
    }

    pub fn connected_node_ids(&self) -> Vec<NodeId> {
        self.inner.iter().map(|r| *r.key()).collect()
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
