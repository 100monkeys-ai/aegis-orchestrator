// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §D — operator-managed tag mutation.

use anyhow::Result;
use std::sync::Arc;

use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::shared_kernel::{NodeId, TenantId};

pub struct ManageTagsService {
    edge_repo: Arc<dyn EdgeDaemonRepository>,
}

impl ManageTagsService {
    pub fn new(edge_repo: Arc<dyn EdgeDaemonRepository>) -> Self {
        Self { edge_repo }
    }

    pub async fn add_tags(
        &self,
        tenant: &TenantId,
        node_id: NodeId,
        tags: Vec<String>,
    ) -> Result<Vec<String>> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("edge {node_id} not found"))?;
        if &edge.tenant_id != tenant {
            anyhow::bail!("cross-tenant tag mutation refused");
        }
        let mut current = edge.capabilities.tags;
        for t in tags {
            if !current.contains(&t) {
                current.push(t);
            }
        }
        self.edge_repo.update_tags(&node_id, &current).await?;
        Ok(current)
    }

    pub async fn remove_tags(
        &self,
        tenant: &TenantId,
        node_id: NodeId,
        tags: Vec<String>,
    ) -> Result<Vec<String>> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("edge {node_id} not found"))?;
        if &edge.tenant_id != tenant {
            anyhow::bail!("cross-tenant tag mutation refused");
        }
        let current: Vec<String> = edge
            .capabilities
            .tags
            .into_iter()
            .filter(|t| !tags.contains(t))
            .collect();
        self.edge_repo.update_tags(&node_id, &current).await?;
        Ok(current)
    }
}
