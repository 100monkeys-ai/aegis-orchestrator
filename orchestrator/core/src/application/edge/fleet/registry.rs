// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! In-memory registry of running fleet operations.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::domain::cluster::FleetCommandId;
use crate::domain::shared_kernel::NodeId;

pub struct FleetCommandHandle {
    pub fleet_command_id: FleetCommandId,
    /// Broadcast channel used to fan out cancel signals to every per-node
    /// task driving an in-flight `InvokeToolCommand`.
    pub cancel_tx: broadcast::Sender<()>,
    /// `command_id`s of the per-node InvokeTool dispatches, for direct
    /// `CancelCommand` broadcasts via the connection registry.
    pub per_node_command_ids: dashmap::DashMap<NodeId, Uuid>,
}

#[derive(Clone, Default)]
pub struct FleetRegistry {
    inner: Arc<DashMap<FleetCommandId, Arc<FleetCommandHandle>>>,
}

impl FleetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, handle: FleetCommandHandle) -> Arc<FleetCommandHandle> {
        let arc = Arc::new(handle);
        self.inner.insert(arc.fleet_command_id, arc.clone());
        arc
    }

    pub fn get(&self, id: &FleetCommandId) -> Option<Arc<FleetCommandHandle>> {
        self.inner.get(id).map(|r| r.clone())
    }

    pub fn remove(&self, id: &FleetCommandId) {
        self.inner.remove(id);
    }
}
