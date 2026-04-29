// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! ADR-117 §D — Edge group CRUD.

use chrono::Utc;
use std::sync::Arc;

use crate::domain::edge::{
    EdgeGroup, EdgeGroupId, EdgeGroupRepoError, EdgeGroupRepository, EdgeSelector,
};
use crate::domain::shared_kernel::{NodeId, TenantId};

pub struct ManageGroupsService {
    repo: Arc<dyn EdgeGroupRepository>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManageGroupError {
    #[error("group already exists for tenant")]
    Exists,
    #[error("group not found")]
    NotFound,
    #[error("cross-tenant access denied")]
    CrossTenant,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<EdgeGroupRepoError> for ManageGroupError {
    fn from(e: EdgeGroupRepoError) -> Self {
        match e {
            EdgeGroupRepoError::GroupExists => Self::Exists,
            EdgeGroupRepoError::NotFound => Self::NotFound,
            EdgeGroupRepoError::Other(e) => Self::Other(e),
        }
    }
}

impl ManageGroupsService {
    pub fn new(repo: Arc<dyn EdgeGroupRepository>) -> Self {
        Self { repo }
    }

    pub async fn create(
        &self,
        tenant: TenantId,
        name: String,
        selector: EdgeSelector,
        pinned: Vec<NodeId>,
        created_by: String,
    ) -> Result<EdgeGroup, ManageGroupError> {
        let group = EdgeGroup {
            id: EdgeGroupId::new(),
            tenant_id: tenant,
            name,
            selector,
            pinned_members: pinned,
            created_by,
            created_at: Utc::now(),
        };
        self.repo.create(&group).await?;
        Ok(group)
    }

    pub async fn get(
        &self,
        tenant: &TenantId,
        id: EdgeGroupId,
    ) -> Result<EdgeGroup, ManageGroupError> {
        let g = self
            .repo
            .get(&id)
            .await?
            .ok_or(ManageGroupError::NotFound)?;
        if &g.tenant_id != tenant {
            return Err(ManageGroupError::CrossTenant);
        }
        Ok(g)
    }

    pub async fn list(&self, tenant: &TenantId) -> Result<Vec<EdgeGroup>, ManageGroupError> {
        Ok(self.repo.list_by_tenant(tenant).await?)
    }

    pub async fn update(
        &self,
        tenant: &TenantId,
        mut group: EdgeGroup,
    ) -> Result<EdgeGroup, ManageGroupError> {
        if &group.tenant_id != tenant {
            return Err(ManageGroupError::CrossTenant);
        }
        group.tenant_id = tenant.clone();
        self.repo.update(&group).await?;
        Ok(group)
    }

    pub async fn delete(&self, tenant: &TenantId, id: EdgeGroupId) -> Result<(), ManageGroupError> {
        let g = self
            .repo
            .get(&id)
            .await?
            .ok_or(ManageGroupError::NotFound)?;
        if &g.tenant_id != tenant {
            return Err(ManageGroupError::CrossTenant);
        }
        self.repo.delete(&id).await?;
        Ok(())
    }
}
