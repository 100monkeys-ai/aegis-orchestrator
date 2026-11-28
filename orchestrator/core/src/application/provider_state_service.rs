// Copyright (c) 2026 100monkeys.ai
// SPDX-License-Identifier: AGPL-3.0
//! Provider states (ADR-157 D11): register, version, read and archive a
//! tenant's declared provider objects. Planning and applying are workflow
//! steps (`provider_state_plan`, `provider_state_apply`), because each is a
//! provider tool call made as the run's person with the binding the run
//! chose; this service owns the resource.

use std::sync::Arc;

use crate::domain::provider_state::{
    ProviderState, ProviderStateManifest, ProviderStateRepository,
};
use crate::domain::repository::RepositoryError;
use crate::domain::shared_kernel::TenantId;

#[derive(Debug, thiserror::Error)]
pub enum ProviderStateError {
    #[error("the provider state is not valid: {}", .0.join("; "))]
    Invalid(Vec<String>),
    #[error("no provider state named {0}")]
    NotFound(String),
    #[error("{0}")]
    Repository(#[from] RepositoryError),
}

pub struct ProviderStateService {
    repo: Arc<dyn ProviderStateRepository>,
}

impl ProviderStateService {
    pub fn new(repo: Arc<dyn ProviderStateRepository>) -> Self {
        Self { repo }
    }

    /// Every problem with a manifest, or none.
    pub fn validate(yaml: &str) -> Vec<String> {
        match ProviderStateManifest::parse(yaml) {
            Ok(_) => Vec::new(),
            Err(p) => p,
        }
    }

    /// Store a manifest as the next active version of its name.
    pub async fn register(
        &self,
        tenant: &TenantId,
        yaml: &str,
        actor: &str,
    ) -> Result<ProviderState, ProviderStateError> {
        let manifest = ProviderStateManifest::parse(yaml).map_err(ProviderStateError::Invalid)?;
        Ok(self
            .repo
            .save_version(tenant, yaml, &manifest, actor)
            .await?)
    }

    pub async fn get(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> Result<ProviderState, ProviderStateError> {
        self.repo
            .get_active(tenant, name)
            .await?
            .ok_or_else(|| ProviderStateError::NotFound(name.to_string()))
    }

    pub async fn list(&self, tenant: &TenantId) -> Result<Vec<ProviderState>, ProviderStateError> {
        Ok(self.repo.list_active(tenant).await?)
    }

    pub async fn archive(&self, tenant: &TenantId, name: &str) -> Result<(), ProviderStateError> {
        if self.repo.archive(tenant, name).await? {
            Ok(())
        } else {
            Err(ProviderStateError::NotFound(name.to_string()))
        }
    }
}
