// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Resolves an [`EdgeTarget`] to a concrete `Vec<NodeId>` against the live
//! tenant-scoped edge population.

use std::collections::HashSet;
use std::sync::Arc;

use crate::domain::edge::{EdgeDaemonRepository, EdgeGroupRepository, EdgeRouterError, EdgeTarget};
use crate::domain::shared_kernel::{NodeId, TenantId};
use crate::infrastructure::edge::EdgeConnectionRegistry;

pub struct EdgeFleetResolver {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
    group_repo: Arc<dyn EdgeGroupRepository>,
    registry: EdgeConnectionRegistry,
}

impl EdgeFleetResolver {
    pub fn new(
        edge_repo: Arc<dyn EdgeDaemonRepository>,
        group_repo: Arc<dyn EdgeGroupRepository>,
        registry: EdgeConnectionRegistry,
    ) -> Self {
        Self {
            edge_repo,
            group_repo,
            registry,
        }
    }

    pub async fn resolve(
        &self,
        tenant: &TenantId,
        target: &EdgeTarget,
    ) -> Result<Vec<NodeId>, EdgeRouterError> {
        let connected: HashSet<NodeId> = self.registry.connected_node_ids().into_iter().collect();
        let edges = self
            .edge_repo
            .list_by_tenant(tenant)
            .await
            .map_err(|_| EdgeRouterError::NoMatchingEdges)?;

        match target {
            EdgeTarget::Node(id) => {
                if let Some(e) = edges.iter().find(|e| e.node_id == *id) {
                    if connected.contains(&e.node_id) {
                        Ok(vec![*id])
                    } else {
                        Err(EdgeRouterError::EdgeUnavailable { node_id: *id })
                    }
                } else {
                    Err(EdgeRouterError::CrossTenantAccessDenied {
                        node_id: *id,
                        tenant: tenant.as_str().to_string(),
                    })
                }
            }
            EdgeTarget::Selector(sel) => {
                let out: Vec<NodeId> = edges
                    .iter()
                    .filter(|e| connected.contains(&e.node_id))
                    .filter(|e| e.capabilities.satisfies(sel))
                    .map(|e| e.node_id)
                    .collect();
                if out.is_empty() {
                    Err(EdgeRouterError::NoMatchingEdges)
                } else {
                    Ok(out)
                }
            }
            EdgeTarget::All => {
                let out: Vec<NodeId> = edges
                    .iter()
                    .filter(|e| connected.contains(&e.node_id))
                    .map(|e| e.node_id)
                    .collect();
                if out.is_empty() {
                    Err(EdgeRouterError::NoMatchingEdges)
                } else {
                    Ok(out)
                }
            }
            EdgeTarget::Group(group_id) => {
                let group = self
                    .group_repo
                    .get(group_id)
                    .await
                    .map_err(|_| EdgeRouterError::GroupNotFound(*group_id))?
                    .ok_or(EdgeRouterError::GroupNotFound(*group_id))?;
                if &group.tenant_id != tenant {
                    return Err(EdgeRouterError::GroupNotFound(*group_id));
                }
                let mut set: HashSet<NodeId> = edges
                    .iter()
                    .filter(|e| connected.contains(&e.node_id))
                    .filter(|e| e.capabilities.satisfies(&group.selector))
                    .map(|e| e.node_id)
                    .collect();
                for pinned in &group.pinned_members {
                    if connected.contains(pinned) && edges.iter().any(|e| &e.node_id == pinned) {
                        set.insert(*pinned);
                    }
                }
                if set.is_empty() {
                    return Err(EdgeRouterError::NoMatchingEdges);
                }
                Ok(set.into_iter().collect())
            }
        }
    }
}
