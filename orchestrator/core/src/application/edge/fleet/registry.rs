// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! In-memory registry of running fleet operations.

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::domain::cluster::FleetCommandId;
use crate::domain::shared_kernel::{NodeId, TenantId};

pub struct FleetCommandHandle {
    pub fleet_command_id: FleetCommandId,
    /// The tenant that invoked the fleet command. Cancellation is authorised
    /// against it (ADR-117 §F; ADR-073 §3e for the operator exception).
    pub tenant_id: TenantId,
    /// Broadcast channel used to fan out cancel signals to every per-node
    /// task driving an in-flight `InvokeToolCommand`.
    pub cancel_tx: broadcast::Sender<()>,
    /// `command_id`s of the per-node InvokeTool dispatches, for direct
    /// `CancelCommand` broadcasts via the connection registry.
    pub per_node_command_ids: dashmap::DashMap<NodeId, Uuid>,
}

/// The record of which member runs each command, and this member (ADR-152
/// D4.7).
type Ownership = (
    Arc<dyn crate::infrastructure::edge::FleetCommandOwners>,
    NodeId,
);

#[derive(Clone, Default)]
pub struct FleetRegistry {
    inner: Arc<DashMap<FleetCommandId, Arc<FleetCommandHandle>>>,
    ownership: Arc<std::sync::OnceLock<Ownership>>,
}

impl FleetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// ADR-152 D4.7: record each command this member runs, so a cancel
    /// received by another member reaches it.
    pub fn set_ownership(
        &self,
        owners: Arc<dyn crate::infrastructure::edge::FleetCommandOwners>,
        me: NodeId,
    ) {
        let _ = self.ownership.set((owners, me));
    }

    pub fn register(&self, handle: FleetCommandHandle) -> Arc<FleetCommandHandle> {
        let arc = Arc::new(handle);
        self.inner.insert(arc.fleet_command_id, arc.clone());
        if let Some((owners, me)) = self.ownership.get() {
            let (owners, me, id, tenant) = (
                owners.clone(),
                *me,
                arc.fleet_command_id,
                arc.tenant_id.clone(),
            );
            tokio::spawn(async move {
                if let Err(e) = owners.record(id, me, &tenant).await {
                    tracing::warn!(error = %e, "could not record the fleet command's member");
                }
            });
        }
        arc
    }

    pub fn get(&self, id: &FleetCommandId) -> Option<Arc<FleetCommandHandle>> {
        self.inner.get(id).map(|r| r.clone())
    }

    pub fn remove(&self, id: &FleetCommandId) {
        self.inner.remove(id);
        if let Some((owners, _)) = self.ownership.get() {
            let (owners, id) = (owners.clone(), *id);
            tokio::spawn(async move {
                if let Err(e) = owners.release(id).await {
                    tracing::warn!(error = %e, "could not release the fleet command's record");
                }
            });
        }
    }
}
