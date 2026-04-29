// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §D — operator-managed tag mutation.

use std::sync::Arc;

use crate::domain::edge::EdgeDaemonRepository;
use crate::domain::shared_kernel::{NodeId, TenantId};

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum ManageTagsError {
    #[error("edge daemon not found")]
    NotFound,
    #[error("cross-tenant tag mutation refused")]
    Forbidden,
    #[error("repository error: {0}")]
    Repo(String),
}

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
    ) -> Result<Vec<String>, ManageTagsError> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await
            .map_err(|e| ManageTagsError::Repo(e.to_string()))?
            .ok_or(ManageTagsError::NotFound)?;
        if &edge.tenant_id != tenant {
            return Err(ManageTagsError::Forbidden);
        }
        let mut current = edge.capabilities.tags;
        for t in tags {
            if !current.contains(&t) {
                current.push(t);
            }
        }
        self.edge_repo
            .update_tags(&node_id, &current)
            .await
            .map_err(|e| ManageTagsError::Repo(e.to_string()))?;
        Ok(current)
    }

    pub async fn remove_tags(
        &self,
        tenant: &TenantId,
        node_id: NodeId,
        tags: Vec<String>,
    ) -> Result<Vec<String>, ManageTagsError> {
        let edge = self
            .edge_repo
            .get(&node_id)
            .await
            .map_err(|e| ManageTagsError::Repo(e.to_string()))?
            .ok_or(ManageTagsError::NotFound)?;
        if &edge.tenant_id != tenant {
            return Err(ManageTagsError::Forbidden);
        }
        let current: Vec<String> = edge
            .capabilities
            .tags
            .into_iter()
            .filter(|t| !tags.contains(t))
            .collect();
        self.edge_repo
            .update_tags(&node_id, &current)
            .await
            .map_err(|e| ManageTagsError::Repo(e.to_string()))?;
        Ok(current)
    }
}
