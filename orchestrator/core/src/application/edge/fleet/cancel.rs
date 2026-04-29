// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Cancel a running fleet operation. Broadcasts cancel to every per-node
//! task and additionally sends `CancelCommand` over the connection registry
//! for any node whose `command_id` is known.

use std::sync::Arc;

use super::registry::FleetRegistry;
use crate::domain::cluster::FleetCommandId;
use crate::infrastructure::aegis_cluster_proto::{
    edge_command::Command as OutCmd, CancelCommand, EdgeCommand,
};
use crate::infrastructure::edge::EdgeConnectionRegistry;

pub struct CancelFleetService {
    registry: FleetRegistry,
    conn_registry: EdgeConnectionRegistry,
}

impl CancelFleetService {
    pub fn new(registry: FleetRegistry, conn_registry: EdgeConnectionRegistry) -> Self {
        Self {
            registry,
            conn_registry,
        }
    }

    pub async fn cancel(&self, fleet_id: FleetCommandId) -> bool {
        let Some(handle) = self.registry.get(&fleet_id) else {
            return false;
        };
        let _ = handle.cancel_tx.send(());
        for entry in handle.per_node_command_ids.iter() {
            let node_id = *entry.key();
            let cid = *entry.value();
            if let Some(tx) = self.conn_registry.get(&node_id) {
                let _ = tx
                    .send(EdgeCommand {
                        command: Some(OutCmd::Cancel(CancelCommand {
                            command_id: cid.to_string(),
                        })),
                    })
                    .await;
            }
        }
        true
    }
}
